//! Identity inputs are embedded in the binary, never read from its runtime environment.
use std::env;

fn main() {
    macos_identity();
    let managed = env::var_os("CARGO_FEATURE_MANAGED").is_some();
    let test = env::var_os("CARGO_FEATURE_MANAGED_TEST").is_some();
    if managed {
        assert!(
            env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos"),
            "managed distribution currently targets macOS only"
        );
    }
    println!("cargo:rerun-if-env-changed=NEARSHELL_CORE_BUNDLE_ID");
    let bundle = env::var("NEARSHELL_CORE_BUNDLE_ID").unwrap_or_else(|_| {
        assert!(!managed || test, "managed builds require NEARSHELL_CORE_BUNDLE_ID");
        "com.nearshell.core-test".into()
    });
    assert!(
        !bundle.is_empty()
            && bundle.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            && !bundle.starts_with('.')
            && !bundle.ends_with('.')
            && !bundle.contains(".."),
        "invalid bundle id"
    );
    println!("cargo:rustc-env=NEARSHELL_CORE_BUNDLE_ID={bundle}");
    for (key, fallback) in
        [("COMMIT", "unversioned"), ("PATCHSET", "a1-dev"), ("RELEASE_SEQ", "0"), ("CONFIG", "upstream")]
    {
        let var = format!("NEARSHELL_CORE_{key}");
        println!("cargo:rerun-if-env-changed={var}");
        let mut value = env::var(&var).unwrap_or_else(|_| {
            assert!(!managed || test, "managed builds require {var}");
            fallback.into()
        });
        if managed && !test {
            match key {
                "COMMIT" => assert!(
                    value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()),
                    "commit must be a full Git SHA"
                ),
                "RELEASE_SEQ" => {
                    assert!(value.parse::<u64>().is_ok_and(|n| n > 0), "managed release sequence must be positive")
                }
                "CONFIG" => assert!(matches!(value.as_str(), "dev" | "release"), "config must be dev or release"),
                _ => {}
            }
        }
        if key == "CONFIG" && managed {
            // Includes the addressing contract, so differently configured binaries
            // cannot accidentally advertise the same complete identity.
            value = format!("{value}:{bundle}{}", if test { ":scratch-test" } else { "" });
        }
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
