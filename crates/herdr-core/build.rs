//! Identity inputs are embedded in the binary, never read from its runtime environment.
use std::env;

fn main() {
    macos_identity();
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

fn macos_identity() {
    println!("cargo:rerun-if-changed=../../distribution/macos-Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let plist = include_str!("../../distribution/macos-Info.plist")
        .replace("@VERSION@", &std::env::var("CARGO_PKG_VERSION").unwrap());
    let path = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap()).join("Info.plist");
    std::fs::write(&path, plist).unwrap();
    // Separate arguments preserve paths containing spaces.
    for arg in ["-sectcreate", "__TEXT", "__info_plist"] {
        println!("cargo:rustc-link-arg-bin=herdr-core=-Wl,{arg}");
    }
    println!("cargo:rustc-link-arg-bin=herdr-core={}", path.display());
}
