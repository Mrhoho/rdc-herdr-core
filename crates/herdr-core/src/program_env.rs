//! Managed program environments: base, configured values, call overrides, unset,
//! then task metadata (added by the caller). No merged environment is persisted.
use crate::{api::IDENTITY_VARS, config::Programs, rpc::RpcError};
use std::collections::BTreeMap;

type Env = BTreeMap<String, String>;
const XDG: &[&str] = &["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"];

pub fn validate(extra: &Env) -> Result<(), RpcError> {
    if extra.keys().any(|k| !IDENTITY_VARS.contains(&k.as_str())) {
        return Err(invalid());
    }
    Ok(())
}
fn invalid() -> RpcError {
    RpcError::new(
        "managed_program_env",
        "invalid managed program environment",
        "use programs.env and programs.unset in owner configuration",
    )
}

pub fn build(mut base: Env, settings: &Programs, extra: Env) -> Result<Env, RpcError> {
    validate(&extra)?;
    base.retain(|k, _| !k.starts_with("HC_") && !XDG.contains(&k.as_str()) && !IDENTITY_VARS.contains(&k.as_str()));
    if let Some(lang) = crate::locale::for_programs(&base) {
        base.insert("LANG".into(), lang);
    }
    base.insert("TERM".into(), "xterm-256color".into());
    base.insert("COLORTERM".into(), "truecolor".into());
    base.insert("TERM_PROGRAM".into(), "ghostty".into());
    crate::platform::merge_env(&mut base, settings.env.clone());
    crate::platform::merge_env(&mut base, extra);
    for key in &settings.unset {
        base.remove(key);
    }
    // Configuration and RPC cannot impersonate runtime task metadata either.
    base.retain(|k, _| !k.starts_with("HC_"));
    if base.iter().any(|(k, v)| k.is_empty() || k.contains(['=', '\0']) || v.contains('\0')) {
        return Err(invalid());
    }
    Ok(base)
}

pub fn identity(env: &Env) -> String {
    format!(
        "{} {}",
        env.get("TERM_PROGRAM").map_or("", String::as_str),
        env.get("TERM_PROGRAM_VERSION").map_or("", String::as_str)
    )
    .trim()
    .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn map(items: &[(&str, &str)]) -> Env {
        items.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    #[test]
    fn merge_order_unset_and_metadata_are_shared_by_all_task_types() {
        let mut base = map(&[
            ("HC_SESSION", "private"),
            ("HC_SOCKET", "private"),
            ("HC_TERMINAL_SOCKET", "stale"),
            ("HC_SCOPE", "stale"),
            ("HC_RUNTIME", "1"),
            ("TERM_PROGRAM", "launcher"),
            ("PATH", "base"),
        ]);
        for k in XDG {
            base.insert(k.to_string(), "private".into());
        }
        let settings = Programs {
            env: map(&[
                ("XDG_CONFIG_HOME", "user"),
                ("XDG_RUNTIME_DIR", "user-run"),
                ("PATH", "configured"),
                ("TERM_PROGRAM", "configured"),
                ("HC_TERMINAL_ID", "forged"),
                ("HC_TERMINAL_SOCKET", "forged"),
            ]),
            unset: vec!["TERM".into(), "TERM_PROGRAM_VERSION".into()],
        };
        let result = build(base, &settings, map(&[("TERM_PROGRAM", "caller"), ("TERM_PROGRAM_VERSION", "1")])).unwrap();
        assert_eq!(result["PATH"], "configured");
        assert_eq!(result["XDG_CONFIG_HOME"], "user");
        assert_eq!(result["XDG_RUNTIME_DIR"], "user-run");
        for k in ["XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "TERM", "TERM_PROGRAM_VERSION"] {
            assert!(!result.contains_key(k), "{k}");
        }
        assert!(!result.keys().any(|k| k.starts_with("HC_")));
        assert_eq!(identity(&result), "caller");
    }
    #[test]
    fn overrides_are_allowlisted_and_errors_do_not_echo_data() {
        for key in ["PATH", "XDG_RUNTIME_DIR", "HC_TERMINAL_ID", "HC_SOCKET", "PAGER"] {
            let err = build(Env::new(), &Programs::default(), map(&[(key, "secret")])).unwrap_err();
            assert_eq!(err.kind, "managed_program_env");
            assert!(!format!("{err:?}").contains("secret"));
        }
        let settings = Programs { env: map(&[("BAD=KEY", "value")]), ..Default::default() };
        assert!(build(Env::new(), &settings, Env::new()).is_err());
    }
}
