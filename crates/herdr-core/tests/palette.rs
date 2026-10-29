//! P2: real PTY query answers and a viewer parsing the ordered MARK/VT baseline.
#![cfg(all(unix, not(feature = "managed")))]
mod common;
use common::{framed::Framed, *};
use herdr_core::wire;
use serde_json::{Value, json};
use std::time::{Duration, Instant};

struct View {
    conn: Framed,
    ch: u32,
    vt: ghostty_vt::Terminal,
    ends: usize,
    active: Option<u64>,
    credit: bool,
    marks: Vec<Value>,
    received: usize,
    leaked: Vec<u8>,
}
fn color_defaults(m: &Value) -> ghostty_vt::Colors {
    let c = &m["colors"];
    let palette: Vec<[u8; 3]> = serde_json::from_value(c["palette"].clone()).unwrap();
    ghostty_vt::Colors {
        fg: serde_json::from_value(c["fg"].clone()).unwrap(),
        bg: serde_json::from_value(c["bg"].clone()).unwrap(),
        cursor: serde_json::from_value(c["cursor"].clone()).unwrap(),
        palette: palette.try_into().unwrap(),
        dark: c["dark"].as_bool().unwrap(),
    }
}
impl View {
    fn attach(s: &Session, t: &str, display: Value) -> Self {
        let mut conn = Framed::open(s);
        let r = conn.call("terminal.attach", json!({"terminal":t,"cols":80,"rows":24,"marks":true,"display":display}));
        let ch = r["result"]["channel"].as_u64().unwrap_or_else(|| panic!("{r}")) as u32;
        Self {
            conn,
            ch,
            vt: ghostty_vt::Terminal::new(80, 24, 1 << 20),
            ends: 0,
            active: None,
            credit: true,
            marks: vec![],
            received: 0,
            leaked: vec![],
        }
    }
    fn step(&mut self) -> bool {
        let Some((ch, k, p)) = self.conn.stash.pop_front().or_else(|| self.conn.read(Duration::from_millis(50))) else {
            return false;
        };
        if ch != self.ch {
            return true;
        }
        match k {
            wire::MARK => {
                let m: Value = serde_json::from_slice(&p).unwrap();
                match m["type"].as_str().unwrap() {
                    "discontinuity" => self.active = None,
                    "paint_begin" => {
                        assert!(self.active.is_none());
                        self.active = m["paintGen"].as_u64();
                        self.vt = ghostty_vt::Terminal::new(
                            m["cols"].as_u64().unwrap() as u16,
                            m["rows"].as_u64().unwrap() as u16,
                            1 << 20,
                        );
                        self.vt.set_color_defaults(&color_defaults(&m)).unwrap();
                    }
                    "paint_end" => {
                        assert_eq!(self.active.take(), m["paintGen"].as_u64());
                        self.ends += 1;
                    }
                    _ => panic!("{m}"),
                }
                self.marks.push(m);
            }
            wire::DATA => {
                self.vt.write(&p);
                self.leaked.extend(self.vt.take_responses());
                self.received += p.len();
                if self.credit {
                    self.conn.send(ch, wire::CREDIT, &(p.len() as u32).to_le_bytes());
                }
            }
            wire::CLOSE => panic!("closed {}", String::from_utf8_lossy(&p)),
            _ => panic!("unexpected kind {k}"),
        }
        true
    }
    fn until(&mut self, done: impl Fn(&Self) -> bool) {
        let limit = Instant::now() + Duration::from_secs(10);
        while !done(self) {
            assert!(Instant::now() < limit, "viewer stalled");
            self.step();
        }
    }
    fn drain(&mut self) {
        while self.step() {}
        assert!(self.leaked.is_empty(), "core query reached viewer: {:?}", self.leaked);
    }
    fn facts(&mut self, facts: Value) {
        self.conn.send(self.ch, wire::FACTS, facts.to_string().as_bytes());
    }
    fn close(mut self) {
        self.conn.send(self.ch, wire::CLOSE, b"");
    }
}
struct Program {
    s: Session,
    t: String,
    next: usize,
}
impl Program {
    fn new() -> Self {
        let s = Session::new();
        let script = s.root.join("palette.py");
        std::fs::write(
            &script,
            r#"
import os,tty,time,json,select,sys
from pathlib import Path
tty.setraw(0)
root=Path(os.environ['HOME']);i=0
sys.stdout.buffer.write(b'PALETTE_READY\r\n');sys.stdout.buffer.flush()
while True:
 p=root/('cmd%d.json'%i)
 if not p.exists():time.sleep(.005);continue
 cmd=json.loads(p.read_text());out=sys.stdout.buffer
 if cmd.get('burst'):
  for _ in range(1100):out.write(b'x'*4096+b'\r\n')
 out.write(bytes.fromhex(cmd['hex']));out.flush()
 data=b''
 while select.select([0],[],[],.15)[0]:data+=os.read(0,65536)
 answer=root/'answer.tmp';answer.write_text(json.dumps(list(data)));os.replace(answer,root/('answer%d.json'%i));i+=1
"#,
        )
        .unwrap();
        let mut conn = Framed::open(&s);
        let r = conn.call(
            "terminal.run",
            json!({"argv":["python3",script],"env":{"TERM_PROGRAM":"P2Probe","TERM_PROGRAM_VERSION":"7"}}),
        );
        let t = r["result"]["address"].as_str().unwrap_or_else(|| panic!("{r}")).to_string();
        wait_until("program ready", || s.json(&["read", &t])["text"].as_str().unwrap().contains("PALETTE_READY"));
        Self { s, t, next: 0 }
    }
    fn emit(&mut self, bytes: &[u8], burst: bool) -> String {
        let i = self.next;
        self.next += 1;
        let cmd = json!({"hex":bytes.iter().map(|b|format!("{b:02x}")).collect::<String>(),"burst":burst});
        let temp = self.s.root.join("cmd.tmp");
        std::fs::write(&temp, cmd.to_string()).unwrap();
        std::fs::rename(temp, self.s.root.join(format!("cmd{i}.json"))).unwrap();
        let file = self.s.root.join(format!("answer{i}.json"));
        wait_until("query answered", || file.exists());
        let data: Vec<u8> = serde_json::from_slice(&std::fs::read(file).unwrap()).unwrap();
        String::from_utf8(data).unwrap()
    }
}
fn theme(b: bool) -> Value {
    let p: serde_json::Map<String, Value> = (0..16)
        .map(|i| {
            (i.to_string(), json!(if b { format!("#{:02x}2211", i + 80) } else { format!("#{:02x}3355", i + 16) }))
        })
        .collect();
    json!({"fg":if b {"#abcdef"}else{"#123456"},"bg":if b {"#eeeeee"}else{"#111111"},"cursor":"#778899","palette":p,"dark":b}) // explicitly opposite inferred background
}
fn queries() -> Vec<u8> {
    let mut out = b"\x1b]4".to_vec();
    for i in 0..16 {
        out.extend_from_slice(format!(";{i};?").as_bytes());
    }
    out.extend_from_slice(b"\x07\x1b]10;?\x07\x1b]11;?\x07\x1b]12;?\x07\x1b[?996n");
    out
}
fn compare(p: &mut Program, v: &mut View) {
    let q = queries();
    let actual = p.emit(&q, false);
    v.drain();
    v.vt.write(&q);
    let expected = String::from_utf8(v.vt.take_responses()).unwrap();
    assert_eq!(actual, expected, "query differs from rendered baseline");
}
#[test]
fn palettes_cold_theme_live_reattach_overflow_indexed_text_query_reset_and_identity() {
    for cold in [false, true] {
        let mut p = Program::new();
        let mut v = View::attach(&p.s, &p.t, theme(cold));
        v.until(|v| v.ends == 1);
        compare(&mut p, &mut v);
        let identity = p.emit(b"\x1b[c\x1b[>c\x1b[>q", false);
        v.drain();
        assert_eq!(identity, "\x1b[?62;22c\x1b[>1;10;0c\x1bP>|P2Probe 7\x1b\\");
        // An override equal to the current default must remain an override.
        let equal = if cold { "#512211" } else { "#113355" };
        let set = format!(
            "\x1b]4;1;{equal};2;#deadbe\x07\x1b]10;#102030\x07\x1b]11;#203040\x07\x1b]12;#304050\x07\x1b[31mLIVE"
        );
        assert_eq!(p.emit(set.as_bytes(), false), "");
        v.until(|v| v.vt.plain_text().contains("LIVE"));
        compare(&mut p, &mut v);
        v.facts(theme(!cold));
        v.until(|v| v.ends == 2);
        compare(&mut p, &mut v);
        let saved = v.vt.colors();
        assert_eq!(saved.palette[2], [0xde, 0xad, 0xbe]);
        v.close();
        let mut v = View::attach(&p.s, &p.t, theme(!cold));
        v.until(|v| v.ends == 1);
        assert_eq!(v.vt.colors(), saved);
        compare(&mut p, &mut v);
        v.credit = false;
        assert_eq!(p.emit(b"\x1b[32mAFTER_OVERFLOW\x1b[0m", true), "");
        v.credit = true;
        v.conn.send(v.ch, wire::CREDIT, &(wire::WINDOW as u32).to_le_bytes());
        v.until(|v| v.ends >= 2 && v.vt.plain_text().contains("AFTER_OVERFLOW"));
        assert_eq!(v.vt.colors(), saved);
        compare(&mut p, &mut v);
        // Change theme again after replay; program overrides still win.
        let ends = v.ends;
        v.facts(theme(cold));
        v.until(|v| v.ends > ends);
        compare(&mut p, &mut v);
        assert_eq!(v.vt.colors().palette[2], [0xde, 0xad, 0xbe]);
        assert_eq!(p.emit(b"\x1b]104;1\x07\x1b]110\x07\x1b]111\x07\x1b]112\x07\x1b[31mRESET_TEXT", false), "");
        v.until(|v| v.vt.plain_text().contains("RESET_TEXT"));
        compare(&mut p, &mut v);
        let expected = if cold { [81, 34, 17] } else { [17, 51, 85] };
        assert_eq!(v.vt.colors().palette[1], expected);
        assert_eq!(p.emit(b"\x1b]104\x07", false), "");
        compare(&mut p, &mut v);
        assert_eq!(v.vt.colors(), v.vt.default_colors());
    }
}
#[test]
fn mixed_color_requests_are_answered_once_and_invalid_palette_cannot_claim_authority() {
    let mut p = Program::new();
    let mut v = View::attach(&p.s, &p.t, theme(false));
    v.until(|v| v.ends == 1);
    let reply = p.emit(b"\x1b]4;1;#010203;2;?;1;?\x07\x1b]10;#040506;?\x07", false);
    v.drain();
    assert_eq!(reply.matches("rgb:").count(), 3);
    assert_eq!(v.vt.colors().palette[1], [1, 2, 3]);
    assert_eq!(v.vt.colors().fg, Some([4, 5, 6]));
    compare(&mut p, &mut v);
    let before = v.vt.colors();
    let ends = v.ends;
    v.facts(json!({"palette":{"256":"#ffffff"}}));
    v.facts(json!({"palette":{"1":"not-a-color"}}));
    v.drain();
    assert_eq!(v.ends, ends);
    compare(&mut p, &mut v);
    assert_eq!(v.vt.colors(), before);
    let mut other = Framed::open(&p.s);
    let bad = other
        .call("terminal.attach", json!({"terminal":p.t,"cols":43,"rows":11,"display":{"palette":{"300":"#ffffff"}}}));
    assert!(bad.get("error").is_some());
    assert_eq!(p.s.json(&["get", &p.t])["cols"], 80);
}

#[test]
fn handover_keeps_color_defaults_and_overrides_without_reapplying_facts() {
    let mut p = Program::new();
    let mut v = View::attach(&p.s, &p.t, theme(false));
    v.until(|v| v.ends == 1);
    p.emit(b"\x1b]4;1;#abcdef\x07\x1b]10;#aabbcc\x07", false);
    v.drain();
    let colors = v.vt.colors();
    let defaults = v.vt.default_colors();
    v.close();
    p.s.json(&["server", "restart"]);
    let mut v = View::attach(&p.s, &p.t, json!({}));
    v.until(|v| v.ends == 1);
    // Explicit dark preference is a viewer fact; omit it here and compare the
    // persisted color layers, independently of the headless inferred preference.
    assert_eq!(v.vt.colors().palette, colors.palette);
    assert_eq!(v.vt.colors().fg, colors.fg);
    assert_eq!(v.vt.default_colors().palette, defaults.palette);
    assert_eq!(v.vt.default_colors().fg, defaults.fg);
    compare(&mut p, &mut v);
    p.emit(b"\x1b]104\x07\x1b]110\x07", false);
    compare(&mut p, &mut v);
    assert_eq!(v.vt.colors().palette, defaults.palette);
    assert_eq!(v.vt.colors().fg, defaults.fg);
}
