use crate::app_state::{eval_js, open_in_os, toggle_devtools, with_state};
use crate::ipc::window::get_cef_window;
use cef::*;

pub(crate) fn about_dialog_js() -> String {
    use base64::Engine;
    let logo_b64 =
        base64::engine::general_purpose::STANDARD.encode(include_bytes!("../../tidaluna.png"));
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"(function(){{
if(document.getElementById('tl-about'))return;
var o=document.createElement('div');o.id='tl-about';
o.style.cssText='position:fixed;inset:0;background:rgba(0,0,0,0.6);backdrop-filter:blur(4px);z-index:99999;display:flex;align-items:center;justify-content:center;';
o.innerHTML='<div style="background:#1a1a2e;border-radius:12px;padding:32px;max-width:380px;width:90%;color:#fff;font-family:-apple-system,BlinkMacSystemFont,Segoe UI,Roboto,sans-serif;position:relative;box-shadow:0 20px 60px rgba(0,0,0,0.5);">\
<button id="tl-about-close" style="position:absolute;top:12px;right:16px;background:none;border:none;color:#666;font-size:22px;cursor:pointer;padding:4px 8px;line-height:1;">&times;</button>\
<div style="text-align:center;margin-bottom:20px;">\
<img src="data:image/png;base64,{logo}" style="width:64px;height:64px;margin-bottom:12px;border-radius:12px;">\
<h1 style="margin:0;font-size:20px;font-weight:600;letter-spacing:0.5px;">TidaLunar</h1>\
<p style="margin:4px 0 0;color:#888;font-size:13px;">v{ver} &middot; alpha</p>\
</div>\
<p style="text-align:center;color:#aaa;font-size:13px;margin:0 0 12px;">A native TIDAL client built with Rust</p>\
<div style="text-align:center;margin-bottom:20px;font-size:13px;"><a href="https://github.com/Inrixia/TidaLuna" target="_blank" style="color:#5b8def;text-decoration:none;">GitHub</a> &middot; <a href="https://discord.gg/jK3uHrJGx4" target="_blank" style="color:#7289da;text-decoration:none;">Discord</a></div>\
<hr style="border:none;border-top:1px solid #333;margin:0 0 16px;">\
<p style="color:#666;font-size:11px;text-transform:uppercase;letter-spacing:1px;margin:0 0 10px;">Powered by</p>\
<div style="columns:2;column-gap:16px;color:#999;font-size:12px;line-height:2;">\
<a href="https://github.com/tauri-apps/cef-rs" target="_blank" style="display:block;color:#999;text-decoration:none;">CEF (Chromium)</a>\
<a href="https://github.com/pdeljanov/Symphonia" target="_blank" style="display:block;color:#999;text-decoration:none;">symphonia</a>\
<a href="https://github.com/RustAudio/cpal" target="_blank" style="display:block;color:#999;text-decoration:none;">cpal (Cross-Platform Audio)</a>\
<a href="https://github.com/HEnquist/rubato" target="_blank" style="display:block;color:#999;text-decoration:none;">rubato</a>\
</div>\
<hr style="border:none;border-top:1px solid #333;margin:16px 0;">\
<p style="text-align:center;color:#555;font-size:10px;margin:0;line-height:1.6;">This project is not affiliated with, endorsed by, or connected to TIDAL or its parent companies. TIDAL is a registered trademark of its respective owners.</p>\
</div>';
document.body.appendChild(o);
o.querySelectorAll('a[href]').forEach(function(a){{a.addEventListener('click',function(e){{e.preventDefault();window.cefQuery({{request:JSON.stringify({{channel:'window.open_url',args:[a.href]}}),onSuccess:function(){{}},onFailure:function(){{}}}});}});}});
o.addEventListener('click',function(e){{if(e.target===o)o.remove();}});
document.getElementById('tl-about-close').addEventListener('click',function(){{o.remove();}});
document.addEventListener('keydown',function h(e){{if(e.key==='Escape'){{o.remove();document.removeEventListener('keydown',h);}}}});
}})()"#,
        logo = logo_b64,
        ver = version
    )
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuCommand {
    Settings = 1,
    DevTools = 2,
    About = 3,
    Logout = 4,
    Exit = 5,
    ClearCache = 6,
    OpenData = 7,

    PlayPause = 10,
    Next = 11,
    Prev = 12,
    Stop = 13,
}

impl TryFrom<i32> for MenuCommand {
    type Error = i32;
    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Settings),
            2 => Ok(Self::DevTools),
            3 => Ok(Self::About),
            4 => Ok(Self::Logout),
            5 => Ok(Self::Exit),
            6 => Ok(Self::ClearCache),
            7 => Ok(Self::OpenData),
            10 => Ok(Self::PlayPause),
            11 => Ok(Self::Next),
            12 => Ok(Self::Prev),
            13 => Ok(Self::Stop),
            other => Err(other),
        }
    }
}

wrap_menu_model_delegate! {
    pub(crate) struct HamburgerMenuDelegate {
        _p: u8,
    }
    impl MenuModelDelegate {
        fn execute_command(
            &self,
            _menu_model: Option<&mut MenuModel>,
            command_id: ::std::os::raw::c_int,
            _event_flags: EventFlags,
        ) {
            let Ok(cmd) = MenuCommand::try_from(command_id) else {
                return;
            };
            match cmd {

                MenuCommand::Settings => {
                    eval_js("window.__TL_NAVIGATE__?.('/settings')");
                }
                MenuCommand::DevTools => {
                    toggle_devtools();
                }
                MenuCommand::About => {
                    eval_js(&about_dialog_js());
                }
                MenuCommand::Logout => {
                    eval_js("window.location.href = '/logout';");
                }
                MenuCommand::ClearCache => {
                    if let Ok(mut cache) = crate::state::AUDIO_CACHE.lock()
                        && let Err(e) = cache.clear()
                    {
                        crate::vprintln!("[CACHE]  Clear failed: {e}");
                    }
                }
                MenuCommand::OpenData => {
                    open_in_os(crate::state::cache_data_dir());
                }
                MenuCommand::PlayPause => {
                    eval_js(crate::platform::js_actions::PLAY_PAUSE);
                }
                MenuCommand::Next => {
                    eval_js(crate::platform::js_actions::PLAY_NEXT);
                }
                MenuCommand::Prev => {
                    eval_js(crate::platform::js_actions::PLAY_PREV);
                }
                MenuCommand::Stop => {
                    with_state(|state| {
                        let _ = state.player.stop();
                    });
                }
                MenuCommand::Exit => {
                    with_state(|state| {
                        state.force_quit = true;
                    });
                    let browser = with_state(|state| state.browser.clone()).flatten();
                    if let Some(window) = get_cef_window(browser) {
                        window.close();
                    } else {
                        quit_message_loop();
                    }
                }
            }
        }
    }
}
