//! Remux orchestration: TS segment files → fragmented MP4.
//!
//! The video ES is treated as **continuous**: muxers routinely split a frame
//! mid-NALU across PES packets (large frames exceed what fits before a packet
//! boundary), so each PES payload is appended to a running ES buffer that is
//! re-split into NALUs by start codes. NALUs are grouped into access units
//! (AUD-anchored, with a slice-after-slice fallback for AUD-less streams),
//! and every sample inherits the PTS/DTS of the PES that carries its first
//! byte. Returns `Ok(None)` when no usable H.264/AAC media is found — the
//! caller should fall back to plain TS concatenation.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use super::aac;
use super::h264::{self, H264Config};
use super::mp4::Mp4Writer;
use super::ts::{Pes, TsDemuxer};
use super::{Sample, TrackInfo, TrackKind};

const VIDEO_TIMESCALE: u64 = 90000;
const FALLBACK_FRAME_TICKS: u64 = VIDEO_TIMESCALE / 30;

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_IDR: u8 = 5;
const NAL_AUD: u8 = 9;

#[derive(Debug, Clone)]
pub struct MuxOutcome {
    pub video: Option<TrackInfo>,
    pub audio: Option<TrackInfo>,
}

/// An access unit under construction.
struct Au {
    /// Global ES offset of the AU's first (non-AUD) byte; drives the dts lookup.
    start: Option<u64>,
    /// AVCC-style (length-prefixed) NALU payload.
    data: Vec<u8>,
    keyframe: bool,
    has_slice: bool,
}

struct Muxer {
    demuxer: TsDemuxer,
    video_cfg: Option<H264Config>,
    audio_cfg: Option<aac::AdtsConfig>,
    writer: Option<Mp4Writer<BufWriter<File>>>,
    /// ES bytes not yet split into NALUs. After the first NALU this always
    /// begins with a start code (or holds one open NALU's bytes).
    es: Vec<u8>,
    /// Global ES offset of `es[0]`.
    es_base: u64,
    /// (global offset, dts, pts) at each video PES payload start.
    pes_marks: Vec<(u64, u64, u64)>,
    au: Option<Au>,
    /// Completed samples ready to be written.
    video_out: Vec<Sample>,
    audio_out: Vec<Sample>,
    /// Newest completed AU awaiting the next AU's dts (to fix its duration).
    pending: Option<Sample>,
    /// Duration of the most recently completed sample (for the final one).
    last_dur: u64,
    base_ts: Option<u64>,
    tracks: Vec<TrackInfo>,
    out_path: PathBuf,
}

impl Muxer {
    fn new(out_path: &Path) -> Self {
        Self {
            demuxer: TsDemuxer::new(),
            video_cfg: None,
            audio_cfg: None,
            writer: None,
            es: Vec::new(),
            es_base: 0,
            pes_marks: Vec::new(),
            au: None,
            video_out: Vec::new(),
            audio_out: Vec::new(),
            pending: None,
            last_dur: 0,
            base_ts: None,
            tracks: Vec::new(),
            out_path: out_path.to_path_buf(),
        }
    }

    fn feed_file(&mut self, path: &Path) -> Result<(), String> {
        // Cut the PREVIOUS segment's fragment first: its tail PES was already
        // consumed, so every AU it contained is complete. (The final AU of a
        // segment gets the previous frame's duration — exact for CFR streams.)
        if self.writer.is_some() {
            self.flush_and_cut()?;
        }
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let pes_packets = self.demuxer.feed(&data).map_err(|e| e.to_string())?;
        for pes in pes_packets {
            self.consume_pes(pes);
        }
        // TS segments are self-contained: the last PES of the file would
        // otherwise stay buffered (PES are only terminated by the next one).
        let tail = self.demuxer.finish();
        for pes in tail {
            self.consume_pes(pes);
        }
        if self.writer.is_none() && self.configs_ready() {
            self.open_writer()?;
        }
        Ok(())
    }

    fn consume_pes(&mut self, pes: Pes) {
        let is_video = (0xE0..=0xEF).contains(&pes.stream_id);
        let is_audio = (0xC0..=0xDF).contains(&pes.stream_id);
        if !is_video && !is_audio {
            return;
        }
        // Timestamp normalization: everything is shifted by the first dts
        // seen, keeping A/V sync intact.
        let dts_raw = pes.dts.unwrap_or(0);
        let base = *self.base_ts.get_or_insert(dts_raw);
        let dts = dts_raw.saturating_sub(base);
        let pts = pes.pts.unwrap_or(dts_raw).saturating_sub(base);

        if is_video {
            let mark = self.es_base + self.es.len() as u64;
            self.pes_marks.push((mark, dts, pts));
            self.es.extend_from_slice(&pes.payload);
            self.drain_es();
        } else if aac::parse_adts(&pes.payload).is_some() {
            if self.audio_cfg.is_none() {
                self.audio_cfg = aac::parse_adts(&pes.payload).map(|(c, _)| c);
            }
            self.audio_out
                .extend(aac::adts_to_samples(&pes.payload, dts));
        }
    }

    /// Splits complete NALUs off the ES buffer and groups them.
    fn drain_es(&mut self) {
        loop {
            if self.es.len() < 3 {
                break;
            }
            let sc = match find_start_code(&self.es) {
                Some(p) => p,
                None => break,
            };
            let body_start = sc + 3;
            // A NALU is only complete once the next start code appears.
            let next = match find_start_code(&self.es[body_start..]) {
                Some(rel) => body_start + rel,
                None => break,
            };
            // The zero run before the next start code belongs to that code.
            let mut end = next;
            while end > body_start && self.es[end - 1] == 0 {
                end -= 1;
            }
            if body_start < end {
                let nalu = self.es[body_start..end].to_vec();
                let off = self.es_base + body_start as u64;
                self.consume_nalu(&nalu, off);
            }
            self.es.drain(..next);
            self.es_base += next as u64;
        }
    }

    fn consume_nalu(&mut self, nalu: &[u8], off: u64) {
        let ntype = nalu[0] & 0x1F;
        match ntype {
            NAL_SPS => {
                if self
                    .video_cfg
                    .as_ref()
                    .is_none_or(|c| c.sps.is_none())
                {
                    let cfg = self.video_cfg.get_or_insert_with(H264Config::default);
                    if let Some((w, h, p, l)) = h264::parse_sps_geometry(nalu) {
                        cfg.width = w;
                        cfg.height = h;
                        cfg.profile = p;
                        cfg.level = l;
                    }
                    cfg.sps = Some(nalu.to_vec());
                }
            }
            NAL_PPS => {
                if self.video_cfg.as_ref().is_none_or(|c| c.pps.is_none()) {
                    self.video_cfg.get_or_insert_with(H264Config::default).pps = Some(nalu.to_vec());
                }
            }
            NAL_AUD => {
                // The AUD anchors a new access unit; it is dropped from the
                // sample payload (redundant in MP4).
                self.finish_au();
                self.au = Some(Au {
                    start: None,
                    data: Vec::new(),
                    keyframe: false,
                    has_slice: false,
                });
            }
            t if (1..=5).contains(&t) => {
                // VCL NALU. Without AUDs, a slice following a sliced AU starts
                // the next access unit.
                let starts_new = match &self.au {
                    Some(au) => au.has_slice,
                    None => true,
                };
                if starts_new {
                    self.finish_au();
                    self.au = Some(Au {
                        start: None,
                        data: Vec::new(),
                        keyframe: false,
                        has_slice: false,
                    });
                }
                let au = self.au.as_mut().expect("au created above");
                if au.start.is_none() {
                    au.start = Some(off);
                }
                au.has_slice = true;
                if t == NAL_IDR {
                    au.keyframe = true;
                }
                push_length_prefixed(&mut au.data, nalu);
            }
            _ => {
                // SEI / filler / other: attach to the current AU.
                if self.au.is_none() {
                    self.au = Some(Au {
                        start: None,
                        data: Vec::new(),
                        keyframe: false,
                        has_slice: false,
                    });
                }
                let au = self.au.as_mut().expect("au created above");
                if au.start.is_none() {
                    au.start = Some(off);
                }
                push_length_prefixed(&mut au.data, nalu);
            }
        }
    }

    /// Closes the current AU into `pending` (resolving the previous pending
    /// sample's duration now that this AU's dts is known).
    fn finish_au(&mut self) {
        let Some(au) = self.au.take() else { return };
        if !au.has_slice {
            return; // parameter-set/SEI-only tail: nothing decodable
        }
        let start = au.start.unwrap_or(0);
        let (dts, pts) = self.lookup_ts(start);
        // Guard monotonicity (several AUs inside one PES would collide).
        let dts = match self.pending.as_ref().map(|p| p.dts) {
            Some(prev) if dts <= prev => {
                let step = self
                    .video_out
                    .last()
                    .map(|s| s.duration)
                    .filter(|&d| d > 0)
                    .unwrap_or(1);
                prev + step
            }
            _ => dts,
        };
        let sample = Sample {
            data: au.data,
            dts,
            cts: pts.saturating_sub(dts),
            duration: 0,
            keyframe: au.keyframe,
        };
        if let Some(mut prev) = self.pending.take() {
            prev.duration = sample.dts.saturating_sub(prev.dts);
            self.last_dur = prev.duration;
            self.video_out.push(prev);
        }
        self.pending = Some(sample);
    }

    /// dts/pts of the PES whose payload contains `off`.
    fn lookup_ts(&self, off: u64) -> (u64, u64) {
        let idx = self.pes_marks.partition_point(|m| m.0 <= off);
        match idx.checked_sub(1).and_then(|i| self.pes_marks.get(i)) {
            Some(&(_, dts, pts)) => (dts, pts),
            None => (0, 0),
        }
    }

    fn configs_ready(&self) -> bool {
        if self.demuxer.streams().is_empty() {
            return false;
        }
        let video_ok = !self.demuxer.has_video()
            || self
                .video_cfg
                .as_ref()
                .is_some_and(|c| c.sps.is_some() && c.pps.is_some());
        let audio_ok = !self.demuxer.has_audio() || self.audio_cfg.is_some();
        video_ok && audio_ok
    }

    fn open_writer(&mut self) -> Result<(), String> {
        let mut tracks: Vec<TrackInfo> = Vec::new();
        if let Some(cfg) = &self.video_cfg {
            let avcc = h264::build_avcc(cfg).ok_or_else(|| "video config incomplete".to_owned())?;
            tracks.push(TrackInfo {
                kind: TrackKind::Video,
                timescale: VIDEO_TIMESCALE as u32,
                width: cfg.width,
                height: cfg.height,
                codec_config: avcc,
            });
        }
        if let Some(cfg) = &self.audio_cfg {
            tracks.push(TrackInfo {
                kind: TrackKind::Audio,
                timescale: cfg.sample_rate,
                width: 0,
                height: 0,
                codec_config: aac::build_asc(cfg).to_vec(),
            });
        }
        if tracks.is_empty() {
            return Err("no tracks to mux".to_owned());
        }
        self.tracks = tracks.clone();
        let file = std::fs::File::create(&self.out_path).map_err(|e| e.to_string())?;
        let buf = BufWriter::with_capacity(1024 * 1024, file);
        self.writer = Some(Mp4Writer::new(buf, tracks).map_err(|e| e.to_string())?);
        Ok(())
    }

    /// Writes all completed samples and cuts one fragment per segment.
    fn flush_and_cut(&mut self) -> Result<(), String> {
        let Some(writer) = self.writer.as_mut() else { return Ok(()) };
        // The pending sample gets the previous frame's duration (frames are
        // regular; its exact value would only arrive with the next AU, which
        // belongs to the next fragment).
        if let Some(mut p) = self.pending.take() {
            p.duration = if self.last_dur > 0 { self.last_dur } else { FALLBACK_FRAME_TICKS };
            self.video_out.push(p);
        }
        let mut vid = std::mem::take(&mut self.video_out);
        let mut aud = std::mem::take(&mut self.audio_out);
        for (i, kind) in writer.track_kinds().into_iter().enumerate() {
            let samples = match kind {
                TrackKind::Video => std::mem::take(&mut vid),
                TrackKind::Audio => std::mem::take(&mut aud),
            };
            if !samples.is_empty() {
                writer.write_samples(i, samples);
            }
        }
        writer.end_fragment().map_err(|e| e.to_string())
    }

    fn finish(mut self) -> Result<Option<MuxOutcome>, String> {
        if self.writer.is_none() {
            // No usable H.264/AAC media was found.
            return Ok(None);
        }
        // The stream's final PES and NALU never see a following start code.
        for pes in self.demuxer.finish() {
            self.consume_pes(pes);
        }
        let rest = std::mem::take(&mut self.es);
        if let Some(pos) = find_start_code(&rest) {
            let mut nalu = rest[pos + 3..].to_vec();
            while nalu.last() == Some(&0) {
                nalu.pop(); // trailing stuffing zeros
            }
            if !nalu.is_empty() {
                let off = self.es_base + (pos + 3) as u64;
                self.consume_nalu(&nalu, off);
            }
        }
        self.finish_au();
        if let Some(mut p) = self.pending.take() {
            p.duration = if self.last_dur > 0 { self.last_dur } else { FALLBACK_FRAME_TICKS };
            self.video_out.push(p);
        }

        let mut writer = self.writer.take().expect("writer checked above");
        let mut vid = std::mem::take(&mut self.video_out);
        let mut aud = std::mem::take(&mut self.audio_out);
        for (i, kind) in writer.track_kinds().into_iter().enumerate() {
            let samples = match kind {
                TrackKind::Video => std::mem::take(&mut vid),
                TrackKind::Audio => std::mem::take(&mut aud),
            };
            if !samples.is_empty() {
                writer.write_samples(i, samples);
            }
        }
        writer.finish().map_err(|e| e.to_string())?;

        let mut video = None;
        let mut audio = None;
        for t in &self.tracks {
            match t.kind {
                TrackKind::Video => video = Some(t.clone()),
                TrackKind::Audio => audio = Some(t.clone()),
            }
        }
        Ok(Some(MuxOutcome { video, audio }))
    }
}

fn push_length_prefixed(out: &mut Vec<u8>, nalu: &[u8]) {
    out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
    out.extend_from_slice(nalu);
}

/// First 00 00 01 start code in `data`.
fn find_start_code(data: &[u8]) -> Option<usize> {
    if data.len() < 3 {
        return None;
    }
    (0..=data.len() - 3).find(|&i| data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1)
}

/// Demuxes `segment_files` in order and writes a fragmented MP4 to `out_path`.
pub fn remux_ts_to_mp4(
    segment_files: &[PathBuf],
    out_path: &Path,
) -> Result<Option<MuxOutcome>, String> {
    let mut muxer = Muxer::new(out_path);
    for file in segment_files {
        muxer.feed_file(file)?;
    }
    muxer.finish()
}
