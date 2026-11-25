//! Names in authored CLI prose. Never apply this renderer to program output,
//! user templates, terminal data, paths, or arbitrary RPC values.
#[cfg(feature = "nearshell")]
pub const COMMAND: &str = "nhc";
#[cfg(not(feature = "nearshell"))]
pub const COMMAND: &str = "hc";
#[cfg(feature = "nearshell")]
pub const VERSION_PREFIX: &str = "nhc ";
#[cfg(not(feature = "nearshell"))]
pub const VERSION_PREFIX: &str = "hc ";

pub fn text(source: &str) -> String {
    if !cfg!(feature = "nearshell") {
        return source.into();
    }
    // Preserve protocol tags, identifiers and filenames; replace the command
    // word only. This is used exclusively on built-in help/schema prose.
    let word = |c: char| c.is_alphanumeric() || "_-./".contains(c);
    let mut out = String::new();
    let mut end = 0;
    for (at, _) in source.match_indices("hc") {
        let before = source[..at].chars().next_back();
        let after = source[at + 2..].chars().next();
        if before.is_some_and(|c| word(c) || c == '<') || after.is_some_and(|c| word(c) || c == '>') {
            continue;
        }
        out.push_str(&source[end..at]);
        out.push_str(COMMAND);
        end = at + 2;
    }
    out.push_str(&source[end..]);
    out.replace("~/.config/herdr-core", CONFIG_DIR)
        .replace("Tests need their own `HC_SESSION` and scratch XDG paths.", TESTS)
        .replace("latest release, at least X, or a build", "use Nearshell to update its bundled core")
}

/// Where this build's config lives, for prose (the root is the account's, not HOME's).
#[cfg(feature = "managed")]
const CONFIG_DIR: &str =
    concat!("~/Library/Application Support/", env!("NEARSHELL_CORE_BUNDLE_ID"), "/config/herdr-core");
#[cfg(all(feature = "nearshell", not(feature = "managed")))]
const CONFIG_DIR: &str = concat!("~/.local/share/nearshell/", env!("NEARSHELL_CORE_BUNDLE_ID"), "/config/herdr-core");
#[cfg(not(feature = "nearshell"))]
const CONFIG_DIR: &str = "~/.config/herdr-core";
const TESTS: &str = if cfg!(feature = "managed") {
    "Tests require the managed-test build and its scratch root; HC_SESSION and HC_SOCKET are refused."
} else {
    "Tests require the nearshell-test build and its scratch root; HC_SESSION and HC_SOCKET are refused."
};

/// Upstream names files and services after herdr-core and hc; Nearshell's own
/// are recognisably its: systemd units, staged binaries.
pub const SERVICE_PREFIX: &str = if cfg!(feature = "nearshell") { "nearshell" } else { "hc" };
pub const STAGED_PREFIX: &str = if cfg!(feature = "nearshell") { ".nhc.new." } else { ".herdr-core.new." };

/// Only schema descriptions are prose; keys, enums, defaults and examples stay opaque.
pub fn schema(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, v) in map {
                if key == "description" {
                    if let Some(s) = v.as_str() {
                        *v = text(s).into();
                    }
                } else {
                    schema(v);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(schema),
        _ => {}
    }
}

/// Managed distributions must never use upstream's downloader or installer:
/// those target herdr-core/hc paths belonging to the user's own installation.
pub fn upstream_distribution() -> Result<(), crate::rpc::RpcError> {
    if cfg!(feature = "nearshell") {
        return Err(crate::rpc::RpcError::new(
            "managed_distribution",
            "this build is distributed with Nearshell",
            "update it through Nearshell",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(feature = "nearshell")]
    fn managed_command_is_nhc() {
        assert_eq!(COMMAND, "nhc");
        assert_eq!(VERSION_PREFIX, "nhc ");
    }
    #[test]
    fn authored_prose_changes_names_without_changing_protocol_tags() {
        assert_eq!(
            text("hc read <hc> hc.run /bin/hc herdr-core"),
            format!("{COMMAND} read <hc> hc.run /bin/hc herdr-core")
        );
    }
}
