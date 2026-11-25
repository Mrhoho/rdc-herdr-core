//! The remote build's address: one root per bundle ID under the account's home
//! (from the account database), and a short socket directory in /tmp that an
//! SSH login and the user's services agree on. HOME, XDG_*, HC_* and a
//! `socket` in the config move nothing; the config itself is optional.
use super::*;

pub(super) fn from_env() -> Result<Paths, RpcError> {
    reject_overrides()?;
    let (root, socket) = locate()?;
    if !fits(&socket) {
        return Err(address(
            format!("the socket path {} is too long for this system", socket.display()),
            "use a shorter root",
        ));
    }
    let dir = socket.parent().expect("socket has a parent");
    crate::platform::managed_private_dir(dir).map_err(|_| socket_dir(dir))?;
    Ok(Paths {
        holder_socket: socket.with_extension("holder.sock"),
        socket,
        state: root.join("state/herdr-core/default"),
        config: root.join("config/herdr-core/config.toml"),
    })
}

/// Test builds keep everything, socket included, inside the scratch root.
#[cfg(feature = "nearshell-test")]
fn locate() -> Result<(PathBuf, PathBuf), RpcError> {
    let root = test_root().ok_or_else(|| {
        address("NEARSHELL_HC_TEST_ROOT is missing or not absolute".into(), "set it to the test's scratch root")
    })?;
    crate::platform::private_dir(&root)
        .map_err(|e| address(format!("cannot create {}: {e}", root.display()), "check the scratch root"))?;
    let socket = root.join("run/c.sock");
    Ok((root, socket))
}

#[cfg(not(feature = "nearshell-test"))]
fn locate() -> Result<(PathBuf, PathBuf), RpcError> {
    const BUNDLE: &str = env!("NEARSHELL_CORE_BUNDLE_ID");
    let home = crate::platform::account_home().map_err(|_| {
        address("this account has no usable home in the account database".into(), "check `getent passwd $(id -u)`")
    })?;
    let socket = crate::platform::managed_socket_dir(&socket_suffix(BUNDLE)).join("c.sock");
    Ok((root_in(&home, BUNDLE), socket))
}

/// `<home>/.local/share/nearshell/<bundle-id>`: Release and every Dev bundle apart.
#[cfg_attr(feature = "nearshell-test", allow(dead_code))]
pub(crate) fn root_in(home: &Path, bundle: &str) -> PathBuf {
    home.join(".local/share/nearshell").join(bundle)
}

/// `/tmp/nearshell-<uid>-<this>`: the first 8 hex digits of the bundle ID's
/// SHA-256 (the root itself is too long for a holder socket under it).
#[cfg_attr(feature = "nearshell-test", allow(dead_code))]
pub(crate) fn socket_suffix(bundle: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(bundle.as_bytes())[..4].iter().map(|b| format!("{b:02x}")).collect()
}

fn address(message: String, hint: &str) -> RpcError {
    RpcError::new("nearshell_address", message, hint)
}

/// Predictable in /tmp: another account can take the name first. Refused, never repaired.
fn socket_dir(dir: &Path) -> RpcError {
    RpcError::new(
        "nearshell_socket_dir",
        format!("{} is not a private directory of this account", dir.display()),
        "it must be a real directory you own with mode 0700; remove what is there (or ask an administrator), then retry",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_per_bundle_and_the_socket_directory_is_short() {
        let root = root_in(Path::new("/home/me"), "com.nearshell.agent");
        assert_eq!(root, Path::new("/home/me/.local/share/nearshell/com.nearshell.agent"));
        assert_eq!(socket_suffix("com.nearshell.agent"), "445dd0fd");
        assert_ne!(socket_suffix("com.nearshell.agent.dev"), socket_suffix("com.nearshell.agent"));
        // The longest uid still leaves room for every holder socket beside it.
        let worst = format!("/tmp/nearshell-{}-{}/c.sock", u32::MAX, socket_suffix("x"));
        assert!(fits(Path::new(&worst)), "{worst}");
        assert!(!fits(&root.join("state/herdr-core/default").join("a".repeat(80))));
    }
}
