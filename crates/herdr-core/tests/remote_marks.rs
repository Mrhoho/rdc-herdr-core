//! Marked viewers on terminals on another machine: the core there makes the
//! paint marks, and this one relays them in order with the bytes, so a viewer
//! reads them as it would on a direct connection.
#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::framed::{Framed, Viewer};
use common::*;
use herdr_core::wire;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// rohan, whose terminals are 100x24 without a viewer, added to a local core.
/// `marks: false`: the core there says it can't paint marks.
fn with_rohan(marks: bool) -> (Session, Session) {
    let rohan = headless();
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

fn headless() -> Session {
    let rohan = Session::new();
    let config = rohan.root.join("config/herdr-core/config.toml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, "headless = \"100x24\"\n").unwrap();
    rohan
}

/// The line to rohan, which a test can cut, and what came from there on the one up now.
struct Line {
    pid: PathBuf,
    log: PathBuf,
}

impl Line {
    /// `with_rohan(true)` over a line a test can cut (it is back within 200 ms).
    fn to_rohan() -> (Session, Session, Line) {
        let rohan = headless();
        let mut s = Session::new();
        let line = Line { pid: s.root.join("line-pid"), log: s.root.join("line-log") };
        let (ssh, r) = (s.root.join("line-ssh"), rohan.root.display());
        let connect = format!(
            "echo $$ > {}; exec env -i HOME={r} PATH=/usr/bin:/bin HC_SESSION=test HC_SETTLE_MS=0 \
             NEARSHELL_TEST_SESSION_ID={} XDG_RUNTIME_DIR={r}/run XDG_STATE_HOME={r}/state \
             XDG_CONFIG_HOME={r}/config {BIN} connect",
            line.pid.display(),
            rohan.guardian.nonce
        );
        let log = line.log.display();
        std::fs::write(&ssh, format!("#!/bin/sh\nrm -f {log}\nsh -c '{connect}' | tee {log}\n")).unwrap();
        std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        s.env.push(("HC_SSH".into(), ssh.display().to_string()));
        s.env.push(("HC_RECONNECT_MS".into(), "200".into()));
        s.json(&["machine", "add", "rohan"]);
        (rohan, s, line)
    }

    /// Paints begun there for viewers here, on the line up now.
    fn begins(&self) -> usize {
        let log = std::fs::read(&self.log).unwrap_or_default();
        log.windows(11).filter(|w| *w == b"paint_begin").count()
    }

    /// Cuts the line; back, the core there has begun `paints` paints on the next one.
    fn cut(&self, paints: usize) {
        let pid = std::fs::read_to_string(&self.pid).unwrap();
        assert_eq!(unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL) }, 0);
        wait_until("a new line", || std::fs::read_to_string(&self.pid).unwrap_or_default() != pid);
        wait_until("painted again there", || self.begins() >= paints);
    }
}

/// A terminal here with a long history: reads of it fill a connection.
fn filler(s: &Session) -> String {
    let t = run(s, &["sh", "-c", "seq 1 20000; exec sleep 100"]);
    wait_until("filled", || text(s, &t).contains("20000"));
    t
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
    fn params(terminal: &str, extra: Value) -> Value {
        let mut params = json!({"terminal": terminal, "cols": 100, "rows": 24, "marks": true});
        params.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        params
    }

    fn attach(s: &Session, terminal: &str, extra: Value) -> Marked {
        let mut conn = Framed::open(s);
        let params = Marked::params(terminal, extra);
        let reply = conn.call("terminal.attach", params.clone())["result"].clone();
        Marked::on(conn, &params, reply)
    }

    fn on(conn: Framed, params: &Value, reply: Value) -> Marked {
        let marked = params["marks"] == true;
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

    /// Stops reading: answers to reads of `filler` (a terminal here) take
    /// the connection ahead of the stream until the viewer reads again.
    fn hold(&mut self, s: &Session, filler: &str) {
        hold(&mut self.conn, s, filler);
    }

    /// Reads again, until typing there shows up here.
    fn live(&mut self, line: &str) {
        self.conn.send(self.ch, wire::DATA, format!("{line}\r").as_bytes());
        self.until("live output", |v| v.painting.is_none() && v.text().contains(&format!("got-{line}")));
    }

    /// What it saw from `from` on: (marks, bytes).
    fn since(&self, from: usize) -> (Vec<&Value>, usize) {
        let marks = self.seen[from..].iter().filter_map(|s| if let Seen::Mark(m) = s { Some(m) } else { None });
        let bytes = self.seen[from..].iter().map(|s| if let Seen::Data(n) = s { *n } else { 0 }).sum();
        (marks.collect(), bytes)
    }
}

/// `conn` stops reading: see `Marked::hold`.
fn hold(conn: &mut Framed, s: &Session, filler: &str) {
    for id in 0..64 {
        let read = json!({"jsonrpc": "2.0", "id": 1_000_000 + id, "method": "terminal.read",
            "params": {"terminal": filler, "lines": 20000}});
        conn.send(0, wire::DATA, read.to_string().as_bytes());
    }
    // Calls here are answered in turn: once this one is, the reads are queued.
    let label = json!({"jsonrpc": "2.0", "id": 2_000_000, "method": "terminal.label",
        "params": {"terminal": filler, "labels": {"held": "yes"}}});
    conn.send(0, wire::DATA, label.to_string().as_bytes());
    wait_until("the reads answered", || get(s, filler)["labels"]["held"] == "yes");
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

const LOOP: &str = "seq 1 300; echo READY; while read l; do echo got-$l; done";

#[test]
fn a_lost_line_is_one_gap_then_a_stitch_with_the_history_and_unmarked_viewers_are_told_in_words() {
    let (rohan, s, line) = Line::to_rohan();
    let t = run(&s, &["--on", "rohan", "sh", "-c", LOOP]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let mut v = Marked::attach(&s, &t, json!({"history": 200}));
    v.painted("READY");
    let mut plain = Viewer::attach(&s, &t, 100, 24, json!({}));
    plain.shows("READY");
    let from = v.seen.len();
    line.cut(1);
    v.until("the stitch", |v| v.ends() == 2);
    assert!(matches!(&v.seen[from], Seen::Mark(m) if m["type"] == "discontinuity"), "no notice: {:?}", v.seen);
    let (marks, _) = v.since(from);
    assert_eq!(marks.len(), 3, "{marks:?}");
    assert_eq!(marks[0]["type"], "discontinuity");
    assert_eq!((&marks[1]["reason"], &marks[1]["paintGen"]), (&json!("stitch"), &json!(2)), "{}", marks[1]);
    assert_eq!(marks[2], &json!({"type": "paint_end", "paintGen": 2}));
    assert!(v.text().contains("\n100\n"), "the history again: {}", v.text());
    v.live("hi");
    // The unmarked one reads the notice, then the screen painted again.
    let notice = b"[line to rohan lost; reconnecting...]";
    let after = |raw: &[u8]| raw.windows(notice.len()).position(|w| w == notice).map(|at| raw[at..].to_vec());
    plain.until("its repaint", |p| after(&p.raw).is_some_and(|rest| String::from_utf8_lossy(&rest).contains("READY")));
}

#[test]
fn a_line_lost_inside_a_paint_ends_it_with_a_gap() {
    let (rohan, s, line) = Line::to_rohan();
    let wide =
        "yes 012345678901234567890123456789012345678901234567890123456789012345678901234567890123 | head -n 4000";
    let t = run(&s, &["--on", "rohan", "sh", "-c", &format!("{wide}; echo READY; exec sleep 100")]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let mut v = Marked::attach(&s, &t, json!({"history": 4000}));
    v.credit = false;
    v.until("a window of the paint", |v| v.received >= wire::WINDOW);
    assert_eq!(v.painting, Some(1), "{:?}", v.seen);
    line.cut(1);
    v.until("the stitch begun", |v| v.marks().len() == 3);
    assert_eq!(v.marks()[1]["type"], "discontinuity");
    assert_eq!((&v.marks()[2]["reason"], &v.marks()[2]["paintGen"]), (&json!("stitch"), &json!(2)));
    v.credit = true;
    v.conn.send(v.ch, wire::CREDIT, &(64u32 << 20).to_le_bytes());
    v.painted("READY");
    assert_eq!(v.marks().len(), 4, "the first paint never ended: {:?}", v.marks());
}

/// A viewer that stops reading across `cuts` dropped lines, then reads
/// again: (the paint it had, and what it got since: marks and bytes).
fn unread_across(cuts: usize) -> (usize, Vec<Value>, usize) {
    let (rohan, s, line) = Line::to_rohan();
    let t = run(&s, &["--on", "rohan", "sh", "-c", LOOP]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let filler = filler(&s);
    let mut v = Marked::attach(&s, &t, json!({"history": 200}));
    v.painted("READY");
    let (from, painted) = (v.seen.len(), v.received);
    v.hold(&s, &filler);
    for _ in 0..cuts {
        line.cut(1);
    }
    v.live("back");
    let (marks, bytes) = v.since(from);
    (painted, marks.into_iter().cloned().collect(), bytes)
}

#[test]
fn lines_lost_twice_while_the_viewer_does_not_read_are_one_gap_and_one_paint() {
    let (_, marks, _) = unread_across(2);
    let types: Vec<&str> = marks.iter().map(|m| m["type"].as_str().unwrap()).collect();
    assert_eq!(types, ["discontinuity", "paint_begin", "paint_end"], "{marks:?}");
    assert_eq!((&marks[1]["reason"], &marks[1]["paintGen"]), (&json!("stitch"), &json!(3)), "one more per line");
}

#[test]
fn a_viewer_that_does_not_read_across_many_lost_lines_gets_only_the_last_paint() {
    let (painted, marks, bytes) = unread_across(5);
    assert_eq!(marks.iter().filter(|m| m["type"] == "paint_begin").count(), 1, "{marks:?}");
    assert!(bytes < painted * 3 / 2, "{bytes} bytes since, for a paint of {painted}");
}

#[test]
fn a_line_lost_before_the_first_paint_was_read_leaves_the_next_paint_its_attach() {
    let (rohan, s, line) = Line::to_rohan();
    let t = run(&s, &["--on", "rohan", "sh", "-c", LOOP]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let filler = filler(&s);
    let mut conn = Framed::open(&s);
    hold(&mut conn, &s, &filler);
    let params = Marked::params(&t, json!({}));
    let attach = json!({"jsonrpc": "2.0", "id": 3_000_000, "method": "terminal.attach", "params": params});
    conn.send(0, wire::DATA, attach.to_string().as_bytes());
    wait_until("its paint begun there", || line.begins() == 1);
    line.cut(1);
    let reply = loop {
        let frame = conn.read(Duration::from_secs(10)).expect("its reply");
        match serde_json::from_slice::<Value>(&frame.2) {
            Ok(reply) if frame.0 == 0 && reply["id"] == 3_000_000 => break reply,
            _ if frame.0 == 0 => {}
            _ => conn.stash.push_back(frame),
        }
    };
    let mut v = Marked::on(conn, &params, reply["result"].clone());
    v.live("hi");
    let marks = v.marks();
    assert_eq!(marks.len(), 2, "one paint, the one after the line came back: {marks:?}");
    assert_eq!((&marks[0]["reason"], &marks[0]["paintGen"]), (&json!("attach"), &json!(2)));
}

#[test]
fn a_viewer_that_does_not_read_while_the_terminal_there_is_resized_gets_one_paint_after() {
    let (rohan, s) = with_rohan(true);
    let t = run(&s, &["--on", "rohan", "sh", "-c", LOOP]);
    wait_until("ready", || text(&rohan, "t1").contains("READY"));
    let filler = filler(&s);
    let mut v = Marked::attach(&s, &t, json!({}));
    v.painted("READY");
    let from = v.seen.len();
    v.hold(&s, &filler);
    let mut there = Marked::attach(&rohan, "t1", json!({}));
    there.painted("READY");
    // Each a reset there, with marks that need no credit: more than a stream
    // here may hold. One at a time, or the core there takes them as one.
    let sizes = (0..wire::MAX_MARKS / 2 + 64).map(|i| (80 + i as u8 % 2, 20)).chain([(77, 19)]);
    for (cols, rows) in sizes {
        there.conn.send(there.ch, wire::RESIZE, &[cols, 0, rows, 0]);
        there.until("its paint", |v| v.painting.is_none() && v.screen.size() == (cols.into(), rows.into()));
    }
    v.until("the last size painted", |v| v.painting.is_none() && v.screen.size() == (77, 19));
    let (marks, _) = v.since(from);
    let begins = marks.iter().filter(|m| m["type"] == "paint_begin").count();
    assert!(begins <= 4, "{begins} paints held for it");
    v.live("back");
}
