//! H.264 Annex-B → MP4 (AVCC) sample extraction, port of the video path of
//! `TsDemuxer`/`TransmuxingMuxer`.
//!
//! TS carries H.264 in Annex-B (start codes). MP4 needs length-prefixed NALUs
//! (AVCC) with SPS/PPS in the `avcC` box, so we split NALUs, hoist SPS/PPS
//! into the decoder config, and geometry-parse the SPS for tkhd width/height.

use super::Sample;

/// NAL unit types we care about (ITU-T H.264 Table 7-1).
const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

#[derive(Debug, Clone, Default)]
pub struct H264Config {
    /// SPS payload (with NAL header byte) captured for avcC.
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub width: u16,
    pub height: u16,
    pub profile: u8,
    pub level: u8,
}

/// Converts one Annex-B PES payload into an AVCC sample.
///
/// SPS/PPS NALUs are hoisted into `config` and removed from the sample.
/// Returns `None` when the payload contains no slice data.
pub fn annexb_to_sample(
    payload: &[u8],
    dts: u64,
    cts: u64,
    random_access: bool,
    config: &mut H264Config,
) -> Option<Sample> {
    let mut sample: Vec<u8> = Vec::with_capacity(payload.len());
    let mut has_slice = false;
    for nalu in split_nalus(payload) {
        let ntype = nalu[0] & 0x1F;
        match ntype {
            NAL_SPS => {
                if config.sps.is_none() {
                    if let Some((w, h, p, l)) = parse_sps_geometry(nalu) {
                        config.width = w;
                        config.height = h;
                        config.profile = p;
                        config.level = l;
                    }
                    config.sps = Some(nalu.to_vec());
                }
            }
            NAL_PPS => {
                if config.pps.is_none() {
                    config.pps = Some(nalu.to_vec());
                }
            }
            NAL_IDR => {
                has_slice = true;
                push_length_prefixed(&mut sample, nalu);
            }
            t if (1..=4).contains(&t) => {
                has_slice = true;
                push_length_prefixed(&mut sample, nalu);
            }
            _ => push_length_prefixed(&mut sample, nalu), // SEI, AUD, filler…
        }
    }
    if !has_slice {
        return None;
    }
    Some(Sample {
        data: sample,
        dts,
        cts,
        duration: 0, // filled in by the muxer once the next dts is known
        keyframe: random_access || payload_nal_types(payload).contains(&NAL_IDR),
    })
}

/// Builds the avcC box payload from the captured SPS/PPS.
pub fn build_avcc(config: &H264Config) -> Option<Vec<u8>> {
    let sps = config.sps.as_ref()?;
    let pps = config.pps.as_ref()?;
    let mut out = Vec::with_capacity(7 + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push(config.profile); // AVCProfileIndication
    out.push(0); // profile_compatibility (copied byte would be better; 0 is accepted)
    out.push(config.level); // AVCLevelIndication
    out.push(0xFF); // 6 reserved bits + lengthSizeMinusOne = 3 (4-byte lengths)
    out.push(0xE1); // 3 reserved + numSPS = 1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numOfPictureParameterSets (plain count, no reserved bits)
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    Some(out)
}

fn push_length_prefixed(out: &mut Vec<u8>, nalu: &[u8]) {
    out.extend_from_slice(&(nalu.len() as u32).to_be_bytes());
    out.extend_from_slice(nalu);
}

/// Splits Annex-B into NALUs (start codes 00 00 01 / 00 00 00 01).
///
/// The zero run preceding a start code belongs to the code itself, so a NALU
/// followed by a 4-byte start code must not keep a stray 0x00 (it corrupts
/// the bitstream once length-prefixed).
fn split_nalus(data: &[u8]) -> Vec<&[u8]> {
    let mut units = Vec::new();
    let mut i = 0;
    let mut starts: Vec<usize> = Vec::new();
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    for (n, &s) in starts.iter().enumerate() {
        let nalu_start = s + 3; // past the start code
        let end = match starts.get(n + 1) {
            Some(&next) => {
                // Trim the leading zeros of the next start code.
                let mut e = next;
                while e > nalu_start && data[e - 1] == 0 {
                    e -= 1;
                }
                e
            }
            None => data.len(),
        };
        if nalu_start < end {
            units.push(&data[nalu_start..end]);
        }
    }
    units
}

fn payload_nal_types(data: &[u8]) -> Vec<u8> {
    split_nalus(data).iter().map(|n| n[0] & 0x1F).collect()
}

/// Parses width/height/profile/level from an SPS (with NAL header byte).
/// Full exp-golomb walk per ITU-T H.264 7.3.2.1.1.
pub fn parse_sps_geometry(sps: &[u8]) -> Option<(u16, u16, u8, u8)> {
    let mut r = BitReader::new(sps.get(1..)?);
    let profile_idc = r.read_bits(8)? as u8;
    let _constraint = r.read_bits(8);
    let level_idc = r.read_bits(8)? as u8;
    let _sps_id = r.read_ue()?;

    // High profiles carry chroma format fields.
    let high_profile = matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    );
    let chroma_format_idc = if high_profile {
        let cf = r.read_ue()?;
        if cf == 3 {
            r.read_bit()?; // separate_colour_plane_flag
        }
        cf
    } else {
        1
    };

    let _log2_max_frame_num = r.read_ue()? + 4;
    let poc_type = r.read_ue()?;
    match poc_type {
        0 => {
            r.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.read_bit()?; // delta_pic_order_always_zero
            r.read_se()?; // offset_for_non_ref_pic
            r.read_se()?; // offset_for_top_to_bottom_field
            let count = r.read_ue()?;
            for _ in 0..count {
                r.read_se()?;
            }
        }
        _ => {}
    }
    let _max_num_ref_frames = r.read_ue()?;
    let _gaps_in_frame_num_allowed = r.read_bit()?;
    let pic_width_in_mbs = r.read_ue()? + 1;
    let pic_height_in_map_units = r.read_ue()? + 1;
    let frame_mbs_only = r.read_bit()?;
    let height_mbs = if frame_mbs_only == 0 {
        pic_height_in_map_units * 2
    } else {
        pic_height_in_map_units
    };
    if frame_mbs_only == 0 {
        r.read_bit()?; // mb_adaptive_frame_field
    }
    let cropping = r.read_bit()?;
    let (crop_l, crop_r, crop_t, crop_b) = if cropping != 0 {
        (r.read_ue()?, r.read_ue()?, r.read_ue()?, r.read_ue()?)
    } else {
        (0, 0, 0, 0)
    };

    let (unit_l, unit_r, unit_t, unit_b) = match chroma_format_idc {
        1 => (2, 2, 2, 2),
        2 => (2, 2, 1, 1),
        _ => (1, 1, 1, 1),
    };
    let width = pic_width_in_mbs * 16 - crop_l * unit_l - crop_r * unit_r;
    let height = height_mbs * 16 - crop_t * unit_t - crop_b * unit_b;
    Some((width as u16, height as u16, profile_idc, level_idc))
}

struct BitReader<'a> {
    data: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        let byte = *self.data.get(self.bit / 8)?;
        let b = (byte >> (7 - self.bit % 8)) & 1;
        self.bit += 1;
        Some(b)
    }

    fn read_bits(&mut self, n: u32) -> Option<u64> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | self.read_bit()? as u64;
        }
        Some(v)
    }

    /// Unsigned exp-golomb.
    fn read_ue(&mut self) -> Option<u64> {
        let mut zeros = 0u32;
        while self.read_bit()? == 0 {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        Some((1u64 << zeros) - 1 + self.read_bits(zeros)?)
    }

    /// Signed exp-golomb.
    fn read_se(&mut self) -> Option<i64> {
        let ue = self.read_ue()?;
        Some(if ue % 2 == 0 { -(ue as i64 / 2) } else { ue.div_ceil(2) as i64 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_code(nalu: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(nalu);
        v
    }

    fn push_ue(v: u64, bits: &mut Vec<bool>) {
        let x = v + 1;
        let nbits = 64 - x.leading_zeros();
        for _ in 0..nbits - 1 {
            bits.push(false);
        }
        for i in (0..nbits).rev() {
            bits.push((x >> i) & 1 == 1);
        }
    }

    /// Builds a parseable SPS: 32×32, baseline-ish (profile 66), level 30.
    fn synthetic_sps() -> Vec<u8> {
        let mut bits: Vec<bool> = Vec::new();
        let push_bits = |v: u64, n: u32, bits: &mut Vec<bool>| {
            for i in (0..n).rev() {
                bits.push((v >> i) & 1 == 1);
            }
        };
        push_bits(66, 8, &mut bits); // profile_idc (baseline → no chroma fields)
        push_bits(0, 8, &mut bits); // constraint flags
        push_bits(30, 8, &mut bits); // level_idc
        push_ue(0, &mut bits); // sps_id
        push_ue(0, &mut bits); // log2_max_frame_num_minus4
        push_ue(0, &mut bits); // pic_order_cnt_type = 0
        push_ue(0, &mut bits); // log2_max_pic_order_cnt_lsb_minus4
        push_ue(0, &mut bits); // max_num_ref_frames
        bits.push(false); // gaps_in_frame_num_value_allowed
        push_ue(1, &mut bits); // pic_width_in_mbs_minus4 → 2 MBs = 32 px
        push_ue(1, &mut bits); // pic_height_in_map_units_minus4 → 2 → 32 px
        bits.push(true); // frame_mbs_only_flag = 1
        bits.push(false); // frame_cropping_flag = 0
        // trailing: rbsp_stop_one_bit + alignment
        bits.push(true);
        while !bits.len().is_multiple_of(8) {
            bits.push(false);
        }
        let mut sps = vec![0x67]; // NAL header: type 7
        let mut byte = 0u8;
        for (i, b) in bits.iter().enumerate() {
            if *b {
                byte |= 1 << (7 - i % 8);
            }
            if i % 8 == 7 {
                sps.push(byte);
                byte = 0;
            }
        }
        sps
    }

    #[test]
    fn parses_sps_geometry() {
        let sps = synthetic_sps();
        let (w, h, profile, level) = parse_sps_geometry(&sps).unwrap();
        assert_eq!((w, h), (32, 32));
        assert_eq!((profile, level), (66, 30));
    }

    #[test]
    fn annexb_to_avcc_sample() {
        let sps = synthetic_sps();
        let pps = vec![0x68, 0xEE, 0x3C, 0x80];
        let slice = vec![0x65, 0x88, 0x84, 0x21, 0xA0];
        let mut payload = Vec::new();
        payload.extend(start_code(&sps));
        payload.extend(start_code(&pps));
        payload.extend(start_code(&slice));

        let mut cfg = H264Config::default();
        let sample = annexb_to_sample(&payload, 3000, 0, true, &mut cfg).unwrap();
        assert!(sample.keyframe);
        // Sample = length-prefixed slice only (SPS/PPS hoisted); slice is 5 bytes.
        let mut expected = 5u32.to_be_bytes().to_vec();
        expected.extend_from_slice(&slice);
        assert_eq!(sample.data, expected);
        // avcC contains both parameter sets.
        let avcc = build_avcc(&cfg).unwrap();
        assert!(avcc.windows(sps.len()).any(|w| w == sps));
        assert!(avcc.windows(pps.len()).any(|w| w == pps));
    }

    #[test]
    fn non_idr_slice_is_not_keyframe() {
        let slice = vec![0x41, 0x9A, 0x02, 0x05];
        let payload = start_code(&slice);
        let mut cfg = H264Config::default();
        let s = annexb_to_sample(&payload, 0, 0, false, &mut cfg).unwrap();
        assert!(!s.keyframe);
        assert!(s.data.starts_with(&4u32.to_be_bytes()));
    }
}
