//! Machines in the event stream: who hears of them, in what order with their
//! terminals, when a machine is synced, and the state a line is down in.
#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::Framed;
use common::*;
use herdr_core::wire;
use serde_json::{Value, json};
use std::io::BufRead;
use std::path::{Path, PathBuf};

/// rohan, not added yet, over a fake ssh that stops at `<s root>/down` and
/// logs what came from there in `<s root>/from-rohan`.
fn two() -> (Session, Session) {
    let rohan = Session::new();
    let mut s = Session::new();
    let (_, fake) = fake_ssh(&s, &[("rohan", &rohan)]);
    let (root, log) = (s.root.display().to_string(), s.root.join("from-rohan"));
    let ssh = executable(
        &s,
        &format!(
            "#!/bin/sh\n[ -e {root}/down ] && {{ echo \"ssh: connect to host $1 port 22: Connection refused\" >&2; exit 255; }}\n\
             {fake} \"$@\" | tee -a {}\n",
            log.display()
        ),
    );
    s.env.push(("HC_SSH".into(), ssh));
    s.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    (rohan, s)
}

fn machine(s: &Session, name: &str) -> Value {
    let list = s.json(&["machine", "ls"]);
    list["machines"].as_array().unwrap().iter().find(|m| m["name"] == name).cloned().unwrap_or_default()
}

fn synced(s: &Session, name: &str) {
    wait_until(&format!("{name} synced"), || machine(s, name)["synced"] == true);
}

fn subscribe(s: &Session, params: Value) -> (Conn, Value) {
    let mut c = s.connect();
    let reply = c.call(1, "events.subscribe", params);
    (c, reply["result"].clone())
}

/// What a plain subscription says up to the first message `last` picks, that one included.
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

fn is_machine(m: &Value) -> bool {
    m["method"] == "event" && m["params"]["type"] == "machine"
}

fn synced_event(m: &Value) -> bool {
    is_machine(m) && m["params"]["data"]["synced"] == true
}

fn event(kind: &str, terminal: &str) -> impl Fn(&Value) -> bool {
    let (kind, terminal) = (kind.to_string(), terminal.to_string());
    move |m| {
        m["method"] == "event" && m["params"]["type"] == kind.as_str() && m["params"]["terminal"] == terminal.as_str()
    }
}

fn position(seen: &[Value], pick: impl Fn(&Value) -> bool) -> usize {
    seen.iter().position(pick).unwrap_or_else(|| panic!("not in {seen:#?}"))
}

/// The lines on a framed event channel up to the first one `last` picks, crediting as it reads.
fn framed_until(f: &mut Framed, channel: u32, last: impl Fn(&Value) -> bool) -> Vec<Value> {
    let (mut seen, mut partial) = (Vec::new(), Vec::new());
    loop {
        let Some((ch, kind, payload)) = f.stash.pop_front().or_else(|| f.read(std::time::Duration::from_secs(10)))
        else {
            let brief: Vec<String> = seen.iter().map(|m: &Value| m.to_string().chars().take(300).collect()).collect();
            panic!("no frame after {brief:#?} (partial {})", partial.len());
        };
        if (ch, kind) != (channel, wire::DATA) {
            continue;
        }
        f.send(channel, wire::CREDIT, &(payload.len() as u32).to_le_bytes());
        partial.extend_from_slice(&payload);
        while let Some(at) = partial.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = partial.drain(..=at).collect();
            let msg: Value = serde_json::from_slice(&line).unwrap();
            let done = last(&msg);
            seen.push(msg);
            if done {
                return seen;
            }
        }
    }
}

/// How many `server.status` answers came from rohan (its 3-second refresh).
fn status_answers(s: &Session) -> usize {
    let log = std::fs::read(s.root.join("from-rohan")).unwrap_or_default();
    log.windows(16).filter(|w| *w == b"\"viewer_pending\"").count()
}

#[test]
fn only_a_subscriber_of_everything_hears_of_machines_and_only_of_changes() {
    let (_rohan, s) = two();
    let here = run(&s, &["--label", "k=v", "sleep", "100"]);
    s.json(&["machine", "add", "rohan"]);
    synced(&s, "rohan");
    let (mut all, snapshot) = subscribe(&s, json!({}));
    let m = &snapshot["machines"][0];
    assert_eq!((&m["name"], &m["state"], &m["synced"]), (&json!("rohan"), &json!("connected"), &json!(true)), "{m}");
    assert!(m["features"].as_array().unwrap().contains(&json!("machine-events")), "{m}");
    let mut filtered: Vec<(&str, Conn)> = [
        ("labels", json!({"labels": {"k": "v"}})),
        ("terminal", json!({"terminal": here})),
        ("docs", json!({"docs": ["app"]})),
    ]
    .into_iter()
    .map(|(what, params)| {
        let (c, snapshot) = subscribe(&s, params);
        assert!(snapshot.get("machines").is_none(), "{what}: {snapshot}");
        (what, c)
    })
    .collect();
    let mut peer = Framed::open_as_peer(&s, &"0".repeat(32), "osgiliath");
    let reply = peer.call("events.subscribe", json!({}));
    assert!(reply["result"].get("machines").is_none(), "a core that dialed in has machines of its own: {reply}");
    let channel = reply["result"]["channel"].as_u64().unwrap() as u32;

    // The 3-second status refresh finds nothing new: twice, and not a word.
    let before = status_answers(&s);
    wait_until_for("two status refreshes", std::time::Duration::from_secs(15), || status_answers(&s) >= before + 2);
    s.json(&["label", &here, "k2=x"]);
    let quiet = until(&mut all, event("labels", &here));
    assert!(!quiet.iter().any(is_machine), "an unchanged machine was told again: {quiet:#?}");

    s.json(&["machine", "rename", "rohan", "gondor"]);
    let renamed = until(&mut all, is_machine).pop().unwrap();
    assert_eq!(renamed["params"]["data"]["name"], "gondor");
    assert_eq!(renamed["params"]["data"]["id"], m["id"]);
    // Something each of the others hears, after: none heard of the machine before it.
    s.json(&["label", &here, "k2=y"]);
    s.connect().call(1, "doc.set", json!({"name": "app", "value": 1, "version": 0}));
    let marker = |m: &Value| m["params"]["data"]["labels"]["k2"] == "y" || m["params"]["type"] == "doc";
    for (what, c) in &mut filtered {
        let seen = until(c, marker);
        assert!(!seen.iter().any(is_machine), "{what} heard of a machine: {seen:#?}");
    }
    let seen = framed_until(&mut peer, channel, marker);
    assert!(!seen.iter().any(is_machine), "a peer heard of a machine: {seen:#?}");
}

#[test]
fn a_machine_is_told_before_its_terminals_when_added_renamed_and_removed() {
    let (rohan, s) = two();
    let there = run(&rohan, &["sleep", "100"]);
    s.json(&["server", "start"]);
    let (mut all, snapshot) = subscribe(&s, json!({}));
    assert_eq!(snapshot["machines"], json!([]));
    s.json(&["machine", "add", "rohan"]);
    let seen = until(&mut all, synced_event);
    let created = position(&seen, event("created", &format!("rohan/{there}")));
    assert!(position(&seen, is_machine) < created, "{seen:#?}");
    assert!(seen[..created].iter().filter(|m| is_machine(m)).all(|m| m["params"]["data"]["synced"] == false));
    let documents = until(&mut all, |m| m["method"] == "missed").pop().unwrap();
    assert_eq!(documents["params"]["documents"], true, "the line's first snapshot lost no terminal event");

    s.json(&["machine", "rename", "rohan", "gondor"]);
    let seen: Vec<Value> = until(&mut all, event("created", &format!("gondor/{there}")))
        .into_iter()
        .filter(|m| m["method"] == "event")
        .collect();
    assert!(is_machine(&seen[0]) && seen[0]["params"]["data"]["name"] == "gondor", "{seen:#?}");
    position(&seen, event("removed", &format!("rohan/{there}")));

    let id = seen[0]["params"]["data"]["id"].clone();
    s.json(&["machine", "rm", "gondor"]);
    let seen = until(&mut all, event("removed", &format!("gondor/{there}")));
    assert_eq!(seen[0]["params"]["data"], json!({"id": id, "name": "gondor", "removed": true}), "{seen:#?}");
}

#[test]
fn a_cut_line_and_a_restarted_hub_are_not_synced_until_the_changes_are_in() {
    let (rohan, s) = two();
    let here = run(&s, &["sleep", "100"]);
    s.json(&["machine", "add", "rohan"]);
    synced(&s, "rohan");
    let (mut all, _) = subscribe(&s, json!({}));
    let (mut docs, _) = subscribe(&s, json!({"docs": ["app"]}));
    let (mut one, _) = subscribe(&s, json!({"terminal": here}));

    std::fs::write(s.root.join("down"), "").unwrap();
    rohan.hc(&["server", "restart"]);
    let cut = until(&mut all, is_machine).pop().unwrap();
    assert_eq!(cut["params"]["data"]["synced"], false, "{cut}");
    wait_until("rohan unreachable", || machine(&s, "rohan")["state"] == "unreachable");
    let there = format!("rohan/{}", run(&rohan, &["sleep", "100"]));
    std::fs::remove_file(s.root.join("down")).unwrap();
    let back = until(&mut all, synced_event);
    assert!(position(&back, event("created", &there)) < back.len() - 1, "{back:#?}");
    assert!(!back.iter().any(|m| m["method"] == "missed"), "{back:#?}");
    // Documents may have changed meanwhile; terminals were all told.
    let missed = until(&mut all, |m| m["method"] == "missed").pop().unwrap();
    assert_eq!(missed["params"]["documents"], true, "{missed}");
    let terminals = &missed["params"]["terminals"];
    assert!(terminals.as_array().unwrap().iter().any(|t| t["address"] == there.as_str()), "{missed}");
    assert_eq!(missed["params"]["machines"][0]["synced"], true, "{missed}");
    let missed = until(&mut docs, |m| m["method"] == "missed").pop().unwrap();
    assert_eq!((&missed["params"]["documents"], &missed["params"]["terminals"]), (&json!(true), &json!([])));
    s.json(&["label", &here, "k=v"]);
    let seen = until(&mut one, event("labels", &here));
    assert!(!seen.iter().any(|m| m["method"] == "missed"), "it wants no documents: {seen:#?}");

    // A hub that starts again has an empty mirror: not synced until it is filled.
    std::fs::write(s.root.join("down"), "").unwrap();
    s.hc(&["server", "restart"]);
    let (mut all, snapshot) = subscribe(&s, json!({}));
    assert_eq!(snapshot["machines"][0]["synced"], false, "{snapshot}");
    assert!(!snapshot["terminals"].as_array().unwrap().iter().any(|t| t["address"] == there.as_str()));
    std::fs::remove_file(s.root.join("down")).unwrap();
    let back = until(&mut all, synced_event);
    assert!(position(&back, event("created", &there)) < back.len() - 1, "{back:#?}");
}

/// Passes the line through, but answers the first mirror subscription after
/// `refuse` appears with an error; `again` appears once it is asked again.
const REFUSING: &str = r#"
import json, os, struct, subprocess, sys, threading
refuse, again = sys.argv[1], sys.argv[2]
far = subprocess.Popen(sys.argv[3:], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
out, lock = sys.stdout.buffer, threading.Lock()
def frame(f):
    head = f.read(9)
    if len(head) < 9:
        return None
    body = f.read(struct.unpack('<I', head[5:9])[0])
    return head + body
def send(b):
    with lock:
        out.write(b)
        out.flush()
def back():
    send(far.stdout.readline())
    while (f := frame(far.stdout)) is not None:
        send(f)
    os._exit(0)
threading.Thread(target=back, daemon=True).start()
far.stdin.write(sys.stdin.buffer.readline())
far.stdin.flush()
refused = False
while (f := frame(sys.stdin.buffer)) is not None:
    if f[:5] == b'\0\0\0\0\0' and json.loads(f[9:]).get('method') == 'events.subscribe':
        if os.path.exists(refuse):
            os.remove(refuse)
            refused = True
            error = {'code': -32000, 'message': 'connection is closed', 'data': {'kind': 'connection_closed'}}
            reply = json.dumps({'jsonrpc': '2.0', 'id': json.loads(f[9:])['id'], 'error': error}).encode()
            send(struct.pack('<IBI', 0, 0, len(reply)) + reply)
            continue
        if refused:
            open(again, 'w').close()
    far.stdin.write(f)
    far.stdin.flush()
far.stdin.close()
far.wait()
"#;

#[test]
fn a_refused_mirror_subscription_removes_nothing_and_is_not_synced() {
    let rohan = Session::new();
    let mut s = Session::new();
    let (_, fake) = fake_ssh(&s, &[("rohan", &rohan)]);
    let proxy = s.root.join("refusing.py");
    std::fs::write(&proxy, REFUSING).unwrap();
    let (refuse, again) = (s.root.join("refuse"), s.root.join("again"));
    let ssh =
        format!("#!/bin/sh\nexec python3 {} {} {} {fake} \"$@\"\n", proxy.display(), refuse.display(), again.display());
    s.env.push(("HC_SSH".into(), executable(&s, &ssh)));
    s.env.push(("HC_RECONNECT_MS".into(), "200".into()));
    s.json(&["machine", "add", "rohan"]);
    let there = run(&s, &["--on", "rohan", "sleep", "100"]);
    synced(&s, "rohan");
    let (mut all, _) = subscribe(&s, json!({}));
    std::fs::write(&refuse, "").unwrap();
    rohan.hc(&["server", "restart"]);
    let seen = until(&mut all, synced_event);
    assert!(!refuse.exists(), "the subscription was refused");
    assert!(again.exists(), "synced only once a snapshot came: {seen:#?}");
    assert!(!seen.iter().any(event("removed", &there)), "an error is no empty snapshot: {seen:#?}");
}

#[test]
fn one_who_falls_behind_while_a_mirror_is_replaced_gets_it_whole() {
    let (rohan, s) = two();
    s.json(&["machine", "add", "rohan"]);
    synced(&s, "rohan");
    // Big records fill the queue of a subscriber that reads nothing (so gives no
    // credit): 24 label events of 1/32 of it each, then rohan's terminal with 3/8
    // of it comes in the replacement and overflows it. Snapshots stay below half
    // of it, so the one after (documents) fits too.
    let big = |n: usize, c: char| c.to_string().repeat(n);
    let here = run(&s, &["sleep", "100"]);
    let label = |v: String| json!({"terminal": here, "labels": {"big": v}});
    s.connect().call(1, "terminal.label", label(big(wire::MAX_PENDING / 32, '0')));
    let mut behind = Framed::open(&s);
    let channel = behind.call("events.subscribe", json!({}))["result"]["channel"].as_u64().unwrap() as u32;
    std::fs::write(s.root.join("down"), "").unwrap();
    rohan.hc(&["server", "restart"]);
    wait_until("rohan unreachable", || machine(&s, "rohan")["state"] == "unreachable");
    let params = json!({"argv": ["sleep", "100"], "labels": {"big": big(wire::MAX_PENDING * 3 / 8, 'r')}});
    let there = rohan.connect().call(1, "terminal.run", params)["result"]["address"].as_str().unwrap().to_string();
    let there = format!("rohan/{there}");
    for v in "abcdefghijklmnopqrstuvwx".chars() {
        s.connect().call(1, "terminal.label", label(big(wire::MAX_PENDING / 32, v)));
    }
    std::fs::remove_file(s.root.join("down")).unwrap();
    // Read only once it is all queued (or dropped) here.
    synced(&s, "rohan");
    let seen = framed_until(&mut behind, channel, synced_event);
    let missed = &seen[position(&seen, |m| m["method"] == "missed")]["params"];
    let addresses: Vec<&str> =
        missed["terminals"].as_array().unwrap().iter().filter_map(|t| t["address"].as_str()).collect();
    assert!(addresses.contains(&there.as_str()), "the snapshot misses rohan's terminals: {addresses:?}");
    assert!(missed["documents"].is_null(), "events were dropped: a plain missed");
    assert_eq!(missed["machines"][0]["synced"], false, "taken while the changes went out, then told when done");
}

#[test]
fn a_hub_that_is_itself_added_tells_nothing_of_its_own_machines() {
    let (gondor, mut rohan, mut s) = (Session::new(), Session::new(), Session::new());
    rohan.env.push(fake_ssh(&rohan, &[("gondor", &gondor)]));
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    rohan.json(&["machine", "add", "gondor"]); // rohan's core starts with its own way to gondor
    s.json(&["machine", "add", "rohan"]);
    synced(&s, "rohan");
    let (mut all, snapshot) = subscribe(&s, json!({}));
    let names: Vec<&Value> = snapshot["machines"].as_array().unwrap().iter().map(|m| &m["name"]).collect();
    assert_eq!(names, [&json!("rohan")]);
    let (mut theirs, _) = subscribe(&rohan, json!({}));
    rohan.json(&["machine", "rename", "gondor", "minas"]);
    until(&mut theirs, |m| is_machine(m) && m["params"]["data"]["name"] == "minas");
    let marker = format!("rohan/{}", run(&rohan, &["sleep", "100"]));
    let seen = until(&mut all, event("created", &marker));
    let leaked = seen.iter().filter(|m| is_machine(m)).any(|m| m["params"]["data"]["name"] != "rohan");
    assert!(!leaked, "rohan's machines leaked: {seen:#?}");
    rohan.json(&["machine", "rm", "minas"]);
}

const UNKNOWN: &str = "Host key verification failed.\r\n";
const CHANGED: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
    @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n\
    @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
    IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\r\n\
    Host key for rohan has changed and you have requested strict checking.\r\n\
    Host key verification failed.\r\n";

/// rohan over a fake ssh that refuses its host key with what `<s root>/key`
/// holds; a login (`rohan exit`) asks to accept it, and a `yes` does.
fn behind_a_host_key() -> (Session, Session, PathBuf, PathBuf) {
    let rohan = Session::new();
    let mut s = Session::new();
    let (_, fake) = fake_ssh(&s, &[("rohan", &rohan)]);
    let (key, dials) = (s.root.join("key"), s.root.join("dials"));
    let ssh = format!(
        "#!/bin/sh\necho dial >> {d}\nif [ -e {k} ]; then\n  if [ \"$2\" = exit ]; then\n    \
         printf 'Are you sure you want to continue connecting (yes/no/[fingerprint])? '; read a\n    \
         [ \"$a\" = yes ] && rm {k} && exit 0; exit 255\n  fi\n  cat {k} >&2; exit 255\nfi\nexec {fake} \"$@\"\n",
        d = dials.display(),
        k = key.display()
    );
    s.env.push(("HC_SSH".into(), executable(&s, &ssh)));
    s.env.push(("HC_RECONNECT_MS".into(), "100".into()));
    (rohan, s, key, dials)
}

fn in_a_terminal(s: &Session, args: &str, done: &str) {
    let t = run(s, &["sh", "-c", &format!("{BIN} {args}; echo {done}-$?; sleep 100")]);
    wait_until("the host key question", || text(s, &t).contains("continue connecting"));
    s.json(&["send", &t, "yes"]);
    wait_until(done, || text(s, &t).contains(&format!("{done}-0")));
}

#[test]
fn a_host_key_ssh_refuses_is_told_apart_and_a_person_can_accept_an_unknown_one() {
    let (rohan, s, key, dials) = behind_a_host_key();
    for (said, kind) in [(UNKNOWN, "host_key_unknown"), (CHANGED, "host_key_changed")] {
        std::fs::write(&key, said).unwrap();
        let (_, piped) = s.err(&["--json", "machine", "add", "rohan"]);
        assert_eq!(piped["error"]["kind"], kind, "{piped}");
    }
    // In a terminal, adding logs in first: the person accepts the key.
    std::fs::write(&key, UNKNOWN).unwrap();
    in_a_terminal(&s, "machine add rohan", "added");
    wait_until("connected", || machine(&s, "rohan")["state"] == "connected");

    // A key that becomes unknown later holds the line until a person acts.
    let (mut all, _) = subscribe(&s, json!({}));
    std::fs::write(&key, UNKNOWN).unwrap();
    rohan.hc(&["server", "restart"]);
    let held = until(&mut all, |m| is_machine(m) && m["params"]["data"]["state"] == "host_key_unknown").pop().unwrap();
    assert_eq!(held["params"]["data"]["synced"], false);
    assert_eq!(s.err(&["--json", "read", "rohan/t1"]).1["error"]["kind"], "unavailable");
    let before = std::fs::read_to_string(&dials).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert_eq!(std::fs::read_to_string(&dials).unwrap(), before, "redialed as slowly as a login");
    in_a_terminal(&s, "machine login rohan", "login");
    assert_eq!(machine(&s, "rohan")["state"], "connected");
}

/// The core.json under a session's state: its identity.
fn identity(s: &Session) -> PathBuf {
    fn find(dir: &Path) -> Option<PathBuf> {
        std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).find_map(|p| match p.is_dir() {
            true => find(&p),
            false => p.ends_with("core.json").then_some(p),
        })
    }
    find(&s.root.join("state")).expect("core.json")
}

#[test]
fn another_core_answering_there_is_different_core() {
    let (rohan, s) = two();
    s.json(&["machine", "add", "rohan"]);
    synced(&s, "rohan");
    let (mut all, _) = subscribe(&s, json!({}));
    std::fs::write(s.root.join("down"), "").unwrap();
    rohan.hc(&["server", "stop"]);
    std::fs::remove_file(identity(&rohan)).unwrap();
    std::fs::remove_file(s.root.join("down")).unwrap();
    let seen = until(&mut all, |m| is_machine(m) && m["params"]["data"]["state"] == "different_core").pop().unwrap();
    let data = &seen["params"]["data"];
    assert!(data["why"].as_str().unwrap().contains("a different core answers there"), "{data}");
    assert_eq!(data["synced"], false);
    let routed = s.err(&["--json", "read", "rohan/t1"]).1;
    assert!(routed["error"]["hint"].as_str().unwrap().contains("machine rm rohan"), "{routed}");
}
