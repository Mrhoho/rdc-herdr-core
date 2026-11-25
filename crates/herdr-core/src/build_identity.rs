// The `config` field of a Nearshell build identity. build.rs includes this
// file, so the format is written once; plain comments only (no inner docs).

/// `<dev|release>:<bundle-id>`, then `:remote` for the remote build, then
/// `:scratch-test` for test builds, always last: clients read that suffix.
pub fn config_identity(kind: &str, bundle: &str, remote: bool, test: bool) -> String {
    format!("{kind}:{bundle}{}{}", if remote { ":remote" } else { "" }, if test { ":scratch-test" } else { "" })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_test_suffix_stays_last_and_managed_is_unchanged() {
        let id = super::config_identity;
        assert_eq!(id("release", "com.nearshell.agent", false, false), "release:com.nearshell.agent");
        assert_eq!(id("dev", "com.example.dev", true, false), "dev:com.example.dev:remote");
        assert_eq!(id("dev", "com.example.dev", true, true), "dev:com.example.dev:remote:scratch-test");
        assert_eq!(id("dev", "com.example.dev", false, true), "dev:com.example.dev:scratch-test");
    }
}
