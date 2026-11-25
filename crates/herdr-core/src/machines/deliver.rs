//! A Nearshell hub puts its own remote core on a machine: only the payload in
//! its `remote_payloads` directory (never a download, never this binary), for
//! the machine's platform, checked here before a byte is sent and once more by
//! what the copy reports after it put itself in place there.

use super::payload::{self, Landed};
use super::{Reach, last_line, nearshell, run_raw};
use crate::api::BuildInfo;
use crate::branding::COMMAND as CLI_NAME;
use crate::paths::Paths;
use crate::rpc::RpcError;
use serde_json::json;
use std::path::PathBuf;

/// Installs, updates or confirms the remote core there. The caller dials it
/// again: an install proves the copy runs there, not that the hub accepts it.
pub fn deliver(paths: &Paths, to: Reach) -> Result<Landed, RpcError> {
    super::check_target(to)?;
    let here = crate::api::build_info();
    same_as_core(paths, &here)?;
    let dir = payloads(paths)?;
    let shown = dir.display().to_string();
    let manifest = std::fs::read(dir.join("manifest.json")).map_err(|e| {
        RpcError::new(
            "remote_payloads_unreadable",
            format!("{shown}/manifest.json: {e}"),
            format!("check remote_payloads in {}", paths.config.display()),
        )
    })?;
    let entries = payload::entries(&manifest, &shown)?;
    let probe = run_raw(paths, to, "uname -sm", b"")?;
    if !probe.status.success() {
        return Err(RpcError::new(
            "install_failed",
            format!("on {}: {}", to.target, last_line(&probe.stderr)),
            "nothing was changed there",
        ));
    }
    let platform = super::platform(&last_line(&probe.stdout));
    let target = payload::target(&platform).ok_or_else(|| payload::unsupported(to.target, &platform))?;
    // Built for this hub: this binary's own release, as a remote core.
    let expected = BuildInfo { config: nearshell::expected_remote(&here.config), ..here };
    let entry = payload::choose(entries, target, &expected, &shown)?;
    let file = dir.join(&entry.file);
    let bytes = std::fs::read(&file).ok();
    payload::check(&entry, bytes.as_deref(), &file.display().to_string())?;
    let bytes = bytes.unwrap_or_default();
    let line = payload::install_line(nearshell::BUNDLE, bytes.len() as u64, &entry.build);
    let out = run_raw(paths, to, &line, &bytes)?;
    let landed = payload::landed(to.target, out.status.success(), &last_line(&out.stdout), &last_line(&out.stderr))?;
    if landed.build != entry.build {
        return Err(RpcError::new(
            "install_unverified",
            format!("on {}: the remote core reports {}", to.target, payload::describe(&landed.build)),
            format!("its payload {} says {}; write the payloads again", entry.file, payload::describe(&entry.build)),
        ));
    }
    Ok(landed)
}

/// A core still running an older (or newer) build than this binary would
/// install the remote core of another release, which that core then refuses.
fn same_as_core(paths: &Paths, here: &BuildInfo) -> Result<(), RpcError> {
    let Some(stream) = crate::client::connect_existing(paths) else { return Ok(()) };
    let status = crate::client::call(&stream, "server.status", json!({}))?;
    if status["build"] == json!(here) {
        return Ok(());
    }
    let running = match serde_json::from_value::<BuildInfo>(status["build"].clone()) {
        Ok(b) => payload::describe(&b),
        Err(_) => "no build identity".into(),
    };
    let hint = match cfg!(feature = "managed") {
        true => "restart the core onto this build (Nearshell does when it updates), then retry".into(),
        false => format!("restart the core onto this build (`{CLI_NAME} server restart`), then retry"),
    };
    Err(RpcError::new(
        "core_build_mismatch",
        format!("this core runs {running}, but this {CLI_NAME} is {}", payload::describe(here)),
        hint,
    ))
}

/// The payload directory: `remote_payloads` in this core's config, nothing else.
fn payloads(paths: &Paths) -> Result<PathBuf, RpcError> {
    let Some(dir) = crate::config::load(&paths.config)?.remote_payloads else {
        return Err(RpcError::new(
            "remote_payloads_unset",
            "no remote_payloads directory is configured",
            format!("set remote_payloads = \"<directory>\" in {}", paths.config.display()),
        ));
    };
    let dir = PathBuf::from(dir);
    if !dir.is_absolute() {
        return Err(RpcError::new(
            "remote_payloads_invalid",
            format!("remote_payloads {:?} is not an absolute path", dir.display()),
            format!("fix it in {}", paths.config.display()),
        ));
    }
    Ok(dir)
}
