//! A hub core and a far account it reaches over a fake ssh, for the remote
//! build's machine tests (`--features nearshell-test`, Linux). Any core can add
//! machines, so a remote build is the hub; a test build honours HC_SSH. The far
//! account gets its own HOME and test root, and its remote core goes where the
//! hub's line looks for it.
#![allow(dead_code)] // Each test binary uses part of it.
use super::guardian;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

pub const NHC: &str = env!("CARGO_BIN_EXE_nhc");

/// A hub core (`serve`, started here so that HC_SSH reaches it) and one far
/// account it reaches as `far` over the fake ssh; `mac` is the same account
/// answering `uname` as a Mac, `win` a Windows box.
pub struct World {
    pub dir: PathBuf,
    pub hub: PathBuf,
    pub hub_bin: PathBuf,
    pub far_home: PathBuf,
    pub far_root: PathBuf,
    serve: Option<Child>,
    pub hub_guardian: guardian::Guardian,
    pub far_guardian: guardian::Guardian,
}

pub fn build_info(bin: &Path) -> Value {
    value(Command::new(bin).args(["build-info", "--json"]).env_clear().output().unwrap())
}

impl World {
    pub fn new(hub_bin: &Path) -> World {
        let dir = PathBuf::from(format!("/var/tmp/nm-{}", &herdr_core::platform::random_hex()[..8]));
        let config = build_info(hub_bin)["config"].as_str().unwrap().to_string();
        let bundle = config.split(':').nth(1).unwrap();
        let (hub, far_home) = (dir.join("h"), dir.join("f"));
        let far_root = far_home.join(".local/share/nearshell").join(bundle);
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::create_dir_all(&far_root).unwrap();
        // Stops a far core whatever build runs there (the far binary changes).
        let stop = dir.join("stop-nhc");
        std::fs::copy(NHC, &stop).unwrap();
        let env = |root: &Path| json!({"PATH": "/usr/bin:/bin", "NEARSHELL_HC_TEST_ROOT": root});
        let stop_cmd = [stop.to_str().unwrap(), "server", "stop"];
        let far_guardian = guardian::Guardian::with_command(&far_root, &stop_cmd, env(&far_root));
        let hub_guardian = guardian::Guardian::with_command(&hub, &stop_cmd, env(&hub));
        // A Mac's uname, for `mac`.
        let darwin = dir.join("darwin");
        std::fs::create_dir_all(&darwin).unwrap();
        std::fs::write(darwin.join("uname"), "#!/bin/sh\necho 'Darwin arm64'\n").unwrap();
        herdr_core::platform::make_executable(&darwin.join("uname")).unwrap();
        // `far`: an account whose login profile exports the user's own hc
        // session. `win`: a Windows box with cmd as its shell.
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\necho \"$1\" >> {d}/ssh.log\ntarget=$1; shift\nlogin() {{ exec env -i HOME={h} PATH=$1 \
                 HC_SESSION=user HC_SOCKET=/nowhere NEARSHELL_HC_TEST_ROOT={r} NEARSHELL_TEST_SESSION_ID={n} \
                 XDG_RUNTIME_DIR={h}/run sh -c \"$2\"; }}\ncase \"$target\" in\n  \
                 far) login /usr/bin:/bin \"$*\" ;;\n  \
                 mac) login {d}/darwin:/usr/bin:/bin \"$*\" ;;\n  \
                 win) case \"$1\" in cmd*) echo 'nearshell-windows-Windows_NT ' >&2; exit 127 ;; esac\n    \
                 echo \"'sh' is not recognized as an internal or external command,\" >&2; exit 1 ;;\n  \
                 *) echo \"ssh: Could not resolve hostname $target\" >&2; exit 255 ;;\nesac\n",
                d = dir.display(),
                h = far_home.display(),
                r = far_root.display(),
                n = far_guardian.nonce
            ),
        )
        .unwrap();
        herdr_core::platform::make_executable(&ssh).unwrap();
        let log = std::fs::File::create(dir.join("hub.log")).unwrap();
        let serve = Command::new(hub_bin)
            .arg("serve")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", dir.join("hub-home"))
            .env("XDG_RUNTIME_DIR", dir.join("hub-run"))
            .env("NEARSHELL_HC_TEST_ROOT", &hub)
            .env("NEARSHELL_TEST_SESSION_ID", &hub_guardian.nonce)
            .env("HC_RUNTIME", "1")
            .env("HC_SSH", &ssh)
            .env("HC_RECONNECT_MS", "100")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let world = World {
            dir,
            hub,
            hub_bin: hub_bin.into(),
            far_home,
            far_root,
            serve: Some(serve),
            hub_guardian,
            far_guardian,
        };
        eventually("hub core", || herdr_core::platform::connect(&world.hub.join("run/c.sock")).ok());
        world
    }

    pub fn rpc(&self, method: &str, params: Value) -> Result<Value, herdr_core::rpc::RpcError> {
        let stream = herdr_core::platform::connect(&self.hub.join("run/c.sock")).unwrap();
        herdr_core::client::call(&stream, method, params)
    }

    /// `<bin> --json <args>` against the hub, as a person's shell runs it.
    pub fn cli_as(&self, bin: &Path, args: &[&str]) -> Output {
        Command::new(bin)
            .arg("--json")
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NEARSHELL_HC_TEST_ROOT", &self.hub)
            .env("NEARSHELL_TEST_SESSION_ID", &self.hub_guardian.nonce)
            .env("HC_SSH", self.dir.join("ssh"))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    pub fn cli(&self, args: &[&str]) -> Output {
        self.cli_as(&self.hub_bin, args)
    }

    pub fn cli_error(&self, args: &[&str]) -> Value {
        error(self.cli(args))
    }

    /// The hub's config (a remote build reads one if it is there).
    pub fn hub_config(&self, text: &str) {
        let dir = self.hub.join("config/herdr-core");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), text).unwrap();
    }

    pub fn far_bin(&self) -> PathBuf {
        self.far_root.join("bin/nhc")
    }

    /// Runs `line` in the far account as the hub's ssh would, fed `input`.
    pub fn far(&self, line: &str, input: &[u8]) -> Output {
        use std::io::Write;
        let mut child = Command::new(self.dir.join("ssh"))
            .args(["far", line])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(input);
        child.wait_with_output().unwrap()
    }

    /// Puts `bin` (a build, or a script) where the line looks, swapped in whole.
    pub fn place(&self, bin: &Path) {
        let staged = self.far_root.join("bin/.staged");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::copy(bin, &staged).unwrap();
        herdr_core::platform::make_executable(&staged).unwrap();
        std::fs::rename(&staged, self.far_bin()).unwrap();
    }

    /// A stand-in that answers hello with `reply` and hangs up.
    pub fn answer(&self, mut reply: Value) {
        let script = self.dir.join("answer");
        (reply["jsonrpc"], reply["id"]) = ("2.0".into(), 1.into());
        let reply = reply.to_string().replace('\'', "");
        std::fs::write(&script, format!("#!/bin/sh\nread -r _\nprintf '%s\\n' '{reply}'\n")).unwrap();
        self.place(&script);
    }

    /// A payload directory with one remote core for this machine (`bin`, its
    /// entry changed by `edit`), set as the hub's `remote_payloads`.
    pub fn payloads(&self, name: &str, bin: &Path, edit: impl FnOnce(&mut Value)) -> PathBuf {
        use sha2::Digest;
        let dir = self.dir.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let bytes = std::fs::read(bin).unwrap();
        let file = format!("nhc-{}", host_target());
        std::fs::write(dir.join(&file), &bytes).unwrap();
        let sha: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        let mut entry = json!({"target": host_target(), "file": file, "bytes": bytes.len(), "sha256": sha,
            "build": build_info(bin), "executed": true});
        edit(&mut entry);
        let manifest = json!({"schemaVersion": 2, "entries": [entry]});
        std::fs::write(dir.join("manifest.json"), manifest.to_string()).unwrap();
        self.hub_config(&format!("remote_payloads = '{}'\n", dir.display()));
        dir
    }

    /// Stops whatever core runs there, as its own stop does.
    pub fn stop_far(&self) {
        let out = Command::new(self.dir.join("stop-nhc"))
            .args(["--json", "server", "stop"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NEARSHELL_HC_TEST_ROOT", &self.far_root)
            .output()
            .unwrap();
        value(out);
    }

    pub fn dials(&self) -> usize {
        std::fs::read_to_string(self.dir.join("ssh.log")).unwrap_or_default().lines().count()
    }

    pub fn view(&self, name: &str) -> Value {
        let list = self.rpc("machine.list", json!({})).unwrap();
        list["machines"].as_array().unwrap().iter().find(|m| m["name"] == name).cloned().unwrap_or_default()
    }

    pub fn state_becomes(&self, name: &str, state: &str) -> Value {
        eventually(state, || Some(self.view(name)).filter(|m| m["state"] == state))
    }

    /// Held: no dial for a while, though a line that is down redials every 100 ms.
    pub fn stays(&self, name: &str, state: &str) {
        let before = self.dials();
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(self.view(name)["state"], state);
        assert_eq!(self.dials(), before, "{name} was dialled again on its own");
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // The hub first: its lines end, and the far connects with them.
        let hub = self.hub_guardian.finish();
        if let Some(mut serve) = self.serve.take() {
            let _ = serve.kill();
            let _ = serve.wait();
        }
        let far = self.far_guardian.finish();
        if hub && far {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
        assert!(hub && far || std::thread::panicking(), "test_guardian_cleanup_unconfirmed");
    }
}

/// The remote core's target for this machine, as payload manifests name it.
pub fn host_target() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

pub fn value(out: Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

pub fn error(out: Output) -> Value {
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let text = String::from_utf8_lossy(&out.stderr);
    let last = text.lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default();
    serde_json::from_str::<Value>(last).unwrap_or_else(|_| panic!("{text}"))["error"].clone()
}

pub fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(found) = f() {
            return found;
        }
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The builds scripts/check-remote.sh froze: the hub's (`cur`), an older
/// release, another build of the same release, one built for another bundle.
pub fn variants() -> [(PathBuf, Value); 4] {
    let dir = PathBuf::from(std::env::var("NEARSHELL_REMOTE_TEST_BINS").expect("run scripts/check-remote.sh"));
    ["cur", "old", "fork", "other"].map(|name| {
        let bin = dir.join(format!("nhc-{name}"));
        let build = build_info(&bin);
        (bin, build)
    })
}
