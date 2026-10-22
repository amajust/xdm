//! End-to-end tests for the segmented HTTP downloader, against a local mock
//! server (port of the spirit of `TestHttpSegmentedDownload` /
//! `TestHttpSegmentStealing`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::stream::unfold;
use tokio_util::sync::CancellationToken;
use xdm_core::http::{ChunkState, DownloadConfig, DownloadState, HttpDownload};

const MIB: u64 = 1024 * 1024;
/// How long a "poisoned" range request stalls before answering.
const POISON_DELAY: Duration = Duration::from_secs(6);

struct Srv {
    data: Arc<Vec<u8>>,
    etag: String,
    if_range_log: Mutex<Vec<Option<String>>>,
    /// Requests at/after this offset stall once for `POISON_DELAY`.
    poison_start: Option<u64>,
    poison_hit: AtomicBool,
}

/// Spawns a mock file server. `no_ranges` always answers 200 with the full
/// body; `slow` throttles range responses (~6 MiB/s) so tests can observe
/// splitting/cancellation mid-transfer; `poison_start` stalls the first
/// request at/after that offset, forcing the coordinator to steal its tail.
async fn spawn_server(
    data: Vec<u8>,
    etag: &str,
    no_ranges: bool,
    slow: bool,
    poison_start: Option<u64>,
) -> (String, Arc<Srv>) {
    let state = Arc::new(Srv {
        data: Arc::new(data),
        etag: etag.to_owned(),
        if_range_log: Mutex::new(Vec::new()),
        poison_start,
        poison_hit: AtomicBool::new(false),
    });
    let app_state = state.clone();
    let app = Router::new().route(
        "/file",
        get(move |headers: HeaderMap| {
            let st = app_state.clone();
            async move { serve_file(st, headers, no_ranges, slow).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/file"), state)
}

async fn serve_file(st: Arc<Srv>, headers: HeaderMap, no_ranges: bool, slow: bool) -> Response {
    let total = st.data.len() as u64;
    let if_range = headers
        .get("if-range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    st.if_range_log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(if_range.clone());

    // Standard If-Range semantics: only honor Range when the validator matches.
    let validator_ok = if_range.as_deref().is_none_or(|v| v == st.etag);
    let range = if !no_ranges && validator_ok {
        headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .and_then(|r| parse_range(r, total))
    } else {
        None
    };

    if let Some((s, e)) = range {
        if let Some(poison) = st.poison_start
            && s >= poison
            && !st.poison_hit.swap(true, Ordering::Relaxed)
        {
            tokio::time::sleep(POISON_DELAY).await;
        }
        if slow {
            // ~6 MiB/s so tests can observe splitting/cancellation mid-transfer.
            let stream = unfold((s, st.data.clone()), move |(pos, data)| async move {
                if pos > e {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
                let n = (64 * 1024).min((e - pos + 1) as usize) as u64;
                let piece = data[pos as usize..(pos + n) as usize].to_vec();
                Some((Ok::<_, std::io::Error>(piece), (pos + n, data)))
            });
            return (
                StatusCode::PARTIAL_CONTENT,
                [("content-range", format!("bytes {s}-{e}/{total}"))],
                Body::from_stream(stream),
            )
                .into_response();
        }
        let slice = st.data[s as usize..=(e as usize)].to_vec();
        return (
            StatusCode::PARTIAL_CONTENT,
            [("content-range", format!("bytes {s}-{e}/{total}"))],
            slice,
        )
            .into_response();
    }

    (StatusCode::OK, (*st.data).clone()).into_response()
}

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

fn test_data(len: u64) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("xdm-core-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn state_path(dest: &std::path::Path) -> std::path::PathBuf {
    let name = dest.file_name().unwrap().to_str().unwrap();
    dest.with_file_name(format!(".{name}.xdm-state.json"))
}

#[tokio::test]
async fn multi_chunk_download_matches_source() {
    // Slow server: the coordinator must actually split while downloading.
    let data = test_data(8 * MIB);
    let (url, _srv) = spawn_server(data.clone(), "\"etag\"", false, true, None).await;
    let dir = temp_dir("multi");
    let dest = dir.join("file.bin");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    let dl = HttpDownload::with_config(&url, &dest, DownloadConfig { max_chunks: 4, headers: Vec::new() }).unwrap();
    let total = dl.run(CancellationToken::new(), tx).await.unwrap();

    assert_eq!(total, Some(8 * MIB));
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!state_path(&dest).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn resumes_from_saved_state_with_if_range() {
    let data = test_data(4 * MIB);
    let (url, srv) = spawn_server(data.clone(), "\"etag\"", false, false, None).await;
    let dir = temp_dir("resume");
    let dest = dir.join("file.bin");
    // Pretend a previous run already wrote the full file; the engine must
    // only rewrite the unfinished tail of chunk 0.
    std::fs::write(&dest, &data).unwrap();

    let state = DownloadState {
        url: url.clone(),
        total: Some(4 * MIB),
        etag: Some("\"etag\"".into()),
        last_modified: None,
        accept_ranges: true,
        chunks: vec![
            ChunkState { start: 0, end: 2 * MIB - 1, downloaded: 1_500_000, done: false },
            ChunkState { start: 2 * MIB, end: 4 * MIB - 1, downloaded: 0, done: true },
        ],
        headers: Vec::new(),
    };
    std::fs::write(state_path(&dest), serde_json::to_vec(&state).unwrap()).unwrap();

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    let dl = HttpDownload::new(&url, &dest).unwrap();
    let total = dl.run(CancellationToken::new(), tx).await.unwrap();

    assert_eq!(total, Some(4 * MIB));
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(srv
        .if_range_log
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .any(|v| v.as_deref() == Some("\"etag\"")));
    assert!(!state_path(&dest).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rangeless_server_falls_back_to_single_stream() {
    let data = test_data(MIB);
    let (url, _srv) = spawn_server(data.clone(), "\"etag\"", true, false, None).await;
    let dir = temp_dir("norange");
    let dest = dir.join("file.bin");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    let dl = HttpDownload::new(&url, &dest).unwrap();
    let total = dl.run(CancellationToken::new(), tx).await.unwrap();

    assert_eq!(total, Some(MIB));
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn cancel_persists_state_for_resume() {
    let data = test_data(16 * MIB);
    let (url, _srv) = spawn_server(data, "\"etag\"", false, true, None).await;
    let dir = temp_dir("cancel");
    let dest = dir.join("file.bin");

    let cancel = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::watch::channel(Default::default());
    let dl = HttpDownload::with_config(&url, &dest, DownloadConfig { max_chunks: 2, headers: Vec::new() }).unwrap();
    let run_cancel = cancel.clone();
    let handle = tokio::spawn(async move { dl.run(run_cancel, tx).await });

    // Wait until data is flowing, then cancel mid-transfer.
    loop {
        tokio::select! {
            _ = rx.changed() => {
                if rx.borrow().downloaded > 0 {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {
                panic!("download made no progress");
            }
        }
    }
    cancel.cancel();

    let result = handle.await.unwrap();
    assert!(matches!(result, Err(xdm_core::http::DownloadError::Cancelled)));
    assert!(state_path(&dest).exists(), "state must survive cancellation");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stale_validator_restarts_automatically() {
    let data = test_data(MIB);
    let (url, _srv) = spawn_server(data.clone(), "\"new-etag\"", false, false, None).await;
    let dir = temp_dir("stale");
    let dest = dir.join("file.bin");
    std::fs::write(&dest, vec![0u8; MIB as usize]).unwrap();

    let state = DownloadState {
        url: url.clone(),
        total: Some(MIB),
        etag: Some("\"old-etag\"".into()),
        last_modified: None,
        accept_ranges: true,
        chunks: vec![ChunkState { start: 0, end: MIB - 1, downloaded: 100, done: false }],
        headers: Vec::new(),
    };
    std::fs::write(state_path(&dest), serde_json::to_vec(&state).unwrap()).unwrap();

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    let dl = HttpDownload::new(&url, &dest).unwrap();
    let total = dl.run(CancellationToken::new(), tx).await.unwrap();

    assert_eq!(total, Some(MIB));
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    assert!(!state_path(&dest).exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn stalled_connection_tail_is_stolen_and_download_completes() {
    let data = test_data(10 * MIB);
    // The first range request at/after 5 MiB stalls for 6 s: the connection
    // that drew the back half hangs, and the coordinator must steal its tail
    // (STALL_MS is 5 s) so the download still finishes.
    let (url, _srv) = spawn_server(data.clone(), "\"etag\"", false, true, Some(5 * MIB)).await;
    let dir = temp_dir("steal");
    let dest = dir.join("file.bin");

    let (tx, _rx) = tokio::sync::watch::channel(Default::default());
    let dl = HttpDownload::with_config(&url, &dest, DownloadConfig { max_chunks: 8, headers: Vec::new() }).unwrap();
    let total = dl.run(CancellationToken::new(), tx).await.unwrap();

    assert_eq!(total, Some(10 * MIB));
    assert_eq!(std::fs::read(&dest).unwrap(), data);
    let _ = std::fs::remove_dir_all(&dir);
}
