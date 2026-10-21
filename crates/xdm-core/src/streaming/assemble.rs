//! Stream assembly by concatenation — the "no external tools" muxer.
//!
//! Port of the *assembly* half of `StreamingDownloaderTask`/`TransmuxingMuxer`,
//! scoped to what concatenation can do losslessly:
//! - fMP4: init segment followed by moof/mdat media segments is a valid
//!   fragmented MP4 by construction (how DASH/HLS fMP4 is defined),
//! - MPEG-TS: a plain concatenation of TS packets; decoders resync on the
//!   0x47 packet boundary, so this plays in VLC/mpv/ffmpeg,
//! - ADTS AAC / MP3: frame-based; plain concatenation is a valid stream.
//!
//! Full TS→MP4 remuxing (the original's `TransmuxingMuxer`) is future work.

use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Concatenates `parts` (in order) into `out`.
pub(crate) async fn concatenate(parts: &[std::path::PathBuf], out: &Path) -> Result<(), std::io::Error> {
    let mut out_file = tokio::fs::File::create(out).await?;
    let mut buf = [0u8; 64 * 1024];
    for part in parts {
        let mut f = tokio::fs::File::open(part).await?;
        loop {
            let n = f.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            out_file.write_all(&buf[..n]).await?;
        }
    }
    out_file.flush().await?;
    out_file.sync_all().await?;
    Ok(())
}

/// Picks a reasonable file extension from a segment URL.
pub(crate) fn ext_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next().unwrap_or("");
    if ext.is_empty() || ext.contains('/') {
        "bin".to_owned()
    } else {
        ext.to_ascii_lowercase()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ext_from_url_handles_queries_and_missing_extensions() {
        assert_eq!(ext_from_url("https://x.test/a/seg0.ts?token=1"), "ts");
        assert_eq!(ext_from_url("https://x.test/a/video.m4s"), "m4s");
        assert_eq!(ext_from_url("https://x.test/noext"), "bin");
        assert_eq!(ext_from_url("https://x.test/a.b/c"), "bin");
    }
}
