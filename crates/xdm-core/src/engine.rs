//! URL dispatch: routes each URL to the right downloader.
//!
//! Extension-based detection on the path component (query/fragment stripped):
//! `.m3u8` → HLS, `.mpd` → DASH, everything else → segmented HTTP. This is
//! the single entry point used by the CLI and the GUI.

use std::path::PathBuf;

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::http::{DownloadConfig, DownloadError, HttpDownload, Progress};
use crate::streaming::StreamingDownload;

/// Runs the right downloader for `url`. Returns the total bytes on success.
pub async fn download(
    url: &str,
    dest: impl Into<PathBuf>,
    config: DownloadConfig,
    cancel: CancellationToken,
    progress: watch::Sender<Progress>,
) -> Result<Option<u64>, DownloadError> {
    let path_only = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    if path_only.ends_with(".m3u8") {
        StreamingDownload::hls(url, dest, config)?.run(cancel, progress).await
    } else if path_only.ends_with(".mpd") {
        StreamingDownload::dash(url, dest, config)?.run(cancel, progress).await
    } else {
        HttpDownload::new(url, dest)?.run(cancel, progress).await
    }
}
