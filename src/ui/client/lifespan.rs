use crate::app_state::with_state;
use crate::ui::nav::PageKind;
use cef::wrapper::message_router::{BrowserSideRouter, MessageRouterBrowserSideHandlerCallbacks};
use cef::*;
use std::sync::Arc;

use super::open_external_in_os;

wrap_life_span_handler! {
    pub(in crate::ui) struct TidalLifeSpanHandler {
        router: Arc<BrowserSideRouter>,
    }
    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            _target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let url_str = target_url.as_ref().map(|u| u.to_string()).unwrap_or_default();
            crate::vprintln!(
                "[POPUP]  on_before_popup: {}",
                crate::util::truncate_str(&crate::util::redact_url_query(&url_str), 120)
            );
            if target_url.is_some() {
                let kind = PageKind::classify(&url_str);
                if kind == PageKind::AuthHost {
                    crate::vprintln!(
                        "[AUTH]   Opening auth popup: {}",
                        crate::util::truncate_str(&crate::util::redact_url_query(&url_str), 120)
                    );
                    if let Some(wi) = _window_info {
                        wi.window_name = CefString::from("TidaLunar - Login");
                        wi.bounds = cef::Rect { x: 100, y: 100, width: 500, height: 700 };
                        #[cfg(target_os = "windows")]
                        {
                            wi.style = 0x00CF0000; // WS_OVERLAPPEDWINDOW
                        }
                        wi.runtime_style = RuntimeStyle::ALLOY;
                    }
                    // The popup's renderer runs the early runtime too (AuthHost injects it).
                    if crate::ui::early_runtime::COVERED_KINDS.contains(&kind)
                        && let Some(extra) = extra_info
                    {
                        *extra = crate::ui::early_runtime::extra_info();
                    }
                    return 0;
                }
                if kind == PageKind::External {
                    open_external_in_os(&url_str);
                }
            }
            1
        }
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            if let Some(browser) = browser.cloned() {
                let is_popup = browser.is_popup() != 0;
                crate::vprintln!("[CEF]    on_after_created: popup={}", is_popup);
                if !is_popup {
                    // Only store the main browser, not auth popups
                    with_state(|state| {
                        state.browser = Some(browser);
                    });
                    // A deep link that arrived while this was being built waits
                    // for exactly this moment; nothing later would drain it.
                    // After the store, which is where the frame is looked up.
                    crate::ui::deep_link::apply_pending();
                }
            }
        }
        fn do_close(&self, _browser: Option<&mut Browser>) -> i32 {
            0
        }
        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let is_popup = browser
                .as_ref()
                .map(|b| b.is_popup() != 0)
                .unwrap_or(false);
            crate::vprintln!("[CEF]    on_before_close: popup={}", is_popup);
            self.router.on_before_close(browser.cloned());
            if !is_popup {
                with_state(|state| {
                    state.browser = None;
                });
                crate::app_state::quit_loop();
            }
        }
    }
}
