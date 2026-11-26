//! Other machines: their profiles, and the pipe to the core on each.

use crate::addr::{check_machine_name, check_name};
use crate::branding::COMMAND as CLI_NAME;
use crate::rpc::RpcError;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[cfg(feature = "nearshell")]
mod deliver;
mod install;
pub(crate) mod nearshell;
#[cfg(feature = "nearshell")]
mod payload;
mod refused;
#[cfg(feature = "nearshell")]
pub use deliver::deliver;
pub use install::{ARRIVED, Stage, install};
#[cfg(feature = "nearshell")]
use install::{last_line, platform, run_raw};
pub use nearshell::Refusal;
#[cfg(feature = "nearshell")]
pub use payload::{Landed, describe, install_line, not_expected, parse_expect, summary};
use refused::{host_key, host_key_refused, wants_login};

#[derive(Serialize, Deserialize, JsonSchema, Clone)]
pub struct Machine {
    /// What this core calls it: `rohan` in `rohan/t7`.
    pub name: String,
    /// How it is reached: an ssh target, or (`via`) a command.
    pub target: String,
    /// `target` is a command that runs a shell line there, given as its last
    /// argument, like ssh (a provider's own CLI, ssh with its options).
    #[serde(default)]
    pub via: bool,
    /// Short-lived (a sandbox for a task): clients may show it apart from the
    /// machines that stay. hc treats it like any other.
    #[serde(default)]
    pub temporary: bool,
    /// The hc session there (null: its default).
    pub session: Option<String>,
    /// The other core's permanent id.
    pub id: String,
    /// What that core calls itself (its hostname).
    pub host: String,
    /// Its hc version when last seen.
    pub version: String,
    /// Its build identity when last connected (null: none sent, or an old record).
    #[serde(default)]
    pub build: Option<Box<crate::api::BuildInfo>>,
    /// Last observed systemd logout protection; null for old or unsupported cores.
    #[serde(default)]
    pub keeps_running_after_logout: Option<bool>,
    /// Remote account for loginctl enable-linger, when applicable.
    #[serde(default)]
    pub logout_linger_user: Option<String>,
    /// What its core said it can do when last connected (its hello's `features`).
    #[serde(default)]
    pub features: Vec<String>,
}

impl Machine {
    pub fn reach(&self) -> Reach<'_> {
        Reach { target: &self.target, via: self.via }
    }

    /// Takes in what its `hello` says it is now; whether that changed it.
    pub fn seen(&mut self, hello: &Value) -> bool {
        let build = serde_json::from_value(hello["build"].clone()).ok();
        let version = hello["version"].as_str().unwrap_or(&self.version).to_string();
        let features = features(hello);
        let changed = (&version, &build, &features) != (&self.version, &self.build, &self.features);
        (self.version, self.build, self.features) = (version, build, features);
        changed
    }
}

/// What a core's `hello` says it can do.
pub fn features(hello: &Value) -> Vec<String> {
    hello["features"].as_array().into_iter().flatten().filter_map(|f| f.as_str().map(String::from)).collect()
}

/// How hc gets there: `ssh <target> <line>`, or `<target> <line>` (`via`).
#[derive(Clone, Copy)]
pub struct Reach<'a> {
    pub target: &'a str,
    pub via: bool,
}

impl Reach<'_> {
    /// Says how to add it again, for hints.
    fn add(&self) -> String {
        match self.via {
            true => crate::branding::text(&crate::platform::via_add_hint(self.target)),
            false => format!("{CLI_NAME} machine add {}", self.target),
        }
    }

    /// The command that runs `line` there.
    fn command(
        &self,
        paths: &crate::paths::Paths,
        extra: &[&str],
        line: &str,
    ) -> Result<std::process::Command, RpcError> {
        if self.via {
            return crate::platform::via_command(self.target, line).map_err(|e| unreachable(*self, &e.to_string()));
        }
        let mut cmd = ssh(paths, self.target, extra);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // A pipe from the detached core must not open a console of its own.
            cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        }
        if crate::tuning::var_os("HC_SSH").is_none() {
            cmd.arg("-T");
        }
        cmd.arg(self.target).arg(line);
        Ok(cmd)
    }
}

/// No machine by that name: the error lists the ones there are.
pub fn unknown(machines: &[Machine], name: &str) -> crate::rpc::RpcError {
    let names: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
    let hint = if names.is_empty() {
        format!("no machines yet: {CLI_NAME} machine add <ssh-target>")
    } else {
        format!("machines: {}", names.join(", "))
    };
    crate::rpc::RpcError::new("machine_unknown", format!("unknown machine {name:?}"), hint)
}

/// machines.json as this core keeps it. Routing takes a name or an id, so a
/// name it could take for another machine (shaped like a core id, another
/// machine's id, this core's name or id, a name twice) or no valid name at all
/// is renamed after its host, once, and said: its id still reaches it.
pub fn load(state: &Path, core: &crate::api::CoreInfo) -> io::Result<Vec<Machine>> {
    let mut machines: Vec<Machine> = crate::paths::read_state(&state.join("machines.json"))?;
    let renamed = settle_names(&mut machines, core);
    for (old, new) in &renamed {
        let _ = writeln!(io::stderr(), "core: machine {old:?} is now {new:?}: the old name could be taken for another");
    }
    if !renamed.is_empty() {
        save(state, &machines);
    }
    Ok(machines)
}

fn settle_names(machines: &mut [Machine], core: &crate::api::CoreInfo) -> Vec<(String, String)> {
    let mut renamed = Vec::new();
    for i in 0..machines.len() {
        let fresh = {
            let others = |name: &str, before: bool| {
                name == core.name
                    || name == core.id
                    || machines
                        .iter()
                        .enumerate()
                        .any(|(j, m)| j != i && (m.id == name || ((j < i || !before) && m.name == name)))
            };
            let m = &machines[i];
            if check_machine_name(&m.name).is_ok() && !others(&m.name, true) {
                continue;
            }
            let taken = |name: &str| others(name, false);
            let from = if m.via { m.host.as_str() } else { m.target.as_str() };
            default_name(from, taken).or_else(|| default_name("machine", taken)).expect("a numbered name is free")
        };
        renamed.push((std::mem::replace(&mut machines[i].name, fresh.clone()), fresh));
    }
    renamed
}

pub fn save(state: &Path, machines: &[Machine]) {
    let bytes = serde_json::to_vec_pretty(machines).expect("machines serialize");
    if let Err(e) = crate::paths::write_atomic(&state.join("machines.json"), &bytes) {
        let _ = writeln!(io::stderr(), "core: cannot save machines: {e}");
    }
}

/// `can@rohan:2222`, as people write a port, in the form ssh takes it
/// (`ssh://can@rohan:2222`): ssh reads a plain `rohan:2222` as a host name.
pub fn ssh_target(target: String) -> String {
    let host = target.rsplit_once('@').map_or(target.as_str(), |(_, host)| host);
    match host.split_once(':') {
        Some((h, port))
            if !target.starts_with("ssh://")
                && !h.is_empty()
                && !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            format!("ssh://{target}")
        }
        _ => target,
    }
}

/// A free name from the target's host: `can@rohan.lan` -> `rohan`; IPs stay
/// whole. One shaped like a terminal or a core id gets a number too.
pub fn default_name(target: &str, taken: impl Fn(&str) -> bool) -> Option<String> {
    let t = target.strip_prefix("ssh://").unwrap_or(target);
    let host = t.rsplit_once('@').map_or(t, |(_, host)| host);
    let host = host.split(':').next()?.to_ascii_lowercase();
    let ip = host.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()));
    let mut name = if ip { host } else { host.split('.').next()?.to_string() };
    if name.len() > 32 {
        name = name.chars().take(32).collect::<String>().trim_end_matches('-').into();
    }
    if !crate::terminals::id_shaped(&name) {
        check_name(&name).ok()?;
    }
    let base = name.clone();
    let mut number = 1;
    while crate::terminals::id_shaped(&name) || crate::addr::id_like(&name) || taken(&name) {
        number += 1;
        let suffix = format!("-{number}");
        name = format!("{}{}", base[..base.len().min(32 - suffix.len())].trim_end_matches('-'), suffix);
    }
    Some(name)
}

/// An ssh target goes to ssh as one argument: never an option, never two
/// words. A command (`via`) is one local command line.
pub fn check_target(to: Reach) -> Result<(), RpcError> {
    let target = to.target;
    if to.via {
        if target.trim().is_empty() || target.chars().any(char::is_control) {
            return Err(RpcError::invalid_params(format!("bad command {target:?}: one line, no control characters")));
        }
        return crate::platform::check_via(target)
            .map_err(|e| RpcError::invalid_params(format!("bad command {target:?}: {e}")));
    }
    if target.is_empty() || target.starts_with('-') || target.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(RpcError::invalid_params(format!("bad ssh target {target:?}")));
    }
    Ok(())
}

/// One login per target, shared: a person logs in once in a terminal (codes,
/// passwords), and the core's connections ride that login. Short, private path.
#[cfg(unix)]
fn control_path(paths: &crate::paths::Paths, target: &str) -> std::path::PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    target.hash(&mut h);
    paths.socket.parent().expect("socket has a dir").join(format!("ssh-{:012x}", h.finish() & 0xffff_ffff_ffff))
}

/// `ssh` riding the shared login when there is one, with `extra` options; or
/// `$HC_SSH` in its place (tests), which takes only the target and the remote
/// command. Only `login` creates the shared login: one made by the line (or
/// an install) would end with it, and take every connection riding it along
/// (an update restarting hc there ended its own upload that way).
fn ssh(paths: &crate::paths::Paths, target: &str, extra: &[&str]) -> std::process::Command {
    if let Some(fake) = crate::tuning::var_os("HC_SSH") {
        return std::process::Command::new(fake);
    }
    let mut ssh = std::process::Command::new("ssh");
    #[cfg(unix)]
    {
        let master = if extra.iter().any(|o| o.starts_with("ControlPersist")) { "auto" } else { "no" };
        ssh.arg("-S").arg(control_path(paths, target)).args(["-o", &format!("ControlMaster={master}")]);
    }
    #[cfg(windows)]
    let _ = (paths, target); // Windows OpenSSH has no ControlMaster support.
    for option in extra {
        ssh.args(["-o", option]);
    }
    ssh
}

/// Logs in from this terminal, so ssh can ask for a password or a code. The
/// login stays up for the core's connections (10 minutes once unused). A
/// command (`via`) logs in its own way: nothing to do.
pub fn login(paths: &crate::paths::Paths, to: Reach) -> Result<(), RpcError> {
    check_target(to)?;
    let target = to.target;
    if to.via || cfg!(windows) {
        return Ok(());
    }
    paths.create_dirs().map_err(|e| RpcError::internal(e.to_string()))?;
    // The shared login watches its link too: a dead one ends it, and what rides it.
    let mut cmd = ssh(paths, target, &["BatchMode=no", "ControlPersist=600", KEEPALIVE[0], KEEPALIVE[1]]);
    cmd.env_remove("SSH_ASKPASS").env("SSH_ASKPASS_REQUIRE", "never").arg(target).arg("exit");
    match cmd.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => Err(RpcError::new("needs_login", format!("logging in to {target} failed"), "ssh said why, above")),
        Err(e) => Err(unreachable(to, &format!("cannot run ssh: {e}"))),
    }
}

/// Ends the shared login to `target`, if one is up (a forgotten machine).
pub fn logout(paths: &crate::paths::Paths, to: Reach) {
    let target = to.target;
    if cfg!(unix) && crate::tuning::var_os("HC_SSH").is_none() && !to.via {
        let mut cmd = ssh(paths, target, &[]);
        cmd.args(["-O", "exit", target])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let _ = cmd.status();
    }
}

/// ssh's own watch on a link: one silent for a minute is given up.
const KEEPALIVE: [&str; 2] = ["ServerAliveInterval=15", "ServerAliveCountMax=4"];

/// Targets found to be Windows, so redials go straight.
static WINDOWS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn is_windows(target: &str) -> bool {
    WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).iter().any(|t| t == target)
}

/// Where install.ps1 puts hc, from the home folder an ssh session starts in.
/// Not PATH: sessions riding a shared login keep the PATH from before an
/// install. `.\` reads the same in cmd and PowerShell.
const WINDOWS_HC: &str = r".\AppData\Local\Programs\herdr-core\herdr-core.exe";

/// The pipe to the core there: `herdr-core connect`, found on PATH, else in
/// ~/.local/bin. On Windows, whose shell may be cmd or PowerShell, one line
/// both read alike; the installer put hc on PATH. Never prompts: a login a
/// person must type fails fast.
fn pipe(
    paths: &crate::paths::Paths,
    to: Reach,
    session: Option<&str>,
    windows: bool,
) -> Result<tokio::process::Command, RpcError> {
    let remote = if cfg!(feature = "nearshell") {
        // Its own remote core only, and one core per machine: no session.
        match windows {
            true => nearshell::WINDOWS_PROBE.into(),
            false => nearshell::unix_line(nearshell::BUNDLE, NOT_HERE),
        }
    } else if windows {
        let session = session.map(|s| format!(" --session {s}")).unwrap_or_default();
        format!(
            "cmd /d /c \"if exist {WINDOWS_HC} ({WINDOWS_HC} connect{session}) else (echo {NOT_HERE} 1>&2 & exit /b 127)\""
        )
    } else {
        let session = session.map(|s| format!("HC_SESSION={s} ")).unwrap_or_default();
        format!(
            "sh -c 'p=$(command -v herdr-core) || p=$HOME/.local/bin/herdr-core; [ -x \"$p\" ] || {{ echo \"{NOT_HERE}\" >&2; exit 127; }}; {session}exec \"$p\" connect'"
        )
    };
    let options = ["BatchMode=yes", "ConnectTimeout=10", KEEPALIVE[0], KEEPALIVE[1]];
    let mut cmd = tokio::process::Command::from(to.command(paths, &options, &remote)?);
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    cmd.kill_on_drop(true);
    Ok(cmd)
}

/// What the pipe's line says when there is no hc there (Nearshell's own core
/// for a nearshell build). Plain words: it sits inside quoted shell lines.
/// Seeing it is also what spares a missing core a second dial as Windows.
const NOT_HERE: &str = if cfg!(feature = "nearshell") {
    "the Nearshell remote core is not installed here"
} else {
    "herdr-core is not installed here"
};

fn unreachable(to: Reach, why: &str) -> RpcError {
    let target = to.target;
    let hint = match to.via {
        true => format!("check that `{target} 'echo ok'` prints ok without asking anything"),
        false => format!("check that `ssh {target}` logs in without a password prompt (keys or an agent)"),
    };
    RpcError::new("unreachable", format!("cannot reach {target}: {why}"), hint)
}

/// An open pipe to the core there, past its hello.
pub struct Dialed {
    pub child: tokio::process::Child,
    pub stdin: tokio::process::ChildStdin,
    /// Framed from here on (when dialed framed); may hold bytes already read.
    pub stdout: BufReader<tokio::process::ChildStdout>,
    pub stderr: tokio::process::ChildStderr,
    pub hello: Value,
}

/// Dials, says hello and hangs up: the other core's hello, or why not.
pub async fn hello(paths: &crate::paths::Paths, to: Reach<'_>, session: Option<&str>) -> Result<Value, RpcError> {
    Ok(dial(paths, to, session, json!({})).await?.hello)
}

/// Dials and says hello, with `hello` added to its params (`framed`, who we are).
pub async fn dial(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: Option<&str>,
    hello: Value,
) -> Result<Dialed, RpcError> {
    let mut probe_windows = false;
    let dialed = match dial_once(paths, to, session, hello.clone(), is_windows(to.target), &mut probe_windows).await {
        Err(e) if probe_windows => match dial_once(paths, to, session, hello, true, &mut probe_windows).await {
            Err(_) if !is_windows(to.target) => Err(e),
            result => result,
        },
        other => other,
    }?;
    // Nearshell: only the same build of its own remote core may be used.
    if cfg!(feature = "nearshell") {
        nearshell::admit(&crate::api::build_info(), &dialed.hello, to.target, &to.add())?;
    }
    Ok(dialed)
}

async fn dial_once(
    paths: &crate::paths::Paths,
    to: Reach<'_>,
    session: Option<&str>,
    hello: Value,
    windows: bool,
    probe_windows: &mut bool,
) -> Result<Dialed, RpcError> {
    let target = to.target;
    let mut child =
        pipe(paths, to, session, windows)?.spawn().map_err(|e| unreachable(to, &format!("cannot run it: {e}")))?;
    let (mut stdin, stdout, mut stderr) =
        (child.stdin.take().unwrap(), child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let mut params = json!({"generation": crate::api::GENERATION});
    params.as_object_mut().unwrap().extend(hello.as_object().cloned().unwrap_or_default());
    let req = json!({"jsonrpc": "2.0", "id": 1, "method": "hello", "params": params});
    let _ = stdin.write_all(format!("{req}\n").as_bytes()).await;
    // A login may print a banner first: the reply is the first line that is one.
    let mut lines = BufReader::new(stdout);
    let reply = async {
        let mut line = String::new();
        loop {
            line.clear();
            match lines.read_line(&mut line).await {
                Ok(1..) => match serde_json::from_str::<Value>(&line) {
                    Ok(reply) if reply["id"] == 1 => return Some(reply),
                    _ => continue,
                },
                _ => return None,
            }
        }
    };
    let reply = tokio::time::timeout(Duration::from_secs(20), reply).await;
    let reply = match reply {
        Err(_) => return Err(unreachable(to, "no answer within 20 s")),
        Ok(Some(reply)) => reply,
        Ok(None) => {
            // Bounded: something it started may hold stderr open, and a dial
            // that never ends leaves the machine "connecting" for good.
            let mut err = String::new();
            let status = tokio::time::timeout(Duration::from_secs(5), async {
                let _ = stderr.read_to_string(&mut err).await;
                child.wait().await.ok().and_then(|s| s.code())
            })
            .await
            .ok()
            .flatten();
            if !windows {
                // Windows may find Git's sh but fail in its shim or mangle
                // the script's quoting. Only our marker proves the script ran.
                *probe_windows = status != Some(255) && !err.contains(NOT_HERE) && !wants_login(&err);
            }
            let mark = if cfg!(feature = "nearshell") { nearshell::WINDOWS_SEEN } else { NOT_HERE };
            let missing_windows = windows && err.contains(mark);
            if missing_windows && !is_windows(target) {
                WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
            }
            if missing_windows && cfg!(feature = "nearshell") {
                return Err(nearshell::windows_unsupported(target));
            }
            // Some connection wrappers collapse all remote failures to exit code 1.
            if status == Some(127) || err.contains(NOT_HERE) {
                let hint = match cfg!(feature = "nearshell") {
                    true => "add this machine from Nearshell, which installs its remote core there".into(),
                    false => format!("in a terminal, `{}` offers to copy this {CLI_NAME} there", to.add()),
                };
                return Err(RpcError::new("hc_missing", format!("{CLI_NAME} is not installed on {target}"), hint));
            }
            if let Some(kind) = host_key(&err) {
                return Err(host_key_refused(to, kind));
            }
            if wants_login(&err) {
                if cfg!(windows) && !to.via {
                    return Err(RpcError::new(
                        "needs_login",
                        format!("SSH authentication to {target} failed"),
                        format!("check that `ssh {target}` logs in without a password prompt (keys or an agent)"),
                    ));
                }
                return Err(RpcError::new(
                    "needs_login",
                    format!("{target} wants a login typed by a person (a password or a code)"),
                    format!("run `{}` in a terminal", to.add()),
                ));
            }
            let why = err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("the connection closed");
            return Err(unreachable(to, why.trim()));
        }
    };
    if windows && !is_windows(target) {
        WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
    }
    match reply.get("error") {
        Some(e) if e["data"]["kind"] == "unsupported_generation" => Err(RpcError::new(
            "incompatible",
            format!("{CLI_NAME} on {target} speaks another protocol generation"),
            match cfg!(feature = "nearshell") {
                true => Refusal::Incompatible.hint(""),
                false => format!("in a terminal, `{}` offers to update it", to.add()),
            },
        )),
        Some(e) => Err(RpcError::from_json(e)),
        None => Ok(Dialed { child, stdin, stdout: lines, stderr, hello: reply["result"].clone() }),
    }
}

#[cfg(test)]
mod tests {
    use super::install::platform;
    use super::*;

    #[test]
    fn a_name_comes_from_the_target_host() {
        let default_name = |target| super::default_name(target, |_| false);
        assert_eq!(default_name("rohan").as_deref(), Some("rohan"));
        assert_eq!(default_name("can@Rohan.lan").as_deref(), Some("rohan"));
        assert_eq!(default_name("ssh://can@rohan:2222").as_deref(), Some("rohan"));
        assert_eq!(ssh_target("can@10.0.0.7:4396".into()), "ssh://can@10.0.0.7:4396");
        assert_eq!(ssh_target("rohan:2222".into()), "ssh://rohan:2222");
        for unchanged in ["rohan", "can@rohan", "ssh://can@rohan:2222", "can@fe80::1", "[::1]:22x", "can@rohan:"] {
            assert_eq!(ssh_target(unchanged.into()), unchanged);
        }
        assert_eq!(default_name("root@10.0.0.7").as_deref(), Some("10.0.0.7"));
        assert_eq!(default_name("[::1]"), None);
        assert_eq!(default_name(&format!("{}-extra.example.com", "a".repeat(31))), Some("a".repeat(31)));
        assert_eq!(super::default_name("rohan", |name| name == "rohan"), Some("rohan-2".into()));
        let ssh = |target| Reach { target, via: false };
        assert!(check_target(ssh("-oProxyCommand=x")).is_err() && check_target(ssh("a b")).is_err());
        assert!(check_target(Reach { target: "fly ssh console -C", via: true }).is_ok());
        assert_eq!(platform("Darwin arm64"), "macos aarch64");
        assert_eq!(platform("Linux x86_64"), "linux x86_64");
        assert_eq!(platform("MINGW64_NT-10.0 x86_64"), "windows x86_64");
        assert_eq!(platform("MSYS_NT-10.0 x86_64"), "windows x86_64");
    }

    #[test]
    fn id_shaped_hosts_get_a_free_name() {
        for host in ["t440s", "t14", &format!("t1{}", "b".repeat(30)), "0123456789abcdef0123456789abcdef"] {
            let expected = format!("{}-2", &host[..host.len().min(30)]);
            assert_eq!(default_name(host, |_| false), Some(expected));
            assert_eq!(
                default_name(host, |name| name.ends_with("-2")),
                Some(format!("{}-3", &host[..host.len().min(30)]))
            );
        }
        assert_eq!(default_name("t14!", |_| false), None);
    }

    #[test]
    #[cfg(feature = "nearshell")]
    fn a_nearshell_build_dials_its_own_core_by_path_and_never_a_session() {
        let paths = crate::paths::Paths {
            socket: "/nonexistent/c.sock".into(),
            holder_socket: "/nonexistent/c.holder.sock".into(),
            state: "/nonexistent/state".into(),
            config: "/nonexistent/config.toml".into(),
        };
        let args = |to: Reach, windows| -> Vec<String> {
            let cmd = pipe(&paths, to, Some("s1"), windows).unwrap();
            cmd.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        let line = nearshell::unix_line(nearshell::BUNDLE, NOT_HERE);
        assert!(line.contains(&format!("/.local/share/nearshell/{}/bin/nhc", nearshell::BUNDLE)));
        assert!(line.contains(&format!("echo \"{NOT_HERE}\" >&2; exit 127")), "{line}");
        let ssh = args(Reach { target: "box", via: false }, false);
        assert_eq!(ssh.last(), Some(&line));
        assert!(!ssh.iter().any(|a| a.contains("s1") || a.contains("--session")), "{ssh:?}");
        // --via hands the line over as one argument, expanded only there.
        let via = args(Reach { target: "fly ssh console -C", via: true }, false);
        assert_eq!(via, ["-c", "fly ssh console -C \"$1\"", "sh", &line]);
        assert_eq!(args(Reach { target: "box", via: false }, true).last().unwrap(), nearshell::WINDOWS_PROBE);
    }

    #[test]
    fn a_loaded_name_that_routing_could_mistake_is_renamed_after_its_host() {
        let core = crate::api::CoreInfo { id: "c0".into(), name: "here".into() };
        let m = |name: &str, target: &str, id: &str| {
            serde_json::from_value::<Machine>(json!({"name": name, "target": target, "session": null, "id": id,
                "host": "h", "version": "v"}))
            .unwrap()
        };
        let hex = "0123456789abcdef0123456789abcdef";
        let mut machines = vec![
            m("box", "box", "c1"),
            m(hex, "can@rohan.lan", "c2"),
            m("c1", "gondor", "c3"),
            m("box", "box.lan", "c4"),
            m("here", "here", "c5"),
            m("fine", "x", hex),
        ];
        let renamed = settle_names(&mut machines, &core);
        let names: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["box", "rohan", "gondor", "box-2", "here-2", "fine"]);
        assert_eq!(renamed.len(), 4);
        assert_eq!(renamed[0], (hex.to_string(), "rohan".to_string()));
        assert!(settle_names(&mut machines, &core).is_empty(), "once");

        // As a core starts: renamed, and saved so.
        let dir = std::env::temp_dir().join(format!("hc-machines-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let saved = json!([m(hex, "can@rohan.lan", "c2"), m("box", "box", "c1")]);
        std::fs::write(dir.join("machines.json"), saved.to_string()).unwrap();
        let loaded = load(&dir, &core).unwrap();
        let again: Vec<Machine> = serde_json::from_slice(&std::fs::read(dir.join("machines.json")).unwrap()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(loaded.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["rohan", "box"]);
        assert_eq!(again.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["rohan", "box"]);
    }

    #[test]
    fn an_older_machine_record_still_loads_and_takes_in_its_build() {
        let old = json!([{"name":"box","target":"box","session":null,"id":"c1","host":"box","version":"0.1.21"}]);
        let mut machines: Vec<Machine> = serde_json::from_value(old).unwrap();
        assert!(machines[0].build.is_none());
        let build = json!({"commit":"c1","patchset":"p","releaseSeq":3,"config":"dev:x:remote"});
        assert!(machines[0].seen(&json!({"version":"0.1.22","build":build})));
        assert!(!machines[0].seen(&json!({"version":"0.1.22","build":build})));
        let saved = serde_json::to_value(&machines[0]).unwrap();
        assert_eq!((&saved["version"], &saved["build"]), (&json!("0.1.22"), &build));
    }

    #[test]
    #[cfg(not(feature = "nearshell"))]
    fn a_windows_box_gets_one_line_cmd_and_powershell_read_alike() {
        let paths = crate::paths::Paths::from_env().unwrap();
        let cmd = pipe(&paths, Reach { target: "winbox", via: false }, Some("s1"), true).unwrap();
        let line = cmd.as_std().get_args().last().unwrap().to_string_lossy().into_owned();
        assert_eq!(
            line,
            format!(
                "cmd /d /c \"if exist {WINDOWS_HC} ({WINDOWS_HC} connect --session s1) else (echo {NOT_HERE} 1>&2 & exit /b 127)\""
            )
        );
    }
}
