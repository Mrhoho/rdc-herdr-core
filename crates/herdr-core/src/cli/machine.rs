//! `hc machine …`: adding, listing, logging in to, updating other machines.
//! In a terminal, adding, logging in and updating are a person's flow (see
//! `flow`); piped or with --json they print what they always printed.

use super::*;
use crate::api::{MachineAddParams, MachineParams, MachineRenameParams};
use crate::branding::COMMAND as CLI_NAME;
use crate::machines::Reach;

mod flow;
pub(super) use flow::hold;
use flow::{Progress, listed, not_reached, reached};

#[derive(Subcommand)]
pub(super) enum MachineCmd {
    /// Add a machine you reach with ssh: hc machine add vps. In a terminal,
    /// without a target, it asks for one and for a name.
    Add(MachineAddParams),
    /// List the machines added here.
    Ls,
    /// Log in again (a password or code) after the machine says it needs one.
    /// On Windows: reconnect once its SSH keys work.
    Login {
        #[command(flatten)]
        p: MachineParams,
        /// After the outcome, wait for Enter before exiting (same exit status).
        /// Only in a terminal: without one it exits at once.
        #[arg(long)]
        hold: bool,
    },
    /// Copy this hc there and restart hc there (its terminals keep running).
    Update {
        #[command(flatten)]
        p: MachineParams,
        /// After the outcome, wait for Enter before exiting (same exit status).
        /// Only in a terminal: without one it exits at once.
        #[arg(long)]
        hold: bool,
    },
    /// Give a machine a new name here.
    Rename(MachineRenameParams),
    /// Forget a machine (its terminals keep running there).
    Rm(MachineParams),
}

impl MachineCmd {
    /// `--hold`: its outcome stays up until Enter.
    pub(super) fn holds(&self) -> bool {
        match self {
            MachineCmd::Add(p) => p.hold,
            MachineCmd::Login { hold, .. } | MachineCmd::Update { hold, .. } => *hold,
            _ => false,
        }
    }
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

/// Copies this hc there, then adds the machine; also what it did there.
#[cfg(not(feature = "nearshell"))]
fn install_and_add(
    paths: &Paths,
    reach: Reach,
    p: &MachineAddParams,
    call: Call,
    steps: &mut Progress,
) -> Result<(Value, String), RpcError> {
    let version = crate::machines::install(paths, reach, p.session.as_deref(), &mut |s| steps.stage(s))?;
    steps.done("done");
    if !steps.on() {
        eprintln!("{} now has {version}", reach.target);
    }
    steps.start("adding it here");
    let m = call("machine.add", to(p))?;
    steps.done("done");
    Ok((m, format!("installed {version}")))
}

/// Puts this hub's remote core there, then adds the machine: it must answer
/// as the build installed. Also what it did there.
#[cfg(feature = "nearshell")]
fn install_and_add(
    paths: &Paths,
    reach: Reach,
    p: &MachineAddParams,
    call: Call,
    steps: &mut Progress,
) -> Result<(Value, String), RpcError> {
    let landed = crate::machines::deliver(paths, reach, &mut |s| steps.stage(s))?;
    steps.done("done");
    let summary = crate::machines::summary(&landed);
    if !steps.on() {
        eprintln!("{}: {summary}", reach.target);
    }
    steps.start("adding it here");
    let m = call("machine.add", to(p))?;
    match m["build"] == json!(landed.build) {
        true => {
            steps.done("done");
            Ok((m, summary))
        }
        false => Err(unverified(m["name"].as_str().unwrap_or_default(), &format!("answers as {}", m["build"]))),
    }
}

/// Nothing to ask a person for: the usage error clap gives `machine add`
/// without a target, as it always gave it (`None`: a person is asked).
pub(super) fn target_missing(cmd: &Option<Cmd>, json: bool, args: &[std::ffi::OsString]) -> Option<clap::Error> {
    let Some(Cmd::Machine { action: MachineCmd::Add(p) }) = cmd else { return None };
    if p.target.is_some() || p.via.is_some() || (!json && at_terminal()) {
        return None;
    }
    use clap::CommandFactory;
    let strict = branded_command(Cli::command()).mut_subcommand("machine", |m| {
        m.mut_subcommand("add", |a| a.mut_arg("target", |t| t.required_unless_present("via")))
    });
    strict.try_get_matches_from(std::env::args_os().take(1).chain(args.iter().cloned())).err()
}

/// Adds a machine: asked for in a terminal, installed there when it has to be.
fn add(paths: &Paths, mut p: MachineAddParams, person: bool, call: Call) -> Result<Out, RpcError> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    if p.target.is_none() && p.via.is_none() {
        if !person {
            return Err(RpcError::new(
                "usage",
                "the following required arguments were not provided: <TARGET>",
                format!("see `{CLI_NAME} --help`"),
            ));
        }
        flow::ask(&mut p, call)?;
    }
    let target = p.via.clone().or_else(|| p.target.clone().map(crate::machines::ssh_target)).unwrap_or_default();
    let reach = crate::machines::Reach { target: &target, via: p.via.is_some() };
    if cfg!(feature = "nearshell") && p.session.is_some() {
        return Err(RpcError::new(
            "usage",
            "a Nearshell machine runs one remote core: there is no session to pick",
            "leave out --session",
        ));
    }
    let mut steps = Progress::new(person, &target);
    if at_terminal() {
        steps.login(reach, || crate::machines::login(paths, reach))?;
    }
    // Nearshell installs only where its own remote core is missing or
    // older; a newer, conflicting or incompatible one is reported as is.
    let installs: [&str; 2] =
        if cfg!(feature = "nearshell") { ["hc_missing", "needs_update"] } else { ["hc_missing", "incompatible"] };
    steps.start(if cfg!(feature = "nearshell") { "looking for the remote core there" } else { "looking for hc there" });
    let (m, did) = match call("machine.add", to(&p)) {
        Err(e) if (p.install || at_terminal()) && installs.contains(&e.kind.as_str()) => {
            steps.done(if e.kind == "hc_missing" { "not installed" } else { "an older one" });
            let what = match (e.kind.as_str(), cfg!(feature = "nearshell")) {
                ("hc_missing", false) => &format!("has no {CLI_NAME}"),
                (_, false) => &format!("has an {CLI_NAME} too old to talk to"),
                ("hc_missing", true) => "has no Nearshell remote core",
                (_, true) => "runs an older Nearshell remote core",
            };
            if !p.install {
                match cfg!(feature = "nearshell") {
                    true => eprint!("{target} {what}. Install this Mac's remote core there? [Y/n] "),
                    false => {
                        eprint!("{target} {what}. Copy this {CLI_NAME} ({}) there? [Y/n] ", env!("CARGO_PKG_VERSION"))
                    }
                }
                let mut answer = String::new();
                std::io::stdin().read_line(&mut answer).map_err(|err| RpcError::internal(err.to_string()))?;
                if answer.trim().to_lowercase().starts_with('n') {
                    return Err(e);
                }
            }
            install_and_add(paths, reach, &p, call, &mut steps)?
        }
        other => {
            let m = other?;
            steps.done("found");
            let did = format!("{} answers, {CLI_NAME} {}", s(&m["target"]), s(&m["version"]));
            (m, did)
        }
    };
    if !person {
        let human = format!("added {}: {} answers, {CLI_NAME} {}", s(&m["name"]), s(&m["target"]), s(&m["version"]));
        return Ok(Out { human: human + &logout_warning(&m), json: m });
    }
    steps.start("waiting for the line");
    let view = reached(call, &s(&m["id"]))?;
    steps.done(&line_state(&view));
    if view["state"] != "connected" {
        return Err(not_reached(&view, &format!("added {}", s(&m["name"]))));
    }
    let human = format!("added {}: {did}; {}", s(&view["name"]), line_state(&view));
    Ok(Out { human: human + &logout_warning(&view), json: view })
}

/// Logs in from this terminal, then has the hub dial again at once. In a
/// terminal it ends with what the hub reached.
fn login(paths: &Paths, key: &str, person: bool, call: Call) -> Result<Out, RpcError> {
    if !at_terminal() {
        return Err(RpcError::new("usage", "logging in needs a terminal", "a person types the password or code"));
    }
    let m = listed(call, key)?;
    let (name, id) = (m["name"].as_str().unwrap_or_default(), m["id"].as_str().unwrap_or_default());
    let mut steps = Progress::new(person, m["target"].as_str().unwrap_or_default());
    steps.login(reach_of(&m), || crate::machines::login(paths, reach_of(&m)))?;
    let view = call("machine.connect", json!({"machine": id}))?;
    if !person {
        return Ok(Out { human: format!("logged in to {name}: {}", line_state(&view)), json: view });
    }
    steps.start("waiting for the line");
    let view = reached(call, id)?;
    steps.done(&line_state(&view));
    if view["state"] != "connected" {
        return Err(not_reached(&view, &format!("logged in to {} here", reach_of(&m).target)));
    }
    Ok(Out {
        human: format!("logged in to {}: {}", view["name"].as_str().unwrap_or(name), line_state(&view)),
        json: view,
    })
}

/// Copies this hc there and restarts hc there.
#[cfg(not(feature = "nearshell"))]
fn update(paths: &Paths, m: &Value, call: Call, steps: &mut Progress) -> Result<Out, RpcError> {
    let name = m["name"].as_str().unwrap_or_default();
    let version = crate::machines::install(paths, reach_of(m), m["session"].as_str(), &mut |s| steps.stage(s))?;
    steps.done("done");
    let human = format!("{name} now runs {version}; its terminals kept running");
    if steps.on() {
        steps.start("waiting for the line");
        let view = reached(call, m["id"].as_str().unwrap_or_default())?;
        steps.done(&line_state(&view));
        if view["state"] != "connected" {
            return Err(not_reached(&view, &format!("{name} now runs {version}")));
        }
    }
    Ok(Out { json: json!({"machine": name, "version": version}), human })
}

/// Puts this hub's remote core there (restarting the one running there), then
/// dials it until it answers as that build. A machine held for a newer, a
/// conflicting or an incompatible core is left as it is.
#[cfg(feature = "nearshell")]
fn update(paths: &Paths, m: &Value, call: Call, steps: &mut Progress) -> Result<Out, RpcError> {
    use crate::machines::Refusal;
    let (name, id) = (m["name"].as_str().unwrap_or_default(), m["id"].as_str().unwrap_or_default());
    let state = m["state"].as_str().unwrap_or_default();
    if let Some(refusal) = Refusal::named(state).filter(|r| *r != Refusal::NeedsUpdate) {
        return Err(refusal.refused(name, m["why"].as_str().unwrap_or(state)));
    }
    let landed = crate::machines::deliver(paths, reach_of(m), &mut |s| steps.stage(s))?;
    steps.done("done");
    let build = json!(landed.build);
    let mut human = format!("{name}: {}", crate::machines::summary(&landed));
    steps.start("waiting for the line");
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let view = loop {
        let view = call("machine.connect", json!({"machine": id}))?;
        let state = view["state"].as_str().unwrap_or_default();
        if state == "connected" && view["build"] == build {
            break view;
        }
        // Held again, or what only a person can settle: it will not come back by waiting.
        let person = ["needs_login", "host_key_unknown", "host_key_changed", "different_core"].contains(&state);
        if Refusal::named(state).is_some() || person || std::time::Instant::now() > end {
            steps.done(&line_state(&view));
            if steps.on() {
                return Err(not_reached(&view, &human));
            }
            let why = view["why"].as_str().map(|w| format!(": {w}")).unwrap_or_default();
            return Err(unverified(name, &format!("is {state}{why}")));
        }
        // Its old line may not have dropped yet, or it is being dialled.
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    steps.done(&line_state(&view));
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

/// `json`: the output is JSON (asked for, or piped); a person in a terminal
/// gets the flows of `flow` otherwise.
pub(super) fn machine(
    paths: &Paths,
    action: MachineCmd,
    json: bool,
    call: impl Fn(&str, Value) -> Result<Value, RpcError>,
) -> Result<Out, RpcError> {
    let s = |v: &Value| v.as_str().unwrap_or_default().to_string();
    let person = !json && at_terminal();
    Ok(match action {
        MachineCmd::Add(p) => add(paths, p, person, &call)?,
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
        MachineCmd::Login { p, .. } => login(paths, &p.machine, person, &call)?,
        MachineCmd::Update { p, .. } => {
            let m = listed(&call, &p.machine)?;
            let mut steps = Progress::new(person, m["target"].as_str().unwrap_or_default());
            if person {
                steps.login(reach_of(&m), || crate::machines::login(paths, reach_of(&m)))?;
            }
            update(paths, &m, &call, &mut steps)?
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
            let added = machine(&paths, MachineCmd::Add(params), true, |_, _| Ok(remote.clone())).unwrap();
            assert_eq!(added.json, remote);
            assert_eq!(added.human.contains("warning:"), state == false);
            if state == false {
                assert!(added.human.contains("sudo loginctl enable-linger -- -remote-user"));
            }
            let listed = machine(&paths, MachineCmd::Ls, true, |_, _| Ok(json!({"machines":[remote]}))).unwrap();
            assert_eq!(listed.human.contains("stops at logout"), state == false);
        }
    }

    #[test]
    fn machine_ls_says_what_a_person_must_do_and_when_a_line_is_catching_up() {
        let paths = Paths { socket: "u".into(), holder_socket: "u".into(), state: "u".into(), config: "u".into() };
        let row = |m: Value| machine(&paths, MachineCmd::Ls, true, |_, _| Ok(json!({"machines":[m]}))).unwrap().human;
        let m = |state: &str, synced: bool| {
            let mut m = json!({"name":"box","target":"box","version":"0.1.22","state":state,"synced":synced});
            if state != "connected" {
                m["since_ms"] = crate::runtime::now_ms().into();
                m["why"] = "a different core answers there (\"0f\")".into();
            }
            m
        };
        assert!(row(m("connected", true)).contains(" connected  \x1b[2m"), "{}", row(m("connected", true)));
        assert!(row(m("connected", false)).contains("connected, catching up"));
        assert!(
            row(m("host_key_unknown", false))
                .contains(&format!("host key unknown for 0s: {CLI_NAME} machine login box"))
        );
        assert!(row(m("host_key_changed", false)).contains("host key changed for 0s: not the one known here"));
        let other = row(m("different_core", false));
        assert!(
            other.contains(&format!(
                "another core answers for 0s (a different core answers there (\"0f\")): {CLI_NAME} machine rm box"
            )),
            "{other}"
        );
    }

    #[test]
    #[cfg(feature = "nearshell")]
    fn machine_ls_shows_the_build_there_and_why_a_held_one_waits() {
        let paths = Paths { socket: "u".into(), holder_socket: "u".into(), state: "u".into(), config: "u".into() };
        let build = json!({"commit":"0123456789abcdef0123","patchset":"p2","releaseSeq":5,"config":"dev:x:remote"});
        let row = |m: Value| machine(&paths, MachineCmd::Ls, true, |_, _| Ok(json!({"machines":[m]}))).unwrap().human;
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
