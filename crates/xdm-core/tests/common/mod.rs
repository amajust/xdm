//! Shared helpers for E2E tests: a synthetic MPEG-TS builder (PAT/PMT/PES
//! with valid H.264 Annex-B and AAC ADTS payloads) and MP4 box utilities.

// Shared across test binaries; not every binary uses every helper.
#![allow(dead_code)]

use std::collections::HashMap;

/// Builds a parseable SPS: 32×32, profile 66, level 30.
pub fn synthetic_sps() -> Vec<u8> {
    let mut bits: Vec<bool> = Vec::new();
    let push_bits = |v: u64, n: u32, bits: &mut Vec<bool>| {
        for i in (0..n).rev() {
            bits.push((v >> i) & 1 == 1);
        }
    };
    fn push_ue(v: u64, bits: &mut Vec<bool>) {
        let x = v + 1;
        let nbits = 64 - x.leading_zeros();
        for _ in 0..nbits - 1 {
            bits.push(false);
        }
        for i in (0..nbits).rev() {
            bits.push((x >> i) & 1 == 1);
        }
    }
    push_bits(66, 8, &mut bits);
    push_bits(0, 8, &mut bits);
    push_bits(30, 8, &mut bits);
    push_ue(0, &mut bits); // sps_id
    push_ue(0, &mut bits); // log2_max_frame_num_minus4
    push_ue(0, &mut bits); // pic_order_cnt_type = 0
    push_ue(0, &mut bits); // log2_max_pic_order_cnt_lsb_minus4
    push_ue(0, &mut bits); // max_num_ref_frames
    bits.push(false); // gaps_in_frame_num_value_allowed
    push_ue(1, &mut bits); // pic_width_in_mbs → 2 → 32 px
    push_ue(1, &mut bits); // pic_height_in_map_units → 2 → 32 px
    bits.push(true); // frame_mbs_only_flag
    bits.push(false); // frame_cropping_flag
    bits.push(true); // rbsp stop bit
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let mut sps = vec![0x67];
    let mut byte = 0u8;
    for (i, b) in bits.iter().enumerate() {
        if *b {
            byte |= 1 << (7 - i % 8);
        }
        if i % 8 == 7 {
            sps.push(byte);
            byte = 0;
        }
    }
    sps
}

/// One video PES body: SPS + PPS + IDR slice (Annex-B).
pub fn synthetic_video_pes_body(idr: bool) -> Vec<u8> {
    let sps = synthetic_sps();
    let pps = vec![0x68, 0xEE, 0x3C, 0x80];
    let slice_type: u8 = if idr { 0x65 } else { 0x41 };
    let mut body = vec![0, 0, 0, 1];
    body.extend_from_slice(&sps);
    body.extend_from_slice(&[0, 0, 0, 1]);
    body.extend_from_slice(&pps);
    body.extend_from_slice(&[0, 0, 0, 1]);
    body.extend_from_slice(&[slice_type, 0x88, 0x84, 0x21, 0xA0]);
    body
}

/// One audio PES body: two ADTS frames (AAC-LC, 48 kHz, mono).
pub fn synthetic_audio_pes_body(seq: u8) -> Vec<u8> {
    let frame = |payload: &[u8]| {
        let frame_len = 7 + payload.len();
        let mut f = vec![0xFF, 0xF1];
        f.push((1 << 6) | (3 << 2)); // AAC-LC, 48 kHz (idx 3), channel hi=0
        f.push((0x01 << 6) | (((frame_len >> 11) & 0x3) as u8)); // channel lo=01
        f.push(((frame_len >> 3) & 0xFF) as u8);
        f.push(((frame_len & 0x7) as u8) << 5);
        f.push(0);
        f.extend_from_slice(payload);
        f
    };
    let mut body = frame(&[seq; 32]);
    body.extend(frame(&[seq.wrapping_add(10); 32]));
    body
}

/// Builds TS packets: PAT + PMT (H.264 @ 256, AAC @ 257) and PES packets.
pub struct TsBuilder {
    counters: HashMap<u16, u8>,
}

impl TsBuilder {
    pub fn new() -> Self {
        Self { counters: HashMap::new() }
    }

    pub fn pat(&mut self) -> Vec<u8> {
        let section: Vec<u8> = vec![
            0x00, 0x00, 0xB0, 0x0D, 0x00, 0x01, 0xC1, 0x00, 0x00, 0x00, 0x01, 0xF0, 0x00, 0x12,
            0x34, 0x56, 0x78,
        ];
        self.raw_packet(0x0000, true, &section, false)
    }

    pub fn pmt(&mut self) -> Vec<u8> {
        let mut section = vec![
            0x00, 0x02, 0xB0, 0x17, 0x00, 0x01, 0xC1, 0x00, 0x00, 0xE1, 0x00, 0xF0, 0x00,
        ];
        section.extend_from_slice(&[0x1B, 0xE1, 0x00, 0xF0, 0x00]); // H.264 @ 256
        section.extend_from_slice(&[0x0F, 0xE1, 0x01, 0xF0, 0x00]); // AAC @ 257
        section.extend_from_slice(&[0x9A, 0xBC, 0xDE, 0xF0]); // CRC (ignored)
        self.raw_packet(0x1000, true, &section, false)
    }

    /// Emits the TS packets carrying one PES with a PTS.
    pub fn pes(&mut self, pid: u16, stream_id: u8, payload: &[u8], pts: u64, random_access: bool) -> Vec<Vec<u8>> {
        // Standard PES header: prefix, sid, length, '10' marker byte,
        // PTS_DTS_flags byte, header_data_length, timestamps, payload.
        let mut pes = vec![0, 0, 1, stream_id];
        let pes_len = 3 + 5 + payload.len();
        pes.extend_from_slice(&(pes_len as u16).to_be_bytes());
        pes.push(0x80); // '10' marker + flags
        pes.push(0x80); // PTS_DTS_flags: PTS only
        pes.push(5); // header data length
        pes.extend_from_slice(&pts_bytes(pts));
        pes.extend_from_slice(payload);

        let mut packets = Vec::new();
        let mut off = 0;
        let mut first = true;
        while off < pes.len() || first {
            let take = 184.min(pes.len() - off);
            let chunk = &pes[off..off + take];
            off += take;
            packets.push(self.raw_packet(pid, first, chunk, random_access && first));
            first = false;
            if off >= pes.len() {
                break;
            }
        }
        packets
    }

    fn raw_packet(&mut self, pid: u16, pusi: bool, payload: &[u8], random_access: bool) -> Vec<u8> {
        let cc = self.counters.entry(pid).or_insert(0);
        let mut pkt = vec![0u8; 188];
        pkt[0] = 0x47;
        pkt[1] = (if pusi { 0x40 } else { 0 }) | ((pid >> 8) as u8 & 0x1F);
        pkt[2] = (pid & 0xFF) as u8;
        if random_access {
            pkt[3] = 0x30; // AF + payload
            pkt[4] = 1;
            pkt[5] = 0x40; // random_access_indicator
        } else {
            pkt[3] = 0x10; // payload only
        }
        let start = if random_access { 6 } else { 4 };
        pkt[start..start + payload.len()].copy_from_slice(payload);
        pkt[3] |= *cc & 0x0F;
        *cc = (*cc + 1) & 0x0F;
        pkt
    }
}

fn pts_bytes(pts: u64) -> [u8; 5] {
    [
        (0x21 | ((pts >> 29) & 0x0E) as u8),
        (pts >> 22) as u8,
        (0x01 | ((pts >> 14) & 0xFE) as u8),
        (pts >> 7) as u8,
        (0x01 | ((pts << 1) & 0xFE) as u8),
    ]
}

/// Walks top-level MP4 boxes: (kind, offset, size).
pub fn top_level_boxes(data: &[u8]) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size = u32::from_be_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > data.len() {
            break;
        }
        let kind = String::from_utf8_lossy(&data[pos + 4..pos + 8]).to_string();
        out.push((kind, pos, size));
        pos += size;
    }
    out
}

pub fn find_marker(data: &[u8], marker: &[u8]) -> bool {
    data.windows(marker.len()).any(|w| w == marker)
}
