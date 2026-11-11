use crate::app_state::{open_in_os, with_state};
use crate::ui::nav::{self, NavigationPolicy, PageKind};
use cef::wrapper::message_router::{BrowserSideRouter, MessageRouterBrowserSideHandlerCallbacks};
use cef::*;
use std::sync::Arc;

use super::open_external_in_os;

fn render_crash_reason(status: TerminationStatus) -> &'static str {
    match status {
        TerminationStatus::PROCESS_OOM => "ran out of memory",
        TerminationStatus::PROCESS_CRASHED => "crashed",
        TerminationStatus::PROCESS_WAS_KILLED => "was killed",
        TerminationStatus::ABNORMAL_TERMINATION => "terminated abnormally",
        TerminationStatus::LAUNCH_FAILED => "failed to launch",
        TerminationStatus::INTEGRITY_FAILURE => "failed an integrity check",
        _ => "stopped unexpectedly",
    }
}

/// Write a render-process crash report under the data dir and return its path.
fn write_render_crash_log(
    status: TerminationStatus,
    reason: &str,
    error_code: i32,
    error_string: &str,
) -> Option<std::path::PathBuf> {
    let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
    let (year, month, day) = (now.year(), u8::from(now.month()), now.day());
    let (hour, min, sec) = (now.hour(), now.minute(), now.second());
    // Filename can't contain `/` or `:`, hence the dashes in the name; the body
    // keeps the readable HH:MM:SS DD/MM/YYYY form.
    let file_stamp = format!("{hour:02}-{min:02}-{sec:02}_{day:02}-{month:02}-{year}");
    let human_stamp = format!("{hour:02}:{min:02}:{sec:02} {day:02}/{month:02}/{year}");
    let dir = crate::state::cache_data_dir().join("crashes");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("render-crash-{file_stamp}.log"));
    let body = format!(
        "TidaLunar render process terminated\nversion: {}\ntime: {human_stamp}\nstatus: {reason} (raw {})\nerror_code: {error_code}\nerror_string: {error_string}\n",
        env!("CARGO_PKG_VERSION"),
        status.get_raw(),
    );
    std::fs::write(&path, body).ok()?;
    Some(path)
}

wrap_request_handler! {
    pub(in crate::ui) struct TidalRequestHandler {
        router: Arc<BrowserSideRouter>,
    }
    impl RequestHandler {
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let url = request
                .as_ref()
                .map(|r| {
                    let u = r.url();
                    crate::ui::token_filter::userfree_to_string(&u)
                })
                .unwrap_or_default();

            let kind = PageKind::classify(&url);
            let policy = NavigationPolicy::for_page(kind);

            let safe_url = crate::util::redact_url_query(&url);
            crate::vprintln!(
                "[NAV]    on_before_browse: is_redirect={} url={}",
                _is_redirect,
                crate::util::truncate_str(&safe_url, 200)
            );

            if kind == PageKind::TidalCallback {
                let web_url = url.replacen("tidal://", &format!("https://{}/", nav::HOST_DESKTOP), 1);
                crate::vprintln!(
                    "[AUTH]   Intercepted tidal:// redirect -> {}",
                    crate::util::redact_url_query(&web_url)
                );
                with_state(|state| {
                    if let Some(ref browser) = state.browser
                        && let Some(main_frame) = browser.main_frame()
                    {
                        let cef_url = CefString::from(web_url.as_str());
                        main_frame.load_url(Some(&cef_url));
                    }
                });
                // Only close popup browsers, not the main window
                if let Some(browser) = browser {
                    if browser.is_popup() != 0 {
                        crate::vprintln!("[AUTH]   Closing auth popup after tidal:// redirect");
                        if let Some(host) = browser.host() {
                            host.try_close_browser();
                        }
                    } else {
                        crate::vprintln!("[AUTH]   Main frame redirect, not closing browser");
                    }
                }
                return 1;
            }

            if policy.bypass_router {
                crate::vprintln!("[AUTH]   Bypassing router for auth navigation");
                return 0;
            }

            self.router
                .on_before_browse(browser.cloned(), frame.cloned());
            0
        }
        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            // DevTools link clicks route here, not through on_before_browse; send
            // them to the OS browser instead of navigating the inspected window.
            let url = target_url.as_ref().map(|u| u.to_string()).unwrap_or_default();
            open_external_in_os(&url);
            1
        }
        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            _cert_error: Errorcode,
            _request_url: Option<&CefString>,
            _ssl_info: Option<&mut Sslinfo>,
            _callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            0
        }
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            is_navigation: ::std::os::raw::c_int,
            is_download: ::std::os::raw::c_int,
            request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            let url = crate::ui::nav::RequestUrl::new(
                _request
                    .as_ref()
                    .map(|r| {
                        let u = r.url();
                        crate::ui::token_filter::userfree_to_string(&u)
                    })
                    .unwrap_or_default(),
            );

            // Strip the CSP meta on the doc navigation (first load, no SW yet);
            // the browser handler wins over the context one; peel it off here.
            // TokenResourceHandler is a no-op on the doc GET: nothing is lost.
            if crate::ui::csp_filter::is_document_url(
                &url,
                _request.as_ref().map(|r| r.resource_type()),
            ) {
                return Some(crate::ui::csp_filter::DocumentHandler::new());
            }

            // Every TIDAL chunk goes through the capture filter, which picks its rewrites from the
            // content (`chunk_rewrites`); plugins then share the host React instance
            // (hooks/context) and mount roots against it.
            if crate::ui::module_capture::is_tidal_chunk(&url) {
                return Some(crate::ui::module_capture::CaptureRequestHandler::new());
            }

            // GitHub plugin-store fetches: Chromium rejects the signed release-asset CDN
            // redirect; serve them via reqwest. Mirrored in the context handler (SW path).
            if let Some(h) =
                crate::ui::store_proxy::intercept(url.as_str(), is_navigation, is_download)
            {
                return Some(h);
            }

            // Luna's own plugin ES modules, read from the front folder and served on
            // /__luna__/*.mjs.
            if let Some(h) =
                crate::ui::luna_modules::intercept(url.as_str(), is_navigation, is_download)
            {
                return Some(h);
            }

            // Ahead of the token branch and outside the non-Tidal arm, matching `csp_filter` and
            // `dialog`: `api.tidalhifi.com` and `event-collector.*.tidalhi.fi` clear
            // `should_rewrite_token` without clearing `is_tidal_origin`, so from inside either
            // arm this gate would never run for them.
            if let Some(req) = _request.as_ref()
                && let Some(h) = crate::ui::exfil_guard::block_disallowed_image(
                    req.resource_type(),
                    &url,
                    "renderer",
                    request_initiator,
                )
            {
                return Some(h);
            }

            if crate::ui::token_filter::should_rewrite_token(&url)
                || crate::ui::nav::is_token_endpoint(&url)
            {
                Some(crate::ui::token_filter::TokenResourceHandler::new(
                    std::sync::Arc::new(std::sync::Mutex::new(None)),
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                ))
            } else if !crate::ui::nav::is_tidal_origin(&url) {
                // Exfiltration guard: block sendBeacon to non-Tidal domains.
                // TIDAL doesn't use sendBeacon - safe to block unconditionally.
                if let Some(req) = _request.as_ref()
                    && req.resource_type() == ResourceType::PING
                {
                    crate::vprintln!(
                        "[EXFIL]  BLOCKED sendBeacon to {}",
                        crate::util::truncate_str(url.as_str(), 80)
                    );
                    return Some(crate::ui::exfil_guard::ExfilBlockHandler::new());
                }
                None
            } else {
                None
            }
        }
        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: ::std::os::raw::c_int,
            error_string: Option<&CefString>,
        ) {
            let owned = browser.cloned();
            self.router.on_render_process_terminated(owned.clone());

            let reason = render_crash_reason(status);
            let err = error_string.map(|s| s.to_string()).unwrap_or_default();
            crate::vprintln!("[CRASH]  Render process {reason} (code {error_code}) {err}");
            let log_path = write_render_crash_log(status, reason, error_code, &err);

            // Only the main browser triggers the recovery dialog; a crashing
            // auth popup must not prompt a reload of the wrong window.
            let is_main = owned.as_ref().is_some_and(|b| b.is_popup() == 0);
            if !is_main {
                return;
            }

            // The page died with every control over its music and its plugins, and the dialog
            // below can stay up as long as the user likes: end both here, not at the reload's
            // commit, or a load sent into the dead frame times out and turns a plugin off.
            with_state(|state| {
                state.end_page_music();
                state.end_page_plugins();
            });

            // Stack guard: if the reloaded page crashes again while a dialog is
            // already up, just reload rather than opening another dialog.
            if crate::ui::crash_dialog::CRASH_DIALOG_OPEN
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                if let Some(b) = owned {
                    b.reload();
                }
                return;
            }

            let rx = crate::ui::crash_dialog::show_crash_dialog(
                reason,
                error_code,
                log_path.as_deref(),
            );
            crate::state::rt_handle().spawn(async move {
                let action = rx
                    .await
                    .unwrap_or(crate::ui::crash_dialog::CrashAction::Reload);
                crate::ui::crash_dialog::CRASH_DIALOG_OPEN
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                if action == crate::ui::crash_dialog::CrashAction::Quit {
                    // Exiting here skips the drain that follows the message loop, and queued
                    // settings writes die with the process. This runs on a tokio task, never
                    // the actor's own thread; waiting on it is safe.
                    crate::platform::app_lock::close_channel();
                    crate::state::db().flush();
                    crate::platform::secure_store::flush();
                    crate::ui::deep_link::report_unshown();
                    std::process::exit(0);
                }
                if action == crate::ui::crash_dialog::CrashAction::OpenFolder
                    && let Some(dir) = log_path.as_ref().and_then(|p| p.parent())
                {
                    open_in_os(dir);
                }
                // Reload the main browser on the UI thread (for Reload and
                // OpenFolder; Quit already exited above).
                let mut task = crate::ui::crash_dialog::ReloadMainTask::new(0);
                post_task(ThreadId::UI, Some(&mut task));
            });
        }
    }
}
