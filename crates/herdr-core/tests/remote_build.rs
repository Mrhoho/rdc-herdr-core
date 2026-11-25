//! The remote build (`--features nearshell-test`, Linux): upstream's lifecycle
//! in a private root that no HOME, XDG_* or HC_* moves, beside a user's own hc.
#![cfg(all(target_os = "linux", feature = "nearshell-test", not(feature = "managed")))]
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

#[path = "common/guardian.rs"]
mod guardian;

const NHC: &str = env!("CARGO_BIN_EXE_nhc");

/// A remote root, plus a HOME and XDG dirs that must stay untouched.
struct Remote {
    root: PathBuf,
    guardian: guardian::Guardian,
}

impl Remote {
    fn new() -> Remote {
        let root = PathBuf::from(format!("/var/tmp/nr-{}", &herdr_core::platform::random_hex()[..12]));
        std::fs::create_dir_all(root.join("home")).unwrap();
        let env = json!({"PATH": "/usr/bin:/bin", "HOME": root.join("home"), "NEARSHELL_HC_TEST_ROOT": root});
        let guardian = guardian::Guardian::with_command(&root, &[NHC, "server", "stop"], env);
        Remote { root, guardian }
    }

    /// A shell elsewhere in the same account: its own HOME and XDG, none of them ours.
    fn cmd(&self) -> Command {
        let mut cmd = Command::new(NHC);
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NEARSHELL_HC_TEST_ROOT", &self.root)
            .env("NEARSHELL_TEST_SESSION_ID", &self.guardian.nonce)
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("xdg/config"))
            .env("XDG_STATE_HOME", self.root.join("xdg/state"))
            .env("XDG_DATA_HOME", self.root.join("xdg/data"))
            // Never the real user manager: a test core starts the plain way.
            .env("XDG_RUNTIME_DIR", self.root.join("xdg/run"))
            .stdin(Stdio::null());
        cmd
    }

    /// `--json` first: after a program's argv it would be the program's.
    fn json(&self, mut cmd: Command, args: &[&str]) -> Value {
        value(cmd.arg("--json").args(args).output().unwrap())
    }

    fn nhc(&self, args: &[&str]) -> Value {
        self.json(self.cmd(), args)
    }

    /// `run --bg` names the terminal; its record has the program's pid.
    fn pid(&self, run: &Value) -> Value {
        self.nhc(&["get", run["address"].as_str().unwrap()])["pid"].clone()
    }

    fn rpc(&self, method: &str, params: Value) -> Value {
        let stream = herdr_core::platform::connect(&self.root.join("run/c.sock")).unwrap();
        herdr_core::client::call(&stream, method, params).unwrap()
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let clean = self.guardian.finish();
        if clean {
            let _ = std::fs::remove_dir_all(&self.root);
        }
        assert!(clean || std::thread::panicking(), "test_guardian_cleanup_unconfirmed");
    }
}

fn value(out: Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn error(out: Output, kind: &str) -> Value {
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let v: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["error"]["kind"], kind, "{v}");
    v
}

fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(found) = f() {
            return found;
        }
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `env` output a program left in `file`, once whole.
fn env_file(file: &Path) -> BTreeMap<String, String> {
    eventually("program environment", || {
        let text = std::fs::read_to_string(file).ok().filter(|t| t.ends_with("DONE=1\n"))?;
        Some(text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect())
    })
}

fn hc_keys(env: &BTreeMap<String, String>) -> Vec<&str> {
    env.keys().map(String::as_str).filter(|k| k.starts_with("HC_")).collect()
}

/// HC_* fields of a running process, from /proc.
fn process_hc(pid: i64) -> Vec<String> {
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let mut keys: Vec<String> = environ
        .split(|b| *b == 0)
        .filter_map(|e| std::str::from_utf8(e).ok()?.split_once('=').map(|(k, _)| k.to_string()))
        .filter(|k| k.starts_with("HC_"))
        .collect();
    keys.sort();
    keys
}

#[test]
fn connect_starts_the_core_in_its_root_whatever_home_and_xdg_say() {
    let r = Remote::new();
    let mut connect = r.cmd().arg("connect").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    {
        use std::io::{BufRead, Write};
        let mut stdin = connect.stdin.take().unwrap();
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":1,"method":"hello","params":{{}}}}"#).unwrap();
        let mut line = String::new();
        std::io::BufReader::new(connect.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let hello: Value = serde_json::from_str(&line).unwrap();
        let build = r.nhc(&["build-info"]);
        assert_eq!(hello["result"]["build"], build, "{line}");
        assert!(build["config"].as_str().unwrap().ends_with(":remote:scratch-test"), "{build}");
    }
    assert!(connect.wait().unwrap().success());
    assert!(r.root.join("run/c.sock").exists());
    assert!(r.root.join("state/herdr-core/default/core.json").exists());
    let status = r.nhc(&["server", "status"]);
    // Another HOME and other XDG roots find the same core, and gain nothing.
    let elsewhere = r.root.join("elsewhere");
    let mut moved = r.cmd();
    moved
        .env("HOME", &elsewhere)
        .env("XDG_CONFIG_HOME", elsewhere.join("config"))
        .env("XDG_STATE_HOME", elsewhere.join("state"))
        .env("XDG_RUNTIME_DIR", elsewhere.join("run"));
    let moved = r.json(moved, &["server", "status"]);
    assert_eq!(moved["pid"], status["pid"]);
    assert_eq!(moved["core"]["id"], status["core"]["id"]);
    assert!(!elsewhere.exists() && !r.root.join("xdg").exists());
    assert_eq!(std::fs::read_dir(r.root.join("home")).unwrap().count(), 0);
}

#[test]
fn hc_address_fields_are_refused_and_never_reach_the_core() {
    let r = Remote::new();
    for var in ["HC_SESSION", "HC_SOCKET"] {
        for val in ["", "other"] {
            error(
                r.cmd().env(var, val).args(["--json", "server", "status"]).output().unwrap(),
                "managed_address_override",
            );
        }
    }
    error(r.cmd().args(["--json", "connect", "--session", "other"]).output().unwrap(), "managed_address_override");
    assert!(!r.root.join("run").exists(), "refused before anything was touched");
    // An SSH-started core imports the login's settings; this login exports
    // the user's own hc session. Neither it nor the caller's HC_* may stay.
    let shell = r.root.join("login-shell");
    std::fs::write(
        &shell,
        format!(
            "#!/bin/sh\ntouch '{}'\nexport HC_SESSION=user HC_SOCKET=/nowhere HC_FROM_PROFILE=1\nshift\nexec /bin/sh \"$@\"\n",
            r.root.join("login-ran").display()
        ),
    )
    .unwrap();
    herdr_core::platform::make_executable(&shell).unwrap();
    let mut start = r.cmd();
    start.env("SHELL", &shell).env("SSH_CONNECTION", "10.0.0.1 50000 10.0.0.2 22");
    start.env("HC_TERMINAL_ID", "t9").env("HC_TERMINAL_SOCKET", "/elsewhere.sock").env("HC_SCOPE", "x");
    r.json(start, &["server", "status"]);
    assert!(r.root.join("login-ran").exists(), "the login environment was imported");
    let terminal = r.nhc(&["run", "--bg", "sleep", "60"]);
    let status = r.nhc(&["server", "status"]);
    assert_eq!(process_hc(status["pid"].as_i64().unwrap()), ["HC_RUNTIME"]);
    assert_eq!(process_hc(status["holder"]["pid"].as_i64().unwrap()), ["HC_RUNTIME"]);
    assert_eq!(process_hc(r.pid(&terminal).as_i64().unwrap()), ["HC_SCOPE", "HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"]);
}

#[test]
fn run_keep_and_exec_see_this_cores_nhc_and_no_foreign_hc_fields() {
    let r = Remote::new();
    let shell_env = r.root.join("shell-env");
    std::fs::create_dir_all(r.root.join("config/herdr-core")).unwrap();
    std::fs::write(
        r.root.join("config/herdr-core/config.toml"),
        format!(
            "socket = '/tmp/not-honoured.sock'\nshell = ['/bin/sh', '-c', 'env > {0}.tmp; echo DONE=1 >> {0}.tmp; mv {0}.tmp {0}; exec sleep 60']\n",
            shell_env.display()
        ),
    )
    .unwrap();
    r.nhc(&["server", "status"]);
    assert!(r.root.join("run/c.sock").exists() && !Path::new("/tmp/not-honoured.sock").exists());
    let caller = json!({"HC_SESSION":"user", "HC_SOCKET":"/nowhere", "HC_TERMINAL_ID":"t99", "HC_FROM_CALLER":"1",
        "PATH":"/caller/bin:/usr/bin:/bin", "KEPT_VALUE":"yes"});
    let bin = r.root.join("bin");
    let check = |env: &BTreeMap<String, String>, hc: &[&str]| {
        assert_eq!(env["NEARSHELL_HC"], bin.join("nhc").display().to_string());
        assert_eq!(env["PATH"], format!("{}:/caller/bin:/usr/bin:/bin", bin.display()));
        assert_eq!(env["KEPT_VALUE"], "yes");
        assert_eq!(hc_keys(env), hc);
    };
    let run_env = r.root.join("run-env");
    let line = format!("env > {0}.tmp; echo DONE=1 >> {0}.tmp; mv {0}.tmp {0}; exec sleep 60", run_env.display());
    let t = r.rpc("terminal.run", json!({"argv":["/bin/sh","-c",line], "env":caller, "keep":true}));
    let env = env_file(&run_env);
    check(&env, &["HC_SCOPE", "HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"]);
    assert_eq!(env["HC_TERMINAL_ID"], t["address"].as_str().unwrap());
    assert_eq!(env["HC_TERMINAL_SOCKET"], r.root.join("run/c.sock").display().to_string());
    // Lose its holder: the kept terminal comes back as the configured shell.
    let status = r.nhc(&["server", "status"]);
    unsafe { libc::kill(status["holder"]["pid"].as_i64().unwrap() as i32, libc::SIGKILL) };
    check(&env_file(&shell_env), &["HC_SCOPE", "HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"]);
    let exec = r.rpc("machine.exec", json!({"argv":["/bin/sh","-c","env; echo DONE=1"], "env":caller}));
    let text = String::from_utf8(herdr_core::put::decode(exec["stdout"].as_str().unwrap()).unwrap()).unwrap();
    let env = text.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.into(), v.into())).collect();
    check(&env, &["HC_SCOPE"]);
    // Only programs gain the entry: the core's own PATH stays as it started.
    let environ = std::fs::read(format!("/proc/{}/environ", status["pid"])).unwrap();
    assert!(environ.split(|b| *b == 0).any(|e| e == b"PATH=/usr/bin:/bin"));
}

#[test]
fn restart_keeps_terminals_and_update_never_reaches_a_download() {
    let r = Remote::new();
    let t = r.nhc(&["run", "--bg", "sleep", "60"]); // run starts the core too
    let pid = r.pid(&t);
    assert!(pid.is_number(), "{t}");
    let before = r.nhc(&["server", "status"]);
    for args in [&["update"][..], &["update", "--version", "0.0.1"]] {
        error(r.cmd().arg("--json").args(args).output().unwrap(), "managed_distribution");
    }
    assert_eq!(r.nhc(&["server", "status"])["pid"], before["pid"], "update restarted nothing");
    r.nhc(&["server", "restart"]);
    let after = r.nhc(&["server", "status"]);
    assert_ne!(after["pid"], before["pid"]);
    let kept = r.nhc(&["get", t["address"].as_str().unwrap()]);
    assert_eq!((&kept["state"], &kept["pid"]), (&json!("live"), &pid));
    assert_eq!(r.nhc(&["server", "stop"])["stopped"], true);
    assert!(!r.root.join("run/c.sock").exists());
}

#[test]
fn an_unsafe_socket_directory_is_refused_not_repaired() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let r = Remote::new();
    let run = r.root.join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    error(r.cmd().args(["--json", "server", "status"]).output().unwrap(), "nearshell_socket_dir");
    assert_eq!(std::fs::metadata(&run).unwrap().permissions().mode() & 0o777, 0o755);
    std::fs::remove_dir(&run).unwrap();
    let target = r.root.join("elsewhere");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, &run).unwrap();
    error(r.cmd().args(["--json", "server", "status"]).output().unwrap(), "nearshell_socket_dir");
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
    std::fs::remove_file(&run).unwrap();
}

#[test]
fn a_live_core_whose_socket_was_removed_is_reported() {
    let r = Remote::new();
    let status = r.nhc(&["server", "status"]);
    std::fs::remove_file(r.root.join("run/c.sock")).unwrap();
    let err = error(r.cmd().args(["--json", "server", "status"]).output().unwrap(), "nearshell_socket_lost");
    assert!(err["error"]["hint"].as_str().unwrap().contains("nhc serve"), "{err}");
    // The hint's way out: stop that core, and the next command starts another.
    let pid = status["pid"].as_i64().unwrap() as i32;
    unsafe { libc::kill(pid, libc::SIGTERM) };
    eventually("old core gone", || (unsafe { libc::kill(pid, 0) } != 0).then_some(()));
    assert_ne!(r.nhc(&["server", "status"])["pid"], status["pid"]);
}

#[test]
#[ignore = "scripts/check-remote.sh supplies a frozen default-build hc"]
fn a_users_own_hc_and_the_remote_core_cannot_see_each_other() {
    let r = Remote::new();
    let user = r.root.join("user");
    let user_bin = std::env::var("NEARSHELL_HC_UPSTREAM_TEST_BIN").expect("run scripts/check-remote.sh");
    for dir in ["bin", "config", "state", "run"] {
        herdr_core::platform::private_dir(&user.join(dir)).unwrap();
    }
    std::os::unix::fs::symlink(&user_bin, user.join("bin/hc")).unwrap();
    std::fs::create_dir_all(r.root.join("bin")).unwrap();
    std::fs::copy(NHC, r.root.join("bin/nhc")).unwrap();
    // The user's account: one HOME and XDG set for both, hc on PATH.
    let account = |cmd: &mut Command| {
        cmd.env("HOME", &user)
            .env("PATH", format!("{}:/usr/bin:/bin", user.join("bin").display()))
            .env("XDG_CONFIG_HOME", user.join("config"))
            .env("XDG_STATE_HOME", user.join("state"))
            .env("XDG_RUNTIME_DIR", user.join("run"));
    };
    let user_guardian = guardian::Guardian::new(&user, Path::new(&user_bin), None);
    let user_nonce = user_guardian.nonce.clone();
    let hc = || {
        let mut cmd = Command::new(&user_bin);
        cmd.env_clear().env("NEARSHELL_TEST_SESSION_ID", &user_nonce).stdin(Stdio::null());
        account(&mut cmd);
        cmd
    };
    struct UserCore(Command, guardian::Guardian);
    impl Drop for UserCore {
        fn drop(&mut self) {
            let _ = self.0.args(["server", "stop"]).output();
            assert!(self.1.finish() || std::thread::panicking(), "user hc cleanup unconfirmed");
        }
    }
    let _user_core = UserCore(hc(), user_guardian);
    let nhc = || {
        let mut cmd = r.cmd();
        account(&mut cmd);
        cmd
    };
    let user_t = value(hc().args(["--json", "run", "--bg", "--name", "users", "sleep", "60"]).output().unwrap());
    let user_get = || value(hc().args(["--json", "get", user_t["address"].as_str().unwrap()]).output().unwrap());
    let user_pid = user_get()["pid"].clone();
    assert!(user_pid.is_number());
    r.json(nhc(), &["run", "--bg", "--name", "ours", "sleep", "60"]);
    let user_status = value(hc().args(["--json", "server", "status"]).output().unwrap());
    let own_status = r.json(nhc(), &["server", "status"]);
    assert_ne!(user_status["core"]["id"], own_status["core"]["id"]);
    let names = |v: Value| -> Vec<String> {
        v["terminals"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap_or_default().into()).collect()
    };
    assert_eq!(names(value(hc().args(["--json", "ls"]).output().unwrap())), ["users"]);
    assert_eq!(names(r.json(nhc(), &["ls"])), ["ours"]);
    // From a remote terminal: hc is the user's, nhc (first on PATH) is ours.
    let out = |name: &str| r.root.join(name);
    let line = format!(
        "hc ls --json > {0}.tmp && mv {0}.tmp {0}; nhc ls --json > {1}.tmp && mv {1}.tmp {1}; \"$NEARSHELL_HC\" server status --json > {2}.tmp && mv {2}.tmp {2}",
        out("seen-hc").display(),
        out("seen-nhc").display(),
        out("seen-entry").display()
    );
    r.rpc("terminal.run", json!({"argv":["/bin/sh","-c",line], "name":"probe"}));
    let read = |name: &str| eventually(name, || serde_json::from_slice::<Value>(&std::fs::read(out(name)).ok()?).ok());
    assert_eq!(names(read("seen-hc")), ["users"]);
    assert!(names(read("seen-nhc")).contains(&"ours".into()) && !names(read("seen-nhc")).contains(&"users".into()));
    assert_eq!(read("seen-entry")["pid"], own_status["pid"]);
    // From the user's terminal: its HC_TERMINAL_* never point nhc at hc.
    let line = format!(
        "NEARSHELL_HC_TEST_ROOT={0} {0}/bin/nhc ls --json > {1}.tmp && mv {1}.tmp {1}",
        r.root.display(),
        out("seen-from-hc").display()
    );
    value(hc().args(["--json", "run", "--bg", "/bin/sh", "-c", &line]).output().unwrap());
    assert!(!names(read("seen-from-hc")).contains(&"users".into()));
    // Neither wrote into the other's files: one state each, one socket set each.
    let names_in = |dir: &Path| -> Vec<String> {
        let mut names: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        names
    };
    assert_eq!(names_in(&user.join("state/herdr-core")), ["default"]);
    assert_eq!(names_in(&r.root.join("state/herdr-core")), ["default"]);
    assert!(names_in(&user.join("run/herdr-core")).iter().all(|n| n.starts_with("default.")));
    assert!(names_in(&r.root.join("run")).iter().all(|n| n.starts_with("c.")));
    assert_eq!(r.nhc(&["server", "stop"])["stopped"], true);
    let still = user_get();
    assert_eq!((&still["state"], &still["pid"]), (&json!("live"), &user_pid));
}
