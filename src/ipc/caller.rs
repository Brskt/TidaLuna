//! Who made an IPC call, resolved from a Rust-issued capability rather than a renderer-supplied
//! string: an identity that arrived as an argument was only ever checked for naming something that
//! exists, never for the caller being it.
//!
//! Not a hard boundary: all plugins share one V8 context; an earlier one can poison globals before
//! a later one captures them (`crate::plugins::wrapper`). Closes ambient confusion, not an active
//! attack.

use crate::app_state::IpcMessage;

pub(crate) enum Caller {
    /// A plugin, identified by its install url.
    Plugin(String),
    /// No capability, or one that no longer resolves. The app bundle lands here too: giving it one
    /// would mean putting it where the shared `@luna/lib` can reach, and plugin code reaches that.
    Unattributed,
}

impl Caller {
    pub(crate) fn resolve(msg: &IpcMessage) -> Self {
        let Some(capability) = msg.cap.as_deref() else {
            return Self::Unattributed;
        };
        match crate::app_state::with_state(|state| {
            state.plugin_manager.plugin_for_capability(capability)
        })
        .flatten()
        {
            Some(plugin_id) => Self::Plugin(plugin_id),
            None => Self::Unattributed,
        }
    }

    /// Absent and superseded capabilities share one message on purpose: telling them apart would be
    /// an oracle for probing which capabilities once existed.
    pub(crate) fn require_plugin(&self) -> Result<&str, (i32, &'static str)> {
        match self {
            Self::Plugin(plugin_id) => Ok(plugin_id),
            Self::Unattributed => Err((403, "unattributed call")),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ipc/caller.rs"]
mod tests;
