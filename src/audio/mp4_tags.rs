//! iTunes-style metadata (`udta/meta/ilst`) for a progressive MP4, from the same Vorbis-style
//! comments a FLAC download is tagged with.

use crate::audio::flac_tags::Picture;

/// `data` atom type codes, from Apple's well-known types table.
const UTF8: u32 = 1;
const JPEG: u32 = 13;
const PNG: u32 = 14;
const BMP: u32 = 27;
const SIGNED_INT: u32 = 21;
/// Untyped: `trkn` and `disk` are binary layouts, and a cover of an unknown format is stored as is.
const IMPLICIT: u32 = 0;

/// Freeform atoms are namespaced by a reverse-DNS `mean`; each one written here uses iTunes'.
const ITUNES_MEAN: &str = "com.apple.iTunes";

/// The body of a `udta` box carrying the tags, for the remuxer to place in `moov`. Atoms come in
/// a fixed order, and a field without a usable value is left out.
pub(crate) fn udta_body(
    comments: &[(String, String)],
    picture: Option<&Picture>,
) -> anyhow::Result<Vec<u8>> {
    let first = |field: &str| {
        comments
            .iter()
            .find(|(key, _)| key == field)
            .map(|(_, value)| value.as_str())
    };
    let joined = |field: &str| {
        let values: Vec<&str> = comments
            .iter()
            .filter(|(key, _)| key == field)
            .map(|(_, value)| value.as_str())
            .collect();
        (!values.is_empty()).then(|| values.join("; "))
    };

    let mut items: Vec<Vec<u8>> = Vec::new();
    let mut text = |kind: &[u8; 4], value: Option<&str>| -> anyhow::Result<()> {
        if let Some(value) = value {
            items.push(boxed(kind, &data(UTF8, value.as_bytes())?)?);
        }
        Ok(())
    };
    text(b"\xa9nam", first("TITLE"))?;
    text(b"\xa9ART", joined("ARTIST").as_deref())?;
    text(b"aART", joined("ALBUMARTIST").as_deref())?;
    text(b"\xa9alb", first("ALBUM"))?;
    text(
        b"\xa9day",
        year(first("YEAR"), first("DATE"))
            .map(|y| y.to_string())
            .as_deref(),
    )?;
    text(b"cprt", first("COPYRIGHT"))?;
    text(b"\xa9cmt", first("COMMENT"))?;
    text(b"\xa9gen", joined("GENRE").as_deref())?;

    let number = |field: &str| {
        first(field)
            .and_then(leading_int)
            .and_then(|n| u16::try_from(n).ok())
    };
    let pair = |n: u16, total: u16| {
        let mut payload = vec![0, 0];
        payload.extend_from_slice(&n.to_be_bytes());
        payload.extend_from_slice(&total.to_be_bytes());
        payload.extend_from_slice(&[0, 0]);
        payload
    };
    let (track, total) = (number("TRACKNUMBER"), number("TOTALTRACKS"));
    // Both halves zero is no value at all: no atom is written.
    if track.unwrap_or(0) != 0 || total.unwrap_or(0) != 0 {
        items.push(boxed(
            b"trkn",
            &data(IMPLICIT, &pair(track.unwrap_or(0), total.unwrap_or(0)))?,
        )?);
    }
    // The tags carry no disc total.
    if let Some(disc) = number("DISCNUMBER").filter(|disc| *disc != 0) {
        items.push(boxed(b"disk", &data(IMPLICIT, &pair(disc, 0))?)?);
    }
    if let Some(bpm) = number("BPM").filter(|bpm| *bpm != 0) {
        items.push(boxed(b"tmpo", &data(SIGNED_INT, &bpm.to_be_bytes())?)?);
    }
    if let Some(lyrics) = first("LYRICS") {
        items.push(boxed(b"\xa9lyr", &data(UTF8, lyrics.as_bytes())?)?);
    }

    let gain = first("REPLAYGAIN_TRACK_GAIN")
        .and_then(finite_float)
        .map(|g| format!("{g:.2} dB"));
    let peak = first("REPLAYGAIN_TRACK_PEAK")
        .and_then(finite_float)
        .map(|p| format!("{p:.6}"));
    for (name, value) in [
        ("ISRC", first("ISRC")),
        ("MusicBrainz Track Id", first("MUSICBRAINZ_TRACKID")),
        ("MusicBrainz Album Id", first("MUSICBRAINZ_ALBUMID")),
        ("REPLAYGAIN_TRACK_GAIN", gain.as_deref()),
        ("REPLAYGAIN_TRACK_PEAK", peak.as_deref()),
    ] {
        if let Some(value) = value {
            items.push(freeform(name, value)?);
        }
    }

    if let Some(picture) = picture {
        items.push(boxed(
            b"covr",
            &data(cover_type(&picture.mime), &picture.data)?,
        )?);
    }

    let hdlr = full_box(b"hdlr", &hdlr_body())?;
    let ilst = boxed(b"ilst", &items.concat())?;
    let meta = full_box(b"meta", &[hdlr, ilst].concat())?;
    Ok(meta)
}

/// `YEAR` when it is a plausible year, else the year that opens `DATE`.
fn year(year: Option<&str>, date: Option<&str>) -> Option<i64> {
    let plausible = |y: i64| (1..=9999).contains(&y);
    year.and_then(leading_int)
        .filter(|y| plausible(*y))
        .or_else(|| {
            date.and_then(|d| d.get(..4))
                .and_then(leading_int)
                .filter(|y| plausible(*y))
        })
}

/// What JavaScript's `parseInt(value, 10)` reads: optional leading whitespace and sign, then the
/// longest run of digits. "3/12" is track 3.
fn leading_int(value: &str) -> Option<i64> {
    let value = value.trim_start();
    let (sign, digits) = match value.as_bytes().first() {
        Some(b'-') => (-1, &value[1..]),
        Some(b'+') => (1, &value[1..]),
        _ => (1, value),
    };
    let end = digits.bytes().take_while(u8::is_ascii_digit).count();
    digits[..end].parse::<i64>().ok().map(|n| sign * n)
}

/// The renderer sends these as JavaScript number strings, which `f64` parses whole.
fn finite_float(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// The `data` type of a cover: JPEG, PNG or BMP by MIME type, ASCII case aside, else untyped.
fn cover_type(mime: &str) -> u32 {
    if mime.eq_ignore_ascii_case("image/jpeg") {
        JPEG
    } else if mime.eq_ignore_ascii_case("image/png") {
        PNG
    } else if mime.eq_ignore_ascii_case("image/x-windows-bmp")
        || mime.eq_ignore_ascii_case("image/bmp")
    {
        BMP
    } else {
        IMPLICIT
    }
}

/// `hdlr` of a metadata `meta`: handler `mdir`, manufacturer `appl`, empty name.
fn hdlr_body() -> Vec<u8> {
    let mut out = vec![0; 4]; // pre_defined
    out.extend_from_slice(b"mdir");
    out.extend_from_slice(b"appl");
    out.extend_from_slice(&[0; 8]);
    out.push(0); // empty, NUL-terminated name
    out
}

/// A `data` atom: type code, a zero locale, then the value.
fn data(type_code: u32, value: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut body = type_code.to_be_bytes().to_vec();
    body.extend_from_slice(&[0; 4]);
    body.extend_from_slice(value);
    boxed(b"data", &body)
}

/// `----`: a `mean` and a `name` naming the field, then its `data`.
fn freeform(name: &str, value: &str) -> anyhow::Result<Vec<u8>> {
    let body = [
        full_box(b"mean", ITUNES_MEAN.as_bytes())?,
        full_box(b"name", name.as_bytes())?,
        data(UTF8, value.as_bytes())?,
    ]
    .concat();
    boxed(b"----", &body)
}

/// A full box at version 0 with no flags.
fn full_box(kind: &[u8; 4], body: &[u8]) -> anyhow::Result<Vec<u8>> {
    boxed(kind, &[&[0u8; 4][..], body].concat())
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> anyhow::Result<Vec<u8>> {
    let size = u32::try_from(body.len() + 8).map_err(|_| {
        anyhow::anyhow!(
            "{} atom of {} bytes exceeds 32 bits",
            String::from_utf8_lossy(kind),
            body.len()
        )
    })?;
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&size.to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    Ok(out)
}

#[cfg(test)]
#[path = "../../tests/unit/audio/mp4_tags.rs"]
mod tests;
