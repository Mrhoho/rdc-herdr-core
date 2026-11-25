//! Timing overrides and test hooks read from the environment (`HC_SSH`,
//! `HC_*_MS`). Tests need them; a Nearshell product build ignores them, so a
//! variable left in someone's shell cannot change how its core behaves.

/// Whether this build reads its test hooks at all.
pub const HONOURED: bool = !cfg!(all(feature = "nearshell", not(feature = "nearshell-test")));

pub fn var_os(name: &str) -> Option<std::ffi::OsString> {
    if HONOURED { std::env::var_os(name) } else { None }
}

/// A whole number of milliseconds from `name`, else `default`.
pub fn ms(name: &str, default: u64) -> u64 {
    var_os(name).and_then(|v| v.to_str()?.parse().ok()).unwrap_or(default)
}
