use once_cell::sync::Lazy;
use time::OffsetDateTime;

pub static VERBOSE_LOGS: Lazy<bool> = Lazy::new(|| match std::env::var("LOGS") {
    Ok(v) => matches!(
        v.as_str(),
        "1" | "true" | "TRUE" | "yes" | "YES" | "on" | "ON"
    ),
    Err(_) => cfg!(debug_assertions),
});

#[inline]
pub fn verbose_enabled() -> bool {
    *VERBOSE_LOGS
}

#[inline]
pub fn vlog(args: std::fmt::Arguments<'_>) {
    if !verbose_enabled() {
        return;
    }
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    eprintln!(
        "[{:02}:{:02}:{:02}:{:03}] {}",
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond(),
        args
    );
}

#[macro_export]
macro_rules! vprintln {
    ($($arg:tt)*) => {
        $crate::logging::vlog(format_args!($($arg)*))
    };
}
