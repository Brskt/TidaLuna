use once_cell::sync::Lazy;

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

#[macro_export]
macro_rules! vprintln {
    ($($arg:tt)*) => {
        if $crate::logging::verbose_enabled() {
            eprintln!($($arg)*);
        }
    };
}
