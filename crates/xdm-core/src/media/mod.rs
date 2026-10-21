//! Media remuxing: MPEG-TS → fragmented MP4, pure Rust (port of
//! `TransmuxingMuxer` + `TsDemuxer` + `Mp4Writer`).
//!
//! Pipeline: TS packets → PAT/PMT → PES reassembly → per-track samples
//! (H.264 in Annex-B converted to AVCC; AAC ADTS stripped to raw frames) →
//! progressive fragmented MP4 (ftyp + moov + moof/mdat fragments).

pub mod aac;
pub mod h264;
pub mod mp4;
pub mod mux;
pub mod ts;

/// A demuxed sample (one access unit) with decode/presentation timing.
#[derive(Debug, Clone)]
pub struct Sample {
    pub data: Vec<u8>,
    /// Decode time in track timescale units.
    pub dts: u64,
    /// Composition offset (pts - dts); 0 when PTS == DTS.
    pub cts: u64,
    /// Presentation duration in track timescale units.
    pub duration: u64,
    pub keyframe: bool,
}

/// One output track.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub kind: TrackKind,
    /// Sample rate (audio) or 90000 (video) — also the MP4 media timescale.
    pub timescale: u32,
    pub width: u16,
    pub height: u16,
    /// H.264: avcC box contents; AAC: AudioSpecificConfig.
    pub codec_config: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
}
