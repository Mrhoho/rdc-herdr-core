//! The remote build puts a copy of itself in place (hidden `nhc install`, run
//! by a hub's install line on the copy it just sent to `<root>/bin`): one
//! install at a time under `<root>/install.lock`, never an older release or
//! another build of the same one, the running core restarted onto it and
//! checked. A first install, an update and a broken copy there all take this
//! path; upstream's `update --from` is refused in this build.

use crate::api::BuildInfo;
use crate::machines::{Landed, Refusal};
use crate::paths::Paths;
use crate::rpc::RpcError;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Whether the candidate replaces what is installed (None: nothing, or a
/// copy that does not run). The dial's rule: never older, never another
/// build of the same release; the same build stays as it is.
fn replaces(candidate: &BuildInfo, installed: Option<&BuildInfo>) -> Result<bool, Refusal> {
    match installed.map(|i| crate::machines::nearshell::gate(candidate, Some(i))) {
        None | Some(Err(Refusal::NeedsUpdate)) => Ok(true),
        Some(Ok(())) => Ok(false),
        Some(Err(refusal)) => Err(refusal),
    }
}

/// A temp copy an earlier install left: `<staged prefix><pid>` of a shell that
/// is gone, untouched for a minute. One still arriving is neither.
fn stale(name: &str, own: &str, gone: impl Fn(u32) -> bool, age: Duration) -> bool {
    let Some(pid) = name.strip_prefix(crate::branding::STAGED_PREFIX).and_then(|p| p.parse().ok()) else {
        return false;
    };
    name != own && age >= Duration::from_secs(60) && gone(pid)
}

/// Refused by the build installed or running here.
fn refused(refusal: Refusal, there: &BuildInfo, candidate: &BuildInfo) -> RpcError {
    let short = crate::machines::nearshell::short;
    let message = match refusal {
        Refusal::MachineNewer => format!(
            "a newer Nearshell remote core is here (release {}; this copy: release {})",
            there.release_seq, candidate.release_seq
        ),
        _ if there.config != candidate.config => {
            format!("the remote core here is built as {}, not {}", there.config, candidate.config)
        }
        _ => format!(
            "another build of release {} is here ({}; this copy: {})",
            there.release_seq,
            short(there),
            short(candidate)
        ),
    };
    RpcError::new(refusal.state(), message, refusal.hint(""))
}

fn failed(why: String) -> RpcError {
    RpcError::new("install_failed", why, "retry; nothing is replaced unless the install says so")
}

/// `nhc install --from <copy> --expect <build>`: what the hub reads back.
pub fn land(paths: &Paths, from: &Path, expect: &str) -> Result<Value, RpcError> {
    let bin = paths.root().join("bin");
    let staged = staged(from, &bin)?;
    let landed = land_staged(paths, &staged, &bin, expect);
    // Never left behind; once in place this name is gone already.
    let _ = std::fs::remove_file(&staged);
    Ok(serde_json::to_value(landed?).expect("result serializes"))
}

/// `from` must be this very binary, sent into `<root>/bin` under a temp name.
fn staged(from: &Path, bin: &Path) -> Result<PathBuf, RpcError> {
    let exe = std::env::current_exe().and_then(|e| e.canonicalize()).ok();
    let from = from.canonicalize().ok();
    let named =
        |p: &Path| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(crate::branding::STAGED_PREFIX));
    match (exe, from, bin.canonicalize().ok()) {
        (Some(exe), Some(from), Some(bin)) if exe == from && from.parent() == Some(&bin) && named(&from) => Ok(from),
        _ => Err(RpcError::invalid_params(format!(
            "install --from takes this nhc itself, sent to {} under a temp name",
            bin.display()
        ))),
    }
}

fn land_staged(paths: &Paths, staged: &Path, bin: &Path, expect: &str) -> Result<Landed, RpcError> {
    let candidate = crate::api::build_info();
    let expected = crate::machines::parse_expect(expect)
        .ok_or_else(|| RpcError::invalid_params("the --expect value is not a build identity"))?;
    if expected != candidate {
        return Err(crate::machines::not_expected("this copy", &candidate, &expected));
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(paths.root().join("install.lock"))
        .map_err(|e| failed(format!("cannot open install.lock: {e}")))?;
    // Held to the end: a second install waits, then finds this one in place.
    crate::platform::lock(&lock).map_err(|e| failed(format!("cannot lock install.lock: {e}")))?;
    sweep(bin, staged);
    // The running core counts too: a newer one stays even when its file is gone.
    let running = crate::client::connect_existing(paths)
        .map(|stream| crate::client::call(&stream, "server.status", json!({})))
        .transpose()?
        .map(|status| serde_json::from_value::<BuildInfo>(status["build"].clone()).ok());
    if let Some(Some(core)) = &running {
        replaces(&candidate, Some(core)).map_err(|r| refused(r, core, &candidate))?;
    }
    let dest = bin.join("nhc");
    let was = build_of(&dest);
    let replaced = replaces(&candidate, was.as_ref()).map_err(|r| refused(r, was.as_ref().unwrap(), &candidate))?;
    if replaced {
        std::fs::rename(staged, &dest).map_err(|e| failed(format!("cannot put it at {}: {e}", dest.display())))?;
    }
    let restarted = match running {
        Some(core) if core.as_ref() != Some(&candidate) => restart_onto(paths, &dest, &candidate)?,
        _ => false,
    };
    drop(lock);
    Ok(Landed { build: candidate, was, replaced, restarted })
}

/// Removes temp copies of failed earlier installs, never one still arriving.
fn sweep(bin: &Path, own: &Path) {
    let own = own.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    for entry in std::fs::read_dir(bin).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let age = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if stale(&name, &own, crate::platform::process_gone, age.unwrap_or_default()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// What the installed binary says it is; None when it is missing or does not run.
fn build_of(bin: &Path) -> Option<BuildInfo> {
    let out =
        Command::new(bin).args(["build-info", "--json"]).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    out.status.success().then(|| serde_json::from_slice(&out.stdout).ok()).flatten()
}

/// A running core on an older build restarts onto the installed one (its
/// terminals stay with the holder) and must come back as the candidate. The
/// new core starts from `<root>/bin/nhc`: this process's own path is gone.
fn restart_onto(paths: &Paths, dest: &Path, candidate: &BuildInfo) -> Result<bool, RpcError> {
    let Some(stream) = crate::client::connect_existing(paths) else { return Ok(false) };
    crate::client::call(&stream, "server.restart", json!({}))?;
    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let _ = std::io::Read::read_to_end(&mut &stream, &mut Vec::new());
    let out = Command::new(dest).args(["--json", "server", "status"]).stdin(Stdio::null()).output();
    let status: Option<Value> =
        out.ok().filter(|o| o.status.success()).and_then(|o| serde_json::from_slice(&o.stdout).ok());
    let running = status.as_ref().and_then(|s| serde_json::from_value::<BuildInfo>(s["build"].clone()).ok());
    if running.as_ref() != Some(candidate) {
        let now = running.as_ref().map_or("no core".into(), crate::machines::describe);
        return Err(RpcError::new(
            "install_unverified",
            format!("installed, but after its restart the core here runs {now}"),
            "check `nhc server status` there; an update retries the restart",
        ));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(seq: u64, commit: &str, config: &str) -> BuildInfo {
        BuildInfo { commit: commit.into(), patchset: "p".into(), release_seq: seq, config: config.into() }
    }

    #[test]
    fn a_copy_replaces_only_nothing_a_broken_copy_or_an_older_release() {
        let config = "dev:com.e:remote:scratch-test";
        let candidate = build(5, "c5", config);
        assert_eq!(replaces(&candidate, None), Ok(true));
        assert_eq!(replaces(&candidate, Some(&build(4, "c4", config))), Ok(true));
        assert_eq!(replaces(&candidate, Some(&candidate)), Ok(false));
        assert_eq!(replaces(&candidate, Some(&build(6, "c6", config))), Err(Refusal::MachineNewer));
        assert_eq!(replaces(&candidate, Some(&build(5, "c9", config))), Err(Refusal::BuildConflict));
        // Built for another hub, even an older release: not ours to replace.
        let other = build(4, "c4", "dev:com.e:remote");
        assert_eq!(replaces(&candidate, Some(&other)), Err(Refusal::BuildConflict));
        let e = refused(Refusal::BuildConflict, &other, &candidate);
        assert_eq!(e.message, "the remote core here is built as dev:com.e:remote, not dev:com.e:remote:scratch-test");
        let e = refused(Refusal::MachineNewer, &build(6, "c6", config), &candidate);
        assert_eq!((e.kind.as_str(), e.hint.as_str()), ("machine_newer", "update Nearshell on this Mac"));
    }

    #[test]
    fn only_old_copies_of_shells_that_are_gone_are_stale() {
        let prefix = crate::branding::STAGED_PREFIX;
        let (old, new) = (Duration::from_secs(600), Duration::from_secs(5));
        let gone = |pid| pid == 7;
        assert!(stale(&format!("{prefix}7"), &format!("{prefix}8"), gone, old));
        assert!(!stale(&format!("{prefix}7"), &format!("{prefix}7"), gone, old), "its own");
        assert!(!stale(&format!("{prefix}9"), &format!("{prefix}8"), gone, old), "a live shell's");
        assert!(!stale(&format!("{prefix}7"), &format!("{prefix}8"), gone, new), "still arriving");
        for other in ["nhc", "notes", &format!("{prefix}x")] {
            assert!(!stale(other, &format!("{prefix}8"), gone, old), "{other}");
        }
    }
}
