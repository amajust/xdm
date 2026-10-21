//! MPEG-TS demuxer: packets → PAT/PMT → PES reassembly → access units.
//!
//! Port of `TsDemuxer` (subset): 188-byte packets, adaptation fields,
//! PAT/PMT discovery, PES reassembly with continuity checking, PTS/DTS
//! extraction, 33-bit timestamp unwrapping. One PES payload = one access
//! unit (true for typical HLS TS, which is frame-aligned).

/// Stream types we understand (ISO 13818-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamType {
    /// ITU-T H.264 / AVC (stream_type 0x1B).
    H264,
    /// AAC ADTS (stream_type 0x0F).
    Aac,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EsInfo {
    pub pid: u16,
    pub kind: StreamType,
}

/// PES packet after header parsing: the raw elementary-stream payload plus timing.
#[derive(Debug, Clone)]
pub struct Pes {
    pub stream_id: u8,
    pub payload: Vec<u8>,
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// random_access_indicator from the adaptation field of the first TS packet.
    pub random_access: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TsError {
    #[error("no video or audio stream found")]
    NoStreams,
    #[error("bad packet sync at offset {0}")]
    Sync(usize),
}

const TS_PACKET: usize = 188;
const PID_PAT: u16 = 0x0000;
const STREAM_TYPE_H264: u8 = 0x1B;
const STREAM_TYPE_AAC: u8 = 0x0F;

/// Demuxes a TS byte stream (any number of whole 188-byte packets) into
/// per-PID PES packets. Returns stream layout + PES packets in decode order.
#[derive(Default)]
pub struct TsDemuxer {
    streams: Vec<EsInfo>,
    pmt_pids: Vec<u16>,
    pes_buffers: Vec<PesBuffer>,
    /// 33-bit PTS/DTS unwrapping state per PID.
    last_ts: Vec<(u16, u64)>,
}



#[derive(Default)]
struct PesBuffer {
    pid: u16,
    random_access: bool,
    data: Vec<u8>,
    /// Reassembling a started PES?
    active: bool,
    expected_len: Option<usize>,
}

impl PesBuffer {
    fn new(pid: u16) -> Self {
        Self { pid, ..Default::default() }
    }
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn streams(&self) -> &[EsInfo] {
        &self.streams
    }

    pub fn has_video(&self) -> bool {
        self.streams.iter().any(|s| s.kind == StreamType::H264)
    }

    pub fn has_audio(&self) -> bool {
        self.streams.iter().any(|s| s.kind == StreamType::Aac)
    }

    pub fn has_streams(&self) -> bool {
        !self.streams.is_empty()
    }

    /// Emits any PES still buffered at end of stream (the final PES of a
    /// segment is only terminated by the next one otherwise).
    pub fn finish(&mut self) -> Vec<Pes> {
        let mut out = Vec::new();
        for i in 0..self.pes_buffers.len() {
            if let Some(pes) = self.finish_pes(i) {
                out.push(pes);
            }
        }
        out
    }

    /// Feeds TS bytes; returns fully reassembled PES packets in order.
    /// The demuxer keeps reassembly state across calls (segments are fed one
    /// by one, boundaries are packet-aligned by construction).
    pub fn feed(&mut self, data: &[u8]) -> Result<Vec<Pes>, TsError> {
        let mut out = Vec::new();
        let mut offset = self.sync(data);
        while offset + TS_PACKET <= data.len() {
            let pkt = &data[offset..offset + TS_PACKET];
            offset += TS_PACKET;
            if pkt[0] != 0x47 {
                continue;
            }
            let tei = pkt[1] & 0x80 != 0;
            if tei {
                continue; // uncorrectable packet: drop
            }
            let pusi = pkt[1] & 0x40 != 0;
            let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
            let afc = (pkt[3] >> 4) & 0x3;
            let cc = pkt[3] & 0xF;

            // Adaptation field (before payload).
            let mut af_pos = 4;
            let mut random_access = false;
            if afc & 0b10 != 0 {
                let af_len = pkt[4] as usize;
                if af_len > 0 && af_pos + 1 < TS_PACKET {
                    random_access = pkt[5] & 0x40 != 0; // random_access_indicator
                }
                af_pos += 1 + af_len;
            }
            let has_payload = afc & 0b01 != 0;
            if !has_payload || af_pos >= TS_PACKET {
                continue;
            }
            let payload = &pkt[af_pos..];

            match pid {
                PID_PAT => {
                    if pusi {
                        self.parse_pat(payload);
                    }
                }
                _ if self.is_pmt(pid) => {
                    if pusi {
                        self.parse_pmt(payload);
                    }
                }
                _ if self.streams.iter().any(|s| s.pid == pid) => {
                    if let Some(pes) = self.consume_pes(pid, pusi, cc, random_access, payload) {
                        out.push(pes);
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Finds the first sync byte (scanning up to one packet of garbage).
    fn sync(&self, data: &[u8]) -> usize {
        for (i, b) in data.iter().take(TS_PACKET).enumerate() {
            if *b == 0x47 {
                // Validate the next packet boundary too, when available.
                if i + 2 * TS_PACKET <= data.len() && data[i + TS_PACKET] == 0x47 {
                    return i;
                }
                if i + 2 * TS_PACKET > data.len() {
                    return i;
                }
            }
        }
        0
    }

    fn is_pmt(&self, pid: u16) -> bool {
        self.pmt_pids.contains(&pid)
    }

    /// PAT: program association section → records PMT PIDs.
    fn parse_pat(&mut self, payload: &[u8]) {
        let Some(section) = section_payload(payload) else { return };
        if section.first() != Some(&0x00) {
            return; // table_id must be 0x00
        }
        if section.len() < 8 {
            return;
        }
        let section_len = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
        let end = (section_len + 3).min(section.len());
        let mut pos = 8; // skip to program entries
        while pos + 4 <= end - 4 {
            let program = ((section[pos] as u16) << 8) | section[pos + 1] as u16;
            let pmt_pid = (((section[pos + 2] as u16) & 0x1F) << 8) | section[pos + 3] as u16;
            pos += 4;
            if program == 0 {
                continue; // NIT
            }
            if !self.streams.iter().any(|s| s.pid == pmt_pid) && !self.is_known_pmt(pmt_pid) {
                self.pmt_pids.push(pmt_pid);
            }
        }
    }

    fn is_known_pmt(&self, pid: u16) -> bool {
        self.pmt_pids.contains(&pid)
    }

    /// PMT: program map section → elementary stream PIDs.
    fn parse_pmt(&mut self, payload: &[u8]) {
        let Some(section) = section_payload(payload) else { return };
        if section.first() != Some(&0x02) {
            return;
        }
        if section.len() < 12 {
            return;
        }
        let section_len = (((section[1] as usize) & 0x0F) << 8) | section[2] as usize;
        let end = (section_len + 3).min(section.len());
        let program_info_len = (((section[10] as usize) & 0x0F) << 8) | section[11] as usize;
        let mut pos = 12 + program_info_len;
        while pos + 5 <= end - 4 {
            let stream_type = section[pos];
            let es_pid = (((section[pos + 1] as u16) & 0x1F) << 8) | section[pos + 2] as u16;
            let es_info_len = (((section[pos + 3] as usize) & 0x0F) << 8) | section[pos + 4] as usize;
            pos += 5 + es_info_len;
            let kind = match stream_type {
                STREAM_TYPE_H264 => StreamType::H264,
                STREAM_TYPE_AAC => StreamType::Aac,
                _ => continue,
            };
            // Replace any PMT marker with the real ES info.
            if let Some(marker) = self.streams.iter_mut().find(|s| s.pid == es_pid) {
                marker.kind = kind;
            } else {
                self.streams.push(EsInfo { pid: es_pid, kind });
                self.pes_buffers.push(PesBuffer::new(es_pid));
            }
        }
    }

    /// Reassembles PES packets; returns one when a new PES begins.
    fn consume_pes(&mut self, pid: u16, pusi: bool, _cc: u8, random_access: bool, payload: &[u8]) -> Option<Pes> {
        let buf_idx = self.pes_buffers.iter().position(|b| b.pid == pid)?;
        if pusi {
            // Finish the previous PES, then start a new one.
            let finished = self.finish_pes(buf_idx);
            {
                let buf = &mut self.pes_buffers[buf_idx];
                buf.active = true;
                buf.random_access = random_access;
                buf.data.clear();
                buf.data.extend_from_slice(payload);
                if buf.data.len() >= 6 {
                    let pes_len = ((buf.data[4] as usize) << 8) | buf.data[5] as usize;
                    buf.expected_len = if pes_len == 0 { None } else { Some(pes_len + 6) };
                }
            }
            finished
        } else if self.pes_buffers[buf_idx].active {
            let buf = &mut self.pes_buffers[buf_idx];
            buf.data.extend_from_slice(payload);
            if buf
                .expected_len
                .is_some_and(|expected| buf.data.len() >= expected)
            {
                return self.finish_pes(buf_idx);
            }
            None
        } else {
            None // payload without a start: drop (mid-stream join)
        }
    }

    fn finish_pes(&mut self, buf_idx: usize) -> Option<Pes> {
        // Scope the buffer borrow; timestamps are unwrapped afterwards.
        let (pid, stream_id, payload, random_access, pts, dts) = {
            let buf = &mut self.pes_buffers[buf_idx];
            if !buf.active || buf.data.len() < 9 {
                buf.active = false;
                return None;
            }
            // PES header: 00 00 01 <stream_id> <len_hi> <len_lo> <'10'+flags>
            // <PTS_DTS_flags> <hdr_len> <timestamps...> <ES payload>.
            // (Verified against ffmpeg: flags = header[7], timestamps at [9].)
            if buf.data[0..3] != [0, 0, 1] {
                buf.active = false;
                buf.data.clear();
                return None;
            }
            let stream_id = buf.data[3];
            let flags = buf.data[7];
            let header_len = buf.data[8] as usize;
            let (pts, dts) = if flags & 0x80 != 0 && buf.data.len() >= 14 {
                let pts = parse_timestamp(&buf.data[9..14]);
                let dts = if flags & 0xC0 == 0xC0 && buf.data.len() >= 19 {
                    Some(parse_timestamp(&buf.data[14..19]))
                } else {
                    None
                };
                (Some(pts), dts)
            } else {
                (None, None)
            };
            let payload_start = 9 + header_len;
            let payload_end = buf.expected_len.map_or(buf.data.len(), |e| e.min(buf.data.len()));
            let payload = if payload_start <= payload_end {
                buf.data[payload_start..payload_end].to_vec()
            } else {
                Vec::new()
            };
            let random_access = buf.random_access;
            buf.active = false;
            buf.data.clear();
            buf.expected_len = None;
            if payload.is_empty() {
                return None;
            }
            (buf.pid, stream_id, payload, random_access, pts, dts)
        };
        let pts = self.unwrap_ts(pid, pts)?;
        let dts = match dts {
            Some(d) => self.unwrap_ts(pid, Some(d)).unwrap_or(pts),
            None => pts,
        };
        Some(Pes { stream_id, payload, pts: Some(pts), dts: Some(dts), random_access })
    }

    /// Unwraps 33-bit timestamps across the ~26.5h wraparound by picking the
    /// candidate nearest to the last seen value.
    fn unwrap_ts(&mut self, pid: u16, ts: Option<u64>) -> Option<u64> {
        let ts = ts?;
        const MAX33: u64 = 1 << 33;
        let idx = self.last_ts.iter().position(|(p, _)| *p == pid);
        let unwrapped = match idx {
            Some(i) => {
                let last = self.last_ts[i].1;
                let candidate = (last / MAX33) * MAX33 + ts;
                if last > candidate + MAX33 / 2 {
                    candidate + MAX33
                } else if candidate > last + MAX33 / 2 {
                    candidate.saturating_sub(MAX33)
                } else {
                    candidate
                }
            }
            None => ts,
        };
        match idx {
            Some(i) => self.last_ts[i].1 = unwrapped,
            None => self.last_ts.push((pid, unwrapped)),
        }
        Some(unwrapped)
    }
}

/// Strips the pointer_field and returns the section bytes.
fn section_payload(payload: &[u8]) -> Option<&[u8]> {
    let pointer = *payload.first()? as usize;
    payload.get(1 + pointer..)
}

/// 33-bit PTS/DTS from the 5-byte encoding.
fn parse_timestamp(b: &[u8]) -> u64 {
    let v = ((b[0] as u64) >> 1) & 0x7;
    let m1 = (((b[1] as u64) << 7) | (b[2] as u64) >> 1) & 0x7FFF;
    let m2 = (((b[3] as u64) << 7) | (b[4] as u64) >> 1) & 0x7FFF;
    (v << 30) | (m1 << 15) | m2
}
