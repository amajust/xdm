//! End-to-end tests for the TS → MP4 remuxer (port of the spirit of
//! `TestTransmuxer`): builds synthetic MPEG-TS streams with PAT/PMT/PES
//! carrying valid H.264 (Annex-B) and AAC (ADTS), remuxes them, and walks
//! the resulting MP4 box structure.

mod common;

use common::{
    find_marker, synthetic_audio_pes_body, synthetic_sps, synthetic_video_pes_body, top_level_boxes,
    TsBuilder,
};
use xdm_core::media::h264;
use xdm_core::media::mux::remux_ts_to_mp4;

// ---------- tests ----------

#[test]
fn remux_synthetic_ts_produces_valid_mp4() {
    let dir = std::env::temp_dir().join(format!("xdm-remux-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut ts = TsBuilder::new();
    let mut stream = Vec::new();
    stream.extend(ts.pat());
    stream.extend(ts.pmt());
    // 4 video frames at 30 fps (90000/3000), first is IDR; interleaved audio.
    for i in 0..4u64 {
        let body = synthetic_video_pes_body(i == 0);
        let pts = 126000 + i * 3000;
        for pkt in ts.pes(256, 0xE0, &body, pts, i == 0) {
            stream.extend(pkt);
        }
        let audio = synthetic_audio_pes_body(i as u8);
        for pkt in ts.pes(257, 0xC0, &audio, 126000 + i * 4000, false) {
            stream.extend(pkt);
        }
    }

    let ts_path = dir.join("seg.ts");
    std::fs::write(&ts_path, &stream).unwrap();
    let out_path = dir.join("out.mp4");

    let outcome = remux_ts_to_mp4(std::slice::from_ref(&ts_path), &out_path).unwrap().expect("mux should succeed");
    let mp4 = std::fs::read(&out_path).unwrap();

    // Top-level structure: ftyp, moov, then moof/mdat pairs.
    let boxes = top_level_boxes(&mp4);
    let kinds: Vec<&str> = boxes.iter().map(|(k, _, _)| k.as_str()).collect();
    assert_eq!(kinds.first(), Some(&"ftyp"));
    assert!(kinds.contains(&"moov"));
    assert_eq!(kinds.iter().filter(|k| **k == "moof").count(), 1);
    assert_eq!(kinds.iter().filter(|k| **k == "mdat").count(), 1);

    // Codec configuration present: avcC (video) + esds (audio).
    assert!(find_marker(&mp4, b"avcC"));
    assert!(find_marker(&mp4, b"esds"));

    // avcC carries our synthetic SPS.
    let sps = synthetic_sps();
    assert!(mp4.windows(sps.len()).any(|w| w == sps));

    // Patched durations are non-zero (mvhd v0 duration at marker + 4 + 12).
    let mvhd_pos = mp4.windows(4).position(|w| w == b"mvhd").unwrap() + 4;
    let dur = u32::from_be_bytes(mp4[mvhd_pos + 4 + 12..mvhd_pos + 4 + 16].try_into().unwrap());
    assert!(dur > 0, "mvhd duration must be patched, got {dur}");

    let _ = outcome;
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remux_durations_are_exact() {
    // Ground truth: video = 4 frames × 3000 ticks @90kHz = 12000;
    // audio = 4 PES × 2 frames × 1024 ticks @48kHz = 8192.
    // mvhd/tkhd are in the 1000-scale: max(12000/90, 8192/48) = max(133, 170) = 170.
    let dir = std::env::temp_dir().join(format!("xdm-remux-dur-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut ts = TsBuilder::new();
    let mut stream = Vec::new();
    stream.extend(ts.pat());
    stream.extend(ts.pmt());
    for i in 0..4u64 {
        let body = synthetic_video_pes_body(i == 0);
        for pkt in ts.pes(256, 0xE0, &body, 126000 + i * 3000, i == 0) {
            stream.extend(pkt);
        }
        let audio = synthetic_audio_pes_body(i as u8);
        for pkt in ts.pes(257, 0xC0, &audio, 126000 + i * 4000, false) {
            stream.extend(pkt);
        }
    }
    let ts_path = dir.join("seg.ts");
    std::fs::write(&ts_path, &stream).unwrap();
    let out_path = dir.join("out.mp4");
    remux_ts_to_mp4(&[ts_path], &out_path).unwrap().expect("mux should succeed");
    let mp4 = std::fs::read(&out_path).unwrap();

    // mvhd duration (v0): marker + 4 + 4(version/flags) + 12 → patch target.
    let mvhd_pos = mp4.windows(4).position(|w| w == b"mvhd").unwrap() + 4;
    let mvhd_dur = u32::from_be_bytes(mp4[mvhd_pos + 16..mvhd_pos + 20].try_into().unwrap());
    assert_eq!(mvhd_dur, 170, "mvhd duration must match the longest track in ms");

    // tkhd durations: find each trak's tkhd, duration at marker + 24.
    let mut tkhd_durs = Vec::new();
    let mut search = 0;
    while let Some(rel) = mp4[search..].windows(4).position(|w| w == b"tkhd") {
        let pos = search + rel + 4;
        let d = u32::from_be_bytes(mp4[pos + 20..pos + 24].try_into().unwrap());
        tkhd_durs.push(d);
        search = pos + 4;
    }
    assert_eq!(tkhd_durs, vec![133, 170], "tkhd durations (video, audio) in ms");

    // mdhd durations in track timescales: video 12000, audio 8192.
    let mut mdhd_durs = Vec::new();
    let mut search = 0;
    while let Some(rel) = mp4[search..].windows(4).position(|w| w == b"mdhd") {
        let pos = search + rel + 4;
        let d = u32::from_be_bytes(mp4[pos + 16..pos + 20].try_into().unwrap());
        mdhd_durs.push(d);
        search = pos + 4;
    }
    assert_eq!(mdhd_durs, vec![12000, 8192], "mdhd durations in track timescales");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn remux_falls_back_when_no_supported_media() {
    // TS with neither H.264 nor AAC (unknown stream type) → Ok(None).
    let dir = std::env::temp_dir().join(format!("xdm-remux-empty-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Just garbage packets: demuxer finds no PAT/PMT streams.
    let ts_path = dir.join("seg.ts");
    std::fs::write(&ts_path, vec![0x47u8; 188 * 4]).unwrap();
    let out_path = dir.join("out.mp4");

    let outcome = remux_ts_to_mp4(&[ts_path], &out_path).unwrap();
    assert!(outcome.is_none(), "garbage TS must yield None");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sps_geometry_matches_synthetic() {
    let (w, h, profile, level) = h264::parse_sps_geometry(&synthetic_sps()).unwrap();
    assert_eq!((w, h), (32, 32));
    assert_eq!((profile, level), (66, 30));
}
