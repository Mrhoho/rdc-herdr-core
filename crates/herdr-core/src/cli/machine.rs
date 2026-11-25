//! `hc machine …`: adding, listing, logging in to, updating other machines.

use super::*;
use crate::api::{MachineAddParams, MachineParams, MachineRenameParams};
use crate::branding::COMMAND as CLI_NAME;
use crate::machines::Reach;

#[derive(Subcommand)]
pub(super) enum MachineCmd {
    /// Add a machine you reach with ssh: hc machine add vps.
    Add(MachineAddParams),
    /// List the machines added here.
    Ls,
    /// Log in again (a password or code) after the machine says it needs one.
    /// On Windows: reconnect once its SSH keys work.
    Login(MachineParams),
    /// Copy this hc there and restart hc there (its terminals keep running).
    Update(MachineParams),
    /// Give a machine a new name here.
    Rename(MachineRenameParams),
    /// Forget a machine (its terminals keep running there).
    Rm(MachineParams),
}

fn linger_user(m: &Value) -> String {
    let user = m["logout_linger_user"].as_str().unwrap_or("<user>");
    if user.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c)) {
        user.to_owned()
    } else {
        format!("'{}'", user.replace('\'', "'\\''"))
    }
}

/// Said once a machine is reached: its programs stop at logout, and how to fix it.
fn logout_warning(m: &Value) -> String {
    let name = m["name"].as_str().unwrap_or_default();
    match m["keeps_running_after_logout"] == false {
        true => format!(
            "\nwarning: programs on {name} stop when your last login there ends. To fix it, run this on {name} once: sudo loginctl enable-linger -- {}",
            linger_user(m)
        ),
        false => String::new(),
    }
}

type Call<'a> = &'a dyn Fn(&str, Value) -> Result<Value, RpcError>;

/// Copies this hc there, then adds the machine.
#[cfg(not(feature = "nearshell"))]
fn install_and_add(paths: &Paths, reach: Reach, p: &MachineAddParams, call: Call) -> Result<Value, RpcError> {
    let version = crate::machines::install(paths, reach, p.session.as_deref())?;
    eprintln!("{} now has {version}", reach.target);
    call("machine.add", to(p))
}

/// Puts this hub's remote core there, then adds the machine: it must answer
/// as the build installed.
#[cfg(feature = "nearshell")]
fn install_and_add(paths: &Paths, reach: Reach, p: &MachineAddParams, call: Call) -> Result<Value, RpcError> {
    let landed = crate::machines::deliver(paths, reach)?;
    eprintln!("{}: {}", reach.target, crate::machines::summary(&landed));
    let m = call("machine.add", to(p))?;
    match m["build"] == json!(landed.build) {
        true => Ok(m),
        false => Err(unverified(m["name"].as_str().unwrap_or_default(), &format!("answers as {}", m["build"]))),
    }
}

/// Copies this hc there and restarts hc there.
#[cfg(not(feature = "nearshell"))]
fn update(paths: &Paths, name: &str, m: &Value, _: Call) -> Result<Out, RpcError> {
    let version = crate::machines::install(paths, reach_of(m), m["session"].as_str())?;
    let human = format!("{name} now runs {version}; its terminals kept running");
    Ok(Out { json: json!({"machine": name, "version": version}), human })
}

/// Puts this hub's remote core there (restarting the one running there), then
/// dials it until it answers as that build. A machine held for a newer, a
/// conflicting or an incompatible core is left as it is.
#[cfg(feature = "nearshell")]
fn update(paths: &Paths, name: &str, m: &Value, call: Call) -> Result<Out, RpcError> {
    use crate::machines::Refusal;
    let state = m["state"].as_str().unwrap_or_default();
    if let Some(refusal) = Refusal::named(state).filter(|r| *r != Refusal::NeedsUpdate) {
        return Err(refusal.refused(name, m["why"].as_str().unwrap_or(state)));
    }
    let landed = crate::machines::deliver(paths, reach_of(m))?;
    let build = json!(landed.build);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let view = loop {
        let view = call("machine.connect", json!({"machine": name}))?;
        let state = view["state"].as_str().unwrap_or_default();
        if state == "connected" && view["build"] == build {
            break view;
        }
        // Held again, or a login only a person can give: it will not come back by waiting.
        if Refusal::named(state).is_some() || state == "needs_login" || std::time::Instant::now() > end {
            let why = view["why"].as_str().map(|w| format!(": {w}")).unwrap_or_default();
            return Err(unverified(name, &format!("is {state}{why}")));
        }
        // Its old line may not have dropped yet, or it is being dialled.
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    let mut human = format!("{name}: {}", crate::machines::summary(&landed));
    if landed.restarted {
        human.push_str("; its terminals kept running");
    }
    let mut json = serde_json::to_value(&landed).expect("result serializes");
    (json["machine"], json["state"]) = (name.into(), view["state"].clone());
    Ok(Out { human: human + &logout_warning(&view), json })
}

/// Installed there, but the hub does not reach that build.
#[cfg(feature = "nearshell")]
fn unverified(name: &str, what: &str) -> RpcError {
    RpcError::new(
        "install_unverified",
        format!("{name} {what} after its install"),
        format!("see `{CLI_NAME} machine ls`; `{CLI_NAME} machine update {name}` tries again"),
    )
}

/// Which Nearshell build runs there, for `machine ls`: ` build 5 1a2b3c4d5e6f p2`.
fn build_there(build: &Value) -> String {
    match (cfg!(feature = "nearshell"), build["commit"].as_str()) {
        (true, Some(commit)) => format!(
            " build {} {} {}",
            build["releaseSeq"],
            commit.get(..12).unwrap_or(commit),
            build["patchset"].as_str().unwrap_or("?")
        ),
        (true, None) => " build unknown".into(),
        (false, _) => String::new(),
    }
}

/// A person who can answer ssh's questions (a password, a code, a host key).
fn at_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// How a listed machine is reached.
fn reach_of(m: &Value) -> Reach<'_> {
    Reach { target: m["target"].as_str().unwrap_or_default(), via: m["via"] == true }
}

pub(super) fn machine(
    paths: &Paths,
    action: MachineCmd,
    call: impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Out, RpcError> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    Ok(match action {
        MachineCmd::Add(p) => {
            let target =
                p.via.clone().or_else(|| p.target.clone().map(crate::machines::ssh_target)).unwrap_or_default();
            let reach = crate::machines::Reach { target: &target, via: p.via.is_some() };
            if cfg!(feature = "nearshell") && p.session.is_some() {
                return Err(RpcError::new(
                    "usage",
                    "a Nearshell machine runs one remote core: there is no session to pick",
                    "leave out --session",
                ));
            }
            if at_terminal() {
                crate::machines::login(paths, reach)?;
            }
            // Nearshell installs only where its own remote core is missing or
            // older; a newer, conflicting or incompatible one is reported as is.
            let installs: [&str; 2] = if cfg!(feature = "nearshell") {
                ["hc_missing", "needs_update"]
            } else {
                ["hc_missing", "incompatible"]
            };
            let m = match call("machine.add", to(&p)) {
                Err(e) if (p.install || at_terminal()) && installs.contains(&e.kind.as_str()) => {
                    let what = match (e.kind.as_str(), cfg!(feature = "nearshell")) {
                        ("hc_missing", false) => &format!("has no {CLI_NAME}"),
                        (_, false) => &format!("has an {CLI_NAME} too old to talk to"),
                        ("hc_missing", true) => "has no Nearshell remote core",
                        (_, true) => "runs an older Nearshell remote core",
                    };
                    if !p.install {
                        match cfg!(feature = "nearshell") {
                            true => eprint!("{target} {what}. Install this Mac's remote core there? [Y/n] "),
                            false => eprint!(
                                "{target} {what}. Copy this {CLI_NAME} ({}) there? [Y/n] ",
                                env!("CARGO_PKG_VERSION")
                            ),
                        }
                        let mut answer = String::new();
                        std::io::stdin().read_line(&mut answer).map_err(|err| RpcError::internal(err.to_string()))?;
                        if answer.trim().to_lowercase().starts_with('n') {
                            return Err(e);
                        }
                    }
                    install_and_add(paths, reach, &p, &call)?
                }
                other => other?,
            };
            let human =
                format!("added {}: {} answers, {CLI_NAME} {}", s(&m["name"]), s(&m["target"]), s(&m["version"]));
            Out { human: human + &logout_warning(&m), json: m }
        }
        MachineCmd::Ls => {
            let r = call("machine.list", json!({}))?;
            let rows: Vec<String> = r["machines"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|m| {
                    let session = m["session"].as_str().map(|x| format!(" session {x}")).unwrap_or_default();
                    let state = line_state(m);
                    let logout = if m["keeps_running_after_logout"] == false { "  ! stops at logout" } else { "" };
                    format!(
                        "{:<16} {state}{logout}  \x1b[2m{}{session} \u{b7} {CLI_NAME} {}{}\x1b[22m",
                        s(&m["name"]),
                        s(&m["target"]),
                        s(&m["version"]),
                        build_there(&m["build"])
                    )
                })
                .collect();
            let human = if rows.is_empty() {
                format!("no machines yet: {CLI_NAME} machine add <ssh-target>")
            } else {
                rows.join("\n")
            };
            Out { json: r, human }
        }
        MachineCmd::Login(p) => {
            if !at_terminal() {
                return Err(RpcError::new(
                    "usage",
                    "logging in needs a terminal",
                    "a person types the password or code",
                ));
            }
            let r = call("machine.list", json!({}))?;
            let found = r["machines"].as_array().into_iter().flatten().find(|m| m["name"] == p.machine.as_str());
            let Some(m) = found else {
                return Err(RpcError::new(
                    "machine_unknown",
                    format!("unknown machine {:?}", p.machine),
                    format!("see `{CLI_NAME} machine ls`"),
                ));
            };
            crate::machines::login(paths, reach_of(m))?;
            let m = call("machine.connect", to(&p))?;
            let human = format!("logged in to {}: {}", p.machine, line_state(&m));
            Out { json: m, human }
        }
        MachineCmd::Update(p) => {
            let r = call("machine.list", json!({}))?;
            let found =
                r["machines"].as_array().into_iter().flatten().find(|m| m["name"] == p.machine.as_str()).cloned();
            let Some(m) = found else {
                return Err(RpcError::new(
                    "machine_unknown",
                    format!("unknown machine {:?}", p.machine),
                    format!("see `{CLI_NAME} machine ls`"),
                ));
            };
            update(paths, &p.machine, &m, &call)?
        }
        MachineCmd::Rename(p) => {
            let m = call("machine.rename", to(&p))?;
            let human = format!("{} is now {}", p.machine, s(&m["name"]));
            Out { json: m, human }
        }
        MachineCmd::Rm(p) => {
            let r = call("machine.rm", to(&p))?;
            let human = format!("forgot {}; its terminals keep running there", s(&r["removed"]));
            Out { json: r, human }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logout_warning_is_human_only_and_requires_observed_false() {
        let paths = Paths {
            socket: "unused".into(),
            holder_socket: "unused".into(),
            state: "unused".into(),
            config: "unused".into(),
        };
        for state in [json!(false), json!(true), Value::Null] {
            let remote = json!({"name":"cloud", "target":"user@cloud", "version":"test",
                "keeps_running_after_logout":state, "logout_linger_user":"-remote-user"});
            let params = serde_json::from_value(json!({"via":"ssh user@cloud", "name":"cloud"})).unwrap();
            let added = machine(&paths, MachineCmd::Add(params), |_, _| Ok(remote.clone())).unwrap();
            assert_eq!(added.json, remote);
            assert_eq!(added.human.contains("warning:"), state == false);
            if state == false {
                assert!(added.human.contains("sudo loginctl enable-linger -- -remote-user"));
            }
            let listed = machine(&paths, MachineCmd::Ls, |_, _| Ok(json!({"machines":[remote]}))).unwrap();
            assert_eq!(listed.human.contains("stops at logout"), state == false);
        }
    }

    #[test]
    #[cfg(feature = "nearshell")]
    fn machine_ls_shows_the_build_there_and_why_a_held_one_waits() {
        let paths = Paths { socket: "u".into(), holder_socket: "u".into(), state: "u".into(), config: "u".into() };
        let build = json!({"commit":"0123456789abcdef0123","patchset":"p2","releaseSeq":5,"config":"dev:x:remote"});
        let row = |m: Value| machine(&paths, MachineCmd::Ls, |_, _| Ok(json!({"machines":[m]}))).unwrap().human;
        let m = |state: &str, build: Value| json!({"name":"box","target":"box","version":"0.1.22","build":build,"state":state,"why":"box runs a newer one"});
        assert!(row(m("connected", build.clone())).contains("nhc 0.1.22 build 5 0123456789ab p2"));
        assert!(row(m("connected", Value::Null)).contains("nhc 0.1.22 build unknown"));
        assert!(
            row(m("machine_newer", build.clone())).contains("runs a newer remote core: update Nearshell on this Mac")
        );
        let conflict = row(m("build_conflict", build));
        assert!(conflict.contains("build_conflict (box runs a newer one): not handled automatically"), "{conflict}");
    }
}
