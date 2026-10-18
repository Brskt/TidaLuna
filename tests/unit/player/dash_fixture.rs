//! A DASH track built box by box, attached to `src/player/length.rs` by `#[path]` for every test
//! that needs one: an init segment whose header declares no length, then fragments that carry it,
//! the way a DASH stream arrives. Nothing is decoded: the payloads are filler.

pub(crate) const SAMPLE_RATE: u32 = 44_100;
/// One AAC frame, in ticks of a media timescale equal to the sample rate.
pub(crate) const AAC_FRAME: u32 = 1024;
/// Fragment lengths in frames of a four-second-segment track, the last one short.
pub(crate) const SEGMENTS: [u32; 4] = [172, 173, 173, 105];

fn bx(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut head = vec![version];
    head.extend_from_slice(&flags.to_be_bytes()[1..]);
    head.extend_from_slice(body);
    bx(kind, &head)
}

fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

fn u32s(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_be_bytes()).collect()
}

const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

/// The `esds` of AAC-LC at 44.1 kHz in two channels: the audio specific config `12 10`.
fn esds() -> Vec<u8> {
    let asc = [0x12, 0x10];
    let dec_specific = cat(&[vec![0x05, asc.len() as u8], asc.to_vec()]);
    let mut dec_config = vec![0x40, 0x15, 0, 0, 0];
    dec_config.extend(u32s(&[0, 0]));
    dec_config.extend(&dec_specific);
    let dec_config = cat(&[vec![0x04, dec_config.len() as u8], dec_config]);
    let mut es = vec![0, 1, 0];
    es.extend(&dec_config);
    es.extend([0x06, 0x01, 0x02]);
    full(b"esds", 0, 0, &cat(&[vec![0x03, es.len() as u8], es]))
}

fn mp4a() -> Vec<u8> {
    let mut entry = vec![0u8; 6];
    entry.extend_from_slice(&1u16.to_be_bytes());
    entry.extend_from_slice(&[0u8; 8]);
    entry.extend_from_slice(&2u16.to_be_bytes());
    entry.extend_from_slice(&16u16.to_be_bytes());
    entry.extend_from_slice(&[0u8; 4]);
    entry.extend_from_slice(&(SAMPLE_RATE << 16).to_be_bytes());
    entry.extend(esds());
    bx(b"mp4a", &entry)
}

/// The header of a fragmented MP4 that declares no length: the `mvhd`, `mdhd` and `tkhd`
/// durations read 0 and no `mehd` or `sidx` stands in for them. The media timescale is the
/// sample rate.
pub(crate) fn init_segment() -> Vec<u8> {
    let ftyp = bx(
        b"ftyp",
        &cat(&[b"iso5".to_vec(), u32s(&[0]), b"iso6mp41".to_vec()]),
    );
    let mvhd = full(
        b"mvhd",
        0,
        0,
        &cat(&[
            u32s(&[0, 0, SAMPLE_RATE, 0, 0x0001_0000]),
            vec![0x01, 0x00, 0, 0],
            vec![0; 8],
            u32s(&MATRIX),
            vec![0; 24],
            u32s(&[2]),
        ]),
    );
    let tkhd = full(
        b"tkhd",
        0,
        7,
        &cat(&[
            u32s(&[0, 0, 1, 0, 0]),
            vec![0; 8],
            vec![0; 4],
            vec![0x01, 0x00, 0, 0],
            u32s(&MATRIX),
            u32s(&[0, 0]),
        ]),
    );
    let mdhd = full(
        b"mdhd",
        0,
        0,
        &cat(&[u32s(&[0, 0, SAMPLE_RATE, 0]), vec![0x55, 0xc4, 0, 0]]),
    );
    let hdlr = full(
        b"hdlr",
        0,
        0,
        &cat(&[u32s(&[0]), b"soun".to_vec(), vec![0; 12], vec![0]]),
    );
    let smhd = full(b"smhd", 0, 0, &[0, 0, 0, 0]);
    let dinf = bx(
        b"dinf",
        &full(b"dref", 0, 0, &cat(&[u32s(&[1]), full(b"url ", 0, 1, &[])])),
    );
    let stsd = full(b"stsd", 0, 0, &cat(&[u32s(&[1]), mp4a()]));
    let stbl = bx(
        b"stbl",
        &cat(&[
            stsd,
            full(b"stts", 0, 0, &u32s(&[0])),
            full(b"stsc", 0, 0, &u32s(&[0])),
            full(b"stsz", 0, 0, &u32s(&[0, 0])),
            full(b"stco", 0, 0, &u32s(&[0])),
        ]),
    );
    let minf = bx(b"minf", &cat(&[smhd, dinf, stbl]));
    let mdia = bx(b"mdia", &cat(&[mdhd, hdlr, minf]));
    let trak = bx(b"trak", &cat(&[tkhd, mdia]));
    let mvex = bx(b"mvex", &full(b"trex", 0, 0, &u32s(&[1, 1, 0, 0, 0])));
    cat(&[ftyp, bx(b"moov", &cat(&[mvhd, trak, mvex]))])
}

/// How a fragment states its sample durations.
#[derive(Clone, Copy)]
pub(crate) enum Durations {
    /// Every sample in the `trun`.
    PerSample,
    /// One `default_sample_duration` in the `tfhd`, nothing per sample.
    Default,
}

/// One `moof` + `mdat` holding `frames` AAC frames, whose first begins at `start` ticks.
fn fragment(seq: u32, start: u64, frames: u32, how: Durations) -> Vec<u8> {
    const SAMPLE_BYTES: u32 = 16;
    let (tfhd_flags, tfhd_tail, trun_flags) = match how {
        Durations::PerSample => (0x02_0000, vec![], 0x301),
        Durations::Default => (0x02_0008, AAC_FRAME.to_be_bytes().to_vec(), 0x201),
    };
    let mfhd = full(b"mfhd", 0, 0, &u32s(&[seq]));
    let tfhd = full(b"tfhd", 0, tfhd_flags, &cat(&[u32s(&[1]), tfhd_tail]));
    let tfdt = full(b"tfdt", 1, 0, &start.to_be_bytes());
    let run = |offset: u32| {
        let mut body = u32s(&[frames, offset]);
        for _ in 0..frames {
            if matches!(how, Durations::PerSample) {
                body.extend(u32s(&[AAC_FRAME]));
            }
            body.extend(u32s(&[SAMPLE_BYTES]));
        }
        full(b"trun", 0, trun_flags, &body)
    };
    let moof = |offset: u32| {
        bx(
            b"moof",
            &cat(&[
                mfhd.clone(),
                bx(b"traf", &cat(&[tfhd.clone(), tfdt.clone(), run(offset)])),
            ]),
        )
    };
    // From the first byte of the moof to the first byte of the mdat payload; the moof's own size
    // does not depend on the offset it holds.
    let offset = moof(0).len() as u32 + 8;
    let mdat = bx(b"mdat", &vec![0u8; (frames * SAMPLE_BYTES) as usize]);
    cat(&[moof(offset), mdat])
}

/// A DASH track: the init segment, then one fragment per entry of `frames`.
pub(crate) fn dash_track(frames: &[u32], how: Durations) -> Vec<u8> {
    let mut out = init_segment();
    let mut start = 0u64;
    for (i, n) in frames.iter().enumerate() {
        out.extend(fragment(i as u32 + 1, start, *n, how));
        start += u64::from(*n) * u64::from(AAC_FRAME);
    }
    out
}

/// The length in seconds of `frames` AAC frames.
pub(crate) fn seconds(frames: &[u32]) -> f64 {
    frames.iter().map(|n| u64::from(*n)).sum::<u64>() as f64 * f64::from(AAC_FRAME)
        / f64::from(SAMPLE_RATE)
}
