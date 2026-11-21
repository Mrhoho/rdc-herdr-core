//! Explicit owner lifecycle, separate from upstream's automatic-start/update path.
use super::*;
use crate::owner;
use std::path::Path;
mod shutdown;

fn params(token: &str) -> Value {
    json!({"owner_token":token})
}
fn authorize_server(paths: &Paths, token: &str) -> Result<(), RpcError> {
    owner::authorize(paths, Some(token))?;
    if let Some(stream) = client::connect_existing(paths) {
        client::call(&stream, "server.authorize", params(token))?;
    }
    Ok(())
}

pub(super) fn command(paths: &Paths, cmd: &Cmd, token: Option<&str>) -> Result<Option<Out>, RpcError> {
    if !matches!(
        cmd,
        Cmd::Update { .. } | Cmd::Server { action: ServerCmd::Start | ServerCmd::Stop | ServerCmd::Restart { .. } }
    ) {
        return Ok(None);
    }
    let _operation = owner::operation(paths, token)?;
    if matches!(cmd, Cmd::Server { action: ServerCmd::Restart { fresh: true } }) {
        return Err(RpcError::invalid_params("managed restart environment comes from owner"));
    }
    let token = token.ok_or_else(owner::denied)?;
    authorize_server(paths, token)?;
    let result = match cmd {
        Cmd::Update { from: Some(from), .. } => update(paths, from, token)?,
        Cmd::Update { from: None, .. } => {
            crate::branding::upstream_distribution()?;
            unreachable!()
        }
        Cmd::Server { action: ServerCmd::Stop } => {
            let result = match client::connect_existing(paths) {
                Some(stream) => shutdown::request(paths, &stream, "server.stop", params(token))?,
                None => {
                    shutdown::require_absent(paths)?;
                    json!({"stopped":false})
                }
            };
            Out { human: "managed core stopped".into(), json: result }
        }
        Cmd::Server { action } => {
            if matches!(action, ServerCmd::Restart { .. }) {
                restart(paths, token)?;
            }
            let stream = owner::start(paths, token)?;
            let status = client::call(&stream, "server.start", params(token))?;
            Out { human: "managed core running".into(), json: status }
        }
        _ => unreachable!(),
    };
    Ok(Some(result))
}

fn restart(paths: &Paths, token: &str) -> Result<bool, RpcError> {
    if let Some(stream) = client::connect_existing(paths) {
        shutdown::request(paths, &stream, "server.restart", params(token))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn build(path: &Path) -> Result<Value, RpcError> {
    let output = std::process::Command::new(path)
        .args(["build-info", "--json"])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|_| invalid_build())?;
    if !output.status.success() {
        return Err(invalid_build());
    }
    let value: Value = serde_json::from_slice(&output.stdout).map_err(|_| invalid_build())?;
    if value["config"] != crate::api::build_info().config
        || value["releaseSeq"].as_u64().is_none()
        || ["commit", "patchset"].iter().any(|k| value[k].as_str().is_none_or(str::is_empty))
    {
        return Err(invalid_build());
    }
    Ok(value)
}
fn invalid_build() -> RpcError {
    RpcError::new("managed_build", "managed build identity is incompatible", "use the owning Nearshell distribution")
}
fn forward(candidate: &Value, previous: &Value) -> bool {
    match (candidate["releaseSeq"].as_u64(), previous["releaseSeq"].as_u64()) {
        (Some(a), Some(b)) => a > b || (a == b && candidate == previous),
        _ => false,
    }
}
fn update(paths: &Paths, from: &Path, token: &str) -> Result<Out, RpcError> {
    let dest = std::env::current_exe().and_then(|p| p.canonicalize()).map_err(|_| invalid_build())?;
    // Never rewrite a bundle, a build output, or the user's independent hc.
    if dest != owner::root(paths)?.canonicalize().map_err(|_| invalid_build())?.join("bin/nhc") {
        return Err(invalid_build());
    }
    let candidate = build(from)?;
    let disk = build(&dest)?;
    if !forward(&candidate, &disk) {
        return Err(invalid_build());
    }
    if let Some(stream) = client::connect_existing(paths) {
        let running = client::call(&stream, "server.status", json!({}))?;
        if running["build"]["config"] != candidate["config"] || !forward(&candidate, &running["build"]) {
            return Err(invalid_build());
        }
    }
    authorize_server(paths, token)?;
    // Our shared management.lock lease fences verifier rotation. Recheck
    // inside the staging callback, immediately before the atomic rename.
    let failure = std::cell::RefCell::new(None);
    crate::platform::swap_in(from, &dest, |staged| {
        let result = (|| {
            if build(staged)? != candidate || build(&dest)? != disk {
                return Err(invalid_build());
            }
            authorize_server(paths, token)
        })();
        match result {
            Ok(()) => true,
            Err(e) => {
                *failure.borrow_mut() = Some(e);
                false
            }
        }
    })
    .map_err(|_| failure.borrow_mut().take().unwrap_or_else(invalid_build))?;
    let restarted = restart(paths, token)?;
    let stream = owner::start(paths, token)?;
    let status = client::call(&stream, "server.start", params(token))?;
    if status["build"] != candidate {
        return Err(invalid_build());
    }
    Ok(Out {
        human: "managed core updated".into(),
        json: json!({"updated":true,"restarted":restarted,"build":candidate,"pid":status["pid"]}),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn managed_updates_never_downgrade_or_reinterpret_an_equal_sequence() {
        let current = json!({"releaseSeq":4,"commit":"a"});
        assert!(forward(&current, &current));
        assert!(forward(&json!({"releaseSeq":5,"commit":"b"}), &current));
        assert!(!forward(&json!({"releaseSeq":3,"commit":"b"}), &current));
        assert!(!forward(&json!({"releaseSeq":4,"commit":"b"}), &current));
    }
}
