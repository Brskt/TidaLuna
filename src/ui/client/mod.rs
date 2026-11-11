use crate::app_state::{IpcMessage, open_in_os};
use crate::ipc::player::handle_ipc_message;
use cef::wrapper::message_router::{BrowserSideRouter, MessageRouterBrowserSideHandlerCallbacks};
use cef::*;
use std::sync::Arc;

mod ipc_gate;
mod lifespan;
mod load;
mod request;

pub(super) use ipc_gate::IpcQueryHandler;
pub(crate) use ipc_gate::{is_privileged_channel, log_safe_channel};
pub(super) use lifespan::TidalLifeSpanHandler;
pub(crate) use load::{NEEDS_BOOT_BLOB_PURGE, POST_LOGIN_RELOADED};
pub(super) use load::{PageState, TidalLoadHandler};
pub(super) use request::TidalRequestHandler;

wrap_drag_handler! {
    pub(super) struct TidalDragHandler {
        _p: u8,
    }
    impl DragHandler {
        fn on_draggable_regions_changed(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            regions: Option<&[DraggableRegion]>,
        ) {
            // The desktop's titlebar moves the window: no page strip may drag it.
            if crate::ui::SYSTEM_FRAME {
                return;
            }
            // CEF applies the regions in page order; a drag layer later in the page (TIDAL's
            // header `::before` on macOS) would cover an earlier `no-drag` button. Passed last,
            // `no-drag` regions always win.
            let regions = regions.unwrap_or_default();
            let ordered: Vec<DraggableRegion> = regions
                .iter()
                .filter(|r| r.draggable != 0)
                .chain(regions.iter().filter(|r| r.draggable == 0))
                .cloned()
                .collect();
            if let Some(window) = crate::ui::app_window::AppWindow::from_browser(browser) {
                window.set_draggable_regions(Some(&ordered));
            }
        }
    }
}

wrap_download_handler! {
    pub(super) struct TidalDownloadHandler {
        _p: u8,
    }
    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            let allowed = _url
                .map(|u| u.to_string().starts_with("blob:"))
                .unwrap_or(false);
            if allowed { 1 } else { 0 }
        }
        // Our own dialog picks the destination, not CEF's: a plugin chooses the blob's bytes, and
        // ours refuses a path inside the app's files and asks again. Returning 1 and calling `cont`
        // later is the documented async use; a dropped callback cancels.
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            _download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(cb) = callback.cloned() else {
                return 0;
            };
            let request = crate::ui::destination_dialog::Request {
                mode: FileDialogMode::SAVE,
                title: None,
                default_path: suggested_name.map(|name| name.to_string()),
                filters: Vec::new(),
            };
            crate::state::rt_handle().spawn(async move {
                let guard = crate::platform::install_guard::get();
                // The raw answer is judged: CEF writes the path as given.
                let answer = crate::ui::destination_dialog::choose_destination(
                    &crate::ui::destination_dialog::Native,
                    request,
                    move |answer| {
                        crate::ui::destination_dialog::classify(answer, |path| {
                            crate::ui::file_dialog::vet_destination(guard, path).map(drop)
                        })
                    },
                )
                .await;
                if let Some(path) = answer.into_iter().next() {
                    let mut task = ContinueDownloadTask::new(cb, std::path::PathBuf::from(path));
                    post_task(ThreadId::UI, Some(&mut task));
                }
            });
            1
        }
    }
}

wrap_task! {
    struct ContinueDownloadTask {
        callback: BeforeDownloadCallback,
        path: std::path::PathBuf,
    }
    impl Task {
        fn execute(&self) {
            let path = CefString::from(self.path.to_string_lossy().as_ref());
            self.callback.cont(Some(&path), 0);
        }
    }
}

wrap_permission_handler! {
    pub(super) struct TidalPermissionHandler {
        _p: u8,
    }
    impl PermissionHandler {
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.cancel();
            }
            1
        }
        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.cont(PermissionRequestResult::DENY);
            }
            1
        }
    }
}

wrap_client! {
    pub(super) struct TidalClient {
        life_span: LifeSpanHandler,
        load: LoadHandler,
        request: RequestHandler,
        display: DisplayHandler,
        drag: DragHandler,
        download: DownloadHandler,
        permission: PermissionHandler,
        router: Arc<BrowserSideRouter>,
    }
    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span.clone())
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load.clone())
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(self.request.clone())
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(self.display.clone())
        }
        fn drag_handler(&self) -> Option<DragHandler> {
            Some(self.drag.clone())
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(self.download.clone())
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(self.permission.clone())
        }
        fn on_process_message_received(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            source_process: ProcessId,
            message: Option<&mut ProcessMessage>,
        ) -> i32 {
            // A renderer stopped a page that arrived without its token and network guards. An
            // install that cannot deliver them is broken; refuse like one.
            if let Some(msg) = message.as_ref()
                && crate::ui::token_filter::userfree_to_string(&msg.name())
                    == crate::ui::early_runtime::EARLY_RUNTIME_MISSING
            {
                crate::platform::refusal::refuse_to_start(
                    "TidaLunar could not hand its page guards to the page. Reinstall TidaLunar.",
                );
            }
            if self.router.on_process_message_received(
                browser.cloned(),
                frame.cloned(),
                source_process,
                message.cloned(),
            ) {
                1
            } else {
                0
            }
        }
    }
}

/// Open an external http(s) URL in the OS browser; other schemes are dropped.
fn open_external_in_os(url: &str) {
    if url.starts_with("http://") || url.starts_with("https://") {
        crate::vprintln!(
            "[NAV]    External -> OS browser: {}",
            crate::util::truncate_str(url, 120)
        );
        open_in_os(url);
    } else {
        crate::vprintln!(
            "[NAV]    Blocked external navigation: {}",
            crate::util::truncate_str(url, 120)
        );
    }
}

wrap_display_handler! {
    pub(super) struct TidalDisplayHandler {
        _p: u8,
    }
    impl DisplayHandler {
        fn on_title_change(&self, browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(window) = crate::ui::app_window::AppWindow::from_browser(browser) {
                window.set_title(title);
            }
        }
        fn on_console_message(
            &self,
            _browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: i32,
        ) -> i32 {
            if let Some(msg) = message {
                let s = msg.to_string();
                if let Some(json) = s.strip_prefix("__IPC__:") {
                    // Frame-less bridge: origin can't be established: privileged
                    // channels are refused here (they must use the cefQuery router).
                    if let Ok(parsed) = serde_json::from_str::<IpcMessage>(json)
                        && is_privileged_channel(&parsed.channel)
                    {
                        crate::vprintln!(
                            "[IPC]    Dropped privileged __IPC__ console message: {}",
                            log_safe_channel(&parsed.channel)
                        );
                        return 0;
                    }
                    handle_ipc_message(json);
                    return 0;
                }
                if s.starts_with("[DBG:") || s.starts_with("[withFormat") {
                    crate::vprintln3!("[JS] {s}");
                } else {
                    crate::vprintln!("[JS] {s}");
                }
            }
            0
        }
    }
}
