//! AAC ADTS → raw frames + AudioSpecificConfig, port of the audio path of
//! `TsDemuxer`/`TransmuxingMuxer`.

use super::Sample;

/// MPEG-4 audio object type (ADTS profile + 1): 2 = AAC-LC.
const AAC_LC_OBJECT_TYPE: u8 = 2;

/// Standard AAC sample-rate table (index → Hz).
const SAMPLE_RATES: [u32; 16] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350, 0, 0,
    0,
];

#[derive(Debug, Clone, PartialEq)]
pub struct AdtsConfig {
    pub object_type: u8,
    pub sample_rate: u32,
    pub sample_rate_index: u8,
    pub channels: u8,
}

/// One parsed ADTS frame.
pub struct AdtsFrame {
    /// Raw AAC payload without the ADTS header.
    pub data: Vec<u8>,
    pub channels: u8,
    pub sample_rate_index: u8,
    pub object_type: u8,
    /// Frames carry exactly 1024 samples each (per AAC framing).
    pub duration: u64,
}

/// Splits an ADTS stream into raw AAC frames; returns the config of the
/// first frame. Returns `None` when no ADTS sync is found.
pub fn parse_adts(data: &[u8]) -> Option<(AdtsConfig, Vec<Vec<u8>>)> {
    let mut frames = Vec::new();
    let mut config: Option<AdtsConfig> = None;
    let mut pos = 0;
    while pos + 7 <= data.len() {
        // Sync: 12 bits of 1s.
        if data[pos] != 0xFF || (data[pos + 1] & 0xF0) != 0xF0 {
            pos += 1;
            continue;
        }
        let protection_absent = data[pos + 1] & 0x01 != 0;
        let header_len = if protection_absent { 7 } else { 9 };
        if pos + header_len > data.len() {
            break;
        }
        let profile = (data[pos + 2] >> 6) & 0x3;
        let sample_rate_index = (data[pos + 2] >> 2) & 0xF;
        let channels = ((data[pos + 2] & 0x01) << 2) | ((data[pos + 3] >> 6) & 0x3);
        let frame_len = (((data[pos + 3] as usize) & 0x03) << 11)
            | ((data[pos + 4] as usize) << 3)
            | ((data[pos + 5] as usize) >> 5);
        if frame_len < header_len {
            pos += 1;
            continue;
        }
        if pos + frame_len > data.len() {
            break;
        }
        if config.is_none() {
            config = Some(AdtsConfig {
                object_type: profile + 1,
                sample_rate: SAMPLE_RATES
                    .get(sample_rate_index as usize)
                    .copied()
                    .unwrap_or(48000),
                sample_rate_index,
                channels,
            });
        }
        let body = data[pos + header_len..pos + frame_len].to_vec();
        frames.push(body);
        pos += frame_len;
    }
    config.map(|c| {
        (
            c,
            frames,
        )
    })
}

/// Builds the 2-byte AudioSpecificConfig for the esds box.
pub fn build_asc(config: &AdtsConfig) -> [u8; 2] {
    let ot = if config.object_type == 0 {
        AAC_LC_OBJECT_TYPE
    } else {
        config.object_type
    };
    let v = ((ot as u16) << 11)
        | ((config.sample_rate_index as u16 & 0xF) << 7)
        | ((config.channels as u16 & 0xF) << 3);
    [(v >> 8) as u8, (v & 0xFF) as u8]
}

/// Converts one PES payload (ADTS) into raw-AAC samples with durations.
pub fn adts_to_samples(payload: &[u8], dts_base: u64) -> Vec<Sample> {
    let Some((_, frames)) = parse_adts(payload) else { return Vec::new() };
    // Each AAC frame carries exactly 1024 samples.
    frames
        .into_iter()
        .enumerate()
        .map(|(i, data)| Sample {
            data,
            dts: dts_base + i as u64 * 1024,
            cts: 0,
            duration: 1024,
            keyframe: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adts_frame(payload: &[u8], len_extra: usize) -> Vec<u8> {
        let frame_len = 7 + payload.len() + len_extra;
        let mut f = Vec::with_capacity(7 + payload.len());
        f.extend_from_slice(&[
            0xFF,
            0xF1, // MPEG-4, layer 0, protection absent
            (1 << 6) | (3 << 2), // profile=1 (LC→OT2), 48kHz (idx 3), channel hi=0
            (0x01 << 6) | (((frame_len >> 11) & 0x3) as u8), // channels lo=01 + len hi
            ((frame_len >> 3) & 0xFF) as u8,
            ((frame_len & 0x7) as u8) << 5,
            0,
        ]);
        f.extend_from_slice(payload);
        f
    }

    #[test]
    fn parses_adts_stream() {
        let payload = vec![0x11u8, 0x22, 0x33, 0x44];
        let mut stream = adts_frame(&payload, 0);
        stream.extend(adts_frame(&[0x55, 0x66], 0));
        let (config, frames) = parse_adts(&stream).unwrap();
        assert_eq!(config.object_type, 2); // AAC-LC
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.channels, 1);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], payload);
        assert_eq!(frames[1], vec![0x55, 0x66]);
    }

    #[test]
    fn asc_layout() {
        let cfg = AdtsConfig { object_type: 2, sample_rate: 44100, sample_rate_index: 4, channels: 2 };
        let asc = build_asc(&cfg);
        // OT=2 << 11 | idx 4 << 7 | ch 2 << 3 = 0x12 0x10
        assert_eq!(asc, [0x12, 0x10]);
    }

    #[test]
    fn adts_to_samples_durations() {
        let payload = vec![0xAAu8; 10];
        let stream = adts_frame(&payload, 0);
        let samples = adts_to_samples(&stream, 48000);
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].dts, 48000);
        assert_eq!(samples[0].duration, 1024);
        assert_eq!(samples[0].data, payload);
    }
}
