//! Viewers: raw attach streams on framed connections. First paint, live
//! bytes (minus answered queries), input, and who sets the size.

use crate::branding::COMMAND as CLI_NAME;
use crate::holder_proto as proto;
use crate::relay::Relay;
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use crate::wire::Conn;
use ghostty_vt::Rgb;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

/// What a viewer's own terminal looks like, reported by the client.
#[derive(Serialize, Deserialize, JsonSchema, Clone, Default, Debug, PartialEq)]
pub struct Display {
    /// Sparse default ANSI palette, keys 0..255, values #rrggbb. Some({}) resets
    /// to the built-in palette; omission keeps the previously known palette.
    #[serde(default, deserialize_with = "palette")]
    pub palette: Option<std::collections::BTreeMap<u8, String>>,
    /// Explicit dark/light preference; absent derives from the default background.
    pub dark: Option<bool>,
    /// Default foreground as `#rrggbb`.
    pub fg: Option<String>,
    /// Default background as `#rrggbb`.
    pub bg: Option<String>,
    /// Cursor color as `#rrggbb`.
    pub cursor: Option<String>,
    /// Cell size in pixels: `[width, height]`.
    pub cell: Option<[u32; 2]>,
}

pub struct Viewer {
    pub marks: bool,
    // A marked paint replaces the receiver's parser, including its history.
    history: usize,
    suppressed: u64,
    pub conn: Arc<Conn>,
    pub channel: u32,
    pub observe: bool,
    size: (u16, u16),
    /// None: not active since it came back after a dropped line.
    active_at: Option<Instant>,
    display: Display,
}

fn rgb(hex: &Option<String>) -> Option<Rgb> {
    let hex = hex.as_deref()?.strip_prefix('#')?;
    let n = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6)?;
    Some([(n >> 16) as u8, (n >> 8) as u8, n as u8])
}

fn palette<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<std::collections::BTreeMap<u8, String>>, D::Error> {
    let value = Option::<std::collections::BTreeMap<u8, String>>::deserialize(deserializer)?;
    if value.as_ref().is_some_and(|p| p.values().any(|v| rgb(&Some(v.clone())).is_none())) {
        return Err(serde::de::Error::custom("invalid_palette"));
    }
    Ok(value)
}

pub struct AttachRequest {
    pub marks: bool,
    pub observe: bool,
    pub history: usize,
    pub size: (u16, u16),
    pub display: Display,
    pub resumed: bool,
}

impl Core {
    /// Adds a viewer: queues the first paint, then every live byte follows in order.
    pub fn attach(&self, conn: &Arc<Conn>, id: u32, req: AttachRequest) -> Result<u32, RpcError> {
        let mut st = self.lock();
        conn.admit(&st)?;
        if let Some(t) = st.reg.terminals.get(&id) {
            t.check_program()?;
        }
        if !st.screens.contains_key(&id) {
            return Err(RpcError::new(
                "finished",
                format!("{} has finished", crate::terminals::address(id)),
                format!("read what it left: {CLI_NAME} read {}", crate::terminals::address(id)),
            ));
        }
        let channel = conn.new_channel();
        st.viewers.entry(id).or_default().push(Viewer {
            marks: req.marks,
            history: req.history,
            suppressed: 0,
            conn: conn.clone(),
            channel,
            observe: req.observe,
            size: req.size,
            active_at: (!req.resumed).then(Instant::now),
            display: req.display,
        });
        self.apply_size(&mut st, id);
        conn.with(|out| out.open_viewer(channel, req.marks, self.paint_budget.clone()));
        self.enqueue_paint(&st, id, conn, channel, req.history, crate::paint::Reason::Attach, false, 0);
        Ok(channel)
    }

    /// Program output: through the relay to the screen and every viewer,
    /// then whatever status or title it reported.
    pub fn output(&self, st: &mut State, id: u32, bytes: &[u8]) {
        st.last_output.insert(id, Instant::now());
        *st.output_seq.entry(id).or_default() += 1;
        if let Some(t) = st.reg.terminals.get_mut(&id) {
            t.printed += bytes.iter().filter(|b| **b == b'\n').count() as u64;
        }
        // While a read scrolls the app, its frames are ours alone: viewers get one repaint after.
        let scrolling = st.stitching.contains(&id);
        let instance = st.reg.terminals.get(&id).and_then(|t| t.holder.clone());
        let State { screens, relays, viewers, holders, output_watch, .. } = st;
        let holder = holders.iter().find(|l| Some(&l.instance) == instance.as_ref());
        let Some(screen) = screens.get_mut(&id) else { return };
        let relay = relays.entry(id).or_default();
        let out = relay.feed(screen, bytes);
        let win32_input = relay.win32_input.take();
        if let Some(watchers) = output_watch.get_mut(&id).filter(|_| !scrolling) {
            watchers.retain(|w| w.send(out.clone()).is_ok());
        }
        if let Some(link) = holder {
            link.input(id, &Relay::take_responses(screen));
        }
        let reports = screen.take_status_reports();
        let title = screen.take_title_changed().then(|| screen.title());
        let full_screen = screen.alt_screen();
        let mut behind = Vec::new();
        for v in viewers.get_mut(&id).into_iter().flatten() {
            if scrolling {
                v.suppressed = v.suppressed.saturating_add(out.len() as u64);
                continue;
            }
            v.conn.with(|ob| {
                if !ob.data(v.channel, &out) {
                    behind.push((v.conn.clone(), v.channel));
                }
            });
        }
        // Before any repaint below, so it carries the mode this output just set.
        if let Some(on) = win32_input
            && let Some(t) = st.reg.terminals.get_mut(&id).filter(|t| t.win32_input != on)
        {
            t.win32_input = on;
            self.save(st);
        }
        if full_screen && let Some(t) = st.reg.terminals.get_mut(&id) {
            t.full_screen = true;
        }
        if !reports.is_empty() {
            self.report_status(st, id, &reports);
        }
        if let Some(title) = title {
            st.reg.terminals.get_mut(&id).expect("live terminal").title = title.clone();
            self.save_soon();
            self.emit(st, "title", id, serde_json::json!({"title": title}));
        }
        for (conn, channel) in behind {
            self.enqueue_paint(st, id, &conn, channel, 0, crate::paint::Reason::Resync, false, 0);
        }
    }

    /// Applies 7501 reports and tells subscribers what changed.
    pub fn report_status(&self, st: &mut State, id: u32, reports: &[ghostty_vt::StatusReport]) {
        let word = |st: &State| st.reg.terminals[&id].status.as_ref().map(|s| s.state.clone());
        let mut word_changed = false;
        for r in reports {
            let was = word(st);
            let t = st.reg.terminals.get_mut(&id).expect("live terminal");
            let change = crate::status::apply(t, r);
            self.status_changed(st, id, change);
            word_changed |= word(st) != was;
        }
        // Losing the holder too (a reboot) leaves no tail to replay: a new word, and
        // with it a turn or news, must already be on disk. Messages and progress can wait.
        if word_changed { self.save(st) } else { self.save_soon() }
    }

    pub fn status_changed(&self, st: &mut State, id: u32, change: crate::status::Change) {
        let t = &st.reg.terminals[&id];
        let (status, records, turn, reported) = (t.status.clone(), t.records.clone(), t.turn, t.reported);
        if change.status {
            self.emit(
                st,
                "status",
                id,
                serde_json::json!({"status": status, "records": records, "reported": reported}),
            );
        }
        if change.turn_ended {
            let state = status.map(|s| s.state);
            self.emit(st, "turn", id, serde_json::json!({"turn": turn, "state": state}));
        }
        if (change.turn_ended || change.stuck) && st.reg.terminals[&id].is_live() {
            self.news(st, id);
        }
    }

    fn viewer<'a>(st: &'a mut State, conn: &Arc<Conn>, channel: u32) -> Option<(u32, &'a mut Viewer)> {
        st.viewers.iter_mut().find_map(|(id, vs)| {
            vs.iter_mut().find(|v| Arc::ptr_eq(&v.conn, conn) && v.channel == channel).map(|v| (*id, v))
        })
    }

    /// Typing, resizing or focusing makes a viewer the one whose size and colors count.
    fn touch(&self, st: &mut State, conn: &Arc<Conn>, channel: u32, f: impl FnOnce(&mut Viewer)) -> Option<u32> {
        let (id, v) = Self::viewer(st, conn, channel)?;
        f(v);
        if v.observe {
            return None;
        }
        v.active_at = Some(Instant::now());
        self.apply_size(st, id);
        Some(id)
    }

    pub fn viewer_input(&self, conn: &Arc<Conn>, channel: u32, bytes: &[u8]) {
        let mut st = self.lock();
        if let Some(id) = self.touch(&mut st, conn, channel, |_| {}) {
            let now = st.gates.entry(id).or_default().human(bytes);
            if let Some(link) = st.link(id) {
                link.input(id, &now);
            }
        }
    }

    pub fn viewer_resize(&self, conn: &Arc<Conn>, channel: u32, cols: u16, rows: u16) {
        let mut st = self.lock();
        self.touch(&mut st, conn, channel, |v| v.size = (cols, rows));
    }

    pub fn viewer_facts(&self, conn: &Arc<Conn>, channel: u32, display: Display) {
        let mut st = self.lock();
        self.touch(&mut st, conn, channel, |v| v.display = display);
    }

    /// Focus changes count as activity, and reach programs that asked for them.
    pub fn viewer_focus(&self, conn: &Arc<Conn>, channel: u32, focused: bool) {
        let mut st = self.lock();
        let id = if focused {
            self.touch(&mut st, conn, channel, |_| {})
        } else {
            Self::viewer(&mut st, conn, channel).filter(|(_, v)| !v.observe).map(|(id, _)| id)
        };
        if let Some(id) = id
            && st.screens.get(&id).is_some_and(|s| s.dec_mode(1004))
            && let Some(link) = st.link(id)
        {
            link.input(id, if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// A viewer left (`channel` = None: the whole connection went away).
    pub fn detach(&self, conn: &Arc<Conn>, channel: Option<u32>) {
        let mut st = self.lock();
        if channel.is_none() {
            conn.begin_close(&st);
        }
        self.unsubscribe(&mut st, conn, channel);
        let mut touched = Vec::new();
        for (id, vs) in st.viewers.iter_mut() {
            let before = vs.len();
            vs.retain(|v| !(Arc::ptr_eq(&v.conn, conn) && channel.is_none_or(|c| c == v.channel)));
            if vs.len() != before {
                touched.push(*id);
            }
        }
        st.viewers.retain(|_, vs| !vs.is_empty());
        for id in touched {
            self.apply_size(&mut st, id);
        }
        if channel.is_none() {
            conn.end_close(&st);
        }
    }

    /// The most recently active typing viewer sets size and display facts;
    /// with none, the terminal runs headless.
    fn apply_size(&self, st: &mut State, id: u32) {
        let leader = st.viewers.get(&id).and_then(|vs| vs.iter().filter(|v| !v.observe).max_by_key(|v| v.active_at));
        let (cols, rows) = leader.map_or_else(
            || {
                let (cols, rows) = self.headless();
                st.reg
                    .terminals
                    .get(&id)
                    .map_or((cols, rows), |rec| (rec.headless_cols.unwrap_or(cols), rec.headless_rows.unwrap_or(rows)))
            },
            |v| v.size,
        );
        let display = leader.map(|v| v.display.clone());
        let Some(screen) = st.screens.get_mut(&id) else { return };
        let before_colors = screen.default_colors();
        if let Some(d) = display {
            let entries = d.palette.as_ref().map(|p| {
                p.iter().map(|(i, c)| (*i, rgb(&Some(c.clone())).expect("validated palette"))).collect::<Vec<_>>()
            });
            if screen.set_palette(entries.as_deref()).is_err() {
                for v in st.viewers.get(&id).into_iter().flatten() {
                    v.conn.with(|out| out.close(v.channel, br#"{"reason":"display_failed"}"#));
                }
                return;
            }
            screen.set_display(rgb(&d.fg), rgb(&d.bg), rgb(&d.cursor), d.cell.map(|[w, h]| (w, h)));
            screen.set_dark(d.dark);
        }
        let colors_changed = screen.default_colors() != before_colors;
        let rec = st.reg.terminals.get_mut(&id).expect("live terminal");
        let resized = (rec.cols, rec.rows) != (cols, rows) && cols > 0 && rows > 0;
        if resized {
            screen.resize(cols, rows);
            rec.cols = cols;
            rec.rows = rows;
            if let Some(link) = st.link(id) {
                link.send(&proto::json_frame(proto::RESIZE, id, &proto::Size { cols, rows }));
            }
            self.save_soon();
            self.emit(st, "size", id, serde_json::json!({"cols":cols,"rows":rows}));
        }
        if !resized && !colors_changed {
            return;
        }
        for v in st.viewers.get(&id).into_iter().flatten().filter(|v| v.marks) {
            self.enqueue_paint(st, id, &v.conn, v.channel, 0, crate::paint::Reason::Resync, true, 0);
        }
    }
}

impl Core {
    /// All synthetic output enters the very same ordered viewer queue here.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue_paint(
        &self,
        st: &State,
        id: u32,
        conn: &Arc<Conn>,
        channel: u32,
        history: usize,
        reason: crate::paint::Reason,
        replace: bool,
        loss: u64,
    ) {
        let viewer =
            st.viewers.get(&id).into_iter().flatten().find(|v| Arc::ptr_eq(&v.conn, conn) && v.channel == channel);
        let marks = viewer.is_some_and(|v| v.marks);
        // All marked synthetic paints (resize, facts, overflow, stitch and
        // restore) replay the same bounded history as attach. Unmarked streams
        // retain their upstream screen-only resync behavior.
        let history = viewer.filter(|v| v.marks).map_or(history, |v| v.history);
        // New viewers participate in size selection before their stream opens.
        if !conn.outbox().has_stream(channel) {
            return;
        }
        match crate::paint::Source::capture(st, id, history, marks, &self.paint_budget, &self.paint_metrics) {
            Ok(source) => {
                conn.with(|out| out.enqueue_paint(channel, source, Arc::downgrade(conn), reason, replace, loss))
            }
            Err(_) => conn.with(|out| out.close(channel, br#"{"reason":"paint_limit"}"#)),
        }
    }
    /// Called without the Outbox lock. Lock order is always State -> Outbox.
    pub(crate) fn prepare_paints(&self, conn: &Arc<Conn>) -> bool {
        let st = self.lock();
        let mut prepared = false;
        for (id, viewers) in &st.viewers {
            if st.stitching.contains(id) {
                continue;
            }
            for v in viewers.iter().filter(|v| Arc::ptr_eq(&v.conn, conn)) {
                if conn.outbox().paint_ready(v.channel) {
                    self.enqueue_paint(&st, *id, conn, v.channel, 0, crate::paint::Reason::Resync, false, 0);
                    prepared = true;
                }
            }
        }
        prepared
    }
    pub(crate) fn stitch_paints(&self, st: &mut State, id: u32) {
        let targets: Vec<_> = st
            .viewers
            .get_mut(&id)
            .into_iter()
            .flatten()
            .map(|v| (v.conn.clone(), v.channel, v.marks, std::mem::take(&mut v.suppressed)))
            .collect();
        for (conn, ch, marks, loss) in targets {
            self.enqueue_paint(st, id, &conn, ch, 0, crate::paint::Reason::Stitch, marks, loss);
        }
    }
    /// Freeze the final baseline before the canonical screen is removed. A
    /// deferred retry must not depend on a live terminal after program exit.
    pub(crate) fn complete_paints(&self, st: &State, id: u32) {
        if !st.screens.contains_key(&id) {
            return;
        }
        for v in st.viewers.get(&id).into_iter().flatten() {
            if v.suppressed > 0 || v.conn.outbox().needs_paint(v.channel) {
                self.enqueue_paint(st, id, &v.conn, v.channel, 0, crate::paint::Reason::Resync, true, v.suppressed);
            }
        }
    }
}
