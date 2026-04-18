use cef::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(not(target_os = "linux"))]
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
#[cfg(not(target_os = "linux"))]
use tray_icon::{Icon, TrayIconBuilder};

#[cfg(target_os = "linux")]
use tray_item::{IconSource, TrayItem};

#[cfg(not(target_os = "linux"))]
struct Tray {
    _icon: tray_icon::TrayIcon,
    show_id: MenuId,
    quit_id: MenuId,
}

#[cfg(target_os = "linux")]
struct Tray {
    _item: TrayItem,
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

#[cfg(not(target_os = "linux"))]
fn decode_icon() -> Option<Icon> {
    let (buf, width, height) = decode_rgba()?;
    match Icon::from_rgba(buf, width, height) {
        Ok(icon) => Some(icon),
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to create icon: {e}");
            None
        }
    }
}

fn decode_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let png_data = include_bytes!("../../tidaluna.png");
    let decoder = png::Decoder::new(std::io::Cursor::new(png_data));
    let mut reader = match decoder.read_info() {
        Ok(r) => r,
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to decode PNG: {e}");
            return None;
        }
    };
    let Some(buf_size) = reader.output_buffer_size() else {
        crate::vprintln!("[TRAY]   PNG buffer size overflow");
        return None;
    };
    let mut buf = vec![0u8; buf_size];
    let info = match reader.next_frame(&mut buf) {
        Ok(i) => i,
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to read PNG frame: {e}");
            return None;
        }
    };
    buf.truncate(info.buffer_size());
    Some((buf, info.width, info.height))
}

#[cfg(not(target_os = "linux"))]
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

#[cfg(target_os = "linux")]
pub(crate) fn create_tray() -> bool {
    assert_ui_thread();

    let Some((data, width, height)) = decode_rgba() else {
        return false;
    };

    let icon = IconSource::Data {
        data,
        width: width as i32,
        height: height as i32,
    };

    let mut item = match TrayItem::new("TidaLunar", icon) {
        Ok(i) => i,
        Err(e) => {
            crate::vprintln!("[TRAY]   Failed to create tray item: {e}");
            return false;
        }
    };

    if let Err(e) = item.add_menu_item("Show", post_show_window) {
        crate::vprintln!("[TRAY]   Failed to add Show item: {e}");
        return false;
    }
    if let Err(e) = item.add_menu_item("Quit", post_force_quit) {
        crate::vprintln!("[TRAY]   Failed to add Quit item: {e}");
        return false;
    }

    crate::vprintln!("[TRAY]   Created tray item (ksni)");
    TRAY.with(|t| *t.borrow_mut() = Some(Tray { _item: item }));
    true
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
    if let Some(window) = crate::ui::app_window::AppWindow::current() {
        window.show();
        window.focus_foreground();
    }
}

fn force_quit() {
    crate::app_state::with_state(|state| {
        state.force_quit = true;
    });
    if let Some(window) = crate::ui::app_window::AppWindow::current() {
        window.close();
    } else {
        quit_message_loop();
    }
}

#[cfg(target_os = "linux")]
fn post_show_window() {
    let mut task = TrayShowWindowTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}

#[cfg(target_os = "linux")]
fn post_force_quit() {
    let mut task = TrayForceQuitTask::new(0);
    post_task(ThreadId::UI, Some(&mut task));
}

#[cfg(not(target_os = "linux"))]
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

#[cfg(not(target_os = "linux"))]
pub(crate) fn start_event_polling() {
    if POLLING_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut task = TrayPollTask::new(0);
    post_delayed_task(ThreadId::UI, Some(&mut task), 100);
}

#[cfg(target_os = "linux")]
pub(crate) fn start_event_polling() {
    // Linux uses tray-item/ksni which dispatches callbacks on its own thread;
    // callbacks post tasks back to the CEF UI thread, so no polling loop needed.
    POLLING_STARTED.store(true, Ordering::SeqCst);
}

#[cfg(not(target_os = "linux"))]
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

#[cfg(target_os = "linux")]
wrap_task! {
    struct TrayShowWindowTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            show_window();
        }
    }
}

#[cfg(target_os = "linux")]
wrap_task! {
    struct TrayForceQuitTask {
        _p: u8,
    }
    impl Task {
        fn execute(&self) {
            force_quit();
        }
    }
}
