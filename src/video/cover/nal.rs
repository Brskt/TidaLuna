//! What every NAL passes before a decoder sees it. OpenH264 allocates by the frame size an SPS
//! states, up to 36,864 macroblocks and 16 reference frames: 418 MB per decoder from a 37 KB input.
//! One `decode()` call cannot be interrupted. What one sample may ask of it is bounded here too.

use anyhow::bail;

/// Coded frame size an SPS may state, in macroblocks: H.264 level 4.1's MaxFS. 1920x1088 is 8,160,
/// 1920x960 7,200, 1280x1280 6,400.
const MAX_FRAME_MBS: u64 = 8_192;

/// `profile_idc` values whose SPS carries `chroma_format_idc` and the fields behind it (7.3.2.1.1).
const HIGH_PROFILES: [u32; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// Prefix, subset SPS and SVC slice: OpenH264 decodes a type-20 slice behind a subset SPS as it
/// does a type-1 one. No producer of ours writes them (hls.js drops every type past 12).
const SVC_NALS: [u8; 3] = [14, 15, 20];

/// Slices of one sample, all decoded in one call: the decoder's NAL list grows 16 entries at a
/// time, copied whole each time (the cost of their count is quadratic). 1,024 holds the 906 a
/// conformant level-4.2 picture may have at 24 fps.
const MAX_CALL_SLICES: usize = 1_024;

/// Macroblocks one sample's slices may claim together, each from its first to the frame ceiling:
/// slices may overlap, and each runs to the end of the picture at most. 128 frames hold the 170
/// slices a level-4.0 picture may have at 24 fps, TIDAL's top clip being level 4.0.
const MAX_CALL_EXTENT: u64 = 128 * MAX_FRAME_MBS;

/// A NAL about to reach the decoder: framed as the decoder will split it, no SVC layer, and no SPS
/// stating a frame past `MAX_FRAME_MBS`. Every SPS counts, the decoder activating the last.
pub(crate) fn admit_nal(nal: &[u8]) -> anyhow::Result<()> {
    clean_framing(nal)?;
    let Some(kind) = nal.first().map(|h| h & 0x1F) else {
        bail!("an empty NAL");
    };
    if SVC_NALS.contains(&kind) {
        bail!("an SVC NAL of type {kind}");
    }
    if kind == 7 {
        let mbs = frame_mbs(nal)?;
        if mbs > MAX_FRAME_MBS {
            bail!(
                "an SPS states a frame of {mbs} macroblocks, past the ceiling of {MAX_FRAME_MBS}"
            );
        }
    }
    Ok(())
}

/// What one sample's slices ask of a single `decode()` call, charged NAL by NAL.
#[derive(Default)]
pub(crate) struct CallWork {
    slices: usize,
    extent: u64,
}

impl CallWork {
    /// Charge one admitted NAL: a coded slice counts, and claims every macroblock from its first
    /// to the frame ceiling. Refused past either bound.
    pub(crate) fn charge(&mut self, nal: &[u8]) -> anyhow::Result<()> {
        let Some((header, payload)) = nal.split_first() else {
            bail!("an empty NAL");
        };
        if !matches!(header & 0x1F, 1 | 5) {
            return Ok(());
        }
        self.slices += 1;
        if self.slices > MAX_CALL_SLICES {
            bail!("a sample holds more than {MAX_CALL_SLICES} slices");
        }
        let first_mb = u64::from(Bits::new(payload).ue()?);
        if first_mb >= MAX_FRAME_MBS {
            bail!("a slice starts at macroblock {first_mb}, past the ceiling of {MAX_FRAME_MBS}");
        }
        self.extent += MAX_FRAME_MBS - first_mb;
        if self.extent > MAX_CALL_EXTENT {
            bail!(
                "a sample's slices claim more than {MAX_CALL_EXTENT} macroblocks for one decode call"
            );
        }
        Ok(())
    }
}

/// OpenH264 ends a NAL wherever its bytes read 00 00 00, 01 or 02, and drops 00 00 03 with its
/// zeros when a byte past 3 follows. 7.4.1 allows none of them inside a NAL: refused, the decoder
/// reads the same NAL as `frame_mbs`. Trailing zeros are the decoder's to strip.
fn clean_framing(nal: &[u8]) -> anyhow::Result<()> {
    let end = nal.iter().rposition(|&b| b != 0).map_or(0, |p| p + 1);
    let body = &nal[..end];
    for (i, w) in body.windows(3).enumerate() {
        if w[0] != 0 || w[1] != 0 {
            continue;
        }
        if w[2] <= 2 {
            bail!("a NAL holds a start code");
        }
        if w[2] == 3 && body.get(i + 3).is_some_and(|&next| next > 3) {
            bail!("a NAL holds an escape the decoder reads otherwise");
        }
    }
    Ok(())
}

/// `PicWidthInMbs * FrameHeightInMbs` of one SPS NAL, header byte included: the coded size the
/// decoder allocates, not the cropped one. Read up to `frame_mbs_only_flag`, never the VUI.
fn frame_mbs(nal: &[u8]) -> anyhow::Result<u64> {
    let Some(payload) = nal.get(1..) else {
        bail!("an empty NAL");
    };
    let mut r = Bits::new(payload);
    let profile = r.bits(8)?;
    // constraint_set flags, reserved bits, level_idc
    r.bits(16)?;
    // seq_parameter_set_id
    r.ue()?;
    if HIGH_PROFILES.contains(&profile) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc > 3 {
            bail!("an SPS states a chroma_format_idc past 3");
        }
        if chroma_format_idc == 3 {
            // separate_colour_plane_flag
            r.bit()?;
        }
        // bit_depth_luma_minus8, bit_depth_chroma_minus8, qpprime_y_zero_transform_bypass_flag
        r.ue()?;
        r.ue()?;
        r.bit()?;
        if r.bit()? == 1 {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.bit()? == 1 {
                    r.skip_scaling_list(if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    // log2_max_frame_num_minus4
    r.ue()?;
    match r.ue()? {
        // log2_max_pic_order_cnt_lsb_minus4
        0 => {
            r.ue()?;
        }
        1 => {
            // delta_pic_order_always_zero_flag, offset_for_non_ref_pic, offset_for_top_to_bottom_field
            r.bit()?;
            r.se()?;
            r.se()?;
            let cycle = r.ue()?;
            if cycle > 255 {
                bail!("an SPS states a pic_order_cnt cycle past 255 frames");
            }
            for _ in 0..cycle {
                r.se()?;
            }
        }
        2 => {}
        _ => bail!("an SPS states a pic_order_cnt_type past 2"),
    }
    // max_num_ref_frames, gaps_in_frame_num_value_allowed_flag
    r.ue()?;
    r.bit()?;
    let width = u64::from(r.ue()?) + 1;
    let map_units = u64::from(r.ue()?) + 1;
    let frame_mbs_only = u64::from(r.bit()?);
    Ok(width
        .saturating_mul(map_units)
        .saturating_mul(2 - frame_mbs_only))
}

/// MSB-first reader over a NAL's payload as it travels: the emulation prevention byte of 00 00 03
/// is dropped as it is met; nothing is copied and nothing grows with the input.
struct Bits<'a> {
    rest: &'a [u8],
    cur: u8,
    left: u8,
    zeros: u8,
}

impl<'a> Bits<'a> {
    fn new(rest: &'a [u8]) -> Self {
        Self {
            rest,
            cur: 0,
            left: 0,
            zeros: 0,
        }
    }

    fn byte(&mut self) -> anyhow::Result<u8> {
        loop {
            let Some((&b, rest)) = self.rest.split_first() else {
                bail!("a NAL ends before the field being read");
            };
            self.rest = rest;
            if self.zeros == 2 && b == 3 {
                self.zeros = 0;
                continue;
            }
            self.zeros = if b == 0 { (self.zeros + 1).min(2) } else { 0 };
            return Ok(b);
        }
    }

    fn bit(&mut self) -> anyhow::Result<u32> {
        if self.left == 0 {
            self.cur = self.byte()?;
            self.left = 8;
        }
        self.left -= 1;
        Ok(u32::from(self.cur >> self.left & 1))
    }

    fn bits(&mut self, n: u32) -> anyhow::Result<u32> {
        (0..n).try_fold(0, |v, _| Ok(v << 1 | self.bit()?))
    }

    /// `ue(v)` of 31 leading zeros at most, as OpenH264 reads it: the largest, 2^32 - 2, fits.
    fn ue(&mut self) -> anyhow::Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                bail!("an Exp-Golomb code runs past 32 bits");
            }
        }
        Ok((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> anyhow::Result<i64> {
        let k = i64::from(self.ue()?);
        Ok(if k % 2 == 1 { k / 2 + 1 } else { -(k / 2) })
    }

    /// Only the bit position matters. `nextScale == 0` ends a list early, and a delta outside
    /// -128..=127 is refused as OpenH264 refuses it: past either, the two would read other bits.
    fn skip_scaling_list(&mut self, size: u32) -> anyhow::Result<()> {
        let (mut last, mut next) = (8i64, 8i64);
        for _ in 0..size {
            if next != 0 {
                let delta = self.se()?;
                if !(-128..=127).contains(&delta) {
                    bail!("an SPS scaling list delta is outside -128..=127");
                }
                next = (last + delta + 256) % 256;
            }
            if next != 0 {
                last = next;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/video/cover/nal.rs"]
mod tests;
