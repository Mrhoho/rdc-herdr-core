//! A hub installing its remote core on a machine (`--features nearshell-test`,
//! Linux): only from its payload directory, checked before anything is sent,
//! sent by the install line over the fake ssh, and put in place by the copy
//! itself under the far root's install lock. Builds of differing identity come
//! from scripts/check-remote.sh (the ignored tests).
#![cfg(all(target_os = "linux", feature = "nearshell-test", not(feature = "managed")))]
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

#[path = "common/guardian.rs"]
#[allow(dead_code)] // Its stop modes are for other builds.
mod guardian;
#[path = "common/remote.rs"]
mod remote;
use remote::{NHC, World, build_info, error, eventually, value, variants};

fn bundle(bin: &Path) -> String {
    build_info(bin)["config"].as_str().unwrap().split(':').nth(1).unwrap().into()
}

fn build_of(v: &Value) -> herdr_core::api::BuildInfo {
    serde_json::from_value(v.clone()).unwrap()
}

/// The names in the far bin folder, sorted.
fn far_files(w: &World) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(w.far_root.join("bin"))
        .map(|d| d.map(|e| e.unwrap().file_name().into_string().unwrap()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

/// The hub's install line for `bin`, run in the far account as ssh would.
fn land(w: &World, bin: &Path) -> Output {
    let bytes = std::fs::read(bin).unwrap();
    let line = herdr_core::machines::install_line(&bundle(&w.hub_bin), bytes.len() as u64, &build_of(&build_info(bin)));
    w.far(&line, &bytes)
}

/// `<far nhc> --json <args>` in the far account.
fn far_nhc(w: &World, args: &str) -> Value {
    value(w.far(&format!("unset HC_SESSION HC_SOCKET; {} --json {args}", w.far_bin().display()), b""))
}

#[test]
fn a_hub_installs_only_its_checked_payload_and_the_copy_puts_itself_in_place() {
    let w = World::new(Path::new(NHC));
    let here = build_info(Path::new(NHC));
    let add = ["machine", "add", "far", "--install"];
    // Every refusal comes before a byte is sent: nothing appears there.
    let refused = |kind: &str| {
        let e = w.cli_error(&add);
        assert_eq!(e["kind"], kind, "{e}");
        assert!(!w.far_root.join("bin").exists(), "{kind}: something was sent");
        e
    };
    w.hub_config("remote_payloads = 'relative/dir'\n");
    refused("remote_payloads_invalid");
    w.hub_config(&format!("remote_payloads = '{}'\n", w.dir.join("nowhere").display()));
    refused("remote_payloads_unreadable");
    let other = if std::env::consts::ARCH == "x86_64" { "aarch64" } else { "x86_64" };
    w.payloads("p-target", Path::new(NHC), |e| e["target"] = format!("{other}-unknown-linux-musl").into());
    refused("remote_payload_no_target");
    w.payloads("p-bytes", Path::new(NHC), |e| e["bytes"] = (e["bytes"].as_u64().unwrap() + 1).into());
    assert!(refused("remote_payload_mismatch")["message"].as_str().unwrap().contains("the manifest says"));
    w.payloads("p-sha", Path::new(NHC), |e| e["sha256"] = "0".repeat(64).into());
    refused("remote_payload_mismatch");
    w.payloads("p-build", Path::new(NHC), |e| e["build"]["releaseSeq"] = 99.into());
    refused("remote_payload_build");
    w.payloads("p", Path::new(NHC), |_| {});
    // Darwin there: no remote core for it yet.
    let e = w.cli_error(&["machine", "add", "mac", "--install"]);
    assert_eq!(
        (&e["kind"], &e["message"]),
        (&json!("unsupported_platform"), &json!("mac runs macos aarch64, which Nearshell does not support yet"))
    );
    assert!(!w.far_root.join("bin").exists());
    // Left by failed attempts: a copy of a shell that is gone goes, a live one stays.
    let bin = w.far_root.join("bin");
    herdr_core::platform::private_dir(&bin).unwrap();
    let (gone, live) = (bin.join(".nhc.new.999999999"), bin.join(format!(".nhc.new.{}", std::process::id())));
    for stale in [&gone, &live] {
        std::fs::write(stale, b"partial").unwrap();
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(stale).unwrap().set_modified(hour_ago).unwrap();
    }
    // An empty machine: installed, then added, connected as that build.
    let out = w.cli(&add);
    let added = value(out.clone());
    assert_eq!(added["build"], here);
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains(&format!("far: installed its remote core (release {})", here["releaseSeq"]))
    );
    w.state_becomes("far", "connected");
    assert_eq!(std::fs::read(w.far_bin()).unwrap(), std::fs::read(NHC).unwrap());
    assert_eq!(far_files(&w), [live.file_name().unwrap().to_str().unwrap(), "nhc"]);
    std::fs::remove_file(&live).unwrap();
    use std::os::unix::fs::PermissionsExt;
    for private in [&bin, &w.far_bin()] {
        assert_eq!(std::fs::metadata(private).unwrap().permissions().mode() & 0o777, 0o700, "{private:?}");
    }
    assert!(!w.far_home.join(".local/bin").exists(), "nothing beside the user's own hc");
    let t = w.rpc("terminal.run", json!({"on": "far", "argv": ["sleep", "60"]})).unwrap();
    let address = t["address"].as_str().unwrap().to_string();
    let pid = w.rpc("terminal.get", json!({"terminal": address})).unwrap()["pid"].clone();
    // The same build again: nothing to do.
    let same = value(w.cli(&["machine", "update", "far"]));
    assert_eq!(
        (&same["replaced"], &same["restarted"], &same["was"], &same["state"]),
        (&json!(false), &json!(false), &here, &json!("connected"))
    );
    // A broken copy there (the core keeps running its own): replaced, nothing restarted.
    let broken = w.dir.join("broken");
    std::fs::write(&broken, b"\x7fELF but cut short").unwrap();
    w.place(&broken);
    let fixed = value(w.cli(&["machine", "update", "far"]));
    assert_eq!((&fixed["replaced"], &fixed["was"], &fixed["build"]), (&json!(true), &Value::Null, &here));
    assert_eq!(std::fs::read(w.far_bin()).unwrap(), std::fs::read(NHC).unwrap());
    assert_eq!(far_files(&w), ["nhc"]);
    let kept = w.rpc("terminal.get", json!({"terminal": address})).unwrap();
    assert_eq!((&kept["state"], &kept["pid"]), (&json!("live"), &pid));
    // A stream that ends early: no copy, no change, nothing left.
    let bytes = std::fs::read(NHC).unwrap();
    let line = herdr_core::machines::install_line(&bundle(Path::new(NHC)), bytes.len() as u64, &build_of(&here));
    let short = w.far(&line, &bytes[..bytes.len() / 2]);
    assert_eq!(short.status.code(), Some(3));
    assert_eq!(String::from_utf8_lossy(&short.stderr).trim(), "the copy arrived incomplete");
    assert_eq!(far_files(&w), ["nhc"]);
    assert_eq!(std::fs::read(w.far_bin()).unwrap(), bytes);
    // Upstream's way to swap a binary is no way around the install.
    for args in ["update --from /bin/true", "update --from /bin/true --force"] {
        let e = error(w.far(&format!("{} --json {args}", w.far_bin().display()), b""));
        assert_eq!(e["kind"], "managed_distribution", "{args}");
    }
    assert_eq!(far_nhc(&w, "server status")["build"], here);
}

#[test]
#[ignore = "scripts/check-remote.sh supplies builds of differing identity"]
fn an_update_keeps_terminals_and_an_install_never_goes_back_or_sideways() {
    let [(cur, cur_b), (old, old_b), (fork, _), (other, _)] = variants();
    let w = World::new(&cur);
    w.payloads("p", &cur, |_| {});
    w.place(&cur);
    w.rpc("machine.add", json!({"target": "far"})).unwrap();
    w.state_becomes("far", "connected");
    // An older release takes its place, with a terminal running on it.
    w.place(&old);
    w.stop_far();
    w.state_becomes("far", "needs_update");
    let t = far_nhc(&w, "run --bg sleep 60");
    let address = t["address"].as_str().unwrap().to_string();
    let pid = far_nhc(&w, &format!("get {address}"))["pid"].clone();
    // An nhc older than the core it talks to does not install anything.
    let stale_cli = error(w.cli_as(&old, &["machine", "update", "far"]));
    assert_eq!(stale_cli["kind"], "core_build_mismatch", "{stale_cli}");
    assert!(stale_cli["hint"].as_str().unwrap().contains("server restart"));
    assert_eq!(build_info(&w.far_bin()), old_b);
    let updated = value(w.cli(&["machine", "update", "far"]));
    assert_eq!((&updated["was"], &updated["build"]), (&old_b, &cur_b));
    assert_eq!(
        (&updated["replaced"], &updated["restarted"], &updated["state"]),
        (&json!(true), &json!(true), &json!("connected"))
    );
    let kept = far_nhc(&w, &format!("get {address}"));
    assert_eq!((&kept["state"], &kept["pid"]), (&json!("live"), &pid), "the terminal outlived the update");
    assert_eq!(far_nhc(&w, "server status")["build"], cur_b);
    assert_eq!(w.view("far")["build"], cur_b);
    // The copy itself refuses an older release and another build of this one,
    // under any name it is sent: nothing replaced, nothing restarted, nothing left.
    let core = far_nhc(&w, "server status")["pid"].clone();
    for (bin, kind) in [(&old, "machine_newer"), (&fork, "build_conflict"), (&other, "build_conflict")] {
        let e = error(land(&w, bin));
        assert_eq!(e["kind"], kind, "{e}");
        assert_eq!(build_info(&w.far_bin()), cur_b);
        assert_eq!(far_files(&w), ["nhc"]);
    }
    // A newer core still running counts as much as its file.
    std::fs::remove_file(w.far_bin()).unwrap();
    assert_eq!(error(land(&w, &old))["kind"], "machine_newer");
    assert_eq!(far_files(&w), Vec::<String>::new());
    w.place(&cur);
    assert_eq!(far_nhc(&w, "server status")["pid"], core);
    // A machine held for another build is left as it is.
    w.place(&fork);
    w.stop_far();
    let held = w.state_becomes("far", "build_conflict");
    let before = w.dials();
    let e = w.cli_error(&["machine", "update", "far"]);
    assert_eq!(
        (&e["kind"], &e["message"]),
        (&json!("build_conflict"), &json!(format!("far: {}", held["why"].as_str().unwrap())))
    );
    assert!(e["hint"].as_str().unwrap().starts_with("not handled automatically"), "{e}");
    assert_eq!(w.dials(), before, "refused before anything reached it");
    drop(w);
    // A hub older than the remote core there: the hub is the one to update.
    let w = World::new(&old);
    w.payloads("p", &old, |_| {});
    w.place(&old);
    w.rpc("machine.add", json!({"target": "far"})).unwrap();
    w.place(&cur);
    w.stop_far();
    w.state_becomes("far", "machine_newer");
    let e = w.cli_error(&["machine", "update", "far"]);
    assert_eq!((&e["kind"], &e["hint"]), (&json!("machine_newer"), &json!("update Nearshell on this Mac")));
    assert_eq!(build_info(&w.far_bin()), cur_b);
}

#[test]
#[ignore = "scripts/check-remote.sh supplies builds of differing identity"]
fn concurrent_installs_take_turns_and_never_leave_an_older_build() {
    let [(cur, cur_b), (old, _), _, _] = variants();
    let w = World::new(&cur);
    // Holding the install lock, as an install in progress there would.
    let lock_path = w.far_root.join("install.lock");
    let lock = std::fs::File::options().create(true).truncate(false).write(true).open(&lock_path).unwrap();
    assert!(herdr_core::platform::try_lock(&lock));
    let start = |bin: &PathBuf| {
        let bytes = std::fs::read(bin).unwrap();
        let line = herdr_core::machines::install_line(&bundle(&cur), bytes.len() as u64, &build_of(&build_info(bin)));
        let mut child = Command::new(w.dir.join("ssh"))
            .args(["far", &line])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(&bytes).unwrap();
        (child, bytes.len() as u64)
    };
    let (mut a, a_len) = start(&old);
    let (mut b, b_len) = start(&cur);
    // Both copies arrive whole, then wait for the lock.
    eventually("both copies sent", || {
        let mut sizes: Vec<u64> =
            std::fs::read_dir(w.far_root.join("bin")).ok()?.map(|e| e.unwrap().metadata().unwrap().len()).collect();
        sizes.sort();
        let mut want = vec![a_len, b_len];
        want.sort();
        (sizes == want).then_some(())
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(a.try_wait().unwrap().is_none() && b.try_wait().unwrap().is_none(), "one went past the lock");
    assert!(!w.far_bin().exists());
    drop(lock);
    let (a, b) = (a.wait_with_output().unwrap(), b.wait_with_output().unwrap());
    // The newer one always lands; the older one only before it (then updated over).
    assert!(b.status.success(), "{}", String::from_utf8_lossy(&b.stderr));
    if !a.status.success() {
        assert_eq!(error(a)["kind"], "machine_newer");
    }
    assert_eq!(build_info(&w.far_bin()), cur_b);
    assert_eq!(far_files(&w), ["nhc"]);
    assert!(lock_path.exists(), "the lock stays where every install finds it");
}
