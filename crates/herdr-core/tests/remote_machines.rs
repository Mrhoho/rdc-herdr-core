//! A hub dialing a remote core (`--features nearshell-test`, Linux): over a
//! fake ssh, by the absolute path in its own root, past a login profile that
//! exports the user's hc session, and through the build gate. Several builds
//! of differing identity come from scripts/check-remote.sh (the ignored test).
#![cfg(all(target_os = "linux", feature = "nearshell-test", not(feature = "managed")))]
use serde_json::{Value, json};
use std::path::Path;

#[path = "common/guardian.rs"]
#[allow(dead_code)] // Its stop modes are for other builds.
mod guardian;
#[path = "common/remote.rs"]
mod remote;
use remote::{NHC, World, build_info, value, variants};

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
    // The CLI takes it to the install path, which needs this hub's payloads.
    assert_eq!(w.cli_error(&["machine", "add", "far", "--install"])["kind"], "remote_payloads_unset");
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
                // Its line dials on its own: one still starting up when the
                // far core stops would start that core again.
                w.state_becomes("far", "connected");
                w.rpc("machine.rm", json!({"machine": "far"})).unwrap();
            }
            Err(e) => assert_eq!(e.kind, kind, "{e:?}"),
        }
        // Only a missing or older remote core goes to the installer (which
        // needs payloads this hub has none of).
        if kind != "ok" {
            let installs = if kind == "needs_update" { "remote_payloads_unset" } else { kind };
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
