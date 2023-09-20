use std::sync::atomic::{AtomicU8, Ordering};

pub use chrono;

pub static LOG_LEVEL: AtomicU8 = AtomicU8::new(2);

/// NOTE: will allocate, environment variables are always allocated ...
pub fn init() {
    let level = match std::env::var("RUST_LOG") {
        Err(_) => 2,
        Ok(value) => match value.to_ascii_lowercase().as_str() {
            "error" => 0,
            "warn" => 1,
            "info" => 2,
            "debug" => 3,
            "trace" => 4,
            _ => 2,
        },
    };

    LOG_LEVEL.store(level, Ordering::Relaxed);
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {{ $crate::format_log!(0, "\x1b[31mERROR\x1b[0m", $($arg)*) }}
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {{ $crate::format_log!(1, "\x1b[33mWARN \x1b[0m", $($arg)*) }}
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{ $crate::format_log!(2, "\x1b[32mINFO \x1b[0m", $($arg)*) }}
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {{ $crate::format_log!(3, "\x1b[34mDEBUG\x1b[0m", $($arg)*) }}
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {{ $crate::format_log!(4, "\x1b[36mTRACE\x1b[0m", $($arg)*) }}
}

#[macro_export]
macro_rules! format_log {
    ($log_level_int:literal, $log_level_str:literal, $($arg:tt)*) => {{
        if $crate::LOG_LEVEL.load(std::sync::atomic::Ordering::Relaxed) >= $log_level_int {
            use std::time::{SystemTime, UNIX_EPOCH};

            let delta = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();

            // Extract the current date components (year, month, day)
            let naive = $crate::chrono::NaiveDateTime::from_timestamp_opt(delta.as_secs() as i64, 0).unwrap();

            use $crate::chrono::{Datelike, Timelike};
            let year = naive.year();
            let month = naive.month();
            let day = naive.day();

            let h = naive.hour();
            let m = naive.minute();
            let s = naive.second();

            // [2023-09-19T18:10:27Z INFO  runtime::server] main spawned future for connection 0
            eprintln!(
                "[{timestamp} {log_level} {module_path}] {msg}",
                timestamp = format_args!("{year}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z"),
                log_level = $log_level_str,
                module_path = std::module_path!(),
                msg = format_args!($($arg)*),
            );
        }
    }};
}
