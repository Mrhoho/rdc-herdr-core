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
}

impl Far {
    fn new(s: &Scratch, bundle: &str) -> Far {
        let home = s.0.join("far");
        let bin = home.join(".local/share/nearshell").join(bundle).join("bin/nhc");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        let login = s.0.join("login");
        std::fs::write(
            &login,
            format!(
                "#!/bin/sh\nshell=$1; shift\nexport HOME='{}' HC_SESSION=user HC_SOCKET=/nowhere\nexec \"$shell\" -c \"$1\"\n",
                home.display()
            ),
        )
        .unwrap();
        herdr_core::platform::make_executable(&login).unwrap();
        Far { home, bin, login }
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
    let shells: Vec<PathBuf> = ["sh", "bash", "zsh", "dash", "ksh", "csh", "tcsh", "fish"]
        .iter()
        .map(|sh| Path::new("/bin").join(sh))
        .filter(|sh| sh.exists())
        .collect();
    assert!(shells.len() >= 2, "{shells:?}");
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
    assert_eq!(missing["kind"], "managed_distribution", "the install path, still refused in this build: {missing}");
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
