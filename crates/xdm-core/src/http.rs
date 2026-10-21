//! Segmented, resumable HTTP downloader with dynamic chunk splitting and
//! chunk stealing.
//!
//! Rust port of xdm-core's `HttpDownloaderTask` / `HttpChunkRetriever`:
//! - one probe request detects range support, total size, and a validator,
//! - the download starts as a **single** connection; a coordinator loop
//!   repeatedly splits the largest chunk while connections are free
//!   (`split_offsets`, min 1 MiB per half),
//! - a connection that stops making progress for `STALL_MS` has its tail
//!   **stolen** by a fresh connection (`takeOverChunk` in the original): the
//!   stalled chunk's end shrinks to its current position and a new chunk
//!   carries the remaining range,
//! - chunk ends are shared atomics; every task re-reads its end before each
//!   write, so a shrunk chunk can never write past it — the file can only
//!   ever receive each byte from one owner (modulo same-content overlap in a
//!   tiny check-then-write race, which is harmless),
//! - per-chunk state is persisted periodically and on cancel/failure, so a
//!   crash loses at most one save interval,
//! - resume sends `If-Range` (ETag / Last-Modified); a stale validator fails
//!   loudly with [`DownloadError::RestartRequired`] instead of corrupting
//!   data (retried once automatically, wiping state).

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::header::{CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const USER_AGENT: &str = concat!("xdm-rs/", env!("CARGO_PKG_VERSION"));
/// Never split below this size (the original split halves of >= 256 KB).
const MIN_CHUNK_SIZE: u64 = 1024 * 1024;
/// Retries per chunk without progress (request failures and mid-stream
/// reconnects). Any successful write resets the budget.
pub(crate) const MAX_ATTEMPTS: u32 = 10;
const RETRY_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(8);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Idle timeout between body reads; a stalled server drops the connection.
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// A connection with no written bytes for this long gets its tail stolen.
const STALL_MS: u64 = 5000;
/// How often the coordinator re-evaluates splits, steals, and completion.
const COORDINATOR_TICK: Duration = Duration::from_millis(250);
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
    #[error("manifest: {0}")]
    Manifest(String),
    #[error("decrypt: {0}")]
    Decrypt(String),
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

/// The live, shared form of a chunk. `end` is mutable: the coordinator
/// shrinks it when splitting or stealing. Tasks re-read it before every
/// write, which is what makes shrinking safe.
struct SharedChunk {
    start: u64,
    /// Inclusive; `OPEN_ENDED` = unknown size, never split.
    end: AtomicU64,
    downloaded: AtomicU64,
    done: AtomicBool,
    spawned: AtomicBool,
    failed: AtomicBool,
    error: Mutex<Option<DownloadError>>,
    /// Unix millis of the last successful write (creation time initially).
    last_progress_ms: AtomicU64,
}

impl SharedChunk {
    fn new(start: u64, end: u64) -> Self {
        Self {
            start,
            end: AtomicU64::new(end),
            downloaded: AtomicU64::new(0),
            done: AtomicBool::new(false),
            spawned: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            error: Mutex::new(None),
            last_progress_ms: AtomicU64::new(now_ms()),
        }
    }

    fn position(&self) -> u64 {
        self.start + self.downloaded.load(Ordering::Relaxed)
    }

    fn remaining(&self) -> Option<u64> {
        let end = self.end.load(Ordering::Relaxed);
        if end == OPEN_ENDED {
            return None;
        }
        let pos = self.position();
        Some(if pos > end { 0 } else { end - pos + 1 })
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }

    fn is_running(&self) -> bool {
        self.spawned.load(Ordering::Relaxed) && !self.is_done()
    }
}

/// Immutable-per-run inputs shared by every chunk task.
struct RunCtx {
    client: Client,
    url: String,
    dest: PathBuf,
    accept_ranges: bool,
    validator: Option<String>,
    total_downloaded: Arc<AtomicU64>,
}

pub struct HttpDownload {
    client: Client,
    url: String,
    dest: PathBuf,
    state_path: PathBuf,
    config: DownloadConfig,
}

impl HttpDownload {
    pub fn new(url: impl Into<String>, dest: impl Into<PathBuf>) -> Result<Self, DownloadError> {
        Self::with_config(url, dest, DownloadConfig::default())
    }

    pub fn with_config(
        url: impl Into<String>,
        dest: impl Into<PathBuf>,
        config: DownloadConfig,
    ) -> Result<Self, DownloadError> {
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
            .map_err(DownloadError::Http)?;
        Ok(Self {
            client,
            url: url.into(),
            dest,
            state_path,
            config,
        })
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

        // Trivial case: empty file.
        if probe.total == Some(0) {
            tokio::fs::File::create(&self.dest).await?;
            let _ = tokio::fs::remove_file(&self.state_path).await;
            return Ok(Some(0));
        }

        let dest_exists = tokio::fs::try_exists(&self.dest).await.unwrap_or(false);
        let resumed = saved.as_ref().is_some_and(|s| {
            dest_exists && s.total == probe.total && s.chunks.iter().any(|c| c.downloaded > 0 || c.done)
        });

        // The engine has its own cancel token, derived from the user's, so a
        // failed chunk can stop its siblings without reporting "cancelled"
        // to the caller.
        let internal = cancel.child_token();
        let total_downloaded = Arc::new(AtomicU64::new(0));

        let chunks: Arc<Mutex<Vec<Arc<SharedChunk>>>> = Arc::new(Mutex::new(Vec::new()));
        if resumed {
            if let Some(saved) = saved.as_ref() {
                let mut table =
                    chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                for c in &saved.chunks {
                    total_downloaded.fetch_add(c.downloaded, Ordering::Relaxed);
                    let sc = SharedChunk::new(c.start, c.end);
                    sc.downloaded.store(c.downloaded, Ordering::Relaxed);
                    sc.done.store(c.done, Ordering::Relaxed);
                    sc.spawned.store(c.done, Ordering::Relaxed); // done chunks need no task
                    table.push(Arc::new(sc));
                }
            }
        } else {
            // Fresh start: one connection covering everything; the
            // coordinator splits it while connections are free.
            let f = tokio::fs::File::create(&self.dest).await?;
            if let Some(total) = probe.total {
                f.set_len(total).await?;
            }
            f.sync_all().await?;
            let end = probe.total.map_or(OPEN_ENDED, |t| t - 1);
            chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(Arc::new(SharedChunk::new(0, end)));
        }

        let initial: u64 = total_downloaded.load(Ordering::Relaxed);
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
            let chunks = chunks.clone();
            let cancel_p = internal.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(STATE_SAVE_INTERVAL);
                interval.tick().await; // first tick fires immediately; skip it
                loop {
                    tokio::select! {
                        _ = interval.tick() => {}
                        _ = cancel_p.cancelled() => break,
                    }
                    let snapshot = snapshot_chunks(&chunks);
                    let state = build_state(&url, total, &etag, &last_modified, accept_ranges, &snapshot);
                    save_state(&state_path, &state).await;
                }
            })
        };

        let ctx = RunCtx {
            client: self.client.clone(),
            url: self.url.clone(),
            dest: self.dest.clone(),
            accept_ranges: probe.accept_ranges,
            validator: probe.validator(),
            total_downloaded: total_downloaded.clone(),
        };

        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        {
            let table = chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            for c in table.iter().filter(|c| !c.is_done()) {
                spawn_chunk(&ctx, c.clone(), internal.clone(), &mut handles);
            }
        }

        let mut failure: Option<DownloadError> = None;
        loop {
            if cancel.is_cancelled() {
                failure = Some(DownloadError::Cancelled);
                break;
            }

            let mut all_done = true;
            let mut active = 0usize;
            let mut chunk_failed = false;
            {
                let table = chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                for c in table.iter() {
                    if c.is_done() {
                        continue;
                    }
                    all_done = false;
                    if c.failed.load(Ordering::Relaxed) {
                        chunk_failed = true;
                        break;
                    }
                    if c.is_running() {
                        active += 1;
                    }
                }
            }
            if chunk_failed {
                failure = take_failure(&chunks);
                break;
            }
            if all_done {
                break;
            }

            if active < self.config.max_chunks {
                let mut table = chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let now = now_ms();

                // 1) Rescue stalled connections: hand their tail to a fresh one.
                let mut steals: Vec<(Arc<SharedChunk>, u64, u64)> = Vec::new();
                for c in table.iter() {
                    if active + steals.len() >= self.config.max_chunks {
                        break;
                    }
                    if !c.is_running() {
                        continue;
                    }
                    let stalled =
                        now.saturating_sub(c.last_progress_ms.load(Ordering::Relaxed)) > STALL_MS;
                    if !stalled {
                        continue;
                    }
                    let pos = c.position();
                    let end = c.end.load(Ordering::Relaxed);
                    if end == OPEN_ENDED || pos > end {
                        continue;
                    }
                    let remaining = end - pos + 1;
                    if remaining < 2 * MIN_CHUNK_SIZE {
                        continue;
                    }
                    let head_end = pos + remaining / 2 - 1;
                    steals.push((c.clone(), head_end, end));
                }
                for (c, head_end, end) in steals {
                    c.end.store(head_end, Ordering::Relaxed);
                    let tail = Arc::new(SharedChunk::new(head_end + 1, end));
                    table.push(tail.clone());
                    active += 1;
                    tracing::debug!(from = c.start, stolen_at = head_end + 1, "stalled chunk tail stolen");
                    spawn_chunk(&ctx, tail, internal.clone(), &mut handles);
                }

                // 2) Grow: split the largest chunk while connections are free.
                if active < self.config.max_chunks {
                    let mut best: Option<(u64, Arc<SharedChunk>)> = None;
                    for c in table.iter() {
                        if !c.is_running() {
                            continue;
                        }
                        if let Some(remaining) = c.remaining()
                            && remaining >= 2 * MIN_CHUNK_SIZE
                            && best.as_ref().is_none_or(|(br, _)| remaining > *br)
                        {
                            best = Some((remaining, c.clone()));
                        }
                    }
                    if let Some((_, c)) = best {
                        let pos = c.position();
                        let end = c.end.load(Ordering::Relaxed);
                        if let Some((head_end, tail_start)) = split_offsets(pos, end) {
                            c.end.store(head_end, Ordering::Relaxed);
                            let tail = Arc::new(SharedChunk::new(tail_start, end));
                            table.push(tail.clone());
                            tracing::debug!(at = tail_start, "split chunk");
                            spawn_chunk(&ctx, tail, internal.clone(), &mut handles);
                        }
                    }
                }
            }

            tokio::time::sleep(COORDINATOR_TICK).await;
        }

        // Stop all tasks (no-op when they already finished) and reap them.
        internal.cancel();
        for handle in handles {
            let _ = handle.await;
        }
        persister.abort();
        ticker.abort();

        let snapshot = snapshot_chunks(&chunks);
        match failure {
            None => {
                let _ = tokio::fs::remove_file(&self.state_path).await;
                let _ = progress.send(Progress {
                    downloaded: total_downloaded.load(Ordering::Relaxed),
                    total: probe.total,
                });
                Ok(probe.total)
            }
            Some(e) => {
                let state = build_state(
                    &self.url,
                    probe.total,
                    &probe.etag,
                    &probe.last_modified,
                    probe.accept_ranges,
                    &snapshot,
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
        if let Some(saved) = saved
            && let Some(v) = saved.etag.as_deref().or(saved.last_modified.as_deref())
        {
            req = req.header(IF_RANGE, v);
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

/// Where to split a chunk whose next unwritten byte is `pos`, given the
/// inclusive `end`. Returns `(head_end, tail_start)`, keeping both halves
/// at least `MIN_CHUNK_SIZE` — or `None` when splitting isn't worthwhile.
fn split_offsets(pos: u64, end: u64) -> Option<(u64, u64)> {
    if end == OPEN_ENDED {
        return None;
    }
    let remaining = end.checked_sub(pos)? + 1;
    if remaining < 2 * MIN_CHUNK_SIZE {
        return None;
    }
    let head_end = pos + remaining / 2 - 1;
    Some((head_end, head_end + 1))
}

fn snapshot_chunks(chunks: &Mutex<Vec<Arc<SharedChunk>>>) -> Vec<ChunkState> {
    chunks
        .lock()
        .unwrap()
        .iter()
        .map(|c| ChunkState {
            start: c.start,
            end: c.end.load(Ordering::Relaxed),
            downloaded: c.downloaded.load(Ordering::Relaxed),
            done: c.done.load(Ordering::Relaxed),
        })
        .collect()
}

fn take_failure(chunks: &Mutex<Vec<Arc<SharedChunk>>>) -> Option<DownloadError> {
    let table = chunks.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    for c in table.iter() {
        if c.failed.load(Ordering::Relaxed) {
            let err = c.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
            return Some(err.unwrap_or(DownloadError::ExhaustedRetries {
                attempts: MAX_ATTEMPTS,
                last_error: "chunk failed".to_owned(),
            }));
        }
    }
    None
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

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn backoff(attempt: u32) -> Duration {
    (RETRY_BACKOFF * attempt.min(16)).min(MAX_BACKOFF)
}

fn spawn_chunk(
    ctx: &RunCtx,
    chunk: Arc<SharedChunk>,
    internal: CancellationToken,
    handles: &mut Vec<JoinHandle<()>>,
) {
    if chunk.spawned.swap(true, Ordering::Relaxed) {
        return;
    }
    let task_ctx = RunCtx {
        client: ctx.client.clone(),
        url: ctx.url.clone(),
        dest: ctx.dest.clone(),
        accept_ranges: ctx.accept_ranges,
        validator: ctx.validator.clone(),
        total_downloaded: ctx.total_downloaded.clone(),
    };
    handles.push(tokio::spawn(run_chunk(task_ctx, chunk, internal)));
}

async fn run_chunk(ctx: RunCtx, chunk: Arc<SharedChunk>, internal: CancellationToken) {
    let result = run_chunk_inner(&ctx, &chunk, &internal).await;
    // User/initiated cancellations are not chunk failures.
    if let Err(e) = result
        && !matches!(&e, DownloadError::Cancelled)
    {
        chunk
            .error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(e);
        chunk.failed.store(true, Ordering::Relaxed);
    }
}

async fn run_chunk_inner(
    ctx: &RunCtx,
    chunk: &SharedChunk,
    internal: &CancellationToken,
) -> Result<(), DownloadError> {
    let mut attempt: u32 = 0;
    let mut last_error = "unknown".to_owned();
    loop {
        if internal.is_cancelled() {
            return Err(DownloadError::Cancelled);
        }
        attempt += 1;

        let offset = chunk.position();
        let end = chunk.end.load(Ordering::Relaxed);
        if end != OPEN_ENDED && offset > end {
            // Everything we owned is written (possibly after a shrink).
            chunk.done.store(true, Ordering::Relaxed);
            return Ok(());
        }

        let mut req = ctx.client.get(&ctx.url);
        if ctx.accept_ranges {
            req = req.header(RANGE, format!("bytes={offset}-{}", end_suffix(end)));
            if let Some(v) = &ctx.validator {
                req = req.header(IF_RANGE, v);
            }
        }
        let mut resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                last_error = e.to_string();
                if attempt >= MAX_ATTEMPTS {
                    return Err(DownloadError::ExhaustedRetries { attempts: attempt, last_error });
                }
                tracing::debug!(error = %e, chunk_start = chunk.start, "chunk request failed; retrying");
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        };

        match resp.status() {
            StatusCode::PARTIAL_CONTENT => {}
            // Full body is only acceptable for a range-less server from offset 0.
            StatusCode::OK if !ctx.accept_ranges && offset == 0 => {}
            StatusCode::OK => return Err(DownloadError::RestartRequired),
            s if s.is_success() => return Err(DownloadError::RestartRequired),
            s => {
                last_error = format!("unexpected status {s}");
                if attempt >= MAX_ATTEMPTS {
                    return Err(DownloadError::ExhaustedRetries { attempts: attempt, last_error });
                }
                tracing::debug!(status = %s, chunk_start = chunk.start, "unexpected status; retrying");
                tokio::time::sleep(backoff(attempt)).await;
                continue;
            }
        }

        let mut file = tokio::fs::OpenOptions::new().write(true).open(&ctx.dest).await?;
        file.seek(SeekFrom::Start(offset)).await?;

        'stream: loop {
            if internal.is_cancelled() {
                return Err(DownloadError::Cancelled);
            }
            match resp.chunk().await {
                Ok(Some(bytes)) => {
                    let mut piece: &[u8] = &bytes;
                    while !piece.is_empty() {
                        // Re-read the (mutable) end before every write: the
                        // coordinator may have split or stolen our tail.
                        let cur_end = chunk.end.load(Ordering::Relaxed);
                        let pos = chunk.position();
                        let allowed: u64 = if cur_end == OPEN_ENDED {
                            piece.len() as u64
                        } else if pos > cur_end {
                            0
                        } else {
                            (cur_end - pos + 1).min(piece.len() as u64)
                        };
                        if allowed == 0 {
                            // Our remaining range belongs to someone else now.
                            chunk.done.store(true, Ordering::Relaxed);
                            return Ok(());
                        }
                        let n = allowed as usize;
                        file.write_all(&piece[..n]).await?;
                        chunk.downloaded.fetch_add(n as u64, Ordering::Relaxed);
                        ctx.total_downloaded.fetch_add(n as u64, Ordering::Relaxed);
                        chunk.last_progress_ms.store(now_ms(), Ordering::Relaxed);
                        attempt = 0; // progress resets the retry budget
                        piece = &piece[n..];
                    }
                    // Finished our (possibly shrunk) range?
                    let cur_end = chunk.end.load(Ordering::Relaxed);
                    if cur_end != OPEN_ENDED && chunk.position() > cur_end {
                        chunk.done.store(true, Ordering::Relaxed);
                        return Ok(());
                    }
                }
                Ok(None) => {
                    let _ = file.flush().await;
                    let _ = file.sync_all().await;
                    let cur_end = chunk.end.load(Ordering::Relaxed);
                    let complete = cur_end == OPEN_ENDED || chunk.position() > cur_end;
                    if complete {
                        chunk.done.store(true, Ordering::Relaxed);
                        return Ok(());
                    }
                    break 'stream; // premature close; reconnect from the new offset
                }
                Err(e) => {
                    last_error = e.to_string();
                    tracing::debug!(error = %e, chunk_start = chunk.start, "chunk stream error");
                    break 'stream;
                }
            }
        }
        if attempt >= MAX_ATTEMPTS {
            return Err(DownloadError::ExhaustedRetries { attempts: attempt, last_error });
        }
        tokio::time::sleep(backoff(attempt)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_offsets_halves_the_remaining_range() {
        let (head_end, tail_start) = split_offsets(0, 10 * MIN_CHUNK_SIZE - 1).unwrap();
        assert_eq!(head_end, 5 * MIN_CHUNK_SIZE - 1);
        assert_eq!(tail_start, 5 * MIN_CHUNK_SIZE);
    }

    #[test]
    fn split_offsets_splits_from_current_position() {
        // 6 MiB remaining from pos=2 MiB: head gets 3, tail gets 3.
        let (head_end, tail_start) = split_offsets(2 * MIN_CHUNK_SIZE, 8 * MIN_CHUNK_SIZE - 1).unwrap();
        assert_eq!(head_end, 5 * MIN_CHUNK_SIZE - 1);
        assert_eq!(tail_start, 5 * MIN_CHUNK_SIZE);
    }

    #[test]
    fn split_offsets_refuses_small_or_consumed_chunks() {
        assert!(split_offsets(0, MIN_CHUNK_SIZE - 1).is_none()); // < 2 MiB total
        assert!(split_offsets(2 * MIN_CHUNK_SIZE, 2 * MIN_CHUNK_SIZE + 5).is_none());
        assert!(split_offsets(5 * MIN_CHUNK_SIZE, 4 * MIN_CHUNK_SIZE).is_none()); // consumed
        assert!(split_offsets(0, OPEN_ENDED).is_none()); // unknown size
    }

    #[test]
    fn parses_content_range_total() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("garbage"), None);
    }
}
