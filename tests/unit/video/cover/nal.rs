//! Tests for `src/video/cover/nal.rs`, attached by `#[path]`.

use super::*;

/// MSB-first writer of an SPS's fields.
#[derive(Default)]
struct Writer {
    out: Vec<u8>,
    cur: u8,
    used: u8,
}

impl Writer {
    fn bit(&mut self, b: u32) {
        self.cur = self.cur << 1 | u8::from(b & 1 == 1);
        self.used += 1;
        if self.used == 8 {
            self.out.push(self.cur);
            (self.cur, self.used) = (0, 0);
        }
    }

    fn u(&mut self, bits: u32, v: u32) {
        for i in (0..bits).rev() {
            self.bit(v >> i & 1);
        }
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit(u32::from(x >> i & 1 == 1));
        }
    }

    /// The stop bit, then the header byte and the emulation prevention bytes a real NAL carries.
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.bit(1);
        while self.used != 0 {
            self.bit(0);
        }
        let mut nal = vec![header];
        let mut zeros = 0;
        for b in self.out {
            if zeros == 2 && b <= 3 {
                nal.push(3);
                zeros = 0;
            }
            nal.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        nal
    }
}

/// A High-profile SPS of `width` x `map_units` macroblocks, cropped to `crop` (left, right, top,
/// bottom, in chroma units) when given.
fn sps(width: u32, map_units: u32, frame_mbs_only: bool, crop: Option<[u32; 4]>) -> Vec<u8> {
    let mut w = Writer::default();
    w.u(8, 100);
    w.u(8, 0);
    w.u(8, 40);
    // seq_parameter_set_id, chroma_format_idc 4:2:0, both bit depths 8, no bypass, no matrix
    w.ue(0);
    w.ue(1);
    w.ue(0);
    w.ue(0);
    w.bit(0);
    w.bit(0);
    // log2_max_frame_num_minus4, pic_order_cnt_type 2, max_num_ref_frames 4, no gaps
    w.ue(0);
    w.ue(2);
    w.ue(4);
    w.bit(0);
    w.ue(width - 1);
    w.ue(map_units - 1);
    w.bit(u32::from(frame_mbs_only));
    if !frame_mbs_only {
        w.bit(0);
    }
    // direct_8x8_inference_flag
    w.bit(1);
    match crop {
        Some(offsets) => {
            w.bit(1);
            for offset in offsets {
                w.ue(offset);
            }
        }
        None => w.bit(0),
    }
    // no VUI
    w.bit(0);
    w.nal(0x67)
}

/// OpenH264 allocates by the coded size: an SPS of 4096x2304 cropped to 16x16 costs it 418 MB.
/// Real streams stay admitted, up to 1080p coded 1920x1088.
#[test]
fn the_ceiling_reads_the_coded_frame_not_the_cropped_one() {
    let refused = [
        (
            "4096x2304 cropped to 16x16",
            sps(256, 144, true, Some([0, 2040, 0, 1144])),
        ),
        ("128x65 macroblocks, one row past", sps(128, 65, true, None)),
        (
            "interlaced, 33 map units of two rows",
            sps(128, 33, false, None),
        ),
    ];
    for (what, nal) in &refused {
        assert!(admit_nal(nal).is_err(), "{what} was admitted");
    }
    let admitted = [
        (
            "1080p, coded 1920x1088",
            sps(120, 68, true, Some([0, 0, 0, 4])),
        ),
        (
            "128x64 macroblocks, the ceiling itself",
            sps(128, 64, true, None),
        ),
    ];
    for (what, nal) in &admitted {
        admit_nal(nal).unwrap_or_else(|e| panic!("{what} was refused: {e:#}"));
    }
}

/// OpenH264 decodes an SVC slice behind a subset SPS as it does a plain one, and no slice count
/// sees it: prefix, subset SPS and SVC slice are refused, a one-macroblock subset SPS included.
#[test]
fn svc_nals_are_refused_whatever_they_carry() {
    let mut subset = sps(1, 1, true, None);
    subset[0] = 0x6F;
    for (what, nal) in [
        ("prefix", vec![0x6E, 0x80, 0x80]),
        ("subset SPS", subset),
        ("SVC slice", vec![0x74, 0x80, 0x80, 0x88]),
    ] {
        let e = admit_nal(&nal).unwrap_err();
        assert!(e.to_string().contains("SVC"), "{what}: {e}");
    }
}

/// A slice NAL whose header is `header` and whose first macroblock is `first_mb`.
fn slice(header: u8, first_mb: u32) -> Vec<u8> {
    let mut w = Writer::default();
    w.ue(first_mb);
    w.nal(header)
}

/// One sample is one decode call, which nothing interrupts: its slices may claim 128 frames of
/// macroblocks together, each from its first to the ceiling, and be 1,024 at most.
#[test]
fn one_sample_is_bounded_by_what_its_slices_claim_and_by_their_count() {
    let ceiling = MAX_FRAME_MBS as u32;
    let mut work = CallWork::default();
    for _ in 0..127 {
        work.charge(&slice(0x41, 0)).unwrap();
    }
    // Not a slice: it claims nothing.
    work.charge(&[0x06, 0x05, 0x00, 0x80]).unwrap();
    work.charge(&slice(0x65, 0)).unwrap();
    assert!(
        work.charge(&slice(0x41, ceiling - 1)).is_err(),
        "a 129th frame's last macroblock"
    );

    // A conformant level-4.0 picture of 170 equal slices, 48 macroblocks apart, fits.
    let mut work = CallWork::default();
    for k in 0..170 {
        work.charge(&slice(0x41, k * 48)).unwrap();
    }

    let mut work = CallWork::default();
    for _ in 0..1_024 {
        work.charge(&slice(0x41, ceiling - 1)).unwrap();
    }
    assert!(
        work.charge(&slice(0x41, ceiling - 1)).is_err(),
        "a 1,025th slice"
    );

    assert!(CallWork::default().charge(&slice(0x41, ceiling)).is_err());
}

/// The decoder splits a NAL at any start code inside it and reads 00 00 03 + a byte past 3 its
/// own way: either would hand it an SPS no ceiling on NAL type 7 ever met.
#[test]
fn an_sps_hidden_from_the_ceiling_is_refused_by_the_framing() {
    let big = sps(256, 144, true, None);
    for start_code in [&[0, 0, 0, 1][..], &[0, 0, 1][..]] {
        let mut carrier = vec![0x65, 0x88, 0x84];
        carrier.extend_from_slice(start_code);
        carrier.extend_from_slice(&big);
        assert!(admit_nal(&carrier).is_err(), "behind {start_code:?}");
    }
    // Read by the specification as 8 macroblocks, by OpenH264 as 29,670.
    let escaped = [
        0x67, 0x42, 0x00, 0x34, 0x00, 0x00, 0x03, 0xDA, 0x00, 0xE6, 0x01, 0x03, 0x90,
    ];
    assert!(admit_nal(&escaped).is_err());
    // A legal escape (the next byte 3 at most) and trailing zeros stay admitted.
    admit_nal(&[0x06, 0x05, 0x00, 0x00, 0x03, 0x01, 0x80, 0x00, 0x00]).expect("a clean SEI");
}
