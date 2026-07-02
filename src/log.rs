//! Global verbosity level, set once at startup from `-q`/`-v`, read by the
//! `vlog!` macro. A plain atomic is enough for a handful of status lines —
//! no need for a logging crate.
//!
//! Levels: 0 = errors only, 1 = status (default), 2 = protocol trace.

use std::sync::atomic::{AtomicU8, Ordering};

static VERBOSITY: AtomicU8 = AtomicU8::new(1);

pub fn set(level: u8) {
    VERBOSITY.store(level, Ordering::Relaxed);
}

pub fn level() -> u8 {
    VERBOSITY.load(Ordering::Relaxed)
}

/// Print to stderr if the current verbosity is at least `level`. Real
/// errors bypass this and use `eprintln!` directly — verbosity only
/// controls informational/diagnostic noise.
#[macro_export]
macro_rules! vlog {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::level() >= $level {
            eprintln!($($arg)*);
        }
    };
}
