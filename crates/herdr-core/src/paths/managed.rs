//! Managed addressing never consults terminal XDG variables or session aliases.
use super::*;

fn invalid() -> RpcError {
    RpcError::new(
        "managed_config",
        "managed core configuration is missing or invalid",
        "open Nearshell to repair its configuration",
    )
}

pub(super) fn from_env() -> Result<Paths, RpcError> {
    reject_overrides()?;
    let root = root()?;
    let config = root.join("config/herdr-core/config.toml");
    // Missing is an error here, unlike upstream's optional config.
    let text = std::fs::read_to_string(&config).map_err(|_| invalid())?;
    let cfg: crate::config::Config = toml::from_str(&text).map_err(|_| invalid())?;
    let socket = PathBuf::from(cfg.socket.ok_or_else(invalid)?);
    if !socket.is_absolute() || socket.components().any(|p| matches!(p, std::path::Component::ParentDir)) {
        return Err(invalid());
    }
    let parent = socket.parent().ok_or_else(invalid)?;
    #[cfg(feature = "nearshell-test")]
    if !socket.starts_with(&root) {
        return Err(invalid());
    }
    #[cfg(not(feature = "nearshell-test"))]
    {
        let suffix = if env!("NEARSHELL_CORE_CONFIG").starts_with("release:") { "r" } else { "d" };
        let expected = crate::platform::managed_socket_dir(suffix);
        if parent != expected || socket.file_name().is_none_or(|name| name != "c.sock") {
            return Err(invalid());
        }
    }
    let holder_socket = socket.with_extension("holder.sock");
    if !fits(&socket) {
        return Err(invalid());
    }
    crate::platform::managed_private_dir(parent).map_err(|_| invalid())?;
    Ok(Paths { socket, holder_socket, state: root.join("state/herdr-core/default"), config })
}

fn root() -> Result<PathBuf, RpcError> {
    #[cfg(feature = "nearshell-test")]
    {
        test_root().ok_or_else(invalid)
    }
    #[cfg(not(feature = "nearshell-test"))]
    {
        Ok(crate::platform::account_home()
            .map_err(|_| invalid())?
            .join("Library/Application Support")
            .join(env!("NEARSHELL_CORE_BUNDLE_ID")))
    }
}
