//! Framed mode: `[channel u32 LE][kind u8][len u32 LE][payload]`.
//! Channel 0 carries JSON-RPC messages; attach streams get their own channels.

use std::collections::{BTreeMap, VecDeque};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const HEADER: usize = 9;
pub const MAX_PAYLOAD: usize = 16 << 20;

/// Payload bytes: JSON-RPC on channel 0, terminal bytes on streams.
pub const DATA: u8 = 0;
/// u32 LE: the receiver consumed this many bytes; the sender may send more.
pub const CREDIT: u8 = 1;
/// The channel is over (JSON reason from the core, empty from a client).
pub const CLOSE: u8 = 2;
/// u16 LE cols, u16 LE rows: viewer → core, core → observer.
pub const RESIZE: u8 = 3;
/// Viewer → core: 1 = gained focus, 0 = lost it.
pub const FOCUS: u8 = 4;
/// Viewer → core: JSON display facts (colors, cell size).
pub const FACTS: u8 = 5;
/// Viewer → core: terminal-generated replies, not human activity.
pub const AUTOMATIC_INPUT: u8 = 6;
/// Ordered paint metadata; opt-in viewer streams only, no DATA credit charged.
pub const MARK: u8 = 6;
mod viewer;
use viewer::ViewerQueue;

/// Credit a stream starts with before the viewer acknowledges anything.
pub const WINDOW: usize = 256 * 1024;
/// Output kept for a viewer that stopped reading; past it, it gets a repaint instead.
pub const MAX_PENDING: usize = 4 << 20;
/// Replies kept for a client that stopped reading; past it, it is dropped.
pub const MAX_CONTROL: usize = 64 << 20;
/// Relayed MARKs kept for a viewer that stopped reading, `MAX_PENDING` of
/// them at most; past either, its stream closes (`PAINT_LIMIT`).
pub const MAX_MARKS: usize = 1024;
/// A stream closed though its terminal runs on: what it held grew too big.
pub const PAINT_LIMIT: &[u8] = br#"{"reason":"paint_limit"}"#;
const CHUNK: usize = 16 * 1024;

pub fn encode(channel: u32, kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(&channel.to_le_bytes());
    out.push(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode_header(h: &[u8; HEADER]) -> Result<(u32, u8, usize), String> {
    let channel = u32::from_le_bytes(h[0..4].try_into().unwrap());
    let len = u32::from_le_bytes(h[5..9].try_into().unwrap()) as usize;
    if len > MAX_PAYLOAD {
        return Err(format!("frame of {len} bytes exceeds {MAX_PAYLOAD}"));
    }
    Ok((channel, h[4], len))
}

/// Reads one frame; `Ok(None)` on a clean end of stream between frames.
pub async fn read_frame(r: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Option<(u32, u8, Vec<u8>)>> {
    let mut header = [0u8; HEADER];
    match r.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let (channel, kind, len) = decode_header(&header).map_err(std::io::Error::other)?;
    let mut payload = vec![0; len];
    r.read_exact(&mut payload).await?;
    Ok(Some((channel, kind, payload)))
}

/// When bytes last came in on the readers it watches.
pub struct Heard {
    since: tokio::time::Instant,
    ms: std::sync::atomic::AtomicU64,
}

impl Default for Heard {
    fn default() -> Heard {
        Heard { since: tokio::time::Instant::now(), ms: Default::default() }
    }
}

impl Heard {
    pub fn watch<'a, R: AsyncRead + Unpin>(&'a self, inner: &'a mut R) -> Watched<'a, R> {
        Watched { inner, heard: self }
    }

    /// Returns once nothing came for `quiet`.
    pub async fn silent_for(&self, quiet: std::time::Duration) {
        loop {
            let ms = self.ms.load(std::sync::atomic::Ordering::Relaxed);
            let until = self.since + std::time::Duration::from_millis(ms) + quiet;
            if tokio::time::Instant::now() >= until {
                return;
            }
            tokio::time::sleep_until(until).await;
        }
    }
}

pub struct Watched<'a, R> {
    inner: &'a mut R,
    heard: &'a Heard,
}

impl<R: AsyncRead + Unpin> AsyncRead for Watched<'_, R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut *self.inner).poll_read(cx, buf);
        if buf.filled().len() > before {
            let ms = self.heard.since.elapsed().as_millis() as u64;
            self.heard.ms.store(ms, std::sync::atomic::Ordering::Relaxed);
        }
        polled
    }
}

/// A framed connection as the rest of the core sees it: queue frames, wake the writer.
#[derive(Default)]
pub struct Conn {
    outbox: std::sync::Mutex<Outbox>,
    pub wake: tokio::sync::Notify,
    /// It fell too far behind (`MAX_CONTROL`): the connection must end.
    pub overflowed: tokio::sync::Notify,
    next_channel: std::sync::atomic::AtomicU32,
    /// Its reader ended: calls still running for it must not leave state behind.
    pub gone: std::sync::atomic::AtomicBool,
    /// Puts whose begin was read here and hasn't finished: how many such
    /// begins, and whether an abort for it came meanwhile (calls run side by side).
    pub puts_pending: std::sync::Mutex<std::collections::HashMap<String, (usize, bool)>>,
    /// The core ends it: it misbehaved.
    pub end: tokio::sync::Notify,
    // 0 = open, 1 = closing, 2 = closed. Admission and transitions hold
    // Core::State; the atomic also lets writers discard late replies.
    lifecycle: std::sync::atomic::AtomicU8,
}

impl Conn {
    /// Caller holds Core::State until registration is complete.
    pub(crate) fn admit(&self, _st: &crate::runtime::State) -> Result<(), crate::rpc::RpcError> {
        if self.lifecycle.load(std::sync::atomic::Ordering::Acquire) != 0 {
            return Err(crate::rpc::RpcError::new(
                "connection_closed",
                "connection is closed",
                "open a new connection",
            ));
        }
        Ok(())
    }

    pub(crate) fn begin_close(&self, _st: &crate::runtime::State) {
        let mut out = self.outbox();
        let _ = self.lifecycle.compare_exchange(
            0,
            1,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
        *out = Outbox::default();
    }

    pub(crate) fn end_close(&self, _st: &crate::runtime::State) {
        self.lifecycle.store(2, std::sync::atomic::Ordering::Release);
    }

    pub fn outbox(&self) -> std::sync::MutexGuard<'_, Outbox> {
        self.outbox.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn new_channel(&self) -> u32 {
        self.next_channel.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
    }

    /// Queue frames under the outbox lock, then wake the writer.
    pub fn with(&self, f: impl FnOnce(&mut Outbox)) {
        let mut out = self.outbox();
        if self.lifecycle.load(std::sync::atomic::Ordering::Acquire) != 0 {
            return;
        }
        f(&mut out);
        if out.overflowed {
            self.overflowed.notify_one();
        }
        drop(out);
        self.wake.notify_one();
    }
}

struct Stream {
    viewer: Option<ViewerQueue>,
    pending: VecDeque<u8>,
    /// RESIZE and MARK frames in the order they came, each due once the
    /// bytes queued before it are out: (bytes of `pending` before it, kind, payload).
    boundaries: VecDeque<(usize, u8, Vec<u8>)>,
    paused: bool,
    credit: usize,
    /// The program ended: CLOSE (with this reason) once `pending` and its MARKs are out.
    ending: Option<Vec<u8>>,
}

/// What one connection still has to send. Control frames always go first;
/// streams share what's left in turn, each limited by its own credit, so a
/// viewer that stops reading never holds up typing, replies or other viewers.
#[derive(Default)]
pub struct Outbox {
    control: VecDeque<Vec<u8>>,
    control_bytes: usize,
    /// Past `MAX_CONTROL`: everything dropped, nothing more taken or sent.
    overflowed: bool,
    streams: BTreeMap<u32, Stream>,
    turn: u32,
}

impl Outbox {
    pub fn control(&mut self, frame: Vec<u8>) {
        if self.overflowed {
            return;
        }
        self.control_bytes += frame.len();
        if self.control_bytes > MAX_CONTROL {
            *self = Outbox { overflowed: true, ..Outbox::default() };
            return;
        }
        self.control.push_back(frame);
    }

    pub fn open(&mut self, channel: u32) {
        self.streams.insert(
            channel,
            Stream {
                viewer: None,
                pending: VecDeque::new(),
                boundaries: VecDeque::new(),
                paused: false,
                credit: WINDOW,
                ending: None,
            },
        );
    }

    /// Attach output must wait until the reply installs the receiver's channel mapping.
    pub fn open_paused(&mut self, channel: u32) {
        self.open(channel);
        self.pause(channel);
    }

    pub fn pause(&mut self, channel: u32) {
        if let Some(s) = self.streams.get_mut(&channel) {
            s.paused = true;
        }
    }

    pub fn resume(&mut self, channel: u32) {
        if let Some(s) = self.streams.get_mut(&channel) {
            s.paused = false;
        }
    }

    pub fn can_send(&self, channel: u32) -> bool {
        self.streams.get(&channel).is_some_and(|s| !s.paused && s.credit > 0)
    }

    /// Supersede pending output with a new grid and an authoritative repaint.
    /// A partly delivered escape sequence is cancelled by the repaint's CAN.
    pub fn resize(&mut self, channel: u32, size: &[u8]) {
        if let Some(s) = self.streams.get_mut(&channel) {
            s.pending.clear();
            s.boundaries.clear();
            s.boundaries.push_back((0, RESIZE, size.to_vec()));
        }
    }

    /// Relay boundaries keep bytes already charged to the upstream credit window.
    pub fn ordered_resize(&mut self, channel: u32, size: &[u8]) {
        if let Some(s) = self.streams.get_mut(&channel) {
            s.boundaries.push_back((s.pending.len(), RESIZE, size.to_vec()));
        }
    }

    /// A relayed MARK, after the bytes queued so far. It needs no credit: it
    /// waits only for them. False: too many waited, and the stream is closed.
    pub fn mark(&mut self, channel: u32, mark: &[u8]) -> bool {
        let Some(s) = self.streams.get_mut(&channel) else { return true };
        s.boundaries.push_back((s.pending.len(), MARK, mark.to_vec()));
        if s.boundaries.len() > MAX_MARKS || s.boundaries.iter().map(|(_, _, p)| p.len()).sum::<usize>() > MAX_PENDING {
            self.close(channel, PAINT_LIMIT);
            return false;
        }
        true
    }

    pub fn open_viewer(&mut self, channel: u32, marks: bool, budget: std::sync::Arc<ghostty_vt::budget::Budget>) {
        self.open(channel);
        if let Some(queue) = ViewerQueue::new(marks, budget) {
            self.streams.get_mut(&channel).unwrap().viewer = Some(queue);
        } else {
            self.close(channel, PAINT_LIMIT);
        }
    }

    fn viewer(&mut self, channel: u32) -> Option<&mut ViewerQueue> {
        self.streams.get_mut(&channel)?.viewer.as_mut()
    }

    pub fn has_stream(&self, channel: u32) -> bool {
        self.streams.contains_key(&channel)
    }

    pub fn needs_paint(&self, channel: u32) -> bool {
        self.streams.get(&channel).and_then(|s| s.viewer.as_ref()).is_some_and(|q| q.needs_retry())
    }

    pub fn paint_ready(&self, channel: u32) -> bool {
        self.streams.get(&channel).and_then(|s| s.viewer.as_ref()).is_some_and(|q| q.ready())
    }

    pub fn enqueue_paint(
        &mut self,
        channel: u32,
        source: crate::paint::Source,
        conn: std::sync::Weak<Conn>,
        reason: crate::paint::Reason,
        replace: bool,
        loss: u64,
    ) {
        let Some(q) = self.viewer(channel) else { return };
        if q.enqueue(source, conn, reason, replace, loss).is_err() {
            self.close(channel, PAINT_LIMIT);
        }
    }

    /// Ends a stream now (the viewer detached): whatever is queued is dropped.
    pub fn close(&mut self, channel: u32, reason: &[u8]) {
        self.streams.remove(&channel);
        self.control(encode(channel, CLOSE, reason));
    }

    /// Ends a stream once its queued output is delivered (the program ended:
    /// its last screen is what the viewer most wants to see), including the
    /// end of its last paint.
    pub fn finish(&mut self, channel: u32, reason: &[u8]) {
        match self.streams.get_mut(&channel) {
            Some(s) => s.ending = Some(reason.to_vec()),
            None => self.control(encode(channel, CLOSE, reason)),
        }
    }

    /// Queues stream output. Returns false (and drops the backlog) when the
    /// viewer is too far behind: the caller replaces it with a repaint.
    pub fn data(&mut self, channel: u32, bytes: &[u8]) -> bool {
        let Some(s) = self.streams.get_mut(&channel) else { return true };
        if let Some(q) = &mut s.viewer {
            return q.data(bytes);
        }
        if s.pending.len() + bytes.len() > MAX_PENDING {
            s.pending.clear();
            // The repaint keeps the latest grid; marks of dropped paints go with them.
            let latest = s.boundaries.iter().rposition(|(_, kind, _)| *kind == RESIZE);
            let latest = latest.and_then(|at| s.boundaries.remove(at));
            s.boundaries.clear();
            if let Some((_, kind, size)) = latest {
                s.boundaries.push_back((0, kind, size));
            }
            return false;
        }
        s.pending.extend(bytes);
        true
    }

    /// `data` for a stream of whole lines (events): too far behind, it keeps
    /// the rest of a line whose start may be out already, so what replaces
    /// the backlog starts on a line of its own.
    pub fn data_lines(&mut self, channel: u32, bytes: &[u8]) -> bool {
        let Some(s) = self.streams.get_mut(&channel) else { return true };
        if s.pending.len() + bytes.len() > MAX_PENDING {
            let rest = s.pending.iter().position(|b| *b == b'\n').map_or(s.pending.len(), |at| at + 1);
            s.pending.truncate(rest);
            return false;
        }
        s.pending.extend(bytes);
        true
    }

    pub fn queued(&self, channel: u32) -> usize {
        self.streams.get(&channel).map_or(0, |s| s.viewer.as_ref().map_or(s.pending.len(), |q| q.queued()))
    }

    pub fn credit(&mut self, channel: u32, bytes: usize) {
        if let Some(s) = self.streams.get_mut(&channel) {
            s.credit = s.credit.saturating_add(bytes);
        }
    }

    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        if let Some(frame) = self.control.pop_front() {
            self.control_bytes -= frame.len();
            return Some(frame);
        }
        // A grid announced for bytes that will never come is left out.
        if let Some((&channel, _)) = self.streams.iter().find(|(_, s)| {
            !s.paused
                && s.ending.is_some()
                && s.pending.is_empty()
                && s.boundaries.iter().all(|(_, kind, _)| *kind != MARK)
                && s.viewer.as_ref().is_none_or(|q| q.empty())
        }) {
            let reason = self.streams.remove(&channel).and_then(|s| s.ending).unwrap_or_default();
            return Some(encode(channel, CLOSE, &reason));
        }
        let mut ready: Vec<u32> = self.streams.keys().copied().collect();
        let split = ready.partition_point(|ch| *ch <= self.turn);
        ready.rotate_left(split);
        for channel in ready {
            let s = self.streams.get_mut(&channel).expect("ready stream");
            if s.paused {
                continue;
            }
            if let Some(q) = &mut s.viewer {
                match q.next(channel, &mut s.credit) {
                    Ok(Some(frame)) => {
                        self.turn = channel;
                        return Some(frame);
                    }
                    Ok(None) => continue,
                    Err(()) => {
                        self.streams.remove(&channel);
                        return Some(encode(channel, CLOSE, PAINT_LIMIT));
                    }
                }
            }
            // A grid waits for credit and the paint it announces; a mark waits for neither.
            if let Some((0, kind, _)) = s.boundaries.front()
                && (*kind == MARK || (s.credit > 0 && !s.pending.is_empty()))
            {
                let (_, kind, payload) = s.boundaries.pop_front().unwrap();
                return Some(encode(channel, kind, &payload));
            }
            if s.credit == 0 || s.pending.is_empty() {
                continue;
            }
            let boundary = s.boundaries.front().map_or(usize::MAX, |(at, _, _)| *at);
            let n = s.pending.len().min(s.credit).min(CHUNK).min(boundary);
            for (at, _, _) in &mut s.boundaries {
                *at -= n;
            }
            if n == 0 {
                continue;
            }
            self.turn = channel;
            s.credit -= n;
            let chunk: Vec<u8> = s.pending.drain(..n).collect();
            return Some(encode(channel, DATA, &chunk));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_replaces_stalled_and_partially_sent_paints_atomically() {
        let mut out = Outbox::default();
        out.open(1);
        out.data(1, &vec![b'a'; WINDOW + 10]);
        while out.next_frame().is_some() {}
        out.resize(1, &[80, 0, 24, 0]);
        out.data(1, b"paint A");
        out.resize(1, &[90, 0, 30, 0]);
        out.data(1, &vec![b'b'; CHUNK * 2]);
        assert!(out.next_frame().is_none(), "no size transition ahead of stalled DATA");
        out.credit(1, CHUNK);
        let size = out.next_frame().unwrap();
        assert_eq!(size[4], RESIZE);
        assert_eq!(&size[HEADER..], &[90, 0, 30, 0]);
        assert_eq!(out.next_frame().unwrap()[4], DATA);
        out.resize(1, &[100, 0, 40, 0]);
        assert!(!out.data(1, &vec![0; MAX_PENDING + 1]));
        out.data(1, b"\x18latest");
        assert!(out.next_frame().is_none());
        out.credit(1, 100);
        let size = out.next_frame().unwrap();
        assert_eq!(size[4], RESIZE);
        assert_eq!(&size[HEADER..], &[100, 0, 40, 0]);
        assert_eq!(&out.next_frame().unwrap()[HEADER..], b"\x18latest");
        assert!(out.next_frame().is_none());
    }

    #[test]
    fn a_relay_keeps_charged_bytes_before_their_resize_boundary() {
        let mut out = Outbox::default();
        out.open(1);
        out.data(1, &vec![b'a'; WINDOW + 10]);
        while out.next_frame().is_some() {}
        out.ordered_resize(1, &[90, 0, 30, 0]);
        out.data(1, b"paint");
        assert_eq!(out.queued(1), 15, "upstream bytes must not lose their eventual CREDIT");
        assert!(out.next_frame().is_none());
        out.credit(1, 15);
        assert_eq!(&out.next_frame().unwrap()[HEADER..], &[b'a'; 10]);
        assert_eq!(out.next_frame().unwrap()[4], RESIZE);
        assert_eq!(&out.next_frame().unwrap()[HEADER..], b"paint");
        assert!(out.next_frame().is_none());
    }

    /// Every frame the stream has ready now: (kind, payload).
    fn frames(out: &mut Outbox) -> Vec<(u8, Vec<u8>)> {
        std::iter::from_fn(|| out.next_frame()).map(|f| (f[4], f[HEADER..].to_vec())).collect()
    }

    /// A stream whose window the viewer has used up.
    fn stalled(out: &mut Outbox, channel: u32) {
        out.open(channel);
        out.data(channel, &vec![b'a'; WINDOW]);
        while out.next_frame().is_some() {}
    }

    #[test]
    fn marks_and_grids_go_out_where_they_came_among_the_bytes() {
        let mut out = Outbox::default();
        out.open(1);
        out.data(1, b"a");
        out.mark(1, b"m1");
        out.ordered_resize(1, &[90, 0, 30, 0]);
        out.data(1, b"b");
        out.mark(1, b"m2");
        let expected = [(DATA, &b"a"[..]), (MARK, b"m1"), (RESIZE, &[90, 0, 30, 0]), (DATA, b"b"), (MARK, b"m2")]
            .map(|(k, p)| (k, p.to_vec()));
        assert_eq!(frames(&mut out), expected);
    }

    #[test]
    fn a_mark_needs_no_credit_but_never_overtakes_the_bytes_before_it() {
        let mut out = Outbox::default();
        stalled(&mut out, 1);
        out.mark(1, b"begin");
        assert_eq!(frames(&mut out), [(MARK, b"begin".to_vec())], "no credit, nothing behind it");
        out.data(1, b"paint");
        out.mark(1, b"end");
        assert_eq!(frames(&mut out), [], "the end waits behind the paint");
        out.credit(1, 5);
        assert_eq!(frames(&mut out), [(DATA, b"paint".to_vec()), (MARK, b"end".to_vec())]);
    }

    #[test]
    fn a_paused_stream_sends_no_mark() {
        let mut out = Outbox::default();
        out.open_paused(1);
        out.mark(1, b"begin");
        assert_eq!(frames(&mut out), [], "the viewer has no channel for it yet");
        out.resume(1);
        assert_eq!(frames(&mut out), [(MARK, b"begin".to_vec())]);
    }

    #[test]
    fn an_ended_stream_closes_after_its_last_paint_end() {
        let mut out = Outbox::default();
        stalled(&mut out, 1);
        out.data(1, b"paint");
        out.mark(1, b"end");
        out.finish(1, b"{}");
        assert_eq!(frames(&mut out), []);
        out.credit(1, 5);
        assert_eq!(frames(&mut out), [(DATA, b"paint".to_vec()), (MARK, b"end".to_vec()), (CLOSE, b"{}".to_vec())]);
        // A grid with no paint behind it is not waited for.
        out.open(2);
        out.ordered_resize(2, &[90, 0, 30, 0]);
        out.finish(2, b"{}");
        assert_eq!(frames(&mut out), [(CLOSE, b"{}".to_vec())]);
    }

    #[test]
    fn falling_too_far_behind_keeps_the_latest_grid_but_no_mark() {
        let mut out = Outbox::default();
        stalled(&mut out, 1);
        out.data(1, b"x");
        out.ordered_resize(1, &[80, 0, 24, 0]);
        out.mark(1, b"end");
        out.ordered_resize(1, &[90, 0, 30, 0]);
        out.mark(1, b"begin");
        assert!(!out.data(1, &vec![0; MAX_PENDING]));
        out.data(1, b"\x18paint");
        out.credit(1, 100);
        assert_eq!(frames(&mut out), [(RESIZE, vec![90, 0, 30, 0]), (DATA, b"\x18paint".to_vec())]);
    }

    #[test]
    fn too_many_marks_waiting_close_the_stream() {
        let mut out = Outbox::default();
        stalled(&mut out, 1);
        out.data(1, b"x");
        assert!((0..MAX_MARKS).all(|_| out.mark(1, b"m")));
        assert!(!out.mark(1, b"m"));
        assert_eq!(frames(&mut out), [(CLOSE, PAINT_LIMIT.to_vec())]);
        assert!(!out.has_stream(1));
        stalled(&mut out, 2);
        out.data(2, b"x");
        assert!(out.mark(2, &vec![b'm'; MAX_PENDING - 1]));
        assert!(!out.mark(2, b"mm"), "nor more than MAX_PENDING of them");
        assert_eq!(frames(&mut out), [(CLOSE, PAINT_LIMIT.to_vec())]);
    }

    #[test]
    fn attach_stream_waits_for_its_reply_before_announcing_the_grid() {
        let mut out = Outbox::default();
        out.open_paused(1);
        out.resize(1, &[80, 0, 24, 0]);
        out.data(1, b"paint");
        assert!(out.next_frame().is_none());
        out.control(encode(0, DATA, b"attach reply"));
        out.resume(1);
        assert_eq!(&out.next_frame().unwrap()[HEADER..], b"attach reply");
        assert_eq!(out.next_frame().unwrap()[4], RESIZE);
        assert_eq!(out.next_frame().unwrap()[4], DATA);
    }

    #[test]
    fn round_trips_and_rejects_oversized_frames() {
        let frame = encode(7, DATA, b"hi");
        let (channel, kind, len) = decode_header(frame[..HEADER].try_into().unwrap()).unwrap();
        assert_eq!((channel, kind, len, &frame[HEADER..]), (7, DATA, 2, &b"hi"[..]));

        let mut huge = [0u8; HEADER];
        huge[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_header(&huge).is_err());
    }

    #[test]
    fn replies_past_the_cap_drop_everything_and_take_nothing_more() {
        let mut out = Outbox::default();
        out.control(vec![0; MAX_CONTROL / 2]);
        assert!(out.next_frame().is_some(), "what is sent stops counting");
        out.control(vec![0; MAX_CONTROL / 2]);
        out.control(vec![0; MAX_CONTROL / 2]);
        assert!(!out.overflowed);
        out.control(vec![0; 1]);
        out.control(vec![0; 1]);
        assert!(out.overflowed && out.next_frame().is_none());
    }

    #[test]
    fn a_program_that_ends_still_delivers_its_last_output_first() {
        let mut out = Outbox::default();
        out.open(1);
        assert!(out.data(1, &vec![b'a'; WINDOW]));
        assert!(out.data(1, b"FINAL"));
        out.finish(1, b"{}");
        let kinds = |out: &mut Outbox| {
            std::iter::from_fn(|| out.next_frame()).map(|f| (f[4], f.len() - HEADER)).collect::<Vec<_>>()
        };
        let first = kinds(&mut out);
        assert!(first.iter().all(|(kind, _)| *kind == DATA), "no CLOSE while output waits for credit");
        out.credit(1, 5);
        let rest = kinds(&mut out);
        assert_eq!(rest, [(DATA, 5), (CLOSE, 2)], "the last output, then the end");
    }

    #[test]
    fn a_line_stream_that_falls_behind_still_delivers_whole_lines() {
        let mut out = Outbox::default();
        out.open(1);
        let line = [vec![b'x'; 99], b"\n".to_vec()].concat();
        let mut overflowed = false;
        let mut sent = Vec::new();
        for _ in 0..MAX_PENDING / line.len() + 10_000 {
            if !out.data_lines(1, &line) {
                overflowed = out.data(1, b"MISSED\n");
                break;
            }
            while let Some(f) = out.next_frame() {
                sent.extend(&f[HEADER..]); // the window, ending mid-line
            }
        }
        assert!(overflowed);
        out.credit(1, usize::MAX / 2);
        while let Some(f) = out.next_frame() {
            sent.extend(&f[HEADER..]);
        }
        let text = String::from_utf8(sent).unwrap();
        assert!(text.ends_with("MISSED\n"));
        assert!(text.lines().all(|l| l == "MISSED" || l.len() == 99), "a line was cut");
    }

    #[test]
    fn a_stalled_stream_never_blocks_control_or_other_streams() {
        let mut out = Outbox::default();
        out.open(1);
        out.open(2);
        assert!(out.data(1, &vec![b'a'; WINDOW * 2]));
        // Drain stream 1's whole window: it's now stalled waiting for credit.
        let mut sent = 0;
        while let Some(f) = out.next_frame() {
            sent += f.len() - HEADER;
        }
        assert_eq!(sent, WINDOW);

        out.control(encode(0, DATA, b"reply"));
        assert!(out.data(2, b"other viewer"));
        assert_eq!(&out.next_frame().unwrap()[HEADER..], b"reply");
        assert_eq!(&out.next_frame().unwrap()[HEADER..], b"other viewer");
        assert!(out.next_frame().is_none());

        // Streams take turns rather than one starving the other.
        out.credit(1, 100);
        assert!(out.data(2, b"more"));
        let first = out.next_frame().unwrap();
        let second = out.next_frame().unwrap();
        assert_ne!(first[..4], second[..4]);

        // Too far behind: the backlog is dropped so the caller can repaint.
        assert!(!out.data(1, &vec![b'b'; MAX_PENDING]));
    }
}

#[cfg(test)]
mod paint_tests;
