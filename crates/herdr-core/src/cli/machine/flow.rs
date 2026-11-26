//! Adding, logging in to and updating a machine as a person does it in a
//! terminal: asked what is needed, shown each step as it starts and ends, and
//! told at the end what the hub reached and, when it is not up, what to do.

use super::*;
use crate::machines::Stage;
use std::io::Write;

/// The lines a person reads: a step is said when it starts (`what...`) and
/// ended on the same line (` done`). Only the line of the step still running
/// is ever rewritten (bytes sent so far), so nothing floods the scrollback.
#[derive(Default)]
pub(super) struct Steps {
    /// The running step's words, and what its line shows after them.
    open: Option<(String, String)>,
}

impl Steps {
    pub fn start(&mut self, what: &str) -> String {
        let before = if self.open.is_some() { "\n" } else { "" };
        self.open = Some((what.into(), String::new()));
        format!("{before}{what}...")
    }

    /// The running step's line, rewritten in place, when it shows something new.
    pub fn show(&mut self, now: &str) -> Option<String> {
        let (what, shown) = self.open.as_mut()?;
        if shown == now {
            return None;
        }
        let pad = " ".repeat(shown.chars().count().saturating_sub(now.chars().count()));
        *shown = now.into();
        Some(format!("\r{what}... {now}{pad}"))
    }

    pub fn done(&mut self, result: &str) -> String {
        match self.open.take() {
            None => String::new(),
            Some((_, shown)) if shown.is_empty() => format!(" {result}\n"),
            Some((what, shown)) => {
                let pad = " ".repeat(shown.chars().count().saturating_sub(result.chars().count()));
                format!("\r{what}... {result}{pad}\n")
            }
        }
    }

    /// A line of its own, for a step that a program talks through (ssh asking).
    pub fn say(&mut self, line: &str) -> String {
        format!("{}{line}\n", self.done("done"))
    }

    pub fn running(&self) -> Option<&str> {
        self.open.as_ref().map(|(what, _)| what.as_str())
    }
}

/// `3.2 of 7.5 MB` (`300 of 512 kB` for a small one).
pub(super) fn megabytes(sent: u64, total: u64) -> String {
    match total < 1_000_000 {
        true => format!("{} of {} kB", sent.div_ceil(1000), total.div_ceil(1000)),
        false => format!("{:.1} of {:.1} MB", sent as f64 / 1e6, total as f64 / 1e6),
    }
}

/// Steps shown on stderr to a person (`on`), nothing otherwise. A step left
/// running when it is dropped ended in an error, which comes next.
pub(super) struct Progress {
    on: bool,
    target: String,
    steps: Steps,
    total: u64,
}

fn sending() -> String {
    match cfg!(feature = "nearshell") {
        true => "sending the remote core".into(),
        false => format!("sending this {CLI_NAME}"),
    }
}

impl Progress {
    pub fn new(on: bool, target: &str) -> Progress {
        Progress { on, target: target.into(), steps: Steps::default(), total: 0 }
    }

    /// Whether a person sees these steps.
    pub fn on(&self) -> bool {
        self.on
    }

    fn write(&self, text: &str) {
        if self.on && !text.is_empty() {
            let mut err = std::io::stderr();
            let _ = err.write_all(text.as_bytes());
            let _ = err.flush();
        }
    }

    pub fn start(&mut self, what: &str) {
        let text = self.steps.start(what);
        self.write(&text);
    }

    pub fn done(&mut self, result: &str) {
        let text = self.steps.done(result);
        self.write(&text);
    }

    /// Logs in as `login` does, said around it: ssh may ask the person meanwhile.
    pub fn login(&mut self, to: Reach, login: impl FnOnce() -> Result<(), RpcError>) -> Result<(), RpcError> {
        let says = !to.via && !cfg!(windows);
        if says {
            let text = self.steps.say(&format!("logging in to {} (ssh may ask for a password or a code)", to.target));
            self.write(&text);
        }
        login()?;
        if says {
            let text = self.steps.say("logged in");
            self.write(&text);
        }
        Ok(())
    }

    /// How an install there is getting on.
    pub fn stage(&mut self, stage: Stage) {
        match stage {
            Stage::Probing => self.start(&format!("finding out what {} runs", self.target)),
            Stage::Platform(platform) => self.done(platform),
            Stage::Sent(sent, total) => {
                let sending = sending();
                if self.steps.running() != Some(sending.as_str()) {
                    self.start(&sending);
                }
                self.total = total;
                if let Some(text) = self.steps.show(&megabytes(sent, total)) {
                    self.write(&text);
                }
            }
            Stage::Arrived => {
                self.done(&format!("{}, arrived", megabytes(self.total, self.total)));
                self.start("starting it there");
            }
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        let text = self.steps.done("failed");
        self.write(&text);
    }
}

/// Asks a person for the ssh target, then for a name (the target's host unless
/// they type another), each again until it is one this hub takes.
pub(super) fn ask(p: &mut MachineAddParams, call: Call) -> Result<(), RpcError> {
    let target = ask_until("ssh target (user@host, user@host:port, or a Host from your ssh config): ", target_answer)?;
    let listed = call("machine.list", json!({}))?;
    let taken: Vec<String> = listed["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| [m["name"].as_str(), m["id"].as_str()])
        .flatten()
        .map(String::from)
        .collect();
    let default = crate::machines::default_name(&target, |name| taken.iter().any(|t| t == name));
    let prompt = match &default {
        Some(name) => format!("name for it here [{name}]: "),
        None => "name for it here: ".into(),
    };
    let name = ask_until(&prompt, |answer| name_answer(answer, default.as_deref(), &taken))?;
    (p.target, p.name) = (Some(target), Some(name));
    Ok(())
}

fn ask_until(prompt: &str, check: impl Fn(&str) -> Result<String, String>) -> Result<String, RpcError> {
    loop {
        eprint!("{prompt}");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).map_err(|e| RpcError::internal(e.to_string()))? == 0 {
            eprintln!();
            return Err(RpcError::new("cancelled", "nothing was added", ""));
        }
        match check(answer.trim()) {
            Ok(value) => return Ok(value),
            Err(why) => eprintln!("  {why}"),
        }
    }
}

/// An ssh target as `machine add` takes it.
pub(super) fn target_answer(answer: &str) -> Result<String, String> {
    if answer.is_empty() {
        return Err("type how ssh reaches it, like can@10.0.0.7 or a Host from ~/.ssh/config".into());
    }
    let target = crate::machines::ssh_target(answer.into());
    crate::machines::check_target(Reach { target: &target, via: false }).map_err(|e| e.message)?;
    Ok(target)
}

/// A name for a machine here: nothing typed takes `default`.
pub(super) fn name_answer(answer: &str, default: Option<&str>, taken: &[String]) -> Result<String, String> {
    let name = match (answer, default) {
        ("", Some(default)) => default,
        ("", None) => return Err("type a name for it".into()),
        (answer, _) => answer,
    };
    crate::addr::check_machine_name(name).map_err(|e| e.message)?;
    match taken.iter().any(|t| t == name) {
        true => Err(format!("{name:?} is already a machine here")),
        false => Ok(name.into()),
    }
}

/// The machine `key` (its name or id) as the hub lists it.
pub(super) fn listed(call: Call, key: &str) -> Result<Value, RpcError> {
    let r = call("machine.list", json!({}))?;
    let found = r["machines"].as_array().into_iter().flatten().find(|m| m["name"] == key || m["id"] == key);
    found.cloned().ok_or_else(|| {
        RpcError::new("machine_unknown", format!("unknown machine {key:?}"), format!("see `{CLI_NAME} machine ls`"))
    })
}

/// The machine once its line is past dialing and, if up, caught up (at most
/// 30 s): what the hub reached, not what a step here assumed.
pub(super) fn reached(call: Call, id: &str) -> Result<Value, RpcError> {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let m = listed(call, id)?;
        let waiting = m["state"] == "connecting" || (m["state"] == "connected" && m["synced"] != true);
        if !waiting || std::time::Instant::now() > end {
            return Ok(m);
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// `done` happened here, but the hub's line to `m` is not up: what is the
/// matter, and what a person does about it.
pub(super) fn not_reached(m: &Value, done: &str) -> RpcError {
    let s = |k: &str| m[k].as_str().unwrap_or("?").to_string();
    let (name, target, state) = (s("name"), s("target"), s("state"));
    let why = m["why"].as_str().map(|w| format!(" ({w})")).unwrap_or_default();
    let (what, todo) = match state.as_str() {
        "needs_login" => (
            format!("the hub could not log in to {target}"),
            format!(
                "run `{CLI_NAME} machine login {name}` in a terminal; if it soon needs one again, the hub's own ssh \
                 cannot log in there (it has not this terminal's keys or ssh agent)"
            ),
        ),
        "host_key_unknown" => (
            format!("the hub does not know {target}'s host key"),
            format!("`{CLI_NAME} machine login {name}` in a terminal shows it to check and accept"),
        ),
        "host_key_changed" => (
            format!("{target}'s host key is not the one known here"),
            format!(
                "find out why it changed; if the new key is right, remove the old one from known_hosts yourself, \
                 then `{CLI_NAME} machine login {name}`"
            ),
        ),
        "different_core" => (
            format!("another core answers at {target}{why}"),
            format!("the terminals this hub knew there are gone: `{CLI_NAME} machine rm {name}`, then add it again"),
        ),
        "needs_update" => (format!("{name} runs an older core"), format!("`{CLI_NAME} machine update {name}`")),
        "unreachable" => {
            (format!("the hub cannot reach {target}{why}"), format!("it keeps trying: `{CLI_NAME} machine ls`"))
        }
        "connecting" => {
            (format!("the hub is still connecting to {name}"), format!("`{CLI_NAME} machine ls` shows when it is up"))
        }
        held => match crate::machines::Refusal::named(held) {
            Some(r) => (format!("{name} is {held}{why}"), r.hint(&format!("{CLI_NAME} machine update {name}"))),
            None => (format!("{name} is {held}{why}"), format!("see `{CLI_NAME} machine ls`")),
        },
    };
    RpcError::new(&state, format!("{done}, but {what}"), todo)
}

/// `--hold`: in a terminal, the outcome stays up until Enter.
pub(in crate::cli) fn hold() {
    if std::io::stdin().is_terminal() {
        eprint!("\npress Enter to close ");
        let _ = std::io::stderr().flush();
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_step_is_said_when_it_starts_and_ended_on_its_own_line() {
        let mut s = Steps::default();
        assert_eq!(s.start("finding out what far runs"), "finding out what far runs...");
        assert_eq!(s.done("linux aarch64"), " linux aarch64\n");
        assert_eq!(s.done("again"), "", "nothing running");
        assert_eq!(s.start("sending"), "sending...");
        assert_eq!(s.show(&megabytes(1_200_000, 7_500_000)).unwrap(), "\rsending... 1.2 of 7.5 MB");
        assert_eq!(s.show(&megabytes(1_240_000, 7_500_000)), None, "the same on screen: nothing written");
        assert_eq!(s.show("7.5 of 7.5 MB").unwrap(), "\rsending... 7.5 of 7.5 MB");
        let pad = " ".repeat("7.5 of 7.5 MB".len() - "7.5 MB".len());
        assert_eq!(s.done("7.5 MB"), format!("\rsending... 7.5 MB{pad}\n"), "a shorter end covers the longer line");
        // Starting another ends one left running (an answer ssh needs comes on its own line).
        s.start("waiting");
        assert_eq!(s.say("logged in"), " done\nlogged in\n");
        assert_eq!(s.start("a"), "a...");
        assert_eq!(s.start("b"), "\nb...");
        assert_eq!(megabytes(1, 512_000), "1 of 512 kB");
        for line in ["finding out what far runs... linux aarch64", "sending the remote core... 7.5 of 7.5 MB, arrived"]
        {
            assert!(line.len() < 80, "{line}");
        }
    }

    #[test]
    fn an_answer_is_checked_as_add_checks_it() {
        assert_eq!(target_answer("can@rohan:2222").unwrap(), "ssh://can@rohan:2222");
        assert_eq!(target_answer("vps").unwrap(), "vps");
        for bad in ["", "-oProxyCommand=x", "a b"] {
            assert!(target_answer(bad).is_err(), "{bad}");
        }
        let taken = ["box".to_string(), "0123456789abcdef0123456789abcdef".to_string()];
        assert_eq!(name_answer("", Some("rohan"), &taken).unwrap(), "rohan");
        assert_eq!(name_answer("gondor", Some("rohan"), &taken).unwrap(), "gondor");
        assert!(name_answer("", None, &taken).is_err());
        for bad in ["box", "t7", "Box", "fedcba9876543210fedcba9876543210"] {
            assert!(name_answer(bad, Some("rohan"), &taken).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_line_that_is_not_up_says_what_to_do() {
        let m = |state: &str| json!({"name": "box", "target": "can@box", "state": state, "why": "w"});
        let e = not_reached(&m("needs_login"), "logged in to can@box here");
        assert_eq!(e.kind, "needs_login");
        assert_eq!(e.message, "logged in to can@box here, but the hub could not log in to can@box");
        assert!(e.hint.contains(&format!("{CLI_NAME} machine login box")), "{}", e.hint);
        assert!(not_reached(&m("different_core"), "x").hint.contains("machine rm box"));
        assert!(not_reached(&m("host_key_changed"), "x").hint.contains("known_hosts yourself"));
        assert!(not_reached(&m("machine_newer"), "x").hint.contains("update Nearshell"));
        assert_eq!(not_reached(&m("unreachable"), "x").message, "x, but the hub cannot reach can@box (w)");
    }
}
