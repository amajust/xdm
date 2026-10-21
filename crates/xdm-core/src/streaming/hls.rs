//! HLS playlist parsing (media + master playlists), port of `HlsParser.kt`
//! (subset: EXTINF, EXT-X-BYTERANGE, EXT-X-KEY AES-128, EXT-X-MAP,
//! EXT-X-MEDIA-SEQUENCE; SAMPLE-AES and discontinuity stitching are out of
//! scope for now).

use url::Url;

/// One variant of a master playlist.
#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
}

/// An HLS encryption key reference (AES-128 only; SAMPLE-AES unsupported).
#[derive(Debug, Clone, PartialEq)]
pub struct HlsKey {
    pub url: String,
    /// Explicit IV from the playlist; `None` means derive from media sequence.
    pub iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HlsSegment {
    pub url: String,
    pub duration: f64,
    /// `(offset, length)` into the resource — inclusive HTTP range.
    pub byte_range: Option<(u64, u64)>,
    pub key: Option<HlsKey>,
}

#[derive(Debug, Clone, Default)]
pub struct MediaPlaylist {
    /// fMP4 init segment (EXT-X-MAP): url + optional byte range.
    pub init_segment: Option<(String, Option<(u64, u64)>)>,
    pub segments: Vec<HlsSegment>,
    /// EXT-X-MEDIA-SEQUENCE; used to derive default AES IVs.
    pub media_sequence: u64,
}

pub fn is_master_playlist(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with("#EXT-X-STREAM-INF"))
}

pub fn parse_master(text: &str, base: &Url) -> Result<Vec<Variant>, String> {
    let mut variants = Vec::new();
    let mut pending_bandwidth: Option<u64> = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_bandwidth = attr_value(attrs, "BANDWIDTH").and_then(|v| v.parse().ok());
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = base.join(line).map_err(|e| format!("variant url: {e}"))?;
            variants.push(Variant {
                url: url.into(),
                bandwidth: pending_bandwidth.take().unwrap_or(0),
            });
        }
    }
    if variants.is_empty() {
        return Err("master playlist has no variants".to_owned());
    }
    Ok(variants)
}

pub fn parse_media(text: &str, base: &Url) -> Result<MediaPlaylist, String> {
    let mut pl = MediaPlaylist::default();
    let mut pending_duration: Option<f64> = None;
    // `Some((len, explicit_offset))`; an absent offset continues after the
    // previous sub-range of the same resource.
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    let mut next_range_offset: u64 = 0;
    let mut current_key: Option<HlsKey> = None;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#EXTINF:") {
            let dur = rest.split(',').next().unwrap_or("0").trim();
            pending_duration = dur.parse().ok();
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let (len_s, off_s) = match rest.split_once('@') {
                Some((l, o)) => (l, Some(o)),
                None => (rest, None),
            };
            let len: u64 = len_s
                .trim()
                .parse()
                .map_err(|_| "EXT-X-BYTERANGE: bad length".to_owned())?;
            let offset = match off_s {
                Some(o) => Some(
                    o.trim()
                        .parse()
                        .map_err(|_| "EXT-X-BYTERANGE: bad offset".to_owned())?,
                ),
                None => None,
            };
            pending_range = Some((len, offset));
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-KEY:") {
            current_key = parse_key_attr(attrs, base)?;
        } else if let Some(attrs) = line.strip_prefix("#EXT-X-MAP:") {
            let uri = attr_value(attrs, "URI").ok_or("EXT-X-MAP: missing URI")?;
            let url = base.join(&uri).map_err(|e| format!("init url: {e}"))?;
            let range = attr_value(attrs, "BYTERANGE").and_then(|v| parse_byterange(&v));
            pl.init_segment = Some((url.into(), range));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            pl.media_sequence = rest.trim().parse().unwrap_or(0);
        } else if line.starts_with('#') {
            // Other tags (DISCONTINUITY, PART, ...) are ignored.
        } else {
            let url = base.join(line).map_err(|e| format!("segment url: {e}"))?;
            let byte_range = match pending_range.take() {
                Some((len, Some(off))) => Some((off, len)),
                Some((len, None)) => Some((next_range_offset, len)),
                None => None,
            };
            if let Some((off, len)) = byte_range {
                next_range_offset = off + len;
            }
            pl.segments.push(HlsSegment {
                url: url.into(),
                duration: pending_duration.take().unwrap_or(0.0),
                byte_range,
                key: current_key.clone(),
            });
        }
    }
    Ok(pl)
}

fn parse_key_attr(attrs: &str, base: &Url) -> Result<Option<HlsKey>, String> {
    let method = attr_value(attrs, "METHOD").unwrap_or_default();
    if method.eq_ignore_ascii_case("NONE") {
        return Ok(None);
    }
    // SAMPLE-AES (fairplay etc.) is not supported.
    if !method.eq_ignore_ascii_case("AES-128") {
        return Ok(None);
    }
    let uri = attr_value(attrs, "URI").ok_or("EXT-X-KEY: missing URI")?;
    let url = base.join(&uri).map_err(|e| format!("key url: {e}"))?;
    let iv = attr_value(attrs, "IV").and_then(|v| parse_hex16(&v));
    Ok(Some(HlsKey { url: url.into(), iv }))
}

/// Reads `NAME=VALUE` pairs from an attribute list; values may be quoted.
fn attr_value(attrs: &str, name: &str) -> Option<String> {
    for kv in attrs.split(',') {
        let (k, v) = kv.split_once('=')?;
        if k.trim().eq_ignore_ascii_case(name) {
            return Some(v.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Parses `<len>[@<offset>]` (unquoted, from EXT-X-BYTERANGE) or the quoted
/// BYTERANGE attribute form.
fn parse_byterange(s: &str) -> Option<(u64, u64)> {
    let s = s.trim().trim_matches('"');
    let (len_s, off_s) = match s.split_once('@') {
        Some((l, o)) => (l, Some(o)),
        None => (s, None),
    };
    let len: u64 = len_s.trim().parse().ok()?;
    let offset: u64 = match off_s {
        Some(o) => o.trim().parse().ok()?,
        None => 0,
    };
    Some((offset, len))
}

pub(crate) fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let s = s
        .trim()
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s.trim());
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    parse_hex(s).and_then(|v| v.try_into().ok())
}

/// Default AES IV per the HLS spec: the 128-bit big-endian media sequence.
pub(crate) fn default_iv(media_sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&media_sequence.to_be_bytes());
    iv
}

pub(crate) fn decrypt_aes128_cbc(key: &[u8], iv: &[u8], data: &[u8]) -> Result<Vec<u8>, String> {
    use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
    let key: [u8; 16] = key.try_into().map_err(|_| "key must be 16 bytes".to_owned())?;
    let iv: [u8; 16] = iv.try_into().map_err(|_| "iv must be 16 bytes".to_owned())?;
    let dec = Aes128CbcDec::new((&key).into(), (&iv).into());
    dec.decrypt_padded_vec_mut::<Pkcs7>(data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/live/master/").unwrap()
    }

    #[test]
    fn master_detection_and_variant_parsing() {
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=1280x720\n720p.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=4128000\n1080p.m3u8\n";
        assert!(is_master_playlist(text));
        let base = base();
        let variants = parse_master(text, &base).unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[1].bandwidth, 4128000);
        assert!(variants[0].url.ends_with("/live/master/720p.m3u8"));
    }

    #[test]
    fn media_playlist_parses_segments_keys_and_map() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA-SEQUENCE:42\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090a0b0c0d0e0f\n\
            #EXTINF:4.0,\nseg0.ts\n\
            #EXT-X-KEY:METHOD=NONE\n\
            #EXTINF:4.0,\nseg1.ts\n\
            #EXT-X-MAP:URI=\"init.mp4\"\n";
        let base = base();
        let pl = parse_media(text, &base).unwrap();
        assert_eq!(pl.media_sequence, 42);
        assert_eq!(pl.segments.len(), 2);
        let k = pl.segments[0].key.as_ref().unwrap();
        assert!(k.url.ends_with("/live/master/key.bin"));
        assert_eq!(k.iv.unwrap()[1], 1);
        assert!(pl.segments[1].key.is_none());
        assert!(pl.init_segment.as_ref().unwrap().0.ends_with("init.mp4"));
    }

    #[test]
    fn byterange_offsets_continue_implicitly() {
        let text = "#EXTM3U\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:1000@500\nvideo.ts\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:1000\nvideo.ts\n";
        let pl = parse_media(text, &base()).unwrap();
        assert_eq!(pl.segments[0].byte_range, Some((500, 1000)));
        // Second range continues after the first: [1500, 2500).
        assert_eq!(pl.segments[1].byte_range, Some((1500, 1000)));
    }

    #[test]
    fn hex_and_default_iv() {
        assert_eq!(parse_hex("0x0A0B"), Some(vec![10, 11]));
        assert_eq!(parse_hex("zz"), None);
        let iv = default_iv(1);
        assert_eq!(iv[..8], [0u8; 8]);
        assert_eq!(iv[15], 1);
    }

    #[test]
    fn aes_roundtrip() {
        use cbc::cipher::{BlockEncryptMut, KeyIvInit, block_padding::Pkcs7};
        type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let msg = b"hello hls segment payload 1234567"; // 33 bytes
        let enc = Aes128CbcEnc::new((&key).into(), (&iv).into());
        let ct = enc.encrypt_padded_vec_mut::<Pkcs7>(msg);
        let pt = decrypt_aes128_cbc(&key, &iv, &ct).unwrap();
        assert_eq!(pt, msg);
    }
}
