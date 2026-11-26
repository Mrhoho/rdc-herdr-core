//! What a machine may say about terminals: only its own. A machine that
//! forges another's address in its records or events shadows, removes or
//! tells nothing here. Also the names a machine can have, and where a run
//! sent there starts.
#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::{Value, json};
use std::io::BufRead;

/// rohan's remote core behind a relay that rewrites what it says (its hello,
/// replies and events) by the `[from, to]` pairs in `<rohan root>/forge`,
/// read anew for each message: a machine that lies. (The hub credits what it
/// got, so a longer event only gives rohan more credit.)
fn liar() -> (Session, Session) {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    let relay = format!(
        r#"#!/usr/bin/env python3
import json, os, struct, subprocess, sys
real, forge = {real:?}, {forge:?}
if sys.argv[1:] != ["connect"]:
    os.execv(real, [real] + sys.argv[1:])
p = subprocess.Popen([real, "connect"], stdout=subprocess.PIPE)
src, out = p.stdout, sys.stdout.buffer
def forged(text):
    if os.path.exists(forge):
        for a, b in json.load(open(forge)):
            text = text.replace(a, b)
    return text
out.write(forged(src.readline().decode()).encode()); out.flush()
while True:
    head = src.read(9)
    if len(head) < 9:
        break
    channel, kind, n = struct.unpack("<IBI", head)
    body = src.read(n)
    if kind == 0:
        try:
            body = forged(body.decode()).encode()
        except UnicodeDecodeError:
            pass
    out.write(struct.pack("<IBI", channel, kind, len(body)) + body); out.flush()
sys.exit(p.wait())
"#,
        real = rohan.bin.display().to_string(),
        forge = rohan.root.join("forge").display().to_string(),
    );
    std::fs::create_dir_all(rohan.root.join("fakebin")).unwrap();
    let path = rohan.root.join("fakebin/herdr-core");
    std::fs::write(&path, relay).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    (rohan, s)
}

fn forge(rohan: &Session, pairs: &[(String, String)]) {
    std::fs::write(rohan.root.join("forge"), serde_json::to_string(pairs).unwrap()).unwrap();
}

fn machine(s: &Session, name: &str) -> Value {
    let list = s.json(&["machine", "ls"]);
    list["machines"].as_array().unwrap().iter().find(|m| m["name"] == name).cloned().unwrap_or_default()
}

/// A plain subscription's messages up to the first `last` picks, that one included.
fn until(conn: &mut Conn, last: impl Fn(&Value) -> bool) -> Vec<Value> {
    let mut seen = Vec::new();
    loop {
        let mut line = String::new();
        conn.reader.read_line(&mut line).expect("a message");
        let msg: Value = serde_json::from_str(&line).unwrap();
        let done = last(&msg);
        seen.push(msg);
        if done {
            return seen;
        }
    }
}

/// Whether a message names `address` anywhere.
fn names(msg: &Value, address: &str) -> bool {
    match msg {
        Value::String(s) => s == address,
        Value::Array(a) => a.iter().any(|v| names(v, address)),
        Value::Object(o) => o.values().any(|v| names(v, address)),
        _ => false,
    }
}

/// Whether a subscriber heard rohan about `here`: an event naming it (it says
/// nothing of itself any more), or a snapshot with another record at it.
fn heard_of(msg: &Value, here: &str) -> bool {
    match msg["method"].as_str() {
        Some("missed") => {
            let at: Vec<&Value> =
                msg["params"]["terminals"].as_array().unwrap().iter().filter(|t| t["address"] == here).collect();
            at.len() != 1 || at[0]["labels"]["who"] != "here"
        }
        _ => names(msg, here),
    }
}

fn addresses(s: &Session) -> Vec<String> {
    let list = s.json(&["ls"]);
    list["terminals"].as_array().unwrap().iter().map(|t| t["address"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_machine_that_forges_a_local_address_shadows_and_removes_nothing() {
    let (rohan, s) = liar();
    let here = run(&s, &["--label", "who=here", "sleep", "100"]);
    let (own, liar) = (run(&rohan, &["sleep", "101"]), run(&rohan, &["sleep", "102"]));
    // Everything the local one says of itself said before anyone listens.
    wait_until("the local one in front", || get(&s, &here)["foreground"]["argv"][0] == "sleep");
    let hub = s.json(&["server", "status"])["core"]["id"].as_str().unwrap().to_string();
    // rohan calls its second terminal, and the next one it starts, by the hub's
    // own address of the local one.
    let next = format!("t{}", own.trim_start_matches('t').parse::<u32>().unwrap() + 2);
    let local = format!("\"{hub}/{here}\"");
    forge(&rohan, &[(format!("\"{liar}\""), local.clone()), (format!("\"{next}\""), local)]);
    let mut all = s.connect();
    all.call(1, "events.subscribe", json!({}));
    s.json(&["machine", "add", "rohan"]);
    let seen = until(&mut all, |m| m["params"]["type"] == "machine" && m["params"]["data"]["synced"] == true);
    assert!(!seen.iter().any(|m| heard_of(m, &here)), "a forged record reached a subscriber: {seen:#?}");
    assert_eq!(addresses(&s), [here.clone(), format!("rohan/{own}")], "the forged one is dropped, not shadowing");
    assert_eq!(get(&s, &here)["labels"]["who"], "here", "the local terminal is its own");

    // A new one there and the end of another, told as the local one's: the
    // local one stays as it is, and no one hears of it.
    assert_eq!(run(&rohan, &["sleep", "103"]), next);
    rohan.json(&["kill", &liar]);
    rohan.json(&["label", &own, "k=v"]);
    let own_there = format!("rohan/{own}");
    let seen = until(&mut all, |m| m["params"]["terminal"] == own_there.as_str() && m["params"]["type"] == "labels");
    assert!(!seen.iter().any(|m| heard_of(m, &here)), "a forged event reached a subscriber: {seen:#?}");
    assert_eq!(
        (get(&s, &here)["state"].as_str(), get(&s, &here)["labels"]["who"].as_str()),
        (Some("live"), Some("here"))
    );
    assert_eq!(addresses(&s), [here.clone(), own_there.clone()]);

    // Another machine's address, or none of rohan's, is dropped as well.
    forge(&rohan, &[(format!("\"{own}\""), "\"gondor/t9\"".into()), (format!("\"{next}\""), format!("\"{hub}/t8\""))]);
    rohan.hc(&["server", "restart"]);
    wait_until("rohan's terminal dropped", || !addresses(&s).iter().any(|a| a.starts_with("rohan/")));
    assert_eq!(addresses(&s), [here]);
    std::fs::remove_file(rohan.root.join("forge")).unwrap();
    rohan.hc(&["server", "restart"]);
    wait_until("rohan's own terminal back", || addresses(&s).contains(&own_there));
}

#[test]
fn a_machine_is_never_named_like_a_core_id() {
    let rohan = Session::new();
    let hex = "0123456789abcdef0123456789abcdef";
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan), (hex, &rohan)]));
    let refused = |args: &[&str]| {
        let (code, e) = s.err(&[&["--json", "machine"], args].concat());
        assert_eq!((code, e["error"]["kind"].as_str()), (6, Some("invalid_params")), "{args:?}: {e}");
        e["error"]["message"].as_str().unwrap().to_string()
    };
    assert!(refused(&["add", "rohan", "--name", hex]).contains("32 hex digits"));
    let m = s.json(&["machine", "add", "rohan"]);
    let id = m["id"].as_str().unwrap();
    assert!(refused(&["rename", "rohan", id]).contains("32 hex digits"), "not even its own id");
    // By its id: it stays when it is renamed.
    assert_eq!(s.json(&["machine", "rename", id, "gondor"])["name"], "gondor");
    assert_eq!(machine(&s, "gondor")["id"], id);
    // A host named like a core id gets a name that is not one.
    s.json(&["machine", "rm", id]);
    let added = s.json(&["machine", "add", hex]);
    assert_eq!(added["name"], format!("{}-2", &hex[..30]), "{added}");
}

#[test]
fn a_name_is_never_a_machines_id_whatever_that_id_looks_like() {
    let (rohan, s) = liar();
    // rohan says its id is a word.
    let id = rohan.json(&["server", "status"])["core"]["id"].as_str().unwrap().to_string();
    forge(&rohan, &[(id, "gondor".into())]);
    assert_eq!(s.json(&["machine", "add", "rohan"])["id"], "gondor");
    let (code, e) = s.err(&["--json", "machine", "rename", "rohan", "gondor"]);
    assert_eq!((code, e["error"]["kind"].as_str()), (1, Some("name_taken")), "{e}");
    assert_eq!(s.json(&["machine", "rename", "gondor", "osgiliath"])["name"], "osgiliath", "by its id");
}

#[test]
fn a_run_sent_there_without_a_folder_starts_in_its_home() {
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    let home = rohan.root.display().to_string();
    let core_runs_in = rohan.hc(&["exec", "pwd"]);
    assert_ne!(String::from_utf8_lossy(&core_runs_in.stdout).trim(), home, "the core there runs elsewhere");
    // As a client calls it: no folder, no caller.
    let reply = s.connect().call(1, "terminal.run", json!({"on": "rohan", "argv": ["sh", "-c", "pwd; sleep 100"]}));
    let address = reply["result"]["address"].as_str().unwrap_or_else(|| panic!("{reply}")).to_string();
    wait_until("it printed its folder", || text(&s, &address).lines().any(|l| l.trim() == home));
    let out = s.hc(&["run", "--on", "rohan", "pwd"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains(&home), "{}", String::from_utf8_lossy(&out.stdout));
}
