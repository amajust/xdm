//! Segmented, resumable HTTP downloader.
//!
//! Rust port of xdm-core's `HttpDownloaderTask` / `HttpChunkRetriever`:
//! - one probe request detects range support, total size, and a validator,
//! - the file is split across up to `max_chunks` connections (even split),
//! - every chunk streams into the same destination file at its own offset,
//! - per-chunk state is persisted periodically and on cancel/failure, so a
//!   crash loses at most one save interval,
//! - resume sends `If-Range` (ETag / Last-Modified); a stale validator fails
//!   loudly with [`DownloadError::RestartRequired`] instead of corrupting
//!   data (retried once automatically, wiping state).

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const USER_AGENT: &str = concat!("xdm-rs/", env!("CARGO_PKG_VERSION"));
/// Never split below this size (the original split halves of >= 256 KB).
const MIN_CHUNK_SIZE: u64 = 1024 * 1024;
/// Retries per chunk (request failures and mid-stream reconnects).
const MAX_ATTEMPTS: u32 = 10;
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle timeout between body reads; a stalled server drops the connection.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// How often per-chunk state is flushed to disk while running.
const STATE_SAVE_INTERVAL: Duration = Duration::from_secs(5);
/// Sentinel end for an open-ended chunk (unknown total size).
const OPEN_ENDED: u64 = u64::MAX;

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Server ignored `Range`, or the resume validator no longer matches.
    /// Retried once automatically with wiped state; a second failure surfaces.
    #[error("resume validator changed; download must restart from scratch")]
    RestartRequired,
    #[error("chunk failed after {attempts} attempts (last: {last_error})")]
    ExhaustedRetries { attempts: u32, last_error: String },
    #[error("cancelled")]
    Cancelled,
}

/// Live progress, published through a `watch` channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Progress {
    pub downloaded: u64,
    pub total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Maximum number of concurrent range connections.
    pub max_chunks: usize,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self { max_chunks: 8 }
    }
}

/// Persisted resume state for one download, written atomically
/// (tmp file + rename) next to the destination.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadState {
    pub url: String,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub accept_ranges: bool,
    pub chunks: Vec<ChunkState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkState {
    /// Inclusive byte offsets; `end == u64::MAX` means open-ended.
    pub start: u64,
    pub end: u64,
    /// Bytes already written for this chunk (relative to `start`).
    pub downloaded: u64,
    pub done: bool,
}

pub struct HttpDownload {
    client: Client,
    url: String,
    dest: PathBuf,
    state_path: PathBuf,
    config: DownloadConfig,
}

impl HttpDownload {
    pub fn new(url: impl Into<String>, dest: impl Into<PathBuf>) -> Self {
        Self::with_config(url, dest, DownloadConfig::default())
    }

    pub fn with_config(
        url: impl Into<String>,
        dest: impl Into<PathBuf>,
        config: DownloadConfig,
    ) -> Self {
        let dest = dest.into();
        let state_name = format!(
            ".{}.xdm-state.json",
            dest.file_name().and_then(|n| n.to_str()).unwrap_or("download")
        );
        let state_path = dest
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(state_name);
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self {
            client,
            url: url.into(),
            dest,
            state_path,
            config,
        }
    }

    /// Runs the download to completion.
    ///
    /// Progress is published on `progress` (create with
    /// `tokio::sync::watch::channel(Progress::default())`). Cancel via
    /// `cancel`; state is persisted so a later `run` resumes.
    pub async fn run(
        self,
        cancel: CancellationToken,
        progress: watch::Sender<Progress>,
    ) -> Result<Option<u64>, DownloadError> {
        // One automatic retry for a stale/ignored validator.
        let mut validator_restarts = 0u8;
        loop {
            match self.run_once(&cancel, &progress).await {
                Err(DownloadError::RestartRequired) if validator_restarts == 0 => {
                    validator_restarts += 1;
                    tracing::warn!("resume validator stale; wiping state and restarting");
                    let _ = tokio::fs::remove_file(&self.state_path).await;
                    continue;
                }
                other => return other,
            }
        }
    }

    async fn run_once(
        &self,
        cancel: &CancellationToken,
        progress: &watch::Sender<Progress>,
    ) -> Result<Option<u64>, DownloadError> {
        let saved = self.load_state().await;
        let probe = self.probe(saved.as_ref()).await?;
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }

        let dest_exists = tokio::fs::try_exists(&self.dest).await.unwrap_or(false);
        let resumed = saved.as_ref().is_some_and(|s| {
            dest_exists
                && s.total == probe.total
                && s.chunks.iter().any(|c| c.downloaded > 0 || c.done)
        });
        let chunks = if resumed {
            saved.as_ref().unwrap().chunks.clone()
        } else if probe.accept_ranges {
            split(probe.total, self.config.max_chunks)
        } else {
            // No range support: one open-ended full-body connection.
            vec![ChunkState { start: 0, end: OPEN_ENDED, downloaded: 0, done: false }]
        };

        // Fresh start (or resuming onto a vanished file): truncate/preallocate.
        if !resumed {
            let f = tokio::fs::File::create(&self.dest).await?;
            if let Some(total) = probe.total {
                f.set_len(total).await?;
            }
            f.sync_all().await?;
        }

        let total_downloaded = Arc::new(AtomicU64::new(0));
        let chunk_downloaded: Vec<Arc<AtomicU64>> = chunks
            .iter()
            .map(|c| Arc::new(AtomicU64::new(c.downloaded)))
            .collect();
        let chunk_done: Vec<Arc<AtomicBool>> =
            chunks.iter().map(|_| Arc::new(AtomicBool::new(false))).collect();

        let initial: u64 = chunks.iter().map(|c| c.downloaded).sum();
        total_downloaded.store(initial, Ordering::Relaxed);
        let _ = progress.send(Progress { downloaded: initial, total: probe.total });

        // Progress ticker: publishes the shared counter every 250 ms.
        let ticker = {
            let counter = total_downloaded.clone();
            let sender = progress.clone();
            let total = probe.total;
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_millis(250));
                loop {
                    interval.tick().await;
                    let _ = sender.send(Progress {
                        downloaded: counter.load(Ordering::Relaxed),
                        total,
                    });
                }
            })
        };

        // Periodic state persister (crash-safety window: one save interval).
        let persister = {
            let state_path = self.state_path.clone();
            let url = self.url.clone();
            let etag = probe.etag.clone();
            let last_modified = probe.last_modified.clone();
            let total = probe.total;
            let accept_ranges = probe.accept_ranges;
            let geometry = chunks.clone();
            let downloaded = chunk_downloaded.clone();
            let done = chunk_done.clone();
            let cancel_p = cancel.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(STATE_SAVE_INTERVAL);
                interval.tick().await; // first tick fires immediately; skip it
                loop {
                    tokio::select! {
                        _ = interval.tick() => {}
                        _ = cancel_p.cancelled() => break,
                    }
                    let merged = merge_progress(&geometry, &downloaded, &done);
                    let state = build_state(&url, total, &etag, &last_modified, accept_ranges, &merged);
                    save_state(&state_path, &state).await;
                }
            })
        };

        // One task per unfinished chunk.
        let mut handles = Vec::new();
        for (i, chunk) in chunks.iter().enumerate() {
            if chunk.done {
                continue;
            }
            handles.push(tokio::spawn(run_chunk(
                self.client.clone(),
                self.url.clone(),
                probe.validator(),
                chunk.start,
                chunk.end,
                chunk_downloaded[i].clone(),
                chunk_done[i].clone(),
                total_downloaded.clone(),
                self.dest.clone(),
                probe.accept_ranges,
                cancel.clone(),
            )));
        }

        let mut result = Ok(());
        for handle in handles {
            match handle.await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    result = Err(e);
                    break;
                }
                Err(join) => {
                    result = Err(DownloadError::Io(std::io::Error::other(join)));
                    break;
                }
            }
        }

        ticker.abort();
        persister.abort();

        let merged = merge_progress(&chunks, &chunk_downloaded, &chunk_done);
        match result {
            Ok(()) => {
                let _ = tokio::fs::remove_file(&self.state_path).await;
                let _ = progress.send(Progress {
                    downloaded: total_downloaded.load(Ordering::Relaxed),
                    total: probe.total,
                });
                Ok(probe.total)
            }
            Err(e) => {
                let state = build_state(
                    &self.url,
                    probe.total,
                    &probe.etag,
                    &probe.last_modified,
                    probe.accept_ranges,
                    &merged,
                );
                save_state(&self.state_path, &state).await;
                tracing::warn!(error = %e, "download interrupted; state saved for resume");
                Err(e)
            }
        }
    }

    async fn load_state(&self) -> Option<DownloadState> {
        let bytes = tokio::fs::read(&self.state_path).await.ok()?;
        let state: DownloadState = serde_json::from_slice(&bytes).ok()?;
        (state.url == self.url).then_some(state)
    }

    /// One tiny range request to learn range support, total size, validator.
    async fn probe(&self, saved: Option<&DownloadState>) -> Result<ProbeInfo, DownloadError> {
        let mut req = self.client.get(&self.url).header(RANGE, "bytes=0-0");
        if let Some(saved) = saved {
            if let Some(v) = saved.etag.as_deref().or(saved.last_modified.as_deref()) {
                req = req.header(IF_RANGE, v);
            }
        }
        let resp = req.send().await?.error_for_status()?;
        let status = resp.status();

        let etag = resp
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let last_modified = resp
            .headers()
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let accept_ranges = status == StatusCode::PARTIAL_CONTENT;

        // A saved state plus a 200 answer means the validator went stale
        // (or the server dropped range support): never resume blind.
        if saved.is_some() && !accept_ranges {
            return Err(DownloadError::RestartRequired);
        }

        let total = resp
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range_total)
            .or_else(|| {
                (status != StatusCode::PARTIAL_CONTENT)
                    .then_some(())
                    .and_then(|_| {
                        resp.headers()
                            .get(CONTENT_LENGTH)
                            .and_then(|v| v.to_str().ok())
                            .and_then(|s| s.parse().ok())
                    })
            });

        Ok(ProbeInfo { total, etag, last_modified, accept_ranges })
    }
}

struct ProbeInfo {
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
    accept_ranges: bool,
}

impl ProbeInfo {
    fn validator(&self) -> Option<String> {
        self.etag.clone().or_else(|| self.last_modified.clone())
    }
}

/// Evenly splits `total` bytes across at most `max_chunks` chunks
/// (inclusive ranges), never below `MIN_CHUNK_SIZE`.
fn split(total: Option<u64>, max_chunks: usize) -> Vec<ChunkState> {
    let Some(total) = total else {
        return vec![ChunkState { start: 0, end: OPEN_ENDED, downloaded: 0, done: false }];
    };
    if total == 0 {
        return vec![ChunkState { start: 0, end: 0, downloaded: 0, done: true }];
    }
    let count = ((total / MIN_CHUNK_SIZE).max(1) as usize).min(max_chunks.max(1));
    let per = total / count as u64;
    let mut chunks = Vec::with_capacity(count);
    let mut start = 0;
    for i in 0..count {
        // Last chunk absorbs the remainder.
        let end = if i == count - 1 { total - 1 } else { start + per - 1 };
        chunks.push(ChunkState { start, end, downloaded: 0, done: false });
        start = end + 1;
    }
    chunks
}

/// Rebuilds the chunk list with live counters merged into the saved geometry.
fn merge_progress(
    geometry: &[ChunkState],
    downloaded: &[Arc<AtomicU64>],
    done: &[Arc<AtomicBool>],
) -> Vec<ChunkState> {
    geometry
        .iter()
        .enumerate()
        .map(|(i, c)| ChunkState {
            start: c.start,
            end: c.end,
            downloaded: downloaded[i].load(Ordering::Relaxed),
            done: done[i].load(Ordering::Relaxed),
        })
        .collect()
}

fn build_state(
    url: &str,
    total: Option<u64>,
    etag: &Option<String>,
    last_modified: &Option<String>,
    accept_ranges: bool,
    chunks: &[ChunkState],
) -> DownloadState {
    DownloadState {
        url: url.to_owned(),
        total,
        etag: etag.clone(),
        last_modified: last_modified.clone(),
        accept_ranges,
        chunks: chunks.to_vec(),
    }
}

/// Atomic state write: tmp file + fsync + rename.
async fn save_state(path: &Path, state: &DownloadState) {
    let Ok(json) = serde_json::to_vec_pretty(state) else {
        tracing::warn!("state serialization failed");
        return;
    };
    let tmp = path.with_extension("json.tmp");
    let path = path.to_path_buf();
    if let Err(e) = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })
    .await
    {
        tracing::warn!(error = %e, "failed to persist download state");
    }
}

/// Parses `bytes 0-0/12345` (or `bytes 0-0/*`) into the total.
fn parse_content_range_total(value: &str) -> Option<u64> {
    let after_slash = value.rsplit('/').next()?;
    let after_slash = after_slash.trim();
    (after_slash != "*").then_some(()).and_then(|_| after_slash.parse().ok())
}

fn end_suffix(end: u64) -> String {
    if end == OPEN_ENDED {
        String::new()
    } else {
        end.to_string()
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_chunk(
    client: Client,
    url: String,
    validator: Option<String>,
    start: u64,
    end: u64,
    downloaded: Arc<AtomicU64>,
    done: Arc<AtomicBool>,
    total_downloaded: Arc<AtomicU64>,
    dest: PathBuf,
    accept_ranges: bool,
    cancel: CancellationToken,
) -> Result<(), DownloadError> {
    let expected = (end != OPEN_ENDED).then(|| end - start + 1);
    let mut attempt = 0u32;
    loop {
        if cancel.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        attempt += 1;

        let offset = start + downloaded.load(Ordering::Relaxed);
        if expected.is_some_and(|len| downloaded.load(Ordering::Relaxed) >= len) {
            done.store(true, Ordering::Relaxed);
            return Ok(());
        }

        let mut req = client.get(&url);
        if accept_ranges {
            req = req.header(RANGE, format!("bytes={offset}-{}", end_suffix(end)));
            if let Some(v) = &validator {
                req = req.header(IF_RANGE, v);
            }
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(DownloadError::ExhaustedRetries {
                        attempts: MAX_ATTEMPTS,
                        last_error: e.to_string(),
                    });
                }
                tracing::debug!(error = %e, chunk_start = start, "chunk request failed; retrying");
                tokio::time::sleep((RETRY_BACKOFF * attempt.min(16)).min(MAX_BACKOFF)).await;
                continue;
            }
        };

        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            // Full body is only acceptable for a range-less server from offset 0.
            StatusCode::OK if !accept_ranges && offset == 0 => {}
            StatusCode::OK => return Err(DownloadError::RestartRequired),
            s if s.is_success() => return Err(DownloadError::RestartRequired),
            s => {
                if attempt >= MAX_ATTEMPTS {
                    return Err(DownloadError::ExhaustedRetries {
                        attempts: MAX_ATTEMPTS,
                        last_error: format!("unexpected status {s}"),
                    });
                }
                tracing::debug!(status = %s, chunk_start = start, "unexpected status; retrying");
                tokio::time::sleep((RETRY_BACKOFF * attempt.min(16)).min(MAX_BACKOFF)).await;
                continue;
            }
        }

        let mut file = tokio::fs::OpenOptions::new().write(true).open(&dest).await?;
        file.seek(SeekFrom::Start(offset)).await?;

        let interrupted;
        loop {
            if cancel.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            match resp.chunk().await {
                Ok(Some(bytes)) => {
                    file.write_all(&bytes).await?;
                    let n = bytes.len() as u64;
                    downloaded.fetch_add(n, Ordering::Relaxed);
                    total_downloaded.fetch_add(n, Ordering::Relaxed);
                }
                Ok(None) => {
                    let _ = file.flush().await;
                    let _ = file.sync_all().await;
                    let complete = expected
                        .is_none_or(|len| downloaded.load(Ordering::Relaxed) >= len);
                    if complete {
                        done.store(true, Ordering::Relaxed);
                        return Ok(());
                    }
                    interrupted = true; // premature close; reconnect from the new offset
                    break;
                }
                Err(e) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(DownloadError::ExhaustedRetries {
                            attempts: MAX_ATTEMPTS,
                            last_error: e.to_string(),
                        });
                    }
                    tracing::debug!(error = %e, chunk_start = start, "chunk stream error");
                    interrupted = true;
                    break;
                }
            }
        }
        if !interrupted {
            done.store(true, Ordering::Relaxed);
            return Ok(());
        }
        if attempt >= MAX_ATTEMPTS {
            return Err(DownloadError::ExhaustedRetries {
                attempts: MAX_ATTEMPTS,
                last_error: "stream interrupted repeatedly".to_owned(),
            });
        }
        tokio::time::sleep((RETRY_BACKOFF * attempt.min(16)).min(MAX_BACKOFF)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_evenly_with_remainder_on_last_chunk() {
        let chunks = split(Some(10 * MIN_CHUNK_SIZE), 4);
        assert_eq!(chunks.len(), 4);
        assert_eq!(chunks[0].start, 0);
        assert_eq!(chunks.last().unwrap().end, 10 * MIN_CHUNK_SIZE - 1);
        for w in chunks.windows(2) {
            assert_eq!(w[1].start, w[0].end + 1);
        }
    }

    #[test]
    fn never_splits_below_min_chunk_size() {
        let chunks = split(Some(2 * MIN_CHUNK_SIZE + 1), 8);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn unknown_total_is_open_ended_single_chunk() {
        let chunks = split(None, 8);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].end, OPEN_ENDED);
    }

    #[test]
    fn zero_total_is_immediately_done() {
        let chunks = split(Some(0), 8);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].done);
    }

    #[test]
    fn parses_content_range_total() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("garbage"), None);
    }
}
