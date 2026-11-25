//! A hub dialing a remote core (`--features nearshell-test`, Linux): over a
//! fake ssh, by the absolute path in its own root, past a login profile that
//! exports the user's hc session, and through the build gate. Any core can
//! add machines, so a remote build is the hub here; a test build honours
//! HC_SSH. The far account gets its own HOME and test root, the binary goes
//! where the line looks for it. Several builds of differing identity come
//! from scripts/check-remote.sh (the ignored test).
#![cfg(all(target_os = "linux", feature = "nearshell-test", not(feature = "managed")))]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[path = "common/guardian.rs"]
#[allow(dead_code)] // Its stop modes are for other builds.
mod guardian;

const NHC: &str = env!("CARGO_BIN_EXE_nhc");

/// A hub core (`serve`, started here so that HC_SSH reaches it) and one far
/// account it reaches as `far` over the fake ssh (or as `win`, a Windows box).
struct World {
    dir: PathBuf,
    hub: PathBuf,
    hub_bin: PathBuf,
    far_root: PathBuf,
    serve: Option<Child>,
    hub_guardian: guardian::Guardian,
    far_guardian: guardian::Guardian,
}

fn build_info(bin: &Path) -> Value {
    value(Command::new(bin).args(["build-info", "--json"]).env_clear().output().unwrap())
}

impl World {
    fn new(hub_bin: &Path) -> World {
        let dir = PathBuf::from(format!("/var/tmp/nm-{}", &herdr_core::platform::random_hex()[..8]));
        let config = build_info(hub_bin)["config"].as_str().unwrap().to_string();
        let bundle = config.split(':').nth(1).unwrap();
        let (hub, far_home) = (dir.join("h"), dir.join("f"));
        let far_root = far_home.join(".local/share/nearshell").join(bundle);
        std::fs::create_dir_all(&hub).unwrap();
        std::fs::create_dir_all(far_root.join("bin")).unwrap();
        // Stops a far core whatever build runs there (the far binary changes).
        let stop = dir.join("stop-nhc");
        std::fs::copy(NHC, &stop).unwrap();
        let env = |root: &Path| json!({"PATH": "/usr/bin:/bin", "NEARSHELL_HC_TEST_ROOT": root});
        let stop_cmd = [stop.to_str().unwrap(), "server", "stop"];
        let far_guardian = guardian::Guardian::with_command(&far_root, &stop_cmd, env(&far_root));
        let hub_guardian = guardian::Guardian::with_command(&hub, &stop_cmd, env(&hub));
        // `far`: an account whose login profile exports the user's own hc
        // session. `win`: a Windows box with cmd as its shell.
        let ssh = dir.join("ssh");
        std::fs::write(
            &ssh,
            format!(
                "#!/bin/sh\necho \"$1\" >> {d}/ssh.log\ntarget=$1; shift\ncase \"$target\" in\n  \
                 far) exec env -i HOME={h} PATH=/usr/bin:/bin HC_SESSION=user HC_SOCKET=/nowhere \
                 NEARSHELL_HC_TEST_ROOT={r} NEARSHELL_TEST_SESSION_ID={n} XDG_RUNTIME_DIR={h}/run sh -c \"$*\" ;;\n  \
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
        let world =
            World { dir, hub, hub_bin: hub_bin.into(), far_root, serve: Some(serve), hub_guardian, far_guardian };
        eventually("hub core", || herdr_core::platform::connect(&world.hub.join("run/c.sock")).ok());
        world
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value, herdr_core::rpc::RpcError> {
        let stream = herdr_core::platform::connect(&self.hub.join("run/c.sock")).unwrap();
        herdr_core::client::call(&stream, method, params)
    }

    /// `nhc --json <args>` against the hub, as a person's shell runs it.
    fn cli(&self, args: &[&str]) -> Output {
        Command::new(&self.hub_bin)
            .arg("--json")
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NEARSHELL_HC_TEST_ROOT", &self.hub)
            .env("NEARSHELL_TEST_SESSION_ID", &self.hub_guardian.nonce)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn cli_error(&self, args: &[&str]) -> Value {
        let out = self.cli(args);
        assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        serde_json::from_slice::<Value>(&out.stderr).unwrap()["error"].clone()
    }

    fn far_bin(&self) -> PathBuf {
        self.far_root.join("bin/nhc")
    }

    /// Puts `bin` (a build, or a script) where the line looks, swapped in whole.
    fn place(&self, bin: &Path) {
        let staged = self.far_root.join("bin/.staged");
        std::fs::copy(bin, &staged).unwrap();
        herdr_core::platform::make_executable(&staged).unwrap();
        std::fs::rename(&staged, self.far_bin()).unwrap();
    }

    /// A stand-in that answers hello with `reply` and hangs up.
    fn answer(&self, mut reply: Value) {
        let script = self.dir.join("answer");
        (reply["jsonrpc"], reply["id"]) = ("2.0".into(), 1.into());
        let reply = reply.to_string().replace('\'', "");
        std::fs::write(&script, format!("#!/bin/sh\nread -r _\nprintf '%s\\n' '{reply}'\n")).unwrap();
        self.place(&script);
    }

    /// Stops whatever core runs there, as its own stop does.
    fn stop_far(&self) {
        let out = Command::new(self.dir.join("stop-nhc"))
            .args(["--json", "server", "stop"])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("NEARSHELL_HC_TEST_ROOT", &self.far_root)
            .output()
            .unwrap();
        value(out);
    }

    fn dials(&self) -> usize {
        std::fs::read_to_string(self.dir.join("ssh.log")).unwrap_or_default().lines().count()
    }

    fn view(&self, name: &str) -> Value {
        let list = self.rpc("machine.list", json!({})).unwrap();
        list["machines"].as_array().unwrap().iter().find(|m| m["name"] == name).cloned().unwrap_or_default()
    }

    fn state_becomes(&self, name: &str, state: &str) -> Value {
        eventually(state, || Some(self.view(name)).filter(|m| m["state"] == state))
    }

    /// Held: no dial for a while, though a line that is down redials every 100 ms.
    fn stays(&self, name: &str, state: &str) {
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

fn value(out: Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn eventually<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(found) = f() {
            return found;
        }
        assert!(Instant::now() < end, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn hello_without_build() -> Value {
    json!({"result": {"generation": herdr_core::api::GENERATION, "version": "0.1.22",
        "core": {"id": "stand-in", "name": "far"}, "framed": true, "methods": [], "features": []}})
}

fn another_generation() -> Value {
    json!({"error": {"code": -32000, "message": "no", "data": {"kind": "unsupported_generation", "hint": ""}}})
}

#[test]
fn the_same_build_connects_by_its_path_over_ssh_and_via() {
    let w = World::new(Path::new(NHC));
    w.place(Path::new(NHC));
    let far = build_info(&w.far_bin());
    // --via: the user's own command carries the line there as one argument.
    let via = format!("{} far", w.dir.join("ssh").display());
    let added = w.rpc("machine.add", json!({"via": via, "name": "farvia"})).unwrap();
    assert_eq!(added["build"], far);
    w.state_becomes("farvia", "connected");
    w.rpc("machine.rm", json!({"machine": "farvia"})).unwrap();
    let added = value(w.cli(&["machine", "add", "far"]));
    assert_eq!((&added["name"], &added["build"]), (&json!("far"), &far));
    w.state_becomes("far", "connected");
    // The core there is the one in its root, whatever the profile exported.
    let exec = w.rpc("machine.exec", json!({"on": "far", "line": "echo \"$NEARSHELL_HC\""})).unwrap();
    let out = herdr_core::put::decode(exec["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(String::from_utf8(out).unwrap().trim(), w.far_bin().display().to_string());
    let listed = value(w.cli(&["machine", "ls"]));
    assert_eq!((&listed["machines"][0]["state"], &listed["machines"][0]["build"]), (&json!("connected"), &far));
    // Saved with it: a hub restart still knows which build runs there.
    let saved: Value =
        serde_json::from_slice(&std::fs::read(w.hub.join("state/herdr-core/default/machines.json")).unwrap()).unwrap();
    assert_eq!(saved[0]["build"], far);
}

#[test]
fn a_missing_core_is_hc_missing_after_one_dial_and_windows_is_not_supported() {
    let w = World::new(Path::new(NHC));
    let e = w.rpc("machine.add", json!({"target": "far"})).unwrap_err();
    assert_eq!(e.kind, "hc_missing", "{e:?}");
    assert_eq!(w.dials(), 1, "the line's marker spares it a second dial as Windows");
    // The CLI takes it to the install path (still refused in this build).
    assert_eq!(w.cli_error(&["machine", "add", "far", "--install"])["kind"], "managed_distribution");
    assert_eq!(w.cli_error(&["machine", "add", "far", "--session", "s1"])["kind"], "usage");
    let e = w.rpc("machine.add", json!({"target": "far", "session": "s1"})).unwrap_err();
    assert_eq!(e.kind, "invalid_params");
    let before = w.dials();
    let e = w.rpc("machine.add", json!({"target": "win"})).unwrap_err();
    assert_eq!(e.kind, "unsupported_platform", "{e:?}");
    assert_eq!(w.dials(), before + 2, "the Unix line, then the probe");
    assert_eq!(w.rpc("machine.add", json!({"target": "win"})).unwrap_err().kind, "unsupported_platform");
    assert_eq!(w.dials(), before + 3, "known to be Windows: straight to the probe");
    assert_eq!(w.cli_error(&["machine", "add", "win", "--install"])["kind"], "unsupported_platform");
}

#[test]
fn a_refused_core_is_held_until_machine_connect() {
    let w = World::new(Path::new(NHC));
    w.place(Path::new(NHC));
    w.rpc("machine.add", json!({"target": "far"})).unwrap();
    w.state_becomes("far", "connected");
    // Something without a build takes its place; the line drops and redials.
    w.answer(hello_without_build());
    w.stop_far();
    let held = w.state_becomes("far", "build_conflict");
    assert!(held["why"].as_str().unwrap().contains("without a build identity"), "{held}");
    w.stays("far", "build_conflict");
    let routed = w.rpc("machine.exec", json!({"on": "far", "argv": ["true"]})).unwrap_err();
    assert_eq!(routed.kind, "unavailable", "{routed:?}");
    // machine.connect dials once more, and reports what answers now.
    w.answer(another_generation());
    let before = w.dials();
    assert_eq!(w.rpc("machine.connect", json!({"machine": "far"})).unwrap()["state"], "incompatible");
    assert_eq!(w.dials(), before + 1);
    w.stays("far", "incompatible");
    w.place(Path::new(NHC));
    assert_eq!(w.rpc("machine.connect", json!({"machine": "far"})).unwrap()["state"], "connected");
    // Adding one: the CLI never takes a conflict or another generation to the installer.
    w.rpc("machine.rm", json!({"machine": "far"})).unwrap();
    w.stop_far();
    for (reply, kind) in [(hello_without_build(), "build_conflict"), (another_generation(), "incompatible")] {
        w.answer(reply);
        let e = w.cli_error(&["machine", "add", "far", "--install"]);
        assert_eq!(e["kind"], kind, "{e}");
        assert!(e["hint"].as_str().unwrap().starts_with("not handled automatically"), "{e}");
    }
}

/// The builds scripts/check-remote.sh froze: the hub's (`cur`), an older
/// release, another build of the same release, one built for another bundle.
fn variants() -> [(PathBuf, Value); 4] {
    let dir = PathBuf::from(std::env::var("NEARSHELL_REMOTE_TEST_BINS").expect("run scripts/check-remote.sh"));
    ["cur", "old", "fork", "other"].map(|name| {
        let bin = dir.join(format!("nhc-{name}"));
        let build = build_info(&bin);
        (bin, build)
    })
}

#[test]
#[ignore = "scripts/check-remote.sh supplies builds of differing identity"]
fn each_remote_build_gets_its_outcome_and_only_connect_redials() {
    let [(cur, cur_b), (old, old_b), (fork, fork_b), (other, other_b)] = variants();
    let seq = |b: &Value| b["releaseSeq"].as_u64().unwrap();
    assert!(seq(&old_b) < seq(&cur_b) && seq(&fork_b) == seq(&cur_b) && fork_b["commit"] != cur_b["commit"]);
    assert!(other_b["config"] != cur_b["config"] && seq(&other_b) == seq(&cur_b));
    let w = World::new(&cur);
    let cases = [(&cur, "ok"), (&old, "needs_update"), (&fork, "build_conflict"), (&other, "build_conflict")];
    for (bin, kind) in cases {
        w.place(bin);
        match w.rpc("machine.add", json!({"target": "far"})) {
            Ok(m) => {
                assert_eq!((kind, &m["build"]), ("ok", &cur_b));
                w.rpc("machine.rm", json!({"machine": "far"})).unwrap();
            }
            Err(e) => assert_eq!(e.kind, kind, "{e:?}"),
        }
        // Only a missing or older remote core goes to the installer.
        if kind != "ok" {
            let installs = if kind == "needs_update" { "managed_distribution" } else { kind };
            assert_eq!(w.cli_error(&["machine", "add", "far", "--install"])["kind"], installs);
        }
        w.stop_far();
    }
    // Held after a drop: each refused build waits for machine.connect.
    w.place(&cur);
    w.rpc("machine.add", json!({"target": "far"})).unwrap();
    w.state_becomes("far", "connected");
    for (bin, state) in [(&old, "needs_update"), (&fork, "build_conflict"), (&other, "build_conflict")] {
        w.place(bin);
        w.stop_far();
        let held = w.state_becomes("far", state);
        assert_eq!(held["build"], cur_b, "the record keeps the build it last connected to");
        w.stays("far", state);
        let routed = w.rpc("machine.exec", json!({"on": "far", "argv": ["true"]})).unwrap_err();
        assert_eq!(routed.kind, if state == "needs_update" { "needs_update" } else { "unavailable" });
        let before = w.dials();
        assert_eq!(w.rpc("machine.connect", json!({"machine": "far"})).unwrap()["state"], state);
        assert_eq!(w.dials(), before + 1);
        w.place(&cur);
        w.stop_far();
        assert_eq!(w.rpc("machine.connect", json!({"machine": "far"})).unwrap()["state"], "connected");
    }
    drop(w);
    // A hub older than the remote core there: this hub is the one to update.
    let w = World::new(&old);
    w.place(&old);
    w.rpc("machine.add", json!({"target": "far"})).unwrap();
    w.state_becomes("far", "connected");
    w.place(&cur);
    w.stop_far();
    let held = w.state_becomes("far", "machine_newer");
    assert_eq!(held["build"], old_b);
    w.stays("far", "machine_newer");
    w.rpc("machine.rm", json!({"machine": "far"})).unwrap();
    let e = w.cli_error(&["machine", "add", "far", "--install"]);
    assert_eq!((&e["kind"], &e["hint"]), (&json!("machine_newer"), &json!("update Nearshell on this Mac")));
}
