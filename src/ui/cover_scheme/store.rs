use std::time::Instant;

use cef::*;

use crate::video::cover::CoverKey;

use super::{Answer, GRANTS};

/// The total a POST body's elements add up to, refused past `max` before anything is copied: the
/// sizes are the page's, and a sum computed first is what decides whether to allocate.
fn body_size(
    sizes: impl IntoIterator<Item = usize>,
    max: usize,
) -> Result<usize, (u16, &'static str)> {
    let mut total: usize = 0;
    for size in sizes {
        total = total
            .checked_add(size)
            .filter(|&t| t <= max)
            .ok_or((413, "the upload is larger than a cover may be"))?;
    }
    if total == 0 {
        return Err((400, "the upload is empty"));
    }
    Ok(total)
}

/// Only bytes elements carry anything here: CEF hands a scheme handler a `Blob` or stream body as
/// an empty element list, which is why the page posts a typed array. Kept apart from
/// `token_filter`'s reader, which has no cap and must keep answering as it does.
pub(super) fn read_capped_body(
    request: &mut Request,
    max: usize,
) -> Result<Vec<u8>, (u16, &'static str)> {
    let post_data = request.post_data().ok_or((400, "the upload is empty"))?;
    let count = post_data.element_count();
    let mut elements: Vec<Option<PostDataElement>> = vec![None; count];
    post_data.elements(Some(&mut elements));
    let total = body_size(elements.iter().flatten().map(|el| el.bytes_count()), max)?;
    let mut body = Vec::with_capacity(total);
    for el in elements.iter().flatten() {
        let n = el.bytes_count();
        if n == 0 {
            continue;
        }
        let start = body.len();
        body.resize(start + n, 0);
        let copied = el.bytes(n, body[start..].as_mut_ptr());
        body.truncate(start + copied.min(n));
    }
    Ok(body)
}

/// `store/`: the WebM checked against the grant its pass opened, then cached.
pub(super) fn store_answer(key: &CoverKey, body: &[u8]) -> Answer {
    let grant = GRANTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(key, Instant::now());
    let Some(grant) = grant else {
        return Answer::refusal(409, "no transcode of this cover is awaiting its upload");
    };
    if let Err(why) = crate::video::webm::validate_cover_webm(body, &grant.expected()) {
        crate::verr!("[VIDEO]   Refused a transcoded cover: {why}");
        return Answer::refusal(422, why);
    }
    match crate::video::cover_cache::COVER_CACHE.put(key, body) {
        Ok(()) => {
            crate::vprintln!(
                "[VIDEO]   Cover transcoded and stored ({} bytes)",
                body.len()
            );
            Answer {
                status: 200,
                ..Answer::default()
            }
        }
        Err(e) => {
            crate::verr!("[VIDEO]   Storing a transcoded cover failed: {e}");
            let (status, reason) = put_refusal(&e);
            Answer::refusal(status, reason)
        }
    }
}

/// A full disk heals, with time or with the user: the page asks again later. Any other write
/// failure says nothing of the next one.
pub(super) fn put_refusal(e: &std::io::Error) -> (u16, &'static str) {
    match e.kind() {
        std::io::ErrorKind::StorageFull | std::io::ErrorKind::QuotaExceeded => {
            (507, "the disk has no room for the cover")
        }
        _ => (500, "the cover could not be stored"),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/ui/cover_scheme/store.rs"]
mod tests;
