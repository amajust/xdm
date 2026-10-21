//! Progressive fragmented MP4 writer, port of `Mp4Writer`.
//!
//! Layout: ftyp + moov (stbl sample tables empty; samples live in fragments)
//! then moof/mdat pairs cut on HLS segment boundaries. moov durations are
//! placeholders whose exact file offsets are recorded at write time and
//! patched on `finish` with sample-accurate totals.

use std::io::{self, Seek, SeekFrom, Write};

use super::{Sample, TrackInfo, TrackKind};

const MVHD_TIMESCALE: u32 = 1000;

struct TrackSpec {
    kind: TrackKind,
    track_id: u32,
    timescale: u32,
    width: u16,
    height: u16,
    codec_config: Vec<u8>,
    /// Accumulated total duration in track timescale units.
    duration: u64,
}

/// Absolute file offsets of placeholder duration fields (patched on finish).
#[derive(Default)]
struct DurationPatches {
    mvhd: Option<u64>,
    tkhd: Vec<u64>,
    mdhd: Vec<u64>,
}

/// Progressive fragmented MP4 writer over any `Write + Seek` sink.
pub struct Mp4Writer<W: Write + Seek> {
    out: W,
    tracks: Vec<TrackSpec>,
    pending: Vec<Vec<Sample>>,
    sequence: u32,
    patches: DurationPatches,
    pos: u64,
}

impl<W: Write + Seek> Mp4Writer<W> {
    /// Writes ftyp + moov and prepares for fragments. `tracks` must be
    /// non-empty; order defines track indices (video first is conventional).
    pub fn new(out: W, tracks: Vec<TrackInfo>) -> io::Result<Self> {
        let specs: Vec<TrackSpec> = tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackSpec {
                kind: t.kind,
                track_id: (i + 1) as u32,
                timescale: t.timescale,
                width: t.width,
                height: t.height,
                codec_config: t.codec_config.clone(),
                duration: 0,
            })
            .collect();
        let mut w = Self {
            out,
            tracks: specs,
            pending: vec![Vec::new(); tracks.len()],
            sequence: 0,
            patches: DurationPatches::default(),
            pos: 0,
        };
        w.write_ftyp()?;
        w.write_moov()?;
        Ok(w)
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.out.write_all(bytes)?;
        self.pos += bytes.len() as u64;
        Ok(())
    }

    fn write_ftyp(&mut self) -> io::Result<()> {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"isom");
        payload.extend_from_slice(&0x200u32.to_be_bytes());
        payload.extend_from_slice(b"isom");
        payload.extend_from_slice(b"iso2");
        payload.extend_from_slice(b"avc1");
        payload.extend_from_slice(b"mp41");
        let data = make_box(b"ftyp", &payload);
        self.write(&data)
    }

    fn write_moov(&mut self) -> io::Result<()> {
        let mut moov: Vec<u8> = Vec::new();

        // mvhd (v0). Duration field sits at box_start + 8 (box hdr) + 4
        // (version/flags) + 12 (creation, modification, timescale).
        let mvhd_dur_pos = moov.len() + 24;
        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification
        mvhd.extend_from_slice(&MVHD_TIMESCALE.to_be_bytes());
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // duration (patched)
        mvhd.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        mvhd.extend_from_slice(&[0u8; 10]); // reserved
        mvhd.extend_from_slice(&unity_matrix());
        mvhd.extend_from_slice(&[0u8; 24]); // predefined
        mvhd.extend_from_slice(&((self.tracks.len() as u32 + 1).to_be_bytes()));
        moov.extend_from_slice(&make_full_box(b"mvhd", 0, 0, &mvhd));

        for t in &self.tracks {
            let mut trak: Vec<u8> = Vec::new();
            let trak_start = moov.len() + 8; // after the trak box header

            // tkhd (v0, flags 0x7). Duration at tkhd_start + 8 + 4 + 16.
            let tkhd_dur_pos = trak_start + 8 + 4 + 16;
            let mut tkhd = Vec::new();
            tkhd.extend_from_slice(&0u32.to_be_bytes()); // creation
            tkhd.extend_from_slice(&0u32.to_be_bytes()); // modification
            tkhd.extend_from_slice(&t.track_id.to_be_bytes());
            tkhd.extend_from_slice(&0u32.to_be_bytes()); // reserved
            tkhd.extend_from_slice(&0u32.to_be_bytes()); // duration (patched)
            tkhd.extend_from_slice(&[0u8; 8]); // reserved
            tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer
            tkhd.extend_from_slice(&0u16.to_be_bytes()); // alternate group
            let volume: u16 = if t.kind == TrackKind::Audio { 0x0100 } else { 0 };
            tkhd.extend_from_slice(&volume.to_be_bytes());
            tkhd.extend_from_slice(&[0u8; 2]); // reserved
            tkhd.extend_from_slice(&unity_matrix());
            let (w16, h16) = if t.kind == TrackKind::Video {
                ((t.width as u32) << 16, (t.height as u32) << 16)
            } else {
                (0, 0)
            };
            tkhd.extend_from_slice(&w16.to_be_bytes());
            tkhd.extend_from_slice(&h16.to_be_bytes());
            let tkhd_box = make_full_box(b"tkhd", 0, 0x7, &tkhd);
            trak.extend_from_slice(&tkhd_box);

            // mdia
            let mdia_start = trak_start + tkhd_box.len();
            let mut mdia: Vec<u8> = Vec::new();

            // mdhd (v0). Duration at mdhd_start + 8 + 4 + 12.
            let mdhd_dur_pos = mdia_start + 8 + 8 + 4 + 12;
            let mut mdhd = Vec::new();
            mdhd.extend_from_slice(&0u32.to_be_bytes()); // creation
            mdhd.extend_from_slice(&0u32.to_be_bytes()); // modification
            mdhd.extend_from_slice(&t.timescale.to_be_bytes());
            mdhd.extend_from_slice(&0u32.to_be_bytes()); // duration (patched)
            mdhd.extend_from_slice(&0x55C4u16.to_be_bytes()); // language "und"
            mdhd.extend_from_slice(&0u16.to_be_bytes()); // predefined
            mdia.extend_from_slice(&make_full_box(b"mdhd", 0, 0, &mdhd));

            let (handler, name) = match t.kind {
                TrackKind::Video => (b"vide", "VideoHandler\0"),
                TrackKind::Audio => (b"soun", "SoundHandler\0"),
            };
            let mut hdlr = Vec::new();
            hdlr.extend_from_slice(&0u32.to_be_bytes()); // predefined
            hdlr.extend_from_slice(handler);
            hdlr.extend_from_slice(&[0u8; 12]); // reserved
            hdlr.extend_from_slice(name.as_bytes());
            mdia.extend_from_slice(&make_full_box(b"hdlr", 0, 0, &hdlr));

            let mut minf: Vec<u8> = Vec::new();
            match t.kind {
                TrackKind::Video => {
                    let mut vmhd = Vec::new();
                    vmhd.extend_from_slice(&0u16.to_be_bytes()); // graphicsmode
                    vmhd.extend_from_slice(&[0u8; 6]); // opcolor
                    minf.extend_from_slice(&make_full_box(b"vmhd", 0, 1, &vmhd));
                }
                TrackKind::Audio => {
                    let mut smhd = Vec::new();
                    smhd.extend_from_slice(&0u16.to_be_bytes()); // balance
                    smhd.extend_from_slice(&[0u8; 2]); // reserved
                    minf.extend_from_slice(&make_full_box(b"smhd", 0, 0, &smhd));
                }
            }

            let mut dinf: Vec<u8> = Vec::new();
            let mut dref = Vec::new();
            dref.extend_from_slice(&1u32.to_be_bytes()); // entry count
            dref.extend_from_slice(&make_full_box(b"url ", 0, 1, &[]));
            // dref is a full box: version + flags precede the entry list.
            dinf.extend_from_slice(&make_full_box(b"dref", 0, 0, &dref));
            minf.extend_from_slice(&make_box(b"dinf", &dinf));

            let mut stbl: Vec<u8> = Vec::new();
            stbl.extend_from_slice(&make_stsd(t)?);
            stbl.extend_from_slice(&make_empty_table(b"stts"));
            stbl.extend_from_slice(&make_empty_table(b"stsc"));
            // stsz carries TWO u32 fields even when empty: sample_size and
            // sample_count (an entry-count-only box makes players read the
            // next box's header as the count and reject the track).
            stbl.extend_from_slice(&make_full_box(
                b"stsz",
                0,
                0,
                &0u64.to_be_bytes(),
            ));
            stbl.extend_from_slice(&make_empty_table(b"stco"));
            minf.extend_from_slice(&make_box(b"stbl", &stbl));

            mdia.extend_from_slice(&make_box(b"minf", &minf));
            trak.extend_from_slice(&make_box(b"mdia", &mdia));

            moov.extend_from_slice(&make_box(b"trak", &trak));
            self.patches.tkhd.push(tkhd_dur_pos as u64);
            self.patches.mdhd.push(mdhd_dur_pos as u64);
        }

        // mvex: declares that samples live in fragments (progressive fMP4);
        // without it players see empty sample tables and reject the track.
        let mut mvex: Vec<u8> = Vec::new();
        for t in &self.tracks {
            let mut trex = Vec::new();
            trex.extend_from_slice(&t.track_id.to_be_bytes());
            trex.extend_from_slice(&1u32.to_be_bytes()); // default sample description index
            trex.extend_from_slice(&0u32.to_be_bytes()); // default sample duration
            trex.extend_from_slice(&0u32.to_be_bytes()); // default sample size
            trex.extend_from_slice(&0u32.to_be_bytes()); // default sample flags
            mvex.extend_from_slice(&make_full_box(b"trex", 0, 0, &trex));
        }
        moov.extend_from_slice(&make_box(b"mvex", &mvex));

        self.patches.mvhd = Some(mvhd_dur_pos as u64);
        let moov_box = make_box(b"moov", &moov);
        // Patch offsets are relative to the moov CONTENT start (they were
        // computed from `moov.len()`); the content begins one box-header
        // (8 bytes) after `self.pos`, which points at the moov box header.
        let moov_content_start = self.pos + 8;
        self.patches.mvhd = self.patches.mvhd.map(|o| o + moov_content_start);
        for off in self.patches.tkhd.iter_mut() {
            *off += moov_content_start;
        }
        for off in self.patches.mdhd.iter_mut() {
            *off += moov_content_start;
        }
        self.write(&moov_box)
    }

    /// Queues samples for a track (index order matches the `tracks` vec).
    pub fn write_samples(&mut self, track_idx: usize, samples: Vec<Sample>) {
        self.pending[track_idx].extend(samples);
    }

    /// Track kinds in index order.
    pub fn track_kinds(&self) -> Vec<TrackKind> {
        self.tracks.iter().map(|t| t.kind).collect()
    }

    /// Cuts one moof/mdat fragment containing everything pending.
    pub fn end_fragment(&mut self) -> io::Result<()> {
        let total_pending: usize = self.pending.iter().map(|p| p.len()).sum();
        if total_pending == 0 {
            return Ok(());
        }
        self.sequence += 1;

        let mut trafs: Vec<Vec<u8>> = Vec::new();
        let mut traf_offsets: Vec<usize> = Vec::new();
        let mut payload_total = 0usize;
        for (i, t) in self.tracks.iter().enumerate() {
            let samples = &self.pending[i];
            if samples.is_empty() {
                continue;
            }
            // Each traf's data_offset points at ITS first byte in the mdat;
            // samples are written in track order.
            traf_offsets.push(payload_total);
            payload_total += samples.iter().map(|s| s.data.len()).sum::<usize>();
            trafs.push(make_traf(t.track_id, samples, &t.kind));
        }
        let moof_size: usize = 8 + 16 + trafs.iter().map(|t| t.len()).sum::<usize>();

        let mut moof: Vec<u8> = Vec::with_capacity(moof_size);
        let mut mfhd = Vec::new();
        mfhd.extend_from_slice(&self.sequence.to_be_bytes());
        moof.extend_from_slice(&make_full_box(b"mfhd", 0, 0, &mfhd));
        for (traf, base) in trafs.iter_mut().zip(&traf_offsets) {
            patch_trun_data_offset(traf, (moof_size + 8 + base) as i32);
        }
        for traf in &trafs {
            moof.extend_from_slice(traf);
        }
        self.write(&make_box(b"moof", &moof))?;

        let mut mdat: Vec<u8> = Vec::with_capacity(8 + payload_total);
        for i in 0..self.tracks.len() {
            let samples = std::mem::take(&mut self.pending[i]);
            for s in samples {
                self.tracks[i].duration += s.duration;
                mdat.extend_from_slice(&s.data);
            }
        }
        self.write(&make_box(b"mdat", &mdat))?;
        Ok(())
    }

    /// Flushes the final fragment, patches moov durations in place, and
    /// returns the underlying sink.
    pub fn finish(mut self) -> io::Result<W> {
        self.end_fragment()?;
        self.out.flush()?;

        let mut track_ms: Vec<u64> = Vec::new();
        let mut max_ms: u64 = 0;
        for t in &self.tracks {
            let ms = t.duration * MVHD_TIMESCALE as u64 / t.timescale.max(1) as u64;
            track_ms.push(ms);
            max_ms = max_ms.max(ms);
        }
        if let Some(off) = self.patches.mvhd {
            self.patch(off, max_ms.min(u32::MAX as u64) as u32)?;
        }
        let tkhd = self.patches.tkhd.clone();
        let mdhd = self.patches.mdhd.clone();
        for (i, &off) in tkhd.iter().enumerate() {
            self.patch(off, track_ms[i].min(u32::MAX as u64) as u32)?;
        }
        for (i, &off) in mdhd.iter().enumerate() {
            self.patch(off, self.tracks[i].duration.min(u32::MAX as u64) as u32)?;
        }
        self.out.flush()?;
        Ok(self.out)
    }

    fn patch(&mut self, offset: u64, value: u32) -> io::Result<()> {
        self.out.seek(SeekFrom::Start(offset))?;
        self.out.write_all(&value.to_be_bytes())?;
        self.out.seek(SeekFrom::Start(self.pos))?;
        Ok(())
    }
}

/// One traf: tfhd (default-base-is-moof) + tfdt (v1) + trun (v1, full fields).
fn make_traf(track_id: u32, samples: &[Sample], kind: &TrackKind) -> Vec<u8> {
    let mut tfhd = Vec::new();
    tfhd.extend_from_slice(&track_id.to_be_bytes());
    let tfhd = make_full_box(b"tfhd", 0, 0x020000, &tfhd);

    let mut tfdt = Vec::new();
    tfdt.extend_from_slice(&samples[0].dts.to_be_bytes());
    let tfdt = make_full_box(b"tfdt", 1, 0, &tfdt);

    let mut trun_payload = Vec::new();
    trun_payload.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    trun_payload.extend_from_slice(&0i32.to_be_bytes()); // data_offset (patched)
    for s in samples {
        trun_payload.extend_from_slice(&(s.duration as u32).to_be_bytes());
        trun_payload.extend_from_slice(&(s.data.len() as u32).to_be_bytes());
        let flags: u32 = match kind {
            TrackKind::Video if s.keyframe => 0x02000000, // sync sample
            TrackKind::Video => 0x01010000,               // non-sync, depends on others
            TrackKind::Audio => 0x02000000,
        };
        trun_payload.extend_from_slice(&flags.to_be_bytes());
        trun_payload.extend_from_slice(&(s.cts as i32).to_be_bytes());
    }
    // trun v1 with all per-sample fields. Bit assignments per ISO 14496-12
    // Table 8.10: 0x1 data-offset, 0x100 duration, 0x200 size, 0x400 flags,
    // 0x800 composition offset. (0x4 is FIRST-sample-flags — a common trap.)
    let trun = make_full_box(b"trun", 1, 0x1 | 0x100 | 0x200 | 0x400 | 0x800, &trun_payload);

    let mut traf = Vec::new();
    traf.extend_from_slice(&tfhd);
    traf.extend_from_slice(&tfdt);
    traf.extend_from_slice(&trun);
    make_box(b"traf", &traf)
}

/// Locates the trun inside a traf and writes the data_offset field.
fn patch_trun_data_offset(traf: &mut [u8], offset: i32) {
    if let Some(pos) = traf.windows(4).position(|w| w == b"trun") {
        // pos = box_start + 4 (type); data_offset sits after box hdr (8),
        // version/flags (4) and sample_count (4) → pos + 12.
        let p = pos + 12;
        if p + 4 <= traf.len() {
            traf[p..p + 4].copy_from_slice(&offset.to_be_bytes());
        }
    }
}

fn make_stsd(t: &TrackSpec) -> io::Result<Vec<u8>> {
    let mut entries: Vec<u8> = Vec::new();
    match t.kind {
        TrackKind::Video => {
            let mut avc1: Vec<u8> = Vec::new();
            avc1.extend_from_slice(&[0u8; 6]); // SampleEntry reserved
            avc1.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
            avc1.extend_from_slice(&0u16.to_be_bytes()); // predefined
            avc1.extend_from_slice(&0u16.to_be_bytes()); // reserved
            avc1.extend_from_slice(&[0u8; 12]); // predefined
            avc1.extend_from_slice(&t.width.to_be_bytes());
            avc1.extend_from_slice(&t.height.to_be_bytes());
            avc1.extend_from_slice(&0x00480000u32.to_be_bytes()); // hres 72dpi
            avc1.extend_from_slice(&0x00480000u32.to_be_bytes()); // vres
            avc1.extend_from_slice(&[0u8; 4]); // reserved
            avc1.extend_from_slice(&1u16.to_be_bytes()); // frame count
            let mut compressor = vec![0u8; 32];
            compressor[0] = 0; // pascal-style empty name
            avc1.extend_from_slice(&compressor);
            avc1.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
            avc1.extend_from_slice(&0xFFFFu16.to_be_bytes()); // predefined
            avc1.extend_from_slice(&make_box(b"avcC", &t.codec_config));
            entries.extend_from_slice(&make_box(b"avc1", &avc1));
        }
        TrackKind::Audio => {
            let mut mp4a: Vec<u8> = Vec::new();
            mp4a.extend_from_slice(&[0u8; 6]); // SampleEntry reserved
            mp4a.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
            mp4a.extend_from_slice(&[0u8; 8]); // AudioSampleEntry reserved
            mp4a.extend_from_slice(&t.channels_or_default().to_be_bytes()); // channelcount
            mp4a.extend_from_slice(&16u16.to_be_bytes()); // samplesize
            mp4a.extend_from_slice(&0u16.to_be_bytes()); // predefined
            mp4a.extend_from_slice(&0u16.to_be_bytes()); // reserved
            mp4a.extend_from_slice(&(t.timescale << 16).to_be_bytes()); // samplerate 16.16
            mp4a.extend_from_slice(&make_esds(&t.codec_config, t.track_id));
            entries.extend_from_slice(&make_box(b"mp4a", &mp4a));
        }
    }
    let mut stsd = Vec::new();
    stsd.extend_from_slice(&1u32.to_be_bytes()); // entry count
    stsd.extend_from_slice(&entries);
    Ok(make_full_box(b"stsd", 0, 0, &stsd))
}

/// esds with ES_Descriptor → DecoderConfigDescriptor (AAC, 0x40) →
/// DecoderSpecificInfo (the AudioSpecificConfig) → SLConfigDescriptor.
fn make_esds(asc: &[u8], track_id: u32) -> Vec<u8> {
    let mut dcd: Vec<u8> = Vec::new();
    dcd.push(0x40); // objectTypeIndication: MPEG-4 AAC
    dcd.push((5 << 2) | 1); // streamType audio(5), upStream 0, reserved 1
    dcd.extend_from_slice(&[0, 0, 0]); // bufferSizeDB
    dcd.extend_from_slice(&0u32.to_be_bytes()); // maxBitrate
    dcd.extend_from_slice(&0u32.to_be_bytes()); // avgBitrate
    dcd.push(0x05); // DecoderSpecificInfo tag
    dcd.push(asc.len() as u8);
    dcd.extend_from_slice(asc);

    let mut es: Vec<u8> = Vec::new();
    es.extend_from_slice(&(track_id as u16).to_be_bytes()); // ES_ID
    es.push(0); // flags
    es.extend_from_slice(&descriptor(0x04, &dcd));
    es.extend_from_slice(&descriptor(0x06, &[0x02])); // SLConfigDescriptor

    make_full_box(b"esds", 0, 0, &descriptor(0x03, &es))
}

/// MPEG-4 descriptor with a length prefix (single byte is enough for our sizes).
fn descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    // Extended length encoding (definite-length form); handles > 127 too.
    let mut len = payload.len();
    if len < 128 {
        out.push(len as u8);
    } else {
        let mut bytes = Vec::new();
        while len > 0 {
            bytes.insert(0, (len & 0x7F) as u8);
            len >>= 7;
        }
        for (i, b) in bytes.iter().enumerate() {
            let last = i == bytes.len() - 1;
            out.push(if last { *b } else { *b | 0x80 });
        }
    }
    out.extend_from_slice(payload);
    out
}

fn make_empty_table(tag: &[u8; 4]) -> Vec<u8> {
    make_full_box(tag, 0, 0, &0u32.to_be_bytes())
}

fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn make_full_box(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + payload.len());
    out.extend_from_slice(&((payload.len() + 12) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.push(version);
    // flags is a 24-bit field: drop the (always zero) high byte.
    out.extend_from_slice(&flags.to_be_bytes()[1..4]);
    out.extend_from_slice(payload);
    out
}

fn unity_matrix() -> [u8; 36] {
    let mut m = [0u8; 36];
    m[0..4].copy_from_slice(&0x00010000u32.to_be_bytes());
    m[16..20].copy_from_slice(&0x00010000u32.to_be_bytes());
    m[32..36].copy_from_slice(&0x40000000u32.to_be_bytes());
    m
}

impl TrackSpec {
    fn channels_or_default(&self) -> u16 {
        // Channel count is encoded in the ASC (bits 3..7 of the 16-bit value).
        let ch = ((self.codec_config.first().copied().unwrap_or(0) as u16) & 0x01) << 2
            | ((self.codec_config.get(1).copied().unwrap_or(0) as u16) >> 6);
        if ch == 0 {
            2
        } else {
            ch
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn video_track() -> TrackInfo {
        TrackInfo {
            kind: TrackKind::Video,
            timescale: 90000,
            width: 32,
            height: 32,
            codec_config: vec![1, 2, 3],
        }
    }

    fn audio_track() -> TrackInfo {
        TrackInfo {
            kind: TrackKind::Audio,
            timescale: 48000,
            width: 0,
            height: 0,
            codec_config: vec![0x12, 0x10],
        }
    }

    fn sample(dts: u64, duration: u64, keyframe: bool) -> Sample {
        Sample { data: vec![0xAB; 10], dts, cts: 0, duration, keyframe }
    }

    #[test]
    fn moov_layout_and_duration_patching() -> io::Result<()> {
        let tracks = vec![video_track(), audio_track()];
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), tracks).unwrap();
        w.write_samples(0, vec![sample(0, 3000, true), sample(3000, 3000, false)]);
        w.write_samples(1, vec![sample(0, 1024, true), sample(1024, 1024, true)]);
        w.end_fragment().unwrap();

        let data = w.finish()?.into_inner();
        // ftyp (32) then moov.
        assert_eq!(&data[4..8], b"ftyp");
        assert_eq!(&data[36..40], b"moov");

        // mvhd at 40: size 108, version/flags 0, timescale 1000 at 60.
        assert_eq!(&data[40..44], &108u32.to_be_bytes());
        assert_eq!(&data[44..48], b"mvhd");
        assert_eq!(&data[60..64], &1000u32.to_be_bytes(), "mvhd timescale");
        let mvhd_dur = u32::from_be_bytes(data[64..68].try_into().unwrap());
        // Video: 6000 ticks @90k = 66.7ms; audio: 2048 @48k = 42.7ms → max 66.
        assert_eq!(mvhd_dur, 66, "mvhd duration = max track ms");

        // tkhd: inside the first trak, right after the mvhd.
        let tkhd_pos = data.windows(4).position(|w| w == b"tkhd").unwrap();
        let tkhd_size = u32::from_be_bytes(data[tkhd_pos - 4..tkhd_pos].try_into().unwrap());
        assert_eq!(tkhd_size, 92, "tkhd size");
        // duration at box start + 8 (hdr) + 4 (ver/flags) + 16 (creation, mod, id, reserved)
        let tkhd_dur =
            u32::from_be_bytes(data[tkhd_pos + 24..tkhd_pos + 28].try_into().unwrap());
        assert_eq!(tkhd_dur, 66, "video tkhd duration in ms");

        // tkhd width/height as 16.16 fixed point: payload offset 72.
        let width = u32::from_be_bytes(data[tkhd_pos + 84..tkhd_pos + 88].try_into().unwrap());
        assert_eq!(width >> 16, 32, "tkhd width");

        // mdhd: video timescale 90000, duration 6000.
        // timescale/duration after hdr(8) + ver/flags(4) + creation(4) + mod(4).
        let mdhd_pos = data.windows(4).position(|w| w == b"mdhd").unwrap();

        let ts = u32::from_be_bytes(data[mdhd_pos + 16..mdhd_pos + 20].try_into().unwrap());
        assert_eq!(ts, 90000, "video mdhd timescale");
        let dur = u32::from_be_bytes(data[mdhd_pos + 20..mdhd_pos + 24].try_into().unwrap());
        assert_eq!(dur, 6000, "video mdhd duration");
        Ok(())
    }
}
