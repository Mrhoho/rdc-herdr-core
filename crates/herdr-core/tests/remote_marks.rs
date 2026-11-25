//! Marked viewers on terminals on another machine: the core there makes the
//! paint marks, and this one relays them in order with the bytes, so a viewer
//! reads them as it would on a direct connection.
#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// rohan, whose terminals are 100x24 without a viewer, added to a local core.
/// `marks: false`: the core there says it can't paint marks.
fn with_rohan(marks: bool) -> (Session, Session) {
    let rohan = Session::new();
    let config = rohan.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "headless = \"100x24\"\n").unwrap();
    if !marks {
        let bin = rohan.root.join("fakebin/herdr-core");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        // Its hello reply is the first line; the framed bytes after it pass as they are.
        let filter = r#"{ IFS= read -r hello; printf '%s\n' "$hello" | sed 's/"paint-marks",//'; exec cat; }"#;
        std::fs::write(&bin, format!("#!/bin/sh\n{BIN} \"$@\" | {filter}\n")).unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("rohan", &rohan)]));
    s.json(&["machine", "add", "rohan"]);
    (rohan, s)
}

/// What a marked viewer saw, bytes in a row counted as one.
#[derive(Debug, PartialEq)]
enum Seen {
    Data(usize),
    Mark(Value),
    Close(Value),
}

/// A viewer that holds its stream to what the App's parser accepts: no
/// bytes before the first paint or between a discontinuity and the next
/// paint, generations rising, an attach only first, each end matching its
/// begin, and no CLOSE inside a paint.
struct Marked {
    conn: Framed,
    ch: u32,
    reply: Value,
    marked: bool,
    screen: ghostty_vt::Terminal,
    seen: Vec<Seen>,
    received: usize,
    painting: Option<u64>,
    last: u64,
    gap: bool,
    /// Send credit back as bytes arrive.
    credit: bool,
}

impl Marked {
    fn attach(s: &Session, terminal: &str, extra: Value) -> Marked {
        let mut conn = Framed::open(s);
        let mut params = json!({"terminal": terminal, "cols": 100, "rows": 24, "marks": true});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        let marked = params["marks"] == true;
        let reply = conn.call("terminal.attach", params)["result"].clone();
        let ch = reply["channel"].as_u64().unwrap_or_else(|| panic!("{reply}")) as u32;
        assert!(conn.stash.iter().all(|f| f.0 != ch), "a frame for the stream came before its channel");
        let screen = ghostty_vt::Terminal::new(100, 24, 32 << 20);
        let (seen, received, painting, last, gap) = (Vec::new(), 0, None, 0, false);
        Marked { conn, ch, reply, marked, screen, seen, received, painting, last, gap, credit: true }
    }

    fn step(&mut self) {
        let frame = self.conn.stash.pop_front().or_else(|| self.conn.read(Duration::from_millis(50)));
        let Some((ch, kind, bytes)) = frame else { return };
        if ch != self.ch {
            return;
        }
        match kind {
            wire::DATA => {
                let painted = self.last > 0 && !self.gap;
                assert!(painted || !self.marked, "bytes outside a paint: {:?}", self.seen);
                self.screen.write(&bytes);
                self.received += bytes.len();
                if self.credit {
                    self.conn.send(ch, wire::CREDIT, &(bytes.len() as u32).to_le_bytes());
                }
                match self.seen.last_mut() {
                    Some(Seen::Data(n)) => *n += bytes.len(),
                    _ => self.seen.push(Seen::Data(bytes.len())),
                }
            }
            wire::MARK => {
                assert!(self.marked, "a mark on an unmarked stream");
                let m: Value = serde_json::from_slice(&bytes).unwrap();
                match m["type"].as_str().unwrap() {
                    "discontinuity" => {
                        assert!(self.last > 0 && m["droppedBytes"].is_u64(), "{m}");
                        (self.painting, self.gap) = (None, true);
                    }
                    "paint_begin" => {
                        let generation = m["paintGen"].as_u64().unwrap();
                        assert!(self.painting.is_none() && generation > self.last, "{m} after {:?}", self.seen);
                        assert_eq!(m["reason"] == "attach", self.last == 0, "{m}");
                        (self.painting, self.last, self.gap) = (Some(generation), generation, false);
                        let size = |key: &str| m[key].as_u64().unwrap() as u16;
                        self.screen = ghostty_vt::Terminal::new(size("cols"), size("rows"), 32 << 20);
                    }
                    "paint_end" => assert_eq!(self.painting.take(), m["paintGen"].as_u64(), "{m}"),
                    _ => panic!("unknown mark {m}"),
                }
                self.seen.push(Seen::Mark(m));
            }
            wire::CLOSE => {
                assert!(self.painting.is_none(), "CLOSE inside a paint: {:?}", self.seen);
                self.seen.push(Seen::Close(serde_json::from_slice(&bytes).unwrap_or_default()));
            }
            _ => panic!("unexpected frame {kind}"),
        }
    }

    fn until(&mut self, what: &str, done: impl Fn(&Marked) -> bool) {
        let start = Instant::now();
        while !done(self) {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for {what}: {:?}", self.seen);
            self.step();
        }
    }

    fn marks(&self) -> Vec<&Value> {
        self.seen.iter().filter_map(|s| if let Seen::Mark(m) = s { Some(m) } else { None }).collect()
    }

    fn ends(&self) -> usize {
        self.marks().iter().filter(|m| m["type"] == "paint_end").count()
    }

    fn text(&self) -> String {
        self.screen.plain_text()
    }

    fn painted(&mut self, what: &str) {
        let what = what.to_string();
        self.until(&what.clone(), move |v| v.painting.is_none() && v.ends() > 0 && v.text().contains(&what));
    }
}

fn size(s: &Session, t: &str) -> (Option<u64>, Option<u64>) {
    let t = get(s, t);
    (t["cols"].as_u64(), t["rows"].as_u64())
}

#[test]
fn a_marked_attach_there_paints_as_a_direct_one_does_then_goes_live() {
    let (rohan, s) = with_rohan(true);
    let t = run(&s, &["--on", "rohan", "sh", "-c", "seq 1 300; echo READY; while read l; do echo got-$l; done"]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let mut relayed = Marked::attach(&s, &t, json!({"history": 200}));
    assert_eq!(relayed.reply["resize_notifications"], false);
    relayed.painted("READY");
    let mut direct = Marked::attach(&rohan, "t1", json!({"history": 200, "observe": true}));
    direct.painted("READY");
    assert_eq!(relayed.marks(), direct.marks(), "the same marks, colors and all");
    let begin = relayed.marks()[0].clone();
    assert_eq!(
        (&begin["reason"], &begin["paintGen"], &begin["cols"], &begin["rows"]),
        (&json!("attach"), &json!(1), &json!(100), &json!(24))
    );
    assert!(begin["colors"]["palette"].is_array(), "{begin}");
    assert_eq!(relayed.text(), direct.text());
    assert!(relayed.text().contains("\n100\n"), "history above the screen: {}", relayed.text());
    assert!(matches!(&relayed.seen[..], [Seen::Mark(_), Seen::Data(_), Seen::Mark(_)]), "{:?}", relayed.seen);
    relayed.conn.send(relayed.ch, wire::DATA, b"hi\r");
    relayed.until("live output", |v| v.text().contains("got-hi"));
    assert!(matches!(relayed.seen[3], Seen::Data(_)), "{:?}", relayed.seen);
}

#[test]
fn input_on_a_marked_stream_reaches_the_program_and_unmarked_viewers_keep_theirs() {
    let (rohan, s) = with_rohan(true);
    let t = run(&s, &["--on", "rohan", "sh", "-c", "echo READY; while read l; do echo got-$l; done"]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let mut marked = Marked::attach(&s, &t, json!({}));
    assert_eq!(marked.reply["automatic_input"], true);
    marked.painted("READY");
    let mut plain = Marked::attach(&s, &t, json!({"marks": false}));
    plain.until("its paint", |v| v.text().contains("READY"));
    // Kind 6 from a viewer is its terminal's own replies, not a mark: it still reaches the program.
    marked.conn.send(marked.ch, wire::AUTOMATIC_INPUT, b"auto\r");
    marked.until("the automatic reply there", |v| v.text().contains("got-auto"));
    plain.until("the same output", |v| v.text().contains("got-auto"));
    plain.conn.send(plain.ch, wire::DATA, b"typed\r");
    plain.until("typed there", |v| v.text().contains("got-typed"));
    marked.until("typed by the other viewer", |v| v.text().contains("got-typed"));
    assert_eq!(marked.ends(), 1, "no repaint for either: {:?}", marked.seen);
}

#[test]
fn a_viewer_that_stops_reading_gets_marks_in_order_and_the_repaint_there_whole() {
    let (rohan, s) = with_rohan(true);
    let burst = "yes 0123456789abcdefghijklmnopqrstuvwxyz | head -c 6000000; echo; echo BURST_DONE; exec sleep 100";
    let t = run(&s, &["--on", "rohan", "sh", "-c", &script(&rohan, &format!("echo READY; w burst; {burst}"))]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let mut v = Marked::attach(&s, &t, json!({}));
    v.painted("READY");
    v.credit = false;
    let before = v.received;
    touch(&rohan, "burst");
    // One window of it, then the core there falls 4 MiB behind and starts a
    // repaint: its marks need no credit, its bytes do.
    v.until("the repaint's begin", |v| v.marks().len() == 4);
    assert_eq!(v.received - before, wire::WINDOW);
    let (gap, begin) = (v.marks()[2].clone(), v.marks()[3].clone());
    assert_eq!(
        (&gap["type"], &begin["reason"], &begin["paintGen"]),
        (&json!("discontinuity"), &json!("resync"), &json!(2))
    );
    assert!(gap["droppedBytes"].as_u64().unwrap() > 0, "{gap}");
    wait_until("the burst there", || text(&rohan, "t1").contains("BURST_DONE"));
    v.credit = true;
    v.conn.send(v.ch, wire::CREDIT, &(wire::WINDOW as u32).to_le_bytes());
    v.painted("BURST_DONE");
    assert!(v.ends() >= 2, "{:?}", v.seen);
}

#[test]
fn a_resize_repaints_in_order_and_a_program_ending_right_after_ends_the_stream_after_it() {
    let (rohan, s) = with_rohan(true);
    let t = run(&s, &["--on", "rohan", "sh", "-c", "trap 'exit 0' WINCH; echo READY; while :; do sleep 0.05; done"]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let mut v = Marked::attach(&s, &t, json!({}));
    v.painted("READY");
    v.conn.send(v.ch, wire::RESIZE, &[90, 0, 20, 0]);
    v.until("the end", |v| matches!(v.seen.last(), Some(Seen::Close(_))));
    let marks = v.marks();
    assert_eq!(marks[2]["type"], "discontinuity", "{marks:?}");
    let begin = marks[3];
    assert_eq!(
        (&begin["reason"], &begin["paintGen"], &begin["cols"], &begin["rows"]),
        (&json!("resync"), &json!(2), &json!(90), &json!(20))
    );
    assert_eq!(marks[4], &json!({"type": "paint_end", "paintGen": 2}));
    assert!(v.text().contains("READY"));
    let Some(Seen::Close(end)) = v.seen.last() else { unreachable!() };
    assert_eq!(end["reason"], "exited", "{end}");
}

#[test]
fn a_marked_attach_is_refused_where_the_core_there_cant_paint_marks() {
    let (rohan, s) = with_rohan(false);
    let t = run(&s, &["--on", "rohan", "sh", "-c", "echo ready; exec sleep 100"]);
    wait_until("ready", || text(&rohan, "t1").contains("ready"));
    let headless = size(&rohan, "t1");
    let mut conn = Framed::open(&s);
    let refused = conn.call("terminal.attach", json!({"terminal": t, "cols": 43, "rows": 11, "marks": true}));
    assert_eq!(refused["error"]["data"]["kind"], "marks_local_only", "{refused}");
    assert!(refused["error"]["message"].as_str().unwrap().contains("does not support paint marks"), "{refused}");
    assert_eq!(size(&rohan, "t1"), headless, "a refused relay must not register a viewer there");
    // Unmarked, it attaches over the same line; nothing came for the refused one.
    let reply = conn.call("terminal.attach", json!({"terminal": t, "cols": 80, "rows": 24}));
    let ch = reply["result"]["channel"].as_u64().unwrap_or_else(|| panic!("{reply}")) as u32;
    let mut v = Viewer {
        conn,
        channel: ch,
        screen: ghostty_vt::Terminal::new(80, 24, 1 << 20),
        received: 0,
        raw: Vec::new(),
        closed: None,
        credit: true,
    };
    assert!(v.conn.stash.iter().all(|f| f.0 == 0 || f.0 == ch), "{:?}", v.conn.stash);
    v.shows("ready");
}
