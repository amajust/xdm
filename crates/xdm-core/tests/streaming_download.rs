//! End-to-end tests for HLS/DASH streaming downloads against a local mock
//! server (port of the spirit of `TestHlsE2E` / `TestDashE2E`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path as AxumPath, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tokio_util::sync::CancellationToken;
mod common;

use common::{synthetic_audio_pes_body, synthetic_video_pes_body, top_level_boxes, TsBuilder};
use xdm_core::engine::download;
use xdm_core::http::DownloadConfig;

type FileMap = HashMap<String, (Vec<u8>, &'static str)>;

/// Serves a fixed set of files at `/<path>` and counts requests per path.
async fn spawn_map_server(files: FileMap) -> (String, Arc<Mutex<HashMap<String, usize>>>) {
    let files = Arc::new(files);
    let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let hits_state = hits.clone();
    let app = Router::new()
        .route(
            "/{*path}",
            get(move |AxumPath(path): AxumPath<String>, headers: HeaderMap, State(files): State<Arc<FileMap>>| {
                let hits = hits_state.clone();
                async move {
                    hits.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .entry(path.clone())
                        .and_modify(|c| *c += 1)
                        .or_insert(1);
                    let Some((body, mime)) = files.get(&path) else {
                        return StatusCode::NOT_FOUND.into_response();
                    };
                    // Honor byte ranges like a real CDN (HLS BYTERANGE needs it).
                    if let Some(range) = headers
                        .get(header::RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|r| parse_range(r, body.len() as u64))
                    {
                        let (s, e) = range;
                        return (
                            StatusCode::PARTIAL_CONTENT,
                            [
                                (header::CONTENT_TYPE, mime.to_string()),
                                (header::CONTENT_RANGE, format!("bytes {s}-{e}/{}", body.len())),
                            ],
                            body[s as usize..=(e as usize)].to_vec(),
                        )
                            .into_response();
                    }
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, mime.to_string())],
                        body.clone(),
                    )
                        .into_response()
                }
            }),
        )
        .with_state(files);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("xdm-stream-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn state_path(dest: &std::path::Path) -> std::path::PathBuf {
    let name = dest.file_name().unwrap().to_str().unwrap();
    dest.with_file_name(format!(".{name}.xdm-stream.json"))
}

fn seg_dir(dest: &std::path::Path) -> std::path::PathBuf {
    let name = dest.file_name().unwrap().to_str().unwrap();
    dest.with_file_name(format!(".{name}.xdm-stream"))
}

/// Parses `bytes=N-M` (or `bytes=N-`) against a body of `total` bytes.
fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    let rest = header.strip_prefix("bytes=")?;
    let (s, e) = rest.split_once('-')?;
    let s: u64 = s.trim().parse().ok()?;
    if s >= total {
        return None;
    }
    let e = match e.trim() {
        "" => total - 1,
        m => m.parse::<u64>().ok()?.min(total - 1),
    };
    Some((s, e))
}

#[tokio::test]
async fn hls_ts_playlist_downloads_and_concatenates() {
    let seg = |i: usize| format!("ts-segment-{i}-payload").repeat(3).into_bytes();
    let files: FileMap = [
        ("live/index.m3u8".to_owned(), (b"#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg0.ts\n#EXTINF:4.0,\nseg1.ts\n#EXTINF:4.0,\nseg2.ts\n#EXT-X-ENDLIST\n".to_vec(), "application/vnd.apple.mpegurl")),
        ("live/seg0.ts".to_owned(), (seg(0), "video/mp2t")),
        ("live/seg1.ts".to_owned(), (seg(1), "video/mp2t")),
        ("live/seg2.ts".to_owned(), (seg(2), "video/mp2t")),
    ].into_iter().collect();
    let (base, _hits) = spawn_map_server(files).await;
    let dir = temp_dir("hls-ts");
    let dest = dir.join("video.ts");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    let total = download(
        &format!("{base}/live/index.m3u8"),
        &dest,
        DownloadConfig::default(),
        CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();

    let expected: Vec<u8> = [seg(0), seg(1), seg(2)].concat();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    assert_eq!(total, Some(expected.len() as u64));
    assert!(!state_path(&dest).exists());
    assert!(!seg_dir(&dest).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hls_aes128_segments_are_decrypted() {
    use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    let key = [7u8; 16];
    let iv = [9u8; 16];
    let plain = |i: usize| format!("plaintext-payload-{i}-").repeat(4).into_bytes();
    let encrypt = |data: &[u8]| {
        let enc = Aes128CbcEnc::new((&key).into(), (&iv).into());
        enc.encrypt_padded_vec_mut::<Pkcs7>(data)
    };

    let files: FileMap = [
        ("enc/index.m3u8".to_owned(), (format!("#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x{:032x}\n#EXTINF:4.0,\nseg0.ts\n#EXTINF:4.0,\nseg1.ts\n#EXT-X-ENDLIST\n", u128::from_be_bytes(iv)).into_bytes(), "application/vnd.apple.mpegurl")),
        ("enc/key.bin".to_owned(), (key.to_vec(), "application/octet-stream")),
        ("enc/seg0.ts".to_owned(), (encrypt(&plain(0)), "video/mp2t")),
        ("enc/seg1.ts".to_owned(), (encrypt(&plain(1)), "video/mp2t")),
    ].into_iter().collect();
    let (base, _hits) = spawn_map_server(files).await;
    let dir = temp_dir("hls-aes");
    let dest = dir.join("video.ts");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    download(&format!("{base}/enc/index.m3u8"), &dest, DownloadConfig::default(), CancellationToken::new(), tx)
        .await
        .unwrap();

    let expected: Vec<u8> = [plain(0), plain(1)].concat();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hls_master_playlist_picks_highest_bandwidth() {
    let seg = |name: &str| format!("{name}-payload").repeat(2).into_bytes();
    let files: FileMap = [
        ("master.m3u8".to_owned(), (b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=3000000\nhigh.m3u8\n".to_vec(), "application/vnd.apple.mpegurl")),
        ("low.m3u8".to_owned(), (b"#EXTM3U\n#EXTINF:4.0,\nlow0.ts\n#EXT-X-ENDLIST\n".to_vec(), "application/vnd.apple.mpegurl")),
        ("high.m3u8".to_owned(), (b"#EXTM3U\n#EXTINF:4.0,\nhigh0.ts\n#EXTINF:4.0,\nhigh1.ts\n#EXT-X-ENDLIST\n".to_vec(), "application/vnd.apple.mpegurl")),
        ("low0.ts".to_owned(), (seg("low"), "video/mp2t")),
        ("high0.ts".to_owned(), (seg("high0"), "video/mp2t")),
        ("high1.ts".to_owned(), (seg("high1"), "video/mp2t")),
    ].into_iter().collect();
    let (base, _hits) = spawn_map_server(files).await;
    let dir = temp_dir("hls-master");
    let dest = dir.join("video.ts");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    download(&format!("{base}/master.m3u8"), &dest, DownloadConfig::default(), CancellationToken::new(), tx)
        .await
        .unwrap();

    let expected: Vec<u8> = [seg("high0"), seg("high1")].concat();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hls_fmp4_with_init_map_concatenates_init_first() {
    let files: FileMap = [
        ("fmp4/index.m3u8".to_owned(), (b"#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg0.m4s\n#EXTINF:4.0,\nseg1.m4s\n#EXT-X-ENDLIST\n".to_vec(), "application/vnd.apple.mpegurl")),
        ("fmp4/init.mp4".to_owned(), (b"ftyp-init-box".to_vec(), "video/mp4")),
        ("fmp4/seg0.m4s".to_owned(), (b"moof-mdat-0".to_vec(), "video/mp4")),
        ("fmp4/seg1.m4s".to_owned(), (b"moof-mdat-1".to_vec(), "video/mp4")),
    ].into_iter().collect();
    let (base, _hits) = spawn_map_server(files).await;
    let dir = temp_dir("hls-fmp4");
    let dest = dir.join("video.mp4");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    download(&format!("{base}/fmp4/index.m3u8"), &dest, DownloadConfig::default(), CancellationToken::new(), tx)
        .await
        .unwrap();

    let expected: Vec<u8> = [b"ftyp-init-box".as_slice(), b"moof-mdat-0".as_slice(), b"moof-mdat-1".as_slice()].concat();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hls_byte_range_segments_fetch_correct_slices() {
    // One big file served once; the playlist carves it into 3 byte ranges.
    let mut big: Vec<u8> = Vec::new();
    for i in 0..3 {
        big.extend(format!("RANGE{i}-payload-{:0>20}", i).into_bytes());
    }
    let third = big.len() / 3;
    let playlist = format!(
        "#EXTM3U\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:{}@0\nbig.ts\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:{}\nbig.ts\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:{}\nbig.ts\n#EXT-X-ENDLIST\n",
        third,
        big.len() - 2 * third,
        third
    );
    let files: FileMap = [
        ("br/index.m3u8".to_owned(), (playlist.into_bytes(), "application/vnd.apple.mpegurl")),
        ("br/big.ts".to_owned(), (big.clone(), "video/mp2t")),
    ].into_iter().collect();
    let (base, hits) = spawn_map_server(files).await;
    let dir = temp_dir("hls-byterange");
    let dest = dir.join("video.ts");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    download(&format!("{base}/br/index.m3u8"), &dest, DownloadConfig::default(), CancellationToken::new(), tx)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), big);
    // The big file itself is fetched 3 times (once per range).
    assert_eq!(
        hits.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get("br/big.ts"),
        Some(&3)
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn dash_segment_template_downloads_init_and_segments() {
    let seg = |i: usize| format!("dash-fragment-{i}").repeat(2).into_bytes();
    let mpd = r#"<?xml version="1.0"?>
<MPD mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="v1" bandwidth="900000" mimeType="video/mp4">
        <SegmentTemplate timescale="1" duration="4" startNumber="1"
            initialization="init-v1.m4s" media="chunk-$Number%05d$.m4s"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let files: FileMap = [
        ("dash/manifest.mpd".to_owned(), (mpd.as_bytes().to_vec(), "application/dash+xml")),
        ("dash/init-v1.m4s".to_owned(), (b"ftyp-init".to_vec(), "video/mp4")),
        ("dash/chunk-00001.m4s".to_owned(), (seg(1), "video/mp4")),
        ("dash/chunk-00002.m4s".to_owned(), (seg(2), "video/mp4")),
        ("dash/chunk-00003.m4s".to_owned(), (seg(3), "video/mp4")),
    ].into_iter().collect();
    let (base, _hits) = spawn_map_server(files).await;
    let dir = temp_dir("dash");
    let dest = dir.join("video.mp4");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    download(&format!("{base}/dash/manifest.mpd"), &dest, DownloadConfig::default(), CancellationToken::new(), tx)
        .await
        .unwrap();

    let expected: Vec<u8> = [b"ftyp-init".as_slice(), seg(1).as_slice(), seg(2).as_slice(), seg(3).as_slice()].concat();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn streaming_resume_skips_done_segments() {
    let seg = |i: usize| format!("resume-segment-{i}-").repeat(3).into_bytes();
    let files: FileMap = [
        ("res/index.m3u8".to_owned(), (b"#EXTM3U\n#EXTINF:4.0,\ns0.ts\n#EXTINF:4.0,\ns1.ts\n#EXTINF:4.0,\ns2.ts\n#EXT-X-ENDLIST\n".to_vec(), "application/vnd.apple.mpegurl")),
        ("res/s0.ts".to_owned(), (seg(0), "video/mp2t")),
        ("res/s1.ts".to_owned(), (seg(1), "video/mp2t")),
        ("res/s2.ts".to_owned(), (seg(2), "video/mp2t")),
    ].into_iter().collect();
    let (base, hits) = spawn_map_server(files).await;
    let dir = temp_dir("stream-resume");
    let dest = dir.join("video.ts");

    // Fake a previous run: segment 0 done (file + state), segments 1-2 pending.
    std::fs::create_dir_all(seg_dir(&dest)).unwrap();
    std::fs::write(seg_dir(&dest).join("seg-00000"), seg(0)).unwrap();
    let state = serde_json::json!({
        "url": format!("{base}/res/index.m3u8"),
        "kind": "Hls",
        "output_ext": "ts",
        "init_url": null,
        "init_done": true,
        "segments": [
            {"url": format!("{base}/res/s0.ts"), "done": true, "size": seg(0).len()},
            {"url": format!("{base}/res/s1.ts"), "done": false, "size": 0},
            {"url": format!("{base}/res/s2.ts"), "done": false, "size": 0}
        ],
        "keys": []
    });
    std::fs::write(state_path(&dest), serde_json::to_vec(&state).unwrap()).unwrap();

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    download(&format!("{base}/res/index.m3u8"), &dest, DownloadConfig::default(), CancellationToken::new(), tx)
        .await
        .unwrap();

    let expected: Vec<u8> = [seg(0), seg(1), seg(2)].concat();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    // Segment 0 was never re-fetched; segments 1-2 were.
    let hits = hits.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(hits.get("res/s0.ts"), None, "done segment must not be re-fetched");
    assert_eq!(hits.get("res/s1.ts"), Some(&1));
    assert_eq!(hits.get("res/s2.ts"), Some(&1));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Sanity: concurrent segment downloads never duplicate work (each index is
/// handed out exactly once by the shared counter).
#[tokio::test]
async fn segments_download_exactly_once_across_workers() {
    let n = 40;
    let seg = |i: usize| format!("seg-{i:02}-data").repeat(2).into_bytes();
    let mut files: FileMap = HashMap::new();
    let mut playlist = String::from("#EXTM3U\n");
    for i in 0..n {
        files.insert(format!("many/s{i}.ts"), (seg(i), "video/mp2t"));
        playlist.push_str(&format!("#EXTINF:4.0,\ns{i}.ts\n"));
    }
    playlist.push_str("#EXT-X-ENDLIST\n");
    files.insert("many/index.m3u8".to_owned(), (playlist.into_bytes(), "application/vnd.apple.mpegurl"));
    let (base, hits) = spawn_map_server(files).await;
    let dir = temp_dir("many-segs");
    let dest = dir.join("video.ts");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    let total = download(
        &format!("{base}/many/index.m3u8"),
        &dest,
        DownloadConfig { max_chunks: 6, headers: Vec::new() },
        CancellationToken::new(),
        tx,
    )
    .await
    .unwrap();

    let expected: Vec<u8> = (0..n).flat_map(seg).collect();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    assert_eq!(total, Some(expected.len() as u64));
    // Each segment fetched exactly once despite 6 concurrent workers.
    let hits = hits.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for i in 0..n {
        assert_eq!(hits.get(&format!("many/s{i}.ts")), Some(&1), "segment {i} fetched more than once");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn hls_ts_segments_are_remuxed_to_mp4() {
    // Two HLS segments, each a synthetic TS (PAT/PMT in the first, PES in
    // both). The engine must remux them into a fragmented MP4.
    let mut ts = TsBuilder::new();
    let mut seg0 = Vec::new();
    seg0.extend(ts.pat());
    seg0.extend(ts.pmt());
    for pkt in ts.pes(256, 0xE0, &synthetic_video_pes_body(true), 126000, true) {
        seg0.extend(pkt);
    }
    for pkt in ts.pes(257, 0xC0, &synthetic_audio_pes_body(0), 126000, false) {
        seg0.extend(pkt);
    }
    let mut seg1 = Vec::new();
    for pkt in ts.pes(256, 0xE0, &synthetic_video_pes_body(false), 129000, false) {
        seg1.extend(pkt);
    }
    for pkt in ts.pes(257, 0xC0, &synthetic_audio_pes_body(1), 128000, false) {
        seg1.extend(pkt);
    }

    let files: FileMap = [
        ("live/index.m3u8".to_owned(), (b"#EXTM3U\n#EXTINF:4.0,\nseg0.ts\n#EXTINF:4.0,\nseg1.ts\n#EXT-X-ENDLIST\n".to_vec(), "application/vnd.apple.mpegurl")),
        ("live/seg0.ts".to_owned(), (seg0, "video/mp2t")),
        ("live/seg1.ts".to_owned(), (seg1, "video/mp2t")),
    ].into_iter().collect();
    let (base, _hits) = spawn_map_server(files).await;
    let dir = temp_dir("hls-remux");
    let dest = dir.join("video.mp4");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    download(&format!("{base}/live/index.m3u8"), &dest, DownloadConfig::default(), CancellationToken::new(), tx)
        .await
        .unwrap();

    let mp4 = std::fs::read(&dest).unwrap();
    let kinds: Vec<String> = top_level_boxes(&mp4).into_iter().map(|(k, _, _)| k).collect();
    assert_eq!(kinds.first().map(String::as_str), Some("ftyp"));
    assert!(kinds.contains(&"moov".to_owned()));
    assert!(kinds.contains(&"moof".to_owned()));
    assert!(kinds.contains(&"mdat".to_owned()));
    assert!(mp4.windows(4).any(|w| w == b"avcC"));
    assert!(mp4.windows(4).any(|w| w == b"esds"));
    let _ = std::fs::remove_dir_all(&dir);
}
