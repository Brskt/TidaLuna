use crate::video::cover::CoverKey;

use super::Answer;

/// The part of a `video/` file one `Range` header asks for.
enum RangeAnswer {
    Whole,
    /// Inclusive bounds, as `Content-Range` writes them.
    Part {
        start: usize,
        end: usize,
    },
    Unsatisfiable,
}

/// One `bytes=` range of RFC 9110, section 14.1.2. Anything else (several ranges, another unit,
/// a malformed value) is answered whole, which a server is allowed to do.
fn resolve_range(header: Option<&str>, len: usize) -> RangeAnswer {
    let Some(spec) = header.and_then(|h| h.trim().strip_prefix("bytes=")) else {
        return RangeAnswer::Whole;
    };
    if spec.contains(',') {
        return RangeAnswer::Whole;
    }
    let Some((first, last)) = spec.trim().split_once('-') else {
        return RangeAnswer::Whole;
    };
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // A suffix: the last N bytes.
        return match last.parse::<usize>() {
            Ok(0) => RangeAnswer::Unsatisfiable,
            Ok(_) if len == 0 => RangeAnswer::Unsatisfiable,
            Ok(n) => RangeAnswer::Part {
                start: len - n.min(len),
                end: len - 1,
            },
            Err(_) => RangeAnswer::Whole,
        };
    }
    let Ok(start) = first.parse::<usize>() else {
        return RangeAnswer::Whole;
    };
    let end = if last.is_empty() {
        usize::MAX
    } else {
        match last.parse::<usize>() {
            Ok(end) if end >= start => end,
            _ => return RangeAnswer::Whole,
        }
    };
    if start >= len {
        return RangeAnswer::Unsatisfiable;
    }
    RangeAnswer::Part {
        start,
        end: end.min(len - 1),
    }
}

/// `video/`: the cached WebM, or why there is none.
pub(super) fn video_answer(key: &CoverKey, range: Option<&str>) -> Answer {
    match crate::video::cover_cache::COVER_CACHE.get(key) {
        Some(data) => ranged_answer(data, range),
        None => Answer::refusal(404, "not transcoded"),
    }
}

fn ranged_answer(mut data: Vec<u8>, range: Option<&str>) -> Answer {
    let len = data.len();
    let mut answer = Answer {
        status: 200,
        mime: "video/webm",
        accept_ranges: true,
        ..Answer::default()
    };
    match resolve_range(range, len) {
        RangeAnswer::Whole => {}
        RangeAnswer::Part { start, end } => {
            data.truncate(end + 1);
            data.drain(..start);
            answer.status = 206;
            answer.content_range = Some(format!("bytes {start}-{end}/{len}"));
            answer.first_byte = Some(start);
        }
        RangeAnswer::Unsatisfiable => {
            return Answer {
                status: 416,
                reason: "the range lies outside the cover".to_string(),
                accept_ranges: true,
                content_range: Some(format!("bytes */{len}")),
                ..Answer::default()
            };
        }
    }
    answer.body = data;
    answer
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/cover_scheme/video.rs"]
mod tests;
