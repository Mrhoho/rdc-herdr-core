//! The remote build (nearshell without managed): which HC_* fields its core and
//! programs may carry, and how a program finds this core's own nhc. The user's
//! hc may export HC_SESSION in a login profile, a hub forwards a caller's env
//! unchecked; neither may steer this core or leak into its terminals.
//! Decisions only: callers apply them.

use crate::rpc::RpcError;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// A field this core did not set for itself (HC_RUNTIME marks its own processes).
pub fn foreign(key: &OsStr) -> bool {
    key.as_encoded_bytes().starts_with(b"HC_") && key != crate::sweep::RUNTIME
}

/// What a starting core or holder must not inherit: every foreign HC_* field it
/// would get, and the address fields always, so that one in the user manager's
/// environment is unset for a service too.
pub fn launch_removals(keys: impl IntoIterator<Item = OsString>) -> BTreeSet<OsString> {
    let mut removed: BTreeSet<OsString> = keys.into_iter().filter(|k| foreign(k)).collect();
    removed.extend(["HC_SESSION", "HC_SOCKET"].map(OsString::from));
    removed
}

/// A program's base or per-call environment, before terminal identity is added.
pub fn without_hc(env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.into_iter().filter(|(k, _)| !k.starts_with("HC_")).collect()
}

/// After every merge: `NEARSHELL_HC` names this core's nhc and its folder comes
/// first on PATH, once. Only programs get this; the core's own environment
/// (saved across restarts) never does.
pub fn add_entry(env: &mut BTreeMap<String, String>, root: &Path) {
    let bin = root.join("bin");
    env.insert("NEARSHELL_HC".into(), bin.join("nhc").display().to_string());
    let bin = bin.display().to_string();
    let rest = env.get("PATH").map(|path| path.split(':').filter(|dir| *dir != bin).collect::<Vec<_>>().join(":"));
    let path = match rest.filter(|rest| !rest.is_empty()) {
        Some(rest) => format!("{bin}:{rest}"),
        None => format!("{bin}:/usr/local/bin:/usr/bin:/bin"),
    };
    env.insert("PATH".into(), path);
}

/// A live core holds core.lock, but nothing listens where it should: a /tmp
/// cleaner removed the socket, or this login has a /tmp of its own.
pub fn socket_lost(socket: &Path) -> RpcError {
    RpcError::new(
        "nearshell_socket_lost",
        format!("a core is running, but its socket {} is gone", socket.display()),
        "something removed it (a /tmp cleaner, or a login with its own /tmp); stop that core's `nhc serve` process, then retry",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect()
    }

    #[test]
    fn a_launch_drops_every_inherited_hc_field_but_the_cores_own_mark() {
        let keys = ["HC_SESSION", "HC_TERMINAL_ID", "HC_LOGIN_CAPTURE", "HC_RUNTIME", "PATH", "XHC_KEEP"];
        let removed = launch_removals(keys.map(OsString::from));
        let removed: Vec<_> = removed.iter().map(|k| k.to_str().unwrap()).collect();
        assert_eq!(removed, ["HC_LOGIN_CAPTURE", "HC_SESSION", "HC_SOCKET", "HC_TERMINAL_ID"]);
    }

    #[test]
    fn programs_find_this_cores_nhc_first_and_only_once() {
        let root = Path::new("/home/me/.local/share/nearshell/com.example");
        let mut program = without_hc(env(&[("HC_SESSION", "user"), ("HC_RUNTIME", "1"), ("PATH", "/usr/bin:/bin")]));
        add_entry(&mut program, root);
        add_entry(&mut program, root);
        assert_eq!(
            program,
            env(&[
                ("NEARSHELL_HC", "/home/me/.local/share/nearshell/com.example/bin/nhc"),
                ("PATH", "/home/me/.local/share/nearshell/com.example/bin:/usr/bin:/bin"),
            ])
        );
        let mut bare = BTreeMap::new();
        add_entry(&mut bare, root);
        assert_eq!(bare["PATH"], "/home/me/.local/share/nearshell/com.example/bin:/usr/local/bin:/usr/bin:/bin");
    }
}
