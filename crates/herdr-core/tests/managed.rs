#![cfg(all(unix, feature = "managed-test"))]
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

#[path = "common/guardian.rs"]
mod guardian;
struct Scratch(PathBuf, String, Option<std::fs::File>, guardian::Guardian);
impl Scratch {
    fn new() -> Self {
        let root = PathBuf::from(format!("/var/tmp/nc-{}", herdr_core::platform::random_hex()));
        std::fs::create_dir_all(root.join("config/herdr-core")).unwrap();
        let lock =
            std::fs::File::options().create(true).truncate(false).write(true).open(root.join("owner.lock")).unwrap();
        assert!(herdr_core::platform::try_lock(&lock));
        let token = herdr_core::platform::random_hex() + &herdr_core::platform::random_hex();
        let guardian = guardian::Guardian::new(&root, std::path::Path::new(env!("CARGO_BIN_EXE_nhc")), Some(&token));
        Self(root, token, Some(lock), guardian)
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nhc"));
        c.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("NEARSHELL_HC_TEST_ROOT", &self.0)
            .env("NEARSHELL_TEST_SESSION_ID", &self.3.nonce)
            .env("HOME", self.0.join("wrong-home"))
            .env("XDG_CONFIG_HOME", self.0.join("wrong-xdg"))
            .env("XDG_STATE_HOME", self.0.join("wrong-state"))
            .env("XDG_RUNTIME_DIR", self.0.join("wrong-run"));
        c
    }
    fn config(&self, text: &str) {
        std::fs::write(
            self.0.join("config/herdr-core/config.toml"),
            format!("owner_token_sha256 = '{}'\n{text}", herdr_core::owner::verifier(&self.1)),
        )
        .unwrap();
        guardian::Guardian::register(&self.0, std::path::Path::new(env!("CARGO_BIN_EXE_nhc")), Some(&self.1));
    }
    fn owner_cmd(&self) -> Command {
        let mut cmd = self.cmd();
        cmd.args(["--owner-token", &self.1]);
        cmd
    }
    fn start(&self) -> Value {
        value(self.owner_cmd().args(["server", "start"]).output().unwrap())
    }
    fn socket(&self) -> PathBuf {
        self.0.join("run/c.sock")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        self.2.take(); // Guardian acquires the owner lease before authorized stop.
        let clean = self.3.finish();
        if clean {
            let _ = std::fs::remove_dir_all(&self.0);
        }
        assert!(clean || std::thread::panicking(), "test_guardian_cleanup_unconfirmed");
    }
}
fn value(out: Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}
fn error(out: Output, kind: &str) {
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["error"]["kind"], kind, "{v}");
}
#[test]
fn managed_addressing_fails_closed_and_ignores_shell_xdg() {
    let s = Scratch::new();
    error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    for text in ["[broken", "headless = '80x24'", "socket = 'relative.sock'"] {
        s.config(text);
        error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    }
    s.config(&format!("socket = '{}'", s.socket().display()));
    for var in ["HC_SESSION", "HC_SOCKET"] {
        for val in ["", "other"] {
            error(s.cmd().env(var, val).args(["server", "status"]).output().unwrap(), "managed_address_override");
        }
    }
    error(s.cmd().args(["connect", "--session", "other"]).output().unwrap(), "managed_address_override");
    assert!(!s.socket().exists());
    let local = value(s.cmd().args(["build-info", "--json"]).output().unwrap());
    let status = s.start();
    assert_eq!(status["build"], local);
    assert!(s.socket().exists());
    assert!(s.0.join("state/herdr-core/default/core.json").exists());
    assert!(!s.0.join("wrong-xdg").exists());
    assert!(!s.0.join("wrong-home").exists());
}
#[test]
fn managed_cli_uses_its_own_name_and_keeps_payloads_opaque() {
    let s = Scratch::new();
    for args in [&["--help"][..], &["--skill"], &["run", "--help"], &["--json", "--help"]] {
        let out = s.cmd().args(args).output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("nhc"), "{text}");
        assert!(!text.contains("nearshell-hc"), "{text}");
        assert!(!text.split_whitespace().any(|w| w == "hc"), "{text}");
        assert!(!text.contains("Tests need their own `HC_SESSION`"));
    }
    let out = s.cmd().arg("--version").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), &format!("nhc {}", env!("CARGO_PKG_VERSION")));
    s.config(&format!("socket = '{}'", s.socket().display()));
    s.start();
    let payload = "hc send t1 -- hc";
    let v = value(s.cmd().args(["--json", "exec", "printf", "%s", payload]).output().unwrap());
    assert_eq!(v["stdout"], "aGMgc2VuZCB0MSAtLSBoYw==");
}

#[test]
fn managed_socket_directory_must_be_private_and_not_a_symlink() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let s = Scratch::new();
    let run = s.0.join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.config(&format!("socket = '{}'", s.socket().display()));
    error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    assert_eq!(std::fs::metadata(&run).unwrap().permissions().mode() & 0o777, 0o755);
    std::fs::remove_dir(&run).unwrap();
    let target = s.0.join("elsewhere");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, &run).unwrap();
    error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    assert!(!target.join("c.sock").exists());
}

#[test]
fn scratch_build_has_no_production_fallback_and_cannot_install_upstream() {
    let s = Scratch::new();
    error(s.cmd().env_remove("NEARSHELL_HC_TEST_ROOT").args(["server", "status"]).output().unwrap(), "managed_config");
    // Offline identity bypasses even intentionally hostile addressing overrides.
    let build = value(
        s.cmd()
            .env("HC_SOCKET", "ignored")
            .env("HC_SESSION", "ignored")
            .env("NEARSHELL_CORE_COMMIT", "runtime-must-not-change-identity")
            .args(["build-info", "--json"])
            .output()
            .unwrap(),
    );
    assert_ne!(build["commit"], "runtime-must-not-change-identity");
    assert!(build["config"].as_str().unwrap().ends_with(":scratch-test"));
    s.config(&format!("socket = '{}'", s.socket().display()));
    error(s.cmd().args(["update"]).output().unwrap(), "managed_by_owner");
    error(s.owner_cmd().args(["update"]).output().unwrap(), "managed_distribution");
    let paths = herdr_core::paths::Paths {
        socket: s.socket(),
        holder_socket: s.0.join("h.sock"),
        state: s.0.join("state"),
        config: s.0.join("config"),
    };
    let result = herdr_core::machines::install(
        &paths,
        herdr_core::machines::Reach { target: "example.invalid", via: false },
        None,
    );
    assert_eq!(result.unwrap_err().kind, "managed_distribution");
}

fn rpc(s: &Scratch, method: &str, params: Value) -> Result<Value, herdr_core::rpc::RpcError> {
    let stream = herdr_core::platform::connect(&s.socket()).unwrap();
    herdr_core::client::call(&stream, method, params)
}
fn read_eventually(path: &std::path::Path) -> String {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(text) = std::fs::read_to_string(path)
            && text.contains("DONE=yes")
        {
            return text;
        }
        assert!(std::time::Instant::now() < end, "program did not write environment");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
#[test]
fn managed_owner_path_resolves_program_and_env_shebang_without_restart() {
    use serde_json::json;
    let s = Scratch::new();
    let bin = s.0.join("node bin");
    std::fs::create_dir(&bin).unwrap();
    // Deterministic node CLI stand-in: lookup and /usr/bin/env must both use
    // the owner's PATH, never the RPC caller's environment.
    std::os::unix::fs::symlink("/bin/sh", bin.join("node")).unwrap();
    let program = bin.join("codex-path-fixture");
    std::fs::write(&program, "#!/usr/bin/env node\nprintf 'DONE=yes\\n' > \"$1\"\n").unwrap();
    herdr_core::platform::make_executable(&program).unwrap();
    let config = |path: &str| format!("socket = '{}'\n[programs.env]\nPATH = '{path}'", s.socket().display());
    s.config(&config("/usr/bin:/bin"));
    let before = value(s.owner_cmd().env("PATH", "/usr/bin:/bin").args(["server", "start"]).output().unwrap());
    error(s.cmd().env("PATH", &bin).args(["run", "--bg", "codex-path-fixture"]).output().unwrap(), "spawn_failed");
    let missing = rpc(&s, "machine.exec", json!({"argv":[program]})).unwrap();
    assert_eq!(missing["code"], 127);
    for method in ["terminal.run", "machine.exec"] {
        assert_eq!(
            rpc(&s, method, json!({"argv":["codex-path-fixture"],"env":{"PATH":bin}})).unwrap_err().kind,
            "managed_program_env"
        );
    }
    s.config(&config(&format!("{}:/usr/bin:/bin", bin.display())));
    for (index, executable) in ["codex-path-fixture", program.to_str().unwrap()].iter().enumerate() {
        let output = s.0.join(format!("run-{index}"));
        value(
            s.cmd()
                .env("PATH", "/caller-only")
                .args(["run", "--bg", "--name", &format!("path-{index}"), executable, output.to_str().unwrap()])
                .output()
                .unwrap(),
        );
        assert_eq!(read_eventually(&output), "DONE=yes\n");
        let output = s.0.join(format!("exec-{index}"));
        let result = rpc(&s, "machine.exec", json!({"argv":[executable, output]})).unwrap();
        assert_eq!(result["code"], 0);
        assert_eq!(read_eventually(&output), "DONE=yes\n");
    }
    assert_eq!(rpc(&s, "server.status", json!({})).unwrap()["pid"], before["pid"]);
}

#[test]
fn managed_run_exec_nested_and_keep_share_program_environment() {
    use serde_json::json;
    let s = Scratch::new();
    let user = Scratch::new(); // models the user's unrelated XDG root
    let config = |marker: &str| {
        format!(
            r#"socket = '{}'
[programs]
unset = ['COLORTERM', 'REMOVE_ME']
[programs.env]
XDG_CONFIG_HOME = '{}'
XDG_STATE_HOME = '{}'
XDG_DATA_HOME = '{}'
XDG_CACHE_HOME = '{}'
XDG_RUNTIME_DIR = '{}'
SHELL = '/bin/sh'
TERM_PROGRAM = 'config-{marker}'
REMOVE_ME = 'never'
HC_TERMINAL_ID = 'forged'
HC_TERMINAL_SOCKET = 'forged'
DONE = 'yes'
CONFIG_MARKER = '{marker}'
NEARSHELL_HC = '{}'
"#,
            s.socket().display(),
            user.0.display(),
            user.0.display(),
            user.0.display(),
            user.0.display(),
            user.0.display(),
            env!("CARGO_BIN_EXE_nhc")
        )
    };
    s.config(&config("first"));
    s.start();
    let output = s.0.join("run.env");
    let v = rpc(&s, "terminal.run", json!({"argv":["/bin/sh","-c", format!("env > '{}'; exec sleep 60", output.display())],"keep":true,"env":{"TERM_PROGRAM":"caller"}})).unwrap();
    let id = v["address"].as_str().unwrap();
    let check = |text: &str, terminal: bool| {
        for key in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"] {
            assert!(text.contains(&format!("{key}={}\n", user.0.display())), "{text}");
        }
        for key in ["HC_SESSION=", "HC_SOCKET=", "HC_RUNTIME=", "REMOVE_ME=", "COLORTERM="] {
            assert!(!text.lines().any(|l| l.starts_with(key)), "{text}");
        }
        assert_eq!(text.lines().any(|l| l.starts_with("HC_TERMINAL_ID=")), terminal, "{text}");
        assert_eq!(text.lines().any(|l| l.starts_with("HC_TERMINAL_SOCKET=")), terminal, "{text}");
        assert!(text.contains("HC_SCOPE="));
        assert!(text.contains(&format!("NEARSHELL_HC={}\n", env!("CARGO_BIN_EXE_nhc"))));
    };
    let text = read_eventually(&output);
    check(&text, true);
    assert!(text.contains("TERM_PROGRAM=caller"));
    assert!(text.contains(&format!("HC_TERMINAL_ID={id}\n")));
    assert!(text.contains(&format!("HC_TERMINAL_SOCKET={}\n", s.socket().display())));
    for method in ["terminal.run", "machine.exec"] {
        let env = json!({"XDG_RUNTIME_DIR":"forbidden"});
        assert_eq!(rpc(&s, method, json!({"argv":["env"],"env":env})).unwrap_err().kind, "managed_program_env");
    }
    let exec = rpc(&s, "machine.exec", json!({"argv":["/usr/bin/env"]})).unwrap();
    let text = String::from_utf8(herdr_core::put::decode(exec["stdout"].as_str().unwrap()).unwrap()).unwrap();
    check(&text, false);
    let nested_file = s.0.join("nested.env");
    let nested = format!(
        "'{}' --json run --bg --keep --outlive-parent /bin/sh -c 'env > {}; exec sleep 60' > '{}'",
        env!("CARGO_BIN_EXE_nhc"),
        nested_file.display(),
        s.0.join("nested.json").display()
    );
    let parent = rpc(&s, "terminal.run", json!({"argv":["/bin/sh","-c", nested],"keep":true})).unwrap();
    check(&read_eventually(&nested_file), true);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let nested: Value = loop {
        if let Ok(v) = serde_json::from_slice(&std::fs::read(s.0.join("nested.json")).unwrap_or_default()) {
            break v;
        }
        assert!(std::time::Instant::now() < end, "nested CLI did not reply");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let nested = rpc(&s, "terminal.get", json!({"terminal":nested["address"]})).unwrap();
    assert_eq!(nested["parent"], parent["address"]);
    value(s.cmd().args(["--json", "run", "/usr/bin/true"]).output().unwrap());
    // Restart env stores only the per-call override. Config changes are re-read for keep.
    let reg: Value =
        serde_json::from_slice(&std::fs::read(s.0.join("state/herdr-core/default/terminals.json")).unwrap()).unwrap();
    let record = reg["terminals"].as_object().unwrap().values().find(|t| t["address"] == id).unwrap();
    assert_eq!(record["restart_env"], json!({"TERM_PROGRAM":"caller"}));
    let cli_file = s.0.join("cli.env");
    let cli = value(
        s.cmd()
            .args(["run", "--bg", "--keep", "/bin/sh", "-c", &format!("env > '{}'; exec sleep 60", cli_file.display())])
            .output()
            .unwrap(),
    );
    check(&read_eventually(&cli_file), true);
    let cli = rpc(&s, "terminal.get", json!({"terminal":cli["address"]})).unwrap();
    s.config(&config("second"));
    for (index, terminal) in [&v, &nested, &cli].into_iter().enumerate() {
        let id = terminal["address"].as_str().unwrap();
        let pid = terminal["pid"].as_u64().unwrap();
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let keep_file = s.0.join(format!("keep-{index}.env"));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let t = rpc(&s, "terminal.get", json!({"terminal":id})).unwrap();
            if t["pid"].as_u64().is_some_and(|p| p != pid) {
                break;
            }
            assert!(std::time::Instant::now() < end, "keep did not restart: {t}");
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
        rpc(&s, "terminal.send", json!({"terminal":id,"text":format!("env > '{}'",keep_file.display())})).unwrap();
        let text = read_eventually(&keep_file);
        check(&text, true);
        assert!(text.contains("CONFIG_MARKER=second"));
        assert!(text.contains(&format!("HC_TERMINAL_ID={id}\n")));
        if index == 0 {
            assert!(text.contains("TERM_PROGRAM=caller"));
        }
    }
}

#[test]
fn owner_gates_cli_direct_rpc_and_orphaned_tokens_but_not_ordinary_calls() {
    use serde_json::json;
    let mut s = Scratch::new();
    s.config(&format!("socket = '{}'", s.socket().display()));
    let config_before = std::fs::read(s.0.join("config/herdr-core/config.toml")).unwrap();
    for args in [
        vec!["server", "status"],
        vec!["ls"],
        vec!["run", "--bg", "true"],
        vec!["serve"],
        vec!["server", "start"],
        vec!["server", "stop"],
        vec!["server", "restart"],
        vec!["update", "--from", "/no/such/file"],
        vec!["update", "--version", "0.1.13"],
    ] {
        error(s.cmd().args(args).output().unwrap(), "managed_by_owner");
    }
    assert!(!s.socket().exists());
    assert_eq!(std::fs::read(s.0.join("config/herdr-core/config.toml")).unwrap(), config_before);
    // Dirty calling environments are cleaned by the explicit owner launcher.
    let first = value(
        s.owner_cmd()
            .env("HC_SECRET", "must-not-leak")
            .env("HC_TERMINAL_ID", "t99")
            .env("HC_SCOPE", "old")
            .env("XDG_DATA_HOME", "private-data")
            .args(["server", "start"])
            .output()
            .unwrap(),
    );
    let pid = first["pid"].clone();
    error(s.owner_cmd().args(["server", "restart", "--fresh"]).output().unwrap(), "invalid_params");
    assert_eq!(
        rpc(&s, "server.restart", json!({"owner_token":s.1,"environment":{"PATH":"/usr/bin:/bin"}})).unwrap_err().kind,
        "invalid_params"
    );
    let hello = rpc(&s, "hello", json!({})).unwrap();
    assert!(!hello["features"].as_array().unwrap().iter().any(|f| f == "restart_environment"));
    assert_eq!(value(s.cmd().args(["server", "status"]).output().unwrap())["pid"], pid);
    for method in ["server.start", "server.stop", "server.restart", "server.authorize"] {
        for token in [None, Some("bad"), Some("0000000000000000000000000000000000000000000000000000000000000000")] {
            assert_eq!(rpc(&s, method, json!({"owner_token":token})).unwrap_err().kind, "managed_by_owner");
        }
    }
    for args in [["server", "start"], ["server", "stop"], ["server", "restart"]] {
        error(s.cmd().args(args).output().unwrap(), "managed_by_owner");
    }
    assert_eq!(value(s.cmd().args(["server", "status"]).output().unwrap())["pid"], pid);
    // A second App cannot acquire the same owner inode.
    let other = std::fs::File::options().write(true).open(s.0.join("owner.lock")).unwrap();
    assert!(!herdr_core::platform::try_lock(&other));
    let old = s.1.clone();
    drop(s.2.take()); // owner crash: ordinary commands keep working
    value(s.cmd().args(["ls"]).output().unwrap());
    value(s.cmd().args(["run", "--bg", "/bin/sleep", "60"]).output().unwrap());
    error(s.owner_cmd().args(["server", "restart"]).output().unwrap(), "managed_by_owner");
    assert!(herdr_core::platform::try_lock(&other));
    s.2 = Some(other);
    s.1 = herdr_core::platform::random_hex() + &herdr_core::platform::random_hex();
    s.config(&format!("socket = '{}'", s.socket().display()));
    assert_eq!(rpc(&s, "server.restart", json!({"owner_token":old})).unwrap_err().kind, "managed_by_owner");
    assert_eq!(s.start()["pid"], pid, "new owner adopts a live core without restarting");
    let restarted = value(s.owner_cmd().env("HC_SECRET", "again").args(["server", "restart"]).output().unwrap());
    assert_ne!(restarted["pid"], pid);
    let result = rpc(&s, "machine.exec", json!({"argv":["/usr/bin/env"]})).unwrap();
    let env = String::from_utf8(herdr_core::put::decode(result["stdout"].as_str().unwrap()).unwrap()).unwrap();
    assert!(!env.contains("HC_SECRET="));
    assert!(!env.contains("HC_TERMINAL_ID="));
    assert!(!env.contains("XDG_DATA_HOME="));
    // Explicit local management and owner rotation share a lock.
    let operation = herdr_core::owner::operation(
        &herdr_core::paths::Paths {
            socket: s.socket(),
            holder_socket: s.0.join("h.sock"),
            state: s.0.join("state"),
            config: s.0.join("config/herdr-core/config.toml"),
        },
        Some(&s.1),
    )
    .unwrap();
    error(s.owner_cmd().args(["server", "restart"]).output().unwrap(), "managed_by_owner");
    let rotation = std::fs::File::options().write(true).open(s.0.join("management.lock")).unwrap();
    assert!(!herdr_core::platform::try_lock(&rotation), "an authorized CLI operation fences verifier rotation");
    rpc(&s, "server.authorize", json!({"owner_token":s.1})).unwrap();
    drop(operation);
    assert!(herdr_core::platform::try_lock(&rotation));
    assert_eq!(rpc(&s, "server.authorize", json!({"owner_token":s.1})).unwrap_err().kind, "managed_by_owner");
    drop(rotation);
}

#[test]
fn framed_management_also_requires_an_explicit_owner_capability() {
    use herdr_core::wire;
    use std::io::{Read, Write};
    let s = Scratch::new();
    s.config(&format!("socket = '{}'", s.socket().display()));
    s.start();
    let mut stream = herdr_core::platform::connect(&s.socket()).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
    herdr_core::client::call(&stream, "hello", serde_json::json!({"framed":true})).unwrap();
    for method in ["server.stop", "server.restart", "server.start", "server.authorize"] {
        let payload =
            serde_json::to_vec(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":method,"params":{}})).unwrap();
        stream.write_all(&wire::encode(0, wire::DATA, &payload)).unwrap();
        let mut header = [0; wire::HEADER];
        stream.read_exact(&mut header).unwrap();
        let (_, _, len) = wire::decode_header(&header).unwrap();
        let mut payload = vec![0; len];
        stream.read_exact(&mut payload).unwrap();
        let value: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value["error"]["data"]["kind"], "managed_by_owner", "{value}");
    }
}

#[test]
fn managed_update_checks_owner_before_swap_and_preserves_ptys() {
    use serde_json::json;
    let s = Scratch::new();
    let user = Scratch::new();
    s.config(&format!("socket = '{}'\n[programs.env]\nXDG_CONFIG_HOME = '{}'\nXDG_STATE_HOME = '{}'\nXDG_DATA_HOME = '{}'\nXDG_CACHE_HOME = '{}'\nXDG_RUNTIME_DIR = '{}'", s.socket().display(),user.0.display(),user.0.display(),user.0.display(),user.0.display(),user.0.display()));
    std::fs::create_dir_all(s.0.join("bin")).unwrap();
    let dest = s.0.join("bin/nhc");
    std::fs::copy(env!("CARGO_BIN_EXE_nhc"), &dest).unwrap();
    let cmd = |owner: bool| {
        let template = s.cmd();
        let mut c = Command::new(&dest);
        c.env_clear();
        for (k, v) in template.get_envs() {
            if let Some(v) = v {
                c.env(k, v);
            }
        }
        if owner {
            c.args(["--owner-token", &s.1]);
        }
        c
    };
    let first = value(cmd(true).args(["server", "start"]).output().unwrap());
    let running = rpc(&s, "terminal.run", json!({"argv":["/bin/sleep","60"]})).unwrap();
    let before = std::fs::read(&dest).unwrap();
    // Working executables with incompatible identity must also be refused.
    let identity = value(s.cmd().args(["build-info", "--json"]).output().unwrap());
    for field in ["config", "commit"] {
        let mut wrong_identity = identity.clone();
        wrong_identity[field] = json!("incompatible");
        let fake = s.0.join(format!("wrong-{field}"));
        std::fs::write(&fake, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", wrong_identity)).unwrap();
        herdr_core::platform::make_executable(&fake).unwrap();
        error(
            cmd(true).args(["update", "--from", fake.to_str().unwrap(), "--force"]).output().unwrap(),
            "managed_build",
        );
        assert_eq!(std::fs::read(&dest).unwrap(), before);
    }

    let wrong = s.0.join("wrong-build");
    std::fs::write(&wrong, b"not a core").unwrap();
    for args in [
        vec!["update"],
        vec!["update", "--version", "0.1.13"],
        vec!["update", "--from", wrong.to_str().unwrap()],
        vec!["update", "--from", env!("CARGO_BIN_EXE_nhc"), "--force"],
    ] {
        error(cmd(false).args(args).output().unwrap(), "managed_by_owner");
        assert_eq!(std::fs::read(&dest).unwrap(), before);
    }
    error(cmd(true).args(["update", "--from", wrong.to_str().unwrap()]).output().unwrap(), "managed_build");
    assert_eq!(std::fs::read(&dest).unwrap(), before);
    let update = cmd(true)
        .args(["update", "--from", env!("CARGO_BIN_EXE_nhc")])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    error(cmd(false).args(["server", "restart"]).output().unwrap(), "managed_by_owner");
    let updated = value(update.wait_with_output().unwrap());
    assert_ne!(updated["pid"], first["pid"]);
    let after = rpc(&s, "terminal.get", json!({"terminal":running["address"]})).unwrap();
    assert_eq!(after["pid"], running["pid"]);
    let after_env = s.0.join("after-update.env");
    value(
        cmd(false)
            .args([
                "run",
                "--bg",
                "/bin/sh",
                "-c",
                &format!("env > '{}'; echo DONE=yes >> '{}'", after_env.display(), after_env.display()),
            ])
            .output()
            .unwrap(),
    );
    let env = read_eventually(&after_env);
    for key in ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CACHE_HOME", "XDG_RUNTIME_DIR"] {
        assert!(env.contains(&format!("{key}={}\n", user.0.display())), "{env}");
    }
    assert!(!env.lines().any(|l| l.starts_with("HC_SOCKET=") || l.starts_with("HC_SESSION=")));
    assert!(env.contains(&format!("HC_TERMINAL_SOCKET={}\n", s.socket().display())));
    // The installed file may already have been replaced before an owner restart.
    let replaced = s.0.join("replacement");
    std::fs::copy(env!("CARGO_BIN_EXE_nhc"), &replaced).unwrap();
    std::fs::rename(replaced, &dest).unwrap();
    value(cmd(true).args(["server", "restart"]).output().unwrap());
    assert_eq!(rpc(&s, "terminal.get", json!({"terminal":running["address"]})).unwrap()["pid"], running["pid"]);
}

#[test]
#[ignore = "just managed-check supplies a frozen default-build binary"]
fn user_hc_in_a_second_scratch_root_stays_independent_inside_managed_terminals() {
    use serde_json::json;
    let s = Scratch::new();
    let user = Scratch::new();
    let user_bin = std::env::var("NEARSHELL_HC_UPSTREAM_TEST_BIN").expect("run just managed-check");
    struct UserCore(Command);
    impl Drop for UserCore {
        fn drop(&mut self) {
            let _ = self.0.args(["server", "stop"]).output();
        }
    }
    let user_cmd = || {
        let mut cmd = Command::new(&user_bin);
        cmd.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &user.0)
            .env("NEARSHELL_TEST_SESSION_ID", &user.3.nonce)
            .env("XDG_CONFIG_HOME", user.0.join("config"))
            .env("XDG_STATE_HOME", user.0.join("state"))
            .env("XDG_DATA_HOME", user.0.join("data"))
            .env("XDG_CACHE_HOME", user.0.join("cache"))
            .env("XDG_RUNTIME_DIR", user.0.join("run"));
        cmd
    };
    user.config(&format!("socket = '{}'", user.socket().display()));
    guardian::Guardian::register(&user.0, std::path::Path::new(&user_bin), None);
    std::fs::create_dir_all(user.0.join("bin")).unwrap();
    std::os::unix::fs::symlink(&user_bin, user.0.join("bin/hc")).unwrap();
    s.config(&format!(
        r#"socket = '{}'
[programs.env]
XDG_CONFIG_HOME = '{}'
XDG_STATE_HOME = '{}'
XDG_DATA_HOME = '{}'
XDG_CACHE_HOME = '{}'
XDG_RUNTIME_DIR = '{}'
PATH = '{}:/usr/bin:/bin'
NEARSHELL_HC = '{}'
"#,
        s.socket().display(),
        user.0.join("config").display(),
        user.0.join("state").display(),
        user.0.join("data").display(),
        user.0.join("cache").display(),
        user.0.join("run").display(),
        user.0.join("bin").display(),
        env!("CARGO_BIN_EXE_nhc")
    ));
    let status = s.start();
    let _cleanup = UserCore(user_cmd());
    let user_status = value(user_cmd().args(["server", "status"]).output().unwrap());
    assert_ne!(status["core"]["id"], user_status["core"]["id"]);
    let output = s.0.join("user-status");
    let own_output = s.0.join("own-status");
    rpc(&s,"terminal.run",json!({"argv":["/bin/sh","-c",format!("hc server status > '{}'; \"$NEARSHELL_HC\" server status > '{}'",output.display(),own_output.display())]})).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let (got_user, got_own): (Value, Value) = loop {
        let a = serde_json::from_slice(&std::fs::read(&output).unwrap_or_default());
        let b = serde_json::from_slice(&std::fs::read(&own_output).unwrap_or_default());
        if let (Ok(a), Ok(b)) = (a, b) {
            break (a, b);
        }
        assert!(std::time::Instant::now() < deadline, "nested hc did not answer");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert_eq!(got_user["pid"], user_status["pid"]);
    assert_eq!(got_own["pid"], status["pid"]);
    assert!(value(user_cmd().args(["ls"]).output().unwrap())["terminals"].as_array().unwrap().is_empty());
    value(s.owner_cmd().args(["server", "stop"]).output().unwrap());
    assert_eq!(value(user_cmd().args(["server", "status"]).output().unwrap())["pid"], user_status["pid"]);
}

#[test]
fn a_managed_holder_preserves_ptys_without_autostarting_core() {
    use serde_json::json;
    let s = Scratch::new();
    s.config(&format!("socket = '{}'", s.socket().display()));
    let status = s.start();
    let terminal = rpc(&s, "terminal.run", json!({"argv":["/bin/sleep","60"]})).unwrap();
    unsafe {
        libc::kill(status["pid"].as_i64().unwrap() as i32, libc::SIGKILL);
    }
    // Holder's normal crash-restart grace is 3 seconds. Cross it before
    // observing the absence of an auto-start and the still-live program.
    std::thread::sleep(std::time::Duration::from_secs(4));
    error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_by_owner");
    assert_eq!(unsafe { libc::kill(terminal["pid"].as_i64().unwrap() as i32, 0) }, 0);
    let next = s.start();
    assert_ne!(next["pid"], status["pid"]);
    assert_eq!(rpc(&s, "terminal.get", json!({"terminal":terminal["address"]})).unwrap()["pid"], terminal["pid"]);
}

#[test]
fn managed_schema_publishes_owner_parameters() {
    let schema = herdr_core::api::openrpc();
    for method in ["server.stop", "server.restart", "server.start", "server.authorize"] {
        let entry = schema["methods"].as_array().unwrap().iter().find(|m| m["name"] == method).unwrap();
        let text = serde_json::to_string(entry).unwrap();
        assert!(text.contains("owner_token"), "{text}");
    }
}

#[test]
fn managed_marked_paint_negotiates_and_survives_split_and_coalesced_frames() {
    use herdr_core::wire;
    use serde_json::json;
    use std::io::{BufRead, Read, Write};
    let s = Scratch::new();
    s.config(&format!("socket = '{}'\nheadless='100x30'", s.socket().display()));
    s.start();
    let run =
        rpc(&s, "terminal.run", json!({"argv":["sh","-c","printf MARKED; sleep 60"],"bg":true,"cols":80,"rows":24}))
            .unwrap();
    assert_eq!((run["cols"].as_u64(), run["rows"].as_u64()), (Some(80), Some(24)));
    let mut stream = std::os::unix::net::UnixStream::connect(s.socket()).unwrap();
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
    writeln!(stream, "{}", json!({"jsonrpc":"2.0","id":1,"method":"hello","params":{"framed":true}})).unwrap();
    let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
    let mut hello = String::new();
    reader.read_line(&mut hello).unwrap();
    let hello: Value = serde_json::from_str(&hello).unwrap();
    assert!(hello["result"]["features"].as_array().unwrap().iter().any(|f| f == "paint-marks"));
    assert!(hello["result"]["features"].as_array().unwrap().iter().any(|f| f == "display-palette"));
    let req = json!({"jsonrpc":"2.0","id":2,"method":"terminal.attach","params":{"terminal":run["address"],"cols":80,"rows":24,"marks":true,"display":{"palette":{"1":"#123456","255":"#abcdef"},"bg":"#ffffff","dark":true}}});
    for byte in wire::encode(0, wire::DATA, req.to_string().as_bytes()) {
        stream.write_all(&[byte]).unwrap();
    }
    let mut began = None;
    let mut data = Vec::new();
    let mut ch = 0;
    let mut ended = false;
    while ch == 0 || !ended {
        let mut header = [0; wire::HEADER];
        reader.read_exact(&mut header).unwrap();
        let (channel, kind, n) = wire::decode_header(&header).unwrap();
        let mut payload = vec![0; n];
        reader.read_exact(&mut payload).unwrap();
        if channel == 0 {
            let reply: Value = serde_json::from_slice(&payload).unwrap();
            ch = reply["result"]["channel"].as_u64().unwrap() as u32;
            continue;
        }
        if kind == wire::DATA {
            assert!(began.is_some());
            data.extend_from_slice(&payload);
        }
        if kind == wire::MARK {
            let mark: Value = serde_json::from_slice(&payload).unwrap();
            match mark["type"].as_str().unwrap() {
                "paint_begin" => {
                    assert!(began.is_none());
                    began = Some(mark["paintGen"].clone());
                    assert_eq!(mark["colors"]["palette"][1], json!([18, 52, 86]));
                    assert_eq!(mark["colors"]["palette"][255], json!([171, 205, 239]));
                    assert_eq!(mark["colors"]["bg"], json!([255, 255, 255]));
                    assert_eq!(mark["colors"]["dark"], true);
                }
                "paint_end" => {
                    assert_eq!(began, Some(mark["paintGen"].clone()));
                    ended = true;
                }
                _ => panic!("unexpected mark {mark}"),
            }
        }
    }
    assert!(ch > 0);
    assert!(!data.is_empty());
    // Two complete frames in one socket write, with ordinary RPC after detach.
    let req = json!({"jsonrpc":"2.0","id":3,"method":"server.status","params":{}});
    stream
        .write_all(
            &[wire::encode(ch, wire::CLOSE, b""), wire::encode(0, wire::DATA, req.to_string().as_bytes())].concat(),
        )
        .unwrap();
    loop {
        let mut h = [0; wire::HEADER];
        reader.read_exact(&mut h).unwrap();
        let (ch, _, n) = wire::decode_header(&h).unwrap();
        let mut bytes = vec![0; n];
        reader.read_exact(&mut bytes).unwrap();
        if ch == 0 {
            let reply: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(reply["result"]["viewer_pending"]["limit"].as_u64().unwrap() > 0);
            break;
        }
    }
}

#[test]
fn previous_managed_holder_survives_a_new_binary_owner_restart() {
    let Some(old) = std::env::var_os("NEARSHELL_HC_OLD_MANAGED_TEST_BIN") else {
        eprintln!("skipped: supply a previous managed-test binary in NEARSHELL_HC_OLD_MANAGED_TEST_BIN");
        return;
    };
    let s = Scratch::new();
    s.config(&format!("socket = '{}'", s.socket().display()));
    std::fs::create_dir_all(s.0.join("bin")).unwrap();
    let dest = s.0.join("bin/nhc");
    std::fs::copy(old, &dest).unwrap();
    let cmd = || {
        let mut command = Command::new(&dest);
        command.env_clear();
        for (key, value) in s.cmd().get_envs() {
            if let Some(value) = value {
                command.env(key, value);
            }
        }
        command.args(["--owner-token", &s.1]);
        command
    };
    let before = value(cmd().args(["server", "start"]).output().unwrap());
    let terminal = rpc(
        &s,
        "terminal.run",
        serde_json::json!({
            "argv": ["/bin/sh", "-c", "echo OLD; while read line; do echo got-$line; done"],
            "cols": 91, "rows": 27
        }),
    )
    .unwrap();
    let old_core = before["pid"].clone();
    let replacement = s.0.join("replacement");
    std::fs::copy(env!("CARGO_BIN_EXE_nhc"), &replacement).unwrap();
    std::fs::rename(replacement, &dest).unwrap();
    let after = value(cmd().args(["server", "restart"]).output().unwrap());
    assert_ne!(after["pid"], old_core);
    let kept = rpc(&s, "terminal.get", serde_json::json!({"terminal":terminal["address"]})).unwrap();
    assert_eq!(kept["pid"], terminal["pid"]);
    assert_eq!((kept["cols"].as_u64(), kept["rows"].as_u64()), (Some(91), Some(27)));
    rpc(&s, "terminal.send", serde_json::json!({"terminal":terminal["address"],"text":"new"})).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let read = rpc(&s, "terminal.read", serde_json::json!({"terminal":terminal["address"]})).unwrap();
        if read["text"].as_str().unwrap().contains("got-new") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "old PTY did not accept input after upgrade");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let new = rpc(&s, "terminal.run", serde_json::json!({"argv":["/bin/sleep","60"]})).unwrap();
    assert!(new["pid"].as_u64().is_some());
    assert_ne!(new["pid"], terminal["pid"]);
}
