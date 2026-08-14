//! Shared CEF response filter: buffer the whole body, run a transform once at
//! EOF, then drain the result across CEF's output chunks. `csp_filter`,
//! `token_filter`, and `module_capture` each use this with their own transform.
//! The only behavioral axis is `FilterOutcome`: emit the bytes, or fail closed
//! and emit nothing (a transform that must not leak its input then cannot).

use std::cell::RefCell;
use std::sync::Arc;

use cef::*;

/// What a transform decides for the accumulated response body.
pub(crate) enum FilterOutcome {
    /// Emit these bytes to the renderer (covers both a rewritten body and an
    /// unchanged passthrough).
    Emit(Vec<u8>),
    /// Fail closed: emit nothing and abort the response. Used when emitting the
    /// input would leak it (e.g. token redaction failed but the body still holds
    /// the plaintext token).
    Drop,
}

/// Applied once to the full body at EOF. `Arc<dyn Fn>` (not `Box`) because the
/// `wrap_response_filter!`-generated `Clone` clones every field.
pub(crate) type Transform = Arc<dyn Fn(Vec<u8>) -> FilterOutcome>;

#[derive(Clone)]
pub(crate) enum FilterState {
    Accumulating(Vec<u8>),
    Emitting {
        data: Vec<u8>,
        offset: usize,
    },
    Done,
    /// Sticky fail-closed terminal: every call writes 0 bytes and returns ERROR.
    Error,
}

/// Build a buffering filter that accumulates (reserving `capacity` up front)
/// then applies `transform` at EOF.
pub(crate) fn new_buffering_filter(capacity: usize, transform: Transform) -> ResponseFilter {
    BufferingFilter::new(
        transform,
        RefCell::new(FilterState::Accumulating(Vec::with_capacity(capacity))),
    )
}

wrap_response_filter! {
    pub(crate) struct BufferingFilter {
        transform: Transform,
        state: RefCell<FilterState>,
    }

    impl ResponseFilter {
        fn init_filter(&self) -> ::std::os::raw::c_int {
            1
        }

        fn filter(
            &self,
            data_in: Option<&mut Vec<u8>>,
            data_in_read: Option<&mut usize>,
            data_out: Option<&mut Vec<u8>>,
            data_out_written: Option<&mut usize>,
        ) -> ResponseFilterStatus {
            let out_written = match data_out_written {
                Some(w) => w,
                None => return ResponseFilterStatus::ERROR,
            };
            let mut state = self.state.borrow_mut();
            run_filter(
                &mut state,
                &*self.transform,
                data_in,
                data_in_read,
                data_out,
                out_written,
            )
        }
    }
}

/// The state machine, split out of the CEF wrapper to be unit-tested
/// without constructing a `BufferingFilter` (the macro erases the concrete type).
fn run_filter(
    state: &mut FilterState,
    transform: &dyn Fn(Vec<u8>) -> FilterOutcome,
    data_in: Option<&mut Vec<u8>>,
    data_in_read: Option<&mut usize>,
    data_out: Option<&mut Vec<u8>>,
    out_written: &mut usize,
) -> ResponseFilterStatus {
    *out_written = 0;
    match state {
        FilterState::Accumulating(buf) => {
            if let Some(input) = data_in {
                if let Some(read) = data_in_read {
                    *read = input.len();
                }
                buf.extend_from_slice(input);
                ResponseFilterStatus::NEED_MORE_DATA
            } else {
                let accumulated = std::mem::take(buf);
                match transform(accumulated) {
                    FilterOutcome::Emit(data) => {
                        *state = FilterState::Emitting { data, offset: 0 };
                        emit(state, data_out, out_written)
                    }
                    FilterOutcome::Drop => {
                        *state = FilterState::Error;
                        ResponseFilterStatus::ERROR
                    }
                }
            }
        }
        FilterState::Emitting { .. } => {
            if let Some(input) = data_in
                && let Some(read) = data_in_read
            {
                *read = input.len();
            }
            emit(state, data_out, out_written)
        }
        FilterState::Done => ResponseFilterStatus::DONE,
        FilterState::Error => ResponseFilterStatus::ERROR,
    }
}

fn emit(
    state: &mut FilterState,
    data_out: Option<&mut Vec<u8>>,
    out_written: &mut usize,
) -> ResponseFilterStatus {
    let (data, offset) = match state {
        FilterState::Emitting { data, offset } => (data, offset),
        _ => return ResponseFilterStatus::ERROR,
    };

    let remaining = &data[*offset..];
    if remaining.is_empty() {
        *state = FilterState::Done;
        return ResponseFilterStatus::DONE;
    }

    let Some(out_buf) = data_out else {
        return ResponseFilterStatus::NEED_MORE_DATA;
    };
    let to_write = remaining.len().min(out_buf.len());
    out_buf[..to_write].copy_from_slice(&remaining[..to_write]);
    *out_written = to_write;
    *offset += to_write;

    if *offset >= data.len() {
        *state = FilterState::Done;
        ResponseFilterStatus::DONE
    } else {
        ResponseFilterStatus::NEED_MORE_DATA
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ui/buffering_filter.rs"]
mod tests;
