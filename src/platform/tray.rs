use cef::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};

struct Tray {
    _icon: tray_icon::TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

thread_local! {
    static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
}

static POLLING_STARTED: AtomicBool = AtomicBool::new(false);

fn assert_ui_thread() {
    assert!(
        currently_on(ThreadId::UI) == 1,
        "tray functions must be called from the CEF UI thread"
    );
}

fn decode_icon() -> Option<Icon> {
    let png_data = include_bytes!("../../tidaluna.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png_data));
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to decode PNG: {e}");
            return None;
        }
    };
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let info = match reader.next_frame(&mut buf) {
        Ok(i) => i,
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to read PNG frame: {e}");
            return None;
        }
    };
    buf.truncate(info.buffer_size());
    match Icon::from_rgba(buf, info.width, info.height) {
        Ok(icon) => Some(icon),
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to create icon: {e}");
            None
        }
    }
}

pub(crate) fn create_tray() -> bool {
    assert_ui_thread();

    let Some(icon) = decode_icon() else {
        return false;
    };

    let show_item = MenuItem::new("Show", true, None);
    let quit_item = MenuItem::new("Quit", true, None);
    let show_id = show_item.id().clone();
    let quit_id = quit_item.id().clone();

    let menu = Menu::new();
    if let Err(e) = menu.append(&show_item) {
        crate::vprintln!("[TRAY]   Failed to append Show menu item: {e}");
        return false;
    }
    if let Err(e) = menu.append(&quit_item) {
        crate::vprintln!("[TRAY]   Failed to append Quit menu item: {e}");
        return false;
    }

    let builder = TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("TidaLunar")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false);

    match builder.build() {
        Ok(tray) => {
            crate::vprintln!("[TRAY]   Created tray icon");
            TRAY.with(|t| {
                *t.borrow_mut() = Some(Tray {
                    _icon: tray,
                    show_id,
                    quit_id,
                });
            });
            true
        }
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to create tray icon: {e}");
            false
        }
    }
}

pub(crate) fn destroy_tray() {
    assert_ui_thread();
    TRAY.with(|t| {
        if t.borrow().is_some() {
            *t.borrow_mut() = None;
            crate::vprintln!("[TRAY]   Destroyed tray icon");
        }
    });
}

pub(crate) fn has_tray() -> bool {
    assert_ui_thread();
    TRAY.with(|t| t.borrow().is_some())
}

fn show_window() {
    let browser = crate::app_state::with_state(|s| s.browser.clone()).flatten();
    if let Some(window) = crate::ipc::window::get_cef_window(browser) {
        window.show();
        #[cfg(target_os = "windows")]
        {
            let hwnd = window.window_handle().0 as windows_sys::Win32::Foundation::HWND;
            if !hwnd.is_null() {
                unsafe {
                    windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
                }
            }
        }
    }
}

fn force_quit() {
    crate::app_state::with_state(|state| {
        state.force_quit = true;
    });
    let browser = crate::app_state::with_state(|s| s.browser.clone()).flatten();
    if let Some(window) = crate::ipc::window::get_cef_window(browser) {
        window.close();
    } else {
        quit_message_loop();
    }
}

fn poll_events() {
    let ids = TRAY.with(|t| {
        t.borrow()
            .as_ref()
            .map(|tray| (tray.show_id.clone(), tray.quit_id.clone()))
    });
    let Some((show_id, quit_id)) = ids else {
        return;
    };

    while let Ok(event) = MenuEvent::receiver().try_recv() {
        if event.id == show_id {
            show_window();
        } else if event.id == quit_id {
            force_quit();
            return;
        }
    }

    #[cfg(target_os = "windows")]
    {
        use tray_icon::{MouseButton, MouseButtonState, TrayIconEvent};
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                show_window();
            }
        }
    }
}

// --- CEF polling task ---

pub(crate) fn start_event_polling() {
    if POLLING_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut task = TrayPollTask::new(0);
    post_delayed_task(ThreadId::UI, Some(&mut task), 100);
}

wrap_task! {
    struct TrayPollTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            poll_events();
            let mut task = TrayPollTask::new(0);
            post_delayed_task(ThreadId::UI, Some(&mut task), 100);
        }
    }
}
