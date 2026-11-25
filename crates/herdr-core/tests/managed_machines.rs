//! A managed hub dialing a machine (macOS, no Linux far end): its line reaches
//! the remote core by absolute path under each login shell here, through a
//! `--via` command, and the build gate decides from that core's `hello`.
#![cfg(all(unix, feature = "managed-test"))]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[path = "common/guardian.rs"]
mod guardian;
#[path = "common/managed.rs"]
mod scratch;
use scratch::{Scratch, rpc, value};

/// A far account: its home, where the line looks for nhc, and a login that
/// runs the line as sshd would (`<shell> -c <line>`) after a profile that
/// exports the user's own hc session.
struct Far {
    home: PathBuf,
    bin: PathBuf,
    login: PathBuf,
    /// The same login with a `uname` that says `Linux aarch64`.
    linux: PathBuf,
}

impl Far {
    fn new(s: &Scratch, bundle: &str) -> Far {
        let home = s.0.join("far");
        let bin = home.join(".local/share/nearshell").join(bundle).join("bin/nhc");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        let fake = s.0.join("fake-linux");
        std::fs::create_dir_all(&fake).unwrap();
        std::fs::write(fake.join("uname"), "#!/bin/sh\necho 'Linux aarch64'\n").unwrap();
        herdr_core::platform::make_executable(&fake.join("uname")).unwrap();
        let login = |name: &str, path: String| {
            let login = s.0.join(name);
            std::fs::write(
                &login,
                format!(
                    "#!/bin/sh\nshell=$1; shift\nexport HOME='{}' HC_SESSION=user HC_SOCKET=/nowhere{path}\nexec \"$shell\" -c \"$1\"\n",
                    home.display()
                ),
            )
            .unwrap();
            herdr_core::platform::make_executable(&login).unwrap();
            login
        };
        let (login, linux) =
            (login("login", String::new()), login("login-linux", format!(" PATH='{}':$PATH", fake.display())));
        Far { home, bin, login, linux }
    }

    /// A stand-in for the remote core: notes how it was started, answers
    /// hello with `reply`, then stays until the hub hangs up (a line that
    /// closed at once would be dialled again at once).
    fn answer(&self, reply: Value) {
        let script = format!(
            "#!/bin/sh\necho \"$* ${{HC_SESSION-unset}} ${{HC_SOCKET-unset}}\" > '{}'\nread -r _\nprintf '%s\\n' '{}'\nexec cat > /dev/null\n",
            self.home.join("ran").display(),
            reply.to_string().replace('\'', "")
        );
        std::fs::write(&self.bin, script).unwrap();
        herdr_core::platform::make_executable(&self.bin).unwrap();
    }

    fn via(&self, shell: &Path) -> String {
        format!("{} {}", self.login.display(), shell.display())
    }

    fn via_linux(&self, shell: &Path) -> String {
        format!("{} {}", self.linux.display(), shell.display())
    }

    /// What is in the remote core's bin folder there, sorted.
    fn files(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.bin.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }
}

fn login_shells() -> Vec<PathBuf> {
    let shells: Vec<PathBuf> = ["sh", "bash", "zsh", "dash", "ksh", "csh", "tcsh", "fish"]
        .iter()
        .map(|sh| Path::new("/bin").join(sh))
        .filter(|sh| sh.exists())
        .collect();
    assert!(shells.len() >= 2, "{shells:?}");
    shells
}

fn hello(build: Value) -> Value {
    let result = json!({"generation": herdr_core::api::GENERATION, "version": env!("CARGO_PKG_VERSION"), "build": build,
        "core": {"id": "far-core-id", "name": "far"}, "framed": true, "methods": [], "features": []});
    json!({"jsonrpc": "2.0", "id": 1, "result": result})
}

#[test]
fn a_managed_hub_reaches_its_remote_core_by_path_and_gates_on_its_build() {
    let s = Scratch::new();
    s.config(&format!("socket = '{}'", s.socket().display()));
    s.start();
    let here = value(s.cmd().args(["build-info", "--json"]).output().unwrap());
    let config = here["config"].as_str().unwrap();
    let bundle = config.split(':').nth(1).unwrap();
    let far = Far::new(&s, bundle);
    let add = |via: String| rpc(&s, "machine.add", json!({"via": via, "name": "far"}));
    let shells = login_shells();
    for shell in &shells {
        // Nothing there: the marker and 127, from that shell, through --via.
        let _ = std::fs::remove_file(&far.bin);
        assert_eq!(add(far.via(shell)).unwrap_err().kind, "hc_missing", "{shell:?}");
        // Something there without a build (an upstream hc put at that path).
        far.answer(hello(Value::Null));
        let refused = add(far.via(shell)).unwrap_err();
        assert_eq!(refused.kind, "build_conflict", "{shell:?}: {refused:?}");
        let ran = std::fs::read_to_string(far.home.join("ran")).unwrap();
        assert_eq!(ran.trim(), "connect unset unset", "{shell:?}: exec'd by path, the profile's session unset");
    }
    let via = far.via(&shells[0]);
    let remote = config.replace(":scratch-test", ":remote:scratch-test");
    let build = |seq: u64, commit: &str, config: &str| json!({"commit": commit, "patchset": here["patchset"], "releaseSeq": seq, "config": config});
    let (seq, commit) = (here["releaseSeq"].as_u64().unwrap(), here["commit"].as_str().unwrap());
    for (build, kind) in [
        (build(seq + 1, commit, &remote), "machine_newer"),
        (build(seq, "another-commit", &remote), "build_conflict"),
        (build(seq, commit, config), "build_conflict"), // the managed config itself
    ] {
        far.answer(hello(build));
        assert_eq!(add(via.clone()).unwrap_err().kind, kind);
    }
    // Another protocol generation: hello itself refused.
    let refused = json!({"code": -32000, "message": "no", "data": {"kind": "unsupported_generation", "hint": ""}});
    far.answer(json!({"jsonrpc": "2.0", "id": 1, "error": refused}));
    assert_eq!(add(via.clone()).unwrap_err().kind, "incompatible");
    // The CLI installs only where the remote core is missing (or older); a
    // newer one is reported as is, never handed to the installer.
    let cli = |args: &[&str]| {
        let out = s.cmd().args(["--json", "machine", "add", "--via", &via, "--name", "far"]).args(args).output();
        serde_json::from_slice::<Value>(&out.unwrap().stderr).unwrap()["error"].clone()
    };
    far.answer(hello(build(seq + 1, commit, &remote)));
    let newer = cli(&["--install"]);
    assert_eq!((&newer["kind"], &newer["hint"]), (&json!("machine_newer"), &json!("update Nearshell on this Mac")));
    std::fs::remove_file(&far.bin).unwrap();
    let missing = cli(&["--install"]);
    assert_eq!(missing["kind"], "remote_payloads_unset", "the install path, with no payloads here: {missing}");
    assert_eq!(cli(&["--session", "s1"])["kind"], "usage");
    let session = rpc(&s, "machine.add", json!({"via": via, "name": "far", "session": "s1"}));
    assert_eq!(session.unwrap_err().kind, "invalid_params");
    // The same build, built for a remote: added, with the build it runs.
    let same = build(seq, commit, &remote);
    far.answer(hello(same.clone()));
    let added = add(via.clone()).unwrap();
    assert_eq!(added["build"], same);
    let listed = value(s.cmd().args(["--json", "machine", "ls"]).output().unwrap());
    assert_eq!(listed["machines"][0]["build"], same, "{listed}");
    rpc(&s, "machine.rm", json!({"machine": "far"})).unwrap();
}

/// A stand-in for a Linux remote core sent as a payload: it puts itself in
/// place as the real one does (and notes how it was run), then answers a dial.
fn stand_in(far: &Far, build: &Value) -> Vec<u8> {
    let landed = json!({"build": build, "was": null, "replaced": true, "restarted": false});
    format!(
        "#!/bin/sh\ncase \"$1\" in\n  connect) read -r _; printf '%s\\n' '{}'; exec cat > /dev/null ;;\n  \
         --json) echo \"$*\" > '{}'; mv \"$4\" \"$(dirname \"$4\")/nhc\"; printf '%s\\n' '{}' ;;\n  *) exit 9 ;;\nesac\n",
        hello(build.clone()).to_string().replace('\'', ""),
        far.home.join("landed").display(),
        landed.to_string().replace('\'', "")
    )
    .into_bytes()
}

#[test]
fn a_managed_hub_installs_its_checked_payload_through_via() {
    use sha2::Digest;
    let s = Scratch::new();
    let socket = format!("socket = '{}'", s.socket().display());
    s.config(&socket);
    s.start();
    let here = value(s.cmd().args(["build-info", "--json"]).output().unwrap());
    let bundle = here["config"].as_str().unwrap().split(':').nth(1).unwrap().to_string();
    let far = Far::new(&s, &bundle);
    let mut remote = here.clone();
    remote["config"] = here["config"].as_str().unwrap().replace(":scratch-test", ":remote:scratch-test").into();
    let shells = login_shells();
    let (sh, linux) = (&shells[0], far.via_linux(&shells[0]));
    let add = |via: &str| {
        s.cmd().args(["--json", "machine", "add", "--via", via, "--name", "far", "--install"]).output().unwrap()
    };
    let error = |out: std::process::Output| {
        assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        let text = String::from_utf8(out.stderr).unwrap();
        serde_json::from_str::<Value>(text.lines().last().unwrap()).unwrap()["error"].clone()
    };
    // A payload directory with one stand-in for Linux on aarch64.
    let payload = stand_in(&far, &remote);
    let dir = s.0.join("payloads");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("nhc-aarch64"), &payload).unwrap();
    let sha: String = sha2::Sha256::digest(&payload).iter().map(|b| format!("{b:02x}")).collect();
    let entry = json!({"target": "aarch64-unknown-linux-musl", "file": "nhc-aarch64", "bytes": payload.len(), "sha256": sha, "build": remote});
    let manifest = |entries: Value| {
        std::fs::write(dir.join("manifest.json"), json!({"schemaVersion": 2, "entries": entries}).to_string()).unwrap()
    };
    s.config(&format!("{socket}\nremote_payloads = '{}'", dir.display()));
    // Refused here, before a byte is sent: nothing arrives there.
    let changed = |edit: &dyn Fn(&mut Value)| {
        let mut e = entry.clone();
        edit(&mut e);
        json!([e])
    };
    for (entries, kind) in [
        (json!([]), "remote_payload_no_target"),
        (changed(&|e| e["target"] = "x86_64-unknown-linux-musl".into()), "remote_payload_no_target"),
        (changed(&|e| e["bytes"] = (payload.len() + 1).into()), "remote_payload_mismatch"),
        (changed(&|e| e["sha256"] = "0".repeat(64).into()), "remote_payload_mismatch"),
        (changed(&|e| e["file"] = "nhc-missing".into()), "remote_payload_mismatch"),
        (
            changed(&|e| e["build"]["releaseSeq"] = (here["releaseSeq"].as_u64().unwrap() + 1).into()),
            "remote_payload_build",
        ),
        (changed(&|e| e["build"]["config"] = here["config"].clone()), "remote_payload_build"),
        (changed(&|e| e["file"] = "../nhc-aarch64".into()), "remote_payloads_invalid"),
    ] {
        manifest(entries.clone());
        assert_eq!(error(add(&linux))["kind"], kind, "{entries}");
        assert!(far.files().is_empty(), "{kind}: {:?}", far.files());
    }
    std::fs::write(dir.join("manifest.json"), json!({"schemaVersion": 1, "entry": entry}).to_string()).unwrap();
    assert_eq!(error(add(&linux))["kind"], "remote_payloads_invalid");
    std::fs::remove_file(dir.join("manifest.json")).unwrap();
    assert_eq!(error(add(&linux))["kind"], "remote_payloads_unreadable");
    manifest(json!([entry]));
    // This Mac's own uname: no remote core for macOS.
    let mac = error(add(&far.via(sh)));
    assert_eq!(mac["kind"], "unsupported_platform");
    assert!(mac["message"].as_str().unwrap().contains(" runs macos "), "{mac}");
    assert!(far.files().is_empty());
    // Sent through --via, put in place by the copy itself, then added as that build.
    let out = add(&linux);
    let added = value(out.clone());
    assert_eq!(added["build"], remote);
    let said = String::from_utf8(out.stderr).unwrap();
    assert!(said.contains(&format!("installed its remote core (release {})", here["releaseSeq"])), "{said}");
    assert_eq!(std::fs::read(&far.bin).unwrap(), payload);
    assert_eq!(far.files(), ["nhc"]);
    let ran = std::fs::read_to_string(far.home.join("landed")).unwrap();
    let words: Vec<&str> = ran.split_whitespace().collect();
    assert_eq!(words[..3], ["--json", "install", "--from"]);
    let staged = Path::new(words[3]);
    assert_eq!(staged.parent(), far.bin.parent());
    assert!(staged.file_name().unwrap().to_str().unwrap().starts_with(".nhc.new."), "{ran}");
    assert_eq!(words[4], "--expect");
    let expected: herdr_core::api::BuildInfo = serde_json::from_value(remote.clone()).unwrap();
    assert_eq!(herdr_core::machines::parse_expect(words[5]), Some(expected.clone()));
    // An update takes the same path, and the machine answers as that build.
    let updated = value(s.cmd().args(["--json", "machine", "update", "far"]).output().unwrap());
    assert_eq!((&updated["build"], &updated["state"]), (&remote, &json!("connected")));
    rpc(&s, "machine.rm", json!({"machine": "far"})).unwrap();
    // The line itself under every login shell here: a whole copy lands, a short one leaves nothing.
    let line = herdr_core::machines::install_line(&bundle, payload.len() as u64, &expected);
    for shell in &shells {
        std::fs::remove_file(&far.bin).unwrap();
        let run = |input: &[u8]| {
            use std::io::Write;
            let mut child = std::process::Command::new(&far.linux)
                .arg(shell)
                .arg(&line)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let _ = child.stdin.take().unwrap().write_all(input);
            child.wait_with_output().unwrap()
        };
        let short = run(&payload[..payload.len() - 1]);
        assert_eq!(short.status.code(), Some(3), "{shell:?}: {}", String::from_utf8_lossy(&short.stderr));
        assert!(far.files().is_empty(), "{shell:?}: {:?}", far.files());
        let whole = run(&payload);
        assert!(whole.status.success(), "{shell:?}: {}", String::from_utf8_lossy(&whole.stderr));
        assert_eq!(
            (std::fs::read(&far.bin).unwrap(), far.files()),
            (payload.clone(), vec!["nhc".to_string()]),
            "{shell:?}"
        );
    }
}
