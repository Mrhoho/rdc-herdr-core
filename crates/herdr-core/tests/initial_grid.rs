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
    }
    assert!(s.json(&["ls"])["terminals"].as_array().unwrap().is_empty());
    for value in ["0", "65536"] {
        assert!(!s.cmd().args(["run", "--bg", "--cols", value, "sleep", "100"]).output().unwrap().status.success());
    }
}
