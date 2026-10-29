//! Identity inputs are embedded in the binary, never read from its runtime environment.
use std::env;

fn main() {
    for (key, fallback) in
        [("COMMIT", "unversioned"), ("PATCHSET", "a1-dev"), ("RELEASE_SEQ", "0"), ("CONFIG", "upstream")]
    {
        let var = format!("NEARSHELL_CORE_{key}");
        println!("cargo:rerun-if-env-changed={var}");
        let value = env::var(&var).unwrap_or_else(|_| fallback.into());
        assert!(!value.is_empty() && !value.contains(['\r', '\n']), "invalid {var}");
        if key == "RELEASE_SEQ" {
            value.parse::<u64>().expect("NEARSHELL_CORE_RELEASE_SEQ must be a u64");
        }
        println!("cargo:rustc-env={var}={value}");
    }
}
