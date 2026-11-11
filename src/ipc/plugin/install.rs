use super::source::{PluginData, fetch_plugin_data};
use super::uninstall::CleanupScope;
use super::{ipc_callback_err, ipc_callback_ok};
use crate::app_state::{IpcCallback, IpcMessage, with_state};

pub(super) fn handle_plugin_install(msg: &IpcMessage, callback: IpcCallback) {
    let url = msg.arg(0).to_string();
    crate::state::rt_handle().spawn(async move {
        match do_plugin_install(url).await {
            Ok(info) => {
                let json = serde_json::to_string(&info).unwrap_or_else(|_| "null".to_string());
                ipc_callback_ok(&callback, &json);
            }
            Err(InstallError::Refused(refusal)) => {
                ipc_callback_err(&callback, refusal.code(), &refusal.to_string());
            }
            Err(InstallError::Failed(e)) => ipc_callback_err(&callback, 500, &e),
        }
    });
}

static INSTALL_CONSENTS: std::sync::LazyLock<super::install_consent::Consents> =
    std::sync::LazyLock::new(Default::default);

enum InstallError {
    Refused(super::install_consent::Refusal),
    Failed(String),
}

impl From<super::install_consent::Refusal> for InstallError {
    fn from(refusal: super::install_consent::Refusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<String> for InstallError {
    fn from(e: String) -> Self {
        Self::Failed(e)
    }
}

/// Install from `url` (`.json` manifest + `.mjs` code) from any host (stores, localhost dev). The
/// fetch precedes the consent dialog and opens nothing `plugin.fetch_package` does not already. The
/// sole writer of a plugin row: nothing is stored until the user confirms.
async fn do_plugin_install(url: String) -> Result<crate::plugins::PluginInfo, InstallError> {
    // Before the dialog: an install is enabled next, which the gate refuses, and that refusal
    // would throw away an install the user had just agreed to.
    if !with_state(|state| state.proactive_refresh_done).unwrap_or(false) {
        return Err(InstallError::Failed(
            super::admission::Closed::Gate.reason().to_string(),
        ));
    }
    let turn = INSTALL_CONSENTS.begin()?;
    let client = &*crate::state::HTTP_CLIENT;
    let PluginData {
        name,
        manifest,
        code,
        hash,
    } = fetch_plugin_data(client, &url)
        .await
        .map_err(|e| format!("{e:#}"))?;
    let manifest = manifest.into_string();
    let name = crate::plugins::name::PluginName::parse(&name).map_err(|r| r.to_string())?;
    check_luna_type(&manifest)?;

    // The user approves the bytes in hand; the write below stores these, never a second fetch.
    let answer = crate::ui::install_dialog::show_install_dialog(name.as_str(), &url, &manifest)
        .await
        .ok();
    turn.settle(answer)?;

    // Under the pass lock, as every writer of a row: the row (one actor trip; no trust write
    // queued on the actor lands between the row and the reset), then what still ran of the old
    // install. A refused or failed row write changes nothing.
    let installed = super::jsrt::in_section(move |pass| {
        let db = crate::state::db();
        let replaced = {
            let url = url.clone();
            db.call_plugins(move |pc| reinstall_guard(pc, &url))?
        };
        {
            let (url, name, manifest, code, hash) = (
                url.clone(),
                name.clone(),
                manifest.clone(),
                code,
                hash.clone(),
            );
            db.call(move |pc, sc| commit_install(pc, sc, &url, &name, &manifest, &code, &hash))?;
        }
        // The old load first; it asks and registers nothing after its trust and channels go. The
        // name installed is reset again: its load could still ask after `commit_install`. A name the
        // source no longer gives is reset too, or a later plugin under it would inherit its trust.
        super::inject::unload_current(pass, &url);
        let old = replaced.filter(|old| *old != name);
        // Every name is cleared before a failed reset is answered: the old load is already gone.
        let mut reset = Ok(());
        for owner in std::iter::once(name.clone()).chain(old) {
            let scope = CleanupScope::Plugin(owner.clone());
            let done = db.call_settings(move |sc| super::native::reset_trust(sc, &owner));
            super::native::clear_native_channels(&scope);
            reset = reset.and(done);
        }
        reset.map_err(|e| format!("DB error: {e}"))?;
        Ok::<_, String>(crate::plugins::PluginInfo {
            url,
            name: name.to_string(),
            manifest,
            hash: Some(hash),
            enabled: false,
            installed: true,
            load_error: None,
        })
    })
    .await?;

    Ok(installed?)
}

/// A reinstall turns the plugin off: refused while enabled plugins depend on the one installed at
/// `url`, as a disable is, under the name its row has now. Answers that name, if it is one.
fn reinstall_guard(
    pc: &mut rusqlite::Connection,
    url: &str,
) -> Result<Option<crate::plugins::name::PluginName>, String> {
    let Some(name) = crate::plugins::store::get_name_by_url(pc, url) else {
        return Ok(None);
    };
    match crate::plugins::store::find_enabled_dependants(pc, &name)? {
        deps if deps.is_empty() => Ok(crate::plugins::name::PluginName::parse(&name).ok()),
        deps => Err(format!(
            "Cannot reinstall '{name}': active dependants: {}",
            deps.join(", ")
        )),
    }
}

/// Refuses a luna.type other than plugin or library; none at all is fine.
pub(super) fn check_luna_type(manifest: &str) -> Result<(), String> {
    match crate::plugins::store::parse_luna_meta(manifest)? {
        Some(crate::plugins::store::LunaMeta {
            plugin_type: Some(t),
            ..
        }) if t != "plugin" && t != "library" => {
            Err(format!("Invalid luna.type '{t}' in manifest"))
        }
        _ => Ok(()), // no luna field, or no type
    }
}

/// Stores the plugin row and resets its native trust, which only a reinstall does: a decision kept
/// across it would answer for identical bytes. The row commits last; a failed reset leaves the
/// previous install as it was, and a commit failing after the reset only asks again.
fn commit_install(
    pc: &rusqlite::Connection,
    sc: &mut rusqlite::Connection,
    url: &str,
    name: &crate::plugins::name::PluginName,
    manifest: &str,
    code: &str,
    hash: &str,
) -> Result<(), String> {
    let tx = pc
        .unchecked_transaction()
        .map_err(|e| format!("DB error: {e}"))?;
    crate::plugins::store::install(&tx, url, name, manifest, code, hash)?;
    super::native::reset_trust(sc, name).map_err(|e| format!("DB error: {e}"))?;
    tx.commit().map_err(|e| format!("DB error: {e}"))
}

#[cfg(test)]
#[path = "../../../tests/unit/ipc/plugin/install.rs"]
mod tests;
