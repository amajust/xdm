//! Shared download types. Port of xdm-core `downloaders/Models.kt`.

/// Kind of download. Mirrors the original `DownloadType` enum.
/// `Torrent` is a placeholder in the original too (a `TODO()` there).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DownloadType {
    Http,
    Hls,
    Dash,
    Batch,
    Torrent,
}
