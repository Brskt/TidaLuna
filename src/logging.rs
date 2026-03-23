use std::sync::LazyLock;
use time::OffsetDateTime;

static LOG_LEVEL: LazyLock<u8> = LazyLock::new(|| match std::env::var("LOGS") {
    Ok(v) => match v.as_str() {
        "3" => 3,
        "2" => 2,
        "1" => 1,
        "0" => 0,
        _ => 0,
    },
    Err(_) => 0,
});

#[inline]
pub fn log_level() -> u8 {
    *LOG_LEVEL
}

#[inline]
pub fn vlog(args: std::fmt::Arguments<'_>) {
    if log_level() < 1 {
        return;
    }
    print_log(args);
}

#[inline]
pub fn vlog2(args: std::fmt::Arguments<'_>) {
    if log_level() < 2 {
        return;
    }
    print_log(args);
}

#[inline]
pub fn vlog3(args: std::fmt::Arguments<'_>) {
    if log_level() < 3 {
        return;
    }
    print_log(args);
}

#[inline]
fn print_log(args: std::fmt::Arguments<'_>) {
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

#[macro_export]
macro_rules! vprintln2 {
    ($($arg:tt)*) => {
        $crate::logging::vlog2(format_args!($($arg)*))
    };
}

#[macro_export]
macro_rules! vprintln3 {
    ($($arg:tt)*) => {
        $crate::logging::vlog3(format_args!($($arg)*))
    };
}
