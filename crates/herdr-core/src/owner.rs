//! Cooperative single-owner management. The App owns owner.lock; CLI/RPC
//! callers supply the token explicitly. No code reads a plaintext token file.
use crate::{paths::Paths, rpc::RpcError};
use sha2::{Digest, Sha256};
use std::{fs::File, path::Path};

pub fn denied() -> RpcError {
    RpcError::new("managed_by_owner", "this core is managed by Nearshell", "use the owning Nearshell instance")
}

pub fn root(paths: &Paths) -> Result<&Path, RpcError> {
    paths.config.parent().and_then(Path::parent).and_then(Path::parent).ok_or_else(denied)
}

pub fn verifier(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// Re-read on every operation: an owner that acquires owner.lock can rotate
/// the verifier and immediately revoke an old owner without restarting core.
pub fn authorize(paths: &Paths, token: Option<&str>) -> Result<File, RpcError> {
    let token = token.filter(|s| s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())).ok_or_else(denied)?;
    let lease = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root(paths)?.join("management.lock"))
        .map_err(|_| denied())?;
    if !crate::platform::try_shared_lock(&lease) {
        return Err(denied());
    }
    let expected = crate::config::load(&paths.config).map_err(|_| denied())?.owner_token_sha256.ok_or_else(denied)?;
    let actual = verifier(token);
    if expected.len() != actual.len() || expected.bytes().zip(actual.bytes()).fold(0u8, |v, (a, b)| v | (a ^ b)) != 0 {
        return Err(denied());
    }
    // An orphaned verifier is not a live owner. App must acquire the lock
    // before rotating it, and keep the same inode locked throughout its life.
    let lock = File::options().write(true).open(root(paths)?.join("owner.lock")).map_err(|_| denied())?;
    if crate::platform::try_lock(&lock) {
        return Err(denied());
    }
    Ok(lease)
}

/// A shared verifier lease and exclusive local lifecycle operation. Shared
/// leases allow the CLI and its server RPCs to validate without deadlocking.
/// Owner rotation takes management.lock exclusively and waits for all leases.
pub struct Operation {
    _lease: File,
    _serial: File,
}
pub fn operation(paths: &Paths, token: Option<&str>) -> Result<Operation, RpcError> {
    let lease = authorize(paths, token)?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root(paths)?.join("lifecycle.lock"))
        .map_err(|_| denied())?;
    if !crate::platform::try_lock(&lock) {
        return Err(denied());
    }
    Ok(Operation { _lease: lease, _serial: lock })
}

/// Start/restart never inherit a terminal's HC fields or user XDG roots.
/// All other launcher values remain available as the base for programs.
pub fn launch_env(paths: &Paths) -> Result<std::collections::BTreeMap<String, String>, RpcError> {
    let mut env: std::collections::BTreeMap<_, _> = crate::platform::utf8_vars()
        .filter(|(k, _)| {
            !k.starts_with("HC_") && !k.starts_with("XDG_") && !crate::api::IDENTITY_VARS.contains(&k.as_str())
        })
        .collect();
    let root = root(paths)?;
    env.insert("XDG_CONFIG_HOME".into(), root.join("config").display().to_string());
    env.insert("XDG_STATE_HOME".into(), root.join("state").display().to_string());
    Ok(env)
}

/// The hidden serve entry point also fails closed: it cannot bypass the clean
/// launcher by inheriting a shell's routing or task metadata.
pub fn check_launch_env(paths: &Paths) -> Result<(), RpcError> {
    let expected = launch_env(paths)?;
    if crate::platform::utf8_vars().any(|(k, v)| {
        (k.starts_with("HC_") && !(k == crate::sweep::RUNTIME && v == "1"))
            || (k.starts_with("XDG_") && expected.get(&k) != Some(&v))
    }) {
        return Err(denied());
    }
    for k in ["XDG_CONFIG_HOME", "XDG_STATE_HOME"] {
        if std::env::var(k).ok().as_ref() != expected.get(k) {
            return Err(denied());
        }
    }
    Ok(())
}

pub fn start(paths: &Paths, token: &str) -> Result<crate::platform::Stream, RpcError> {
    let _lease = authorize(paths, Some(token))?;
    if let Some(stream) = crate::client::connect_existing(paths) {
        return Ok(stream);
    }
    paths.create_dirs().map_err(|_| denied())?;
    let lock = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.state.join("core.lock"))
        .map_err(|_| denied())?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !crate::platform::try_lock(&lock) {
        if std::time::Instant::now() >= deadline {
            return Err(denied());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    drop(lock); // the departing core has released the service lock
    authorize(paths, Some(token))?;
    crate::client::spawn_detached(paths, &["serve", "--owner-token", token], "core.log").map_err(|_| denied())?;
    crate::client::wait_for_socket(&paths.socket, "managed core").map_err(|_| denied())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_verifier_uses_standard_sha256() {
        assert_eq!(verifier("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
