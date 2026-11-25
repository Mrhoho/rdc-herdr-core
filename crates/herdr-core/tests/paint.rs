//! P1 end-to-end contracts: real core, holder, PTY and framed socket.
#![cfg(all(unix, not(feature = "nearshell")))]
mod common;
use common::{framed::Framed, *};
use herdr_core::wire;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

struct Marked {
    closed: Option<Value>,
    conn: Framed,
    ch: u32,
    screen: ghostty_vt::Terminal,
    active: Option<u64>,
    last: u64,
    marks: Vec<Value>,
    received: usize,
    paint_bytes: usize,
    ends: usize,
    credit: bool,
}
impl Marked {
    fn attach(s: &Session, t: &str, history: usize) -> Self {
        Self::attach_mode(s, t, history, true)
    }
    fn attach_mode(s: &Session, t: &str, history: usize, observe: bool) -> Self {
        let mut conn = Framed::open(s);
        let result = conn.call(
            "terminal.attach",
            json!({"terminal":t,"cols":100,"rows":24,"observe":observe,"marks":true,"history":history}),
        );
        let ch = result["result"]["channel"].as_u64().unwrap_or_else(|| panic!("{result}")) as u32;
        Self {
            closed: None,
            conn,
            ch,
            screen: ghostty_vt::Terminal::new(100, 24, 32 << 20),
            active: None,
            last: 0,
            marks: vec![],
            received: 0,
            paint_bytes: 0,
            ends: 0,
            credit: true,
        }
    }
    fn step(&mut self) -> bool {
        let frame = self.conn.stash.pop_front().or_else(|| self.conn.read(Duration::from_millis(50)));
        let Some((ch, kind, bytes)) = frame else { return false };
        if ch != self.ch {
            return true;
        }
        match kind {
            wire::DATA => {
                self.received += bytes.len();
                if self.active.is_some() {
                    self.paint_bytes += bytes.len();
                }
                self.screen.write(&bytes);
                if self.credit {
                    self.conn.send(ch, wire::CREDIT, &(bytes.len() as u32).to_le_bytes());
                }
            }
            wire::MARK => {
                let m: Value = serde_json::from_slice(&bytes).unwrap();
                match m["type"].as_str().unwrap() {
                    "discontinuity" => {
                        self.active = None;
                    }
                    "paint_begin" => {
                        assert!(self.active.is_none(), "begin overtook unfinished paint: {m}");
                        let generation = m["paintGen"].as_u64().unwrap();
                        assert!(generation > self.last);
                        self.active = Some(generation);
                        self.last = generation;
                        self.screen = ghostty_vt::Terminal::new(
                            m["cols"].as_u64().unwrap() as u16,
                            m["rows"].as_u64().unwrap() as u16,
                            32 << 20,
                        );
                    }
                    "paint_end" => {
                        assert_eq!(self.active.take(), m["paintGen"].as_u64(), "orphan end");
                        self.ends += 1;
                    }
                    _ => panic!("unknown mark {m}"),
                }
                self.marks.push(m);
            }
            wire::CLOSE => {
                let end: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(end["reason"], "exited", "unexpected close: {end}");
                assert!(self.active.is_none(), "CLOSE overtook unfinished paint");
                self.closed = Some(end);
            }
            _ => panic!("unexpected frame {kind}"),
        }
        true
    }
    fn until(&mut self, done: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !done(self) {
            assert!(
                start.elapsed() < Duration::from_secs(25),
                "marks={:?}, received={}, ends={}",
                self.marks,
                self.received,
                self.ends
            );
            self.step();
        }
    }
    fn grant(&mut self, n: usize) {
        self.conn.send(self.ch, wire::CREDIT, &(n as u32).to_le_bytes());
    }
    fn discontinuities(&self) -> usize {
        self.marks.iter().filter(|m| m["type"] == "discontinuity").count()
    }
}
fn session() -> Session {
    let s = Session::new();
    let p = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "headless = \"100x24\"\nhistory_mb = 32\n").unwrap();
    s
}
fn stats(s: &Session) -> Value {
    s.json(&["server", "status"])["viewer_pending"].clone()
}
fn program(s: &Session, body: &str) -> String {
    let path = s.root.join("paint.py");
    std::fs::write(&path, body).unwrap();
    s.json(&["run", "--bg", "python3", path.to_str().unwrap()])["address"].as_str().unwrap().into()
}
fn signal(s: &Session, name: &str) {
    std::fs::write(s.root.join(name), "").unwrap();
}
fn text(s: &Session, t: &str) -> String {
    s.json(&["read", t])["text"].as_str().unwrap().into()
}
const GIANT: &str = r#"
import os,time
from pathlib import Path
root=Path(os.environ['HOME'])
line=b''.join(b'\x1b[38;2;%d;%d;%dmX'%(i,255-i,i//2) for i in range(100))+b'\r\n'
for i in range(4600): os.write(1,line)
os.write(1,b'\x1b[0mGIANT_READY\r\n')
while not (root/'burst').exists():time.sleep(.01)
for i in range(1100):os.write(1,b'z'*4096+b'\r\n')
os.write(1,b'\r\nBURST_DONE\r\n')
while not (root/'burst2').exists():time.sleep(.01)
for i in range(1100):os.write(1,b'y'*4096+b'\r\n')
os.write(1,b'\r\nSECOND_DONE\r\n')
while True:time.sleep(1)
"#;

#[test]
fn giant_paint_obeys_credit_and_survives_overflow_during_repaint() {
    let s = session();
    let t = program(&s, GIANT);
    wait_until("giant ready", || text(&s, &t).contains("GIANT_READY"));
    let mut v = Marked::attach(&s, &t, 10000);
    v.credit = false;
    v.until(|v| v.received == wire::WINDOW);
    assert_eq!(v.ends, 0);
    assert!(v.active.is_some());
    let before = stats(&s);
    std::thread::sleep(Duration::from_millis(150));
    let after = stats(&s);
    assert_eq!(before["generated_paint_bytes"], after["generated_paint_bytes"], "no credit means no more generation");
    v.credit = true;
    v.grant(wire::WINDOW);
    v.until(|v| v.ends == 1);
    assert!(v.paint_bytes > wire::MAX_PENDING, "{}", v.paint_bytes);
    assert!(v.screen.plain_text().contains("GIANT_READY"));
    // A second attachment is deliberately interrupted in the middle of a giant first paint.
    let mut slow = Marked::attach(&s, &t, 10000);
    slow.credit = false;
    slow.until(|v| v.received == wire::WINDOW);
    signal(&s, "burst");
    wait_until("first burst", || text(&s, &t).contains("BURST_DONE"));
    slow.until(|v| v.discontinuities() > 0);
    assert_eq!(slow.ends, 0, "old end must have been discarded");
    signal(&s, "burst2");
    wait_until("second burst", || text(&s, &t).contains("SECOND_DONE"));
    slow.credit = true;
    slow.grant(wire::WINDOW);
    slow.until(|v| v.ends >= 2 && v.screen.plain_text().contains("SECOND_DONE"));
    assert_eq!(slow.discontinuities(), 2, "one in-progress replacement and one coalesced retry");
    let usage = stats(&s);
    eprintln!("giant/overflow budget: {usage}");
    assert!(usage["peak"].as_u64().unwrap() <= usage["limit"].as_u64().unwrap());
}

#[test]
fn actual_stitch_emits_discontinuity_then_ordered_stitch_paint() {
    let s = session();
    let app = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_tui.py");
    let t =
        s.json(&["run", "--bg", "python3", app, "--lines", "70", "--sync"])["address"].as_str().unwrap().to_string();
    wait_until("app ready", || text(&s, &t).contains("status"));
    let mut v = Marked::attach(&s, &t, 0);
    v.until(|v| v.ends == 1);
    let read = s.json(&["read", &t, "--lines", "200"]);
    assert_eq!(read["how"], "stitched");
    assert_eq!(read["complete"], true);
    v.until(|v| v.ends == 2);
    assert_eq!(v.marks[2]["type"], "discontinuity");
    assert!(v.marks[2]["droppedBytes"].as_u64().unwrap() > 0);
    assert_eq!(v.marks[3]["reason"], "stitch");
    assert!(v.screen.plain_text().contains("line 070"));
}

#[test]
fn resize_mid_paint_restarts_with_the_captured_new_grid() {
    let s = session();
    let t = program(&s, GIANT);
    wait_until("giant ready", || text(&s, &t).contains("GIANT_READY"));
    let mut v = Marked::attach(&s, &t, 10000);
    v.credit = false;
    v.until(|v| v.received == wire::WINDOW);
    let mut leader = common::framed::Viewer::attach(&s, &t, 61, 17, json!({}));
    leader.shows("GIANT_READY");
    v.credit = true;
    v.grant(wire::WINDOW);
    v.until(|v| v.ends == 1);
    assert_eq!(v.discontinuities(), 1);
    assert_eq!(v.screen.size(), (61, 17));
    assert_eq!(v.marks.iter().find(|m| m["reason"] == "resync").unwrap()["cols"], 61);
}

#[test]
fn overflow_resets_an_unfinished_viewer_osc_and_live_output_keeps_flowing() {
    let s = session();
    let t = program(
        &s,
        r#"
import os,time
from pathlib import Path
p=Path(os.environ['HOME'])
os.write(1,b'VT_READY\r\n')
while not (p/'prefix').exists():time.sleep(.01)
os.write(1,b'\x1b]2;'+b'P'*600)
while not (p/'flood').exists():time.sleep(.01)
os.write(1,b'\x07')
for i in range(1200):os.write(1,b'z'*4096+b'\r\n')
os.write(1,b'\x1b]2;RECOVERED\x07')
while True:
 os.write(1,b'\r\nCONTINUOUS\r\n');time.sleep(.01)
"#,
    );
    wait_until("VT ready", || text(&s, &t).contains("VT_READY"));
    let mut v = Marked::attach(&s, &t, 0);
    v.until(|v| v.ends == 1);
    v.credit = false;
    let n = v.received;
    signal(&s, "prefix");
    v.until(|v| v.received >= n + 604);
    assert_ne!(v.screen.title(), "RECOVERED");
    signal(&s, "flood");
    wait_until("flood done", || text(&s, &t).contains("CONTINUOUS"));
    // Reader as well as CREDIT was paused throughout the flood.
    v.credit = true;
    v.grant(wire::WINDOW);
    v.until(|v| {
        v.discontinuities() > 0
            && v.ends >= 2
            && v.screen.title() == "RECOVERED"
            && v.screen.plain_text().contains("CONTINUOUS")
    });
}

#[test]
fn twenty_four_paused_readers_share_one_budget_and_detach_reclaims_it() {
    let s = session();
    let t = program(&s, GIANT);
    wait_until("ready", || text(&s, &t).contains("GIANT_READY"));
    // No history here: measure live backlog across distinct slow connections.
    let mut viewers = Vec::new();
    for _ in 0..24 {
        let mut v = Marked::attach(&s, &t, 0);
        v.until(|v| v.ends == 1);
        v.credit = false;
        viewers.push(v);
    }
    signal(&s, "burst");
    wait_until("burst", || text(&s, &t).contains("BURST_DONE"));
    let usage = stats(&s);
    eprintln!("24 paused readers budget: {usage}");
    assert!(usage["peak"].as_u64().unwrap() <= 64 << 20);
    drop(viewers);
    wait_until("all pending and worker allocations released", || stats(&s)["used"] == 0);
    assert_eq!(stats(&s)["paint_workers"], 0);
}

#[test]
fn upgrade_restored_screen_uses_attach_transaction_and_feature_is_negotiable() {
    let s = session();
    let t = program(&s, "import os,time\nos.write(1,b'RESTORED\\r\\n')\nwhile True:time.sleep(1)\n");
    wait_until("ready", || text(&s, &t).contains("RESTORED"));
    s.json(&["server", "restart"]);
    let hello = s.connect().call(1, "hello", json!({}));
    assert!(hello["result"]["features"].as_array().unwrap().iter().any(|v| v == "paint-marks"));
    let mut v = Marked::attach(&s, &t, 0);
    v.until(|v| v.ends == 1);
    assert_eq!(v.marks[0]["reason"], "attach");
    assert!(v.screen.plain_text().contains("RESTORED"));
}

#[test]
fn shared_limit_closes_only_the_viewer_and_releases_its_size_authority() {
    let s = Session::new();
    let p = s.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, "viewer_pending_bytes=1\nheadless='90x30'\n").unwrap();
    let t = program(&s, "import time\nwhile True:time.sleep(1)\n");
    let mut v = common::framed::Viewer::attach(&s, &t, 43, 11, json!({"marks":true}));
    v.until("paint_limit", |v| v.closed.is_some());
    assert_eq!(v.closed.unwrap()["reason"], "paint_limit");
    wait_until("authority released", || s.json(&["get", &t])["cols"] == 90);
    assert_eq!(s.json(&["get", &t])["state"], "live");
    assert_eq!(stats(&s)["used"], 0);
}

#[test]
fn exit_during_coalesced_recovery_delivers_final_baseline_before_close() {
    let s = session();
    let body = GIANT.replace("while True:time.sleep(1)", "os.write(1,b'FINAL_SCREEN\\r\\n')");
    let t = program(&s, &body);
    wait_until("ready", || text(&s, &t).contains("GIANT_READY"));
    let mut v = Marked::attach(&s, &t, 10000);
    v.credit = false;
    v.until(|v| v.received == wire::WINDOW);
    signal(&s, "burst");
    wait_until("burst", || text(&s, &t).contains("BURST_DONE"));
    v.until(|v| v.discontinuities() > 0);
    signal(&s, "burst2");
    wait_until("exited", || s.json(&["get", &t])["state"] == "finished");
    v.credit = true;
    v.grant(wire::WINDOW);
    v.until(|v| v.closed.is_some());
    assert!(v.screen.plain_text().contains("FINAL_SCREEN"));
    assert!(v.ends > 0);
}

#[test]
fn marked_resync_replays_subscribed_history_after_resize_and_facts() {
    let s = session();
    let t = s.json(&["run", "--bg", "/bin/sh", "-c", "seq 1 500; printf HISTORY_READY; exec /bin/sh"])["address"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_until("shell ready", || text(&s, &t).contains("HISTORY_READY"));
    let mut v = Marked::attach_mode(&s, &t, 600, false);
    v.until(|v| v.ends == 1);
    assert!(v.screen.plain_text().contains("100\n"));
    v.conn.send(v.ch, wire::RESIZE, &[72, 0, 18, 0]);
    v.until(|v| v.ends == 2);
    assert!(v.screen.plain_text().contains("100\n"), "resize lost history");
    v.conn.send(v.ch, wire::FACTS, br##"{"bg":"#123456"}"##);
    v.until(|v| v.ends == 3);
    assert!(v.screen.plain_text().contains("100\n"), "theme resync lost history");
}
