#![cfg(unix)]

mod common;

use common::{Session, framed::Framed, wait_until};
use herdr_core::wire;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

fn assert_spawn_grid(s: &Session, params: Value, cols: u16, rows: u16) {
    let mut conn = Framed::open(s);
    let mut params = params;
    params["argv"] = json!(["sh", "-c", "stty size; sleep 100"]);
    let reply = conn.call("terminal.run", params);
    let terminal = reply["result"]["address"].as_str().unwrap_or_else(|| panic!("{reply}"));
    let record = s.json(&["get", terminal]);
    assert_eq!((record["cols"].as_u64(), record["rows"].as_u64()), (Some(cols.into()), Some(rows.into())));
    let first_size = format!("{rows} {cols}");
    wait_until("first stty size", || s.json(&["read", terminal])["text"].as_str().unwrap().contains(&first_size));
    // The first sized viewer agrees with the spawn grid; its first paint must
    // keep that grid. No separate resize RPC is needed.
    let reply = conn.call("terminal.attach", json!({"terminal":terminal,"cols":cols,"rows":rows,"marks":true}));
    let channel = reply["result"]["channel"].as_u64().unwrap() as u32;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        assert!(Instant::now() < deadline, "no first paint");
        let Some((ch, kind, bytes)) = conn.stash.pop_front().or_else(|| conn.read(Duration::from_millis(50))) else {
            continue;
        };
        if ch == channel && kind == wire::MARK {
            let mark: Value = serde_json::from_slice(&bytes).unwrap();
            if mark["type"] == "paint_begin" {
                assert_eq!((mark["cols"].as_u64(), mark["rows"].as_u64()), (Some(cols.into()), Some(rows.into())));
                break;
            }
        }
    }
}

#[test]
fn initial_grid_reaches_registry_child_and_first_paint() {
    let s = Session::new();
    assert_spawn_grid(&s, json!({"cols":91,"rows":27}), 91, 27);
}

#[test]
fn omitted_dimensions_keep_headless_defaults_and_partial_overrides() {
    let s = Session::new();
    assert_spawn_grid(&s, json!({}), 200, 120);
    std::fs::create_dir_all(s.root.join("config/herdr-core")).unwrap();
    std::fs::write(s.root.join("config/herdr-core/config.toml"), "headless = \"103x31\"\n").unwrap();
    assert_spawn_grid(&s, json!({}), 103, 31);
    assert_spawn_grid(&s, json!({"cols":92}), 92, 31);
    assert_spawn_grid(&s, json!({"rows":28}), 103, 28);
}

#[test]
fn invalid_initial_grid_is_rejected_without_creating_a_terminal() {
    let s = Session::new();
    let mut conn = Framed::open(&s);
    for field in ["cols", "rows"] {
        for value in [json!(0), json!(-1), json!(65536), json!(1.5)] {
            let mut params = json!({"argv":["sleep","100"]});
            params[field] = value;
            let reply = conn.call("terminal.run", params);
            assert!(reply.get("error").is_some(), "{reply}");
        }
        let mut params = json!({"argv":["sleep","100"],"size":{"cols":80,"rows":24}});
        params[field] = json!(40);
        let reply = conn.call("terminal.run", params);
        assert_eq!(reply["error"]["data"]["kind"], "invalid_params", "{reply}");
    }
    assert!(s.json(&["ls"])["terminals"].as_array().unwrap().is_empty());
    for value in ["0", "65536"] {
        assert!(!s.cmd().args(["run", "--bg", "--cols", value, "sleep", "100"]).output().unwrap().status.success());
    }
}

#[test]
fn explicit_grid_is_neutral_for_observers_and_restored_after_owner_and_restart() {
    use common::framed::Viewer;
    let s = Session::new();
    let output = s.root.join("winsize.json");
    let script = r#"import os,signal,json,sys
records=[]
def record(kind):
    size=os.get_terminal_size(0)
    records.append([kind,size.columns,size.lines])
    with open(sys.argv[1], 'w') as f: json.dump(records,f)
signal.signal(signal.SIGWINCH,lambda *_:record('winch'))
record('first')
print('READY',flush=True)
while True: signal.pause()
"#;
    let mut conn = Framed::open(&s);
    let reply = conn.call("terminal.run", json!({"argv":["python3","-u","-c",script,output],"cols":91,"rows":27}));
    let t = reply["result"]["address"].as_str().unwrap();
    let read = || std::fs::read(&output).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    wait_until("first PTY size", || read().is_some());
    let pid = s.json(&["get", t])["pid"].clone();
    let observer = Viewer::attach(&s, t, 80, 24, json!({"observe":true}));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(read().unwrap(), json!([["first", 91, 27]]), "an observer must not send SIGWINCH");
    let owner = Viewer::attach(&s, t, 70, 20, json!({}));
    wait_until("owner size", || {
        read().is_some_and(|v| v.as_array().unwrap().last() == Some(&json!(["winch", 70, 20])))
    });
    drop(owner);
    wait_until("spawn grid restored", || {
        read().is_some_and(|v| v.as_array().unwrap().last() == Some(&json!(["winch", 91, 27])))
    });
    assert_eq!(read().unwrap().as_array().unwrap().len(), 3);
    drop(observer);
    drop(conn);
    s.json(&["server", "restart"]);
    assert_eq!(s.json(&["get", t])["pid"], pid, "restart must retain the same PTY");
    let _observer = Viewer::attach(&s, t, 80, 24, json!({"observe":true}));
    let record = s.json(&["get", t]);
    assert_eq!((record["cols"].as_u64(), record["rows"].as_u64()), (Some(91), Some(27)));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(read().unwrap().as_array().unwrap().len(), 3, "persisted fallback must survive core restart");
}

#[test]
fn unspecified_dimensions_still_follow_dynamic_headless_configuration() {
    use common::framed::Viewer;
    let s = Session::new();
    std::fs::create_dir_all(s.root.join("config/herdr-core")).unwrap();
    let config = s.root.join("config/herdr-core/config.toml");
    std::fs::write(&config, "headless = \"100x30\"\n").unwrap();
    for (params, expected) in [(json!({"cols":91}), (91, 40)), (json!({"rows":27}), (110, 27)), (json!({}), (110, 40))]
    {
        std::fs::write(&config, "headless = \"100x30\"\n").unwrap();
        let mut conn = Framed::open(&s);
        let mut params = params;
        params["argv"] = json!(["sleep", "100"]);
        let reply = conn.call("terminal.run", params);
        let t = reply["result"]["address"].as_str().unwrap();
        std::fs::write(&config, "headless = \"110x40\"\n").unwrap();
        // Observers preserve the current size since upstream v0.1.21. Leaving
        // the last interactive viewer re-evaluates the headless configuration.
        let owner = Viewer::attach(&s, t, 80, 24, json!({}));
        drop(owner);
        wait_until("headless dimensions refreshed", || {
            let record = s.json(&["get", t]);
            (record["cols"].as_u64(), record["rows"].as_u64()) == (Some(expected.0), Some(expected.1))
        });
        let _observer = Viewer::attach(&s, t, 80, 24, json!({"observe":true}));
        let record = s.json(&["get", t]);
        assert_eq!((record["cols"].as_u64(), record["rows"].as_u64()), (Some(expected.0), Some(expected.1)));
    }
}
