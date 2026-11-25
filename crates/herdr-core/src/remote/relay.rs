//! What a relayed viewer stream decides on its own, apart from any IO: how
//! much of the viewer's credit the stream there may have, across lines, and
//! how a marked stream's paint marks read to the viewer, gaps included.

use super::*;

/// One relayed stream as both its record (`Relayed`) and its line's sink see
/// it. A reattached stream is switched to as its reply is read, before the
/// reattach that asked for it runs again: the frames read right after the
/// reply, and credit sent meanwhile, are the new stream's already.
pub(crate) type Shared = Arc<Mutex<Relay>>;

#[derive(Default)]
pub(crate) struct Relay {
    pub(super) ledger: Ledger,
    /// Where credit goes: the stream there relayed now (its line's frames, its channel).
    pub(super) to: Option<(mpsc::UnboundedSender<Vec<u8>>, u32)>,
    /// A marked stream's paint marks.
    pub(super) marks: Option<Marks>,
}

impl Relay {
    pub(super) fn marked() -> Relay {
        Relay { marks: Some(Marks::default()), ..Relay::default() }
    }

    pub(super) fn switch(&mut self, frames: &mpsc::UnboundedSender<Vec<u8>>, there: u32) {
        self.ledger.switch();
        if let Some(marks) = &mut self.marks {
            marks.switch();
        }
        self.to = Some((frames.clone(), there));
    }

    /// The stream there ended: true the first time for it on a marked
    /// stream, which is then to drop what its viewer has not read and tell
    /// it of the gap. Its line's cleanup says so, else the next stream's
    /// reply (a first one finds nothing to drop, nothing painted to cut short).
    pub(super) fn ended(&mut self) -> bool {
        self.marks.as_mut().is_some_and(|marks| !std::mem::replace(&mut marks.told, true))
    }

    /// What the viewer had not read was dropped: `bytes`, and `marks` among
    /// them, voided by `gap` (a discontinuity from there, or one made here).
    /// The credit due back to the stream there they came from, if it is
    /// still there, and what the viewer reads in their place.
    pub(super) fn dropped(&mut self, gap: Value, bytes: usize, marks: &[Vec<u8>]) -> (u32, Option<Vec<u8>>) {
        let back = self.ledger.dropped(bytes);
        (back, self.marks.as_mut().and_then(|m| m.dropped(gap, bytes, marks)))
    }
}

/// A MARK from the stream there, as the viewer is to get it.
#[derive(Debug, PartialEq)]
pub(super) enum Rewritten {
    Mark(Vec<u8>),
    /// A discontinuity: what is queued for the viewer is void too.
    Gap(Value),
}

/// Paint marks for one stream here fed by one stream there per line. Each
/// stream there counts its paints from 1 and calls its first an attach; the
/// viewer must see one count rising and only one attach, its first paint.
#[derive(Default, Debug, PartialEq)]
pub(super) struct Marks {
    /// The largest `paintGen` passed on to the viewer.
    pub(super) sent: u64,
    /// What the stream there now adds to its own `paintGen`.
    pub(super) offset: u64,
    /// `paint_begin`s passed on to the viewer: sent, or queued to be.
    pub(super) begins: u64,
    /// The viewer was told the stream there ended: once for each.
    pub(super) told: bool,
}

impl Marks {
    /// A new stream there: its paints come after every one passed on.
    pub(super) fn switch(&mut self) {
        self.offset = self.sent;
        self.told = false;
    }

    /// A MARK from the stream there as the viewer gets it. Err: no mark the
    /// viewer could follow; the stream ends.
    pub(super) fn rewrite(&mut self, payload: &[u8]) -> Result<Rewritten, ()> {
        let Ok(Value::Object(mut mark)) = serde_json::from_slice(payload) else { return Err(()) };
        let begin = match mark.get("type").and_then(Value::as_str) {
            Some("paint_begin") => true,
            Some("paint_end") => false,
            Some("discontinuity") => return Ok(Rewritten::Gap(Value::Object(mark))),
            // A viewer that can't follow one ends its stream; it decides that, not the relay.
            _ => return Ok(Rewritten::Mark(payload.to_vec())),
        };
        let generation = mark.get("paintGen").and_then(Value::as_u64).and_then(|g| g.checked_add(self.offset));
        let generation = generation.ok_or(())?;
        mark.insert("paintGen".into(), generation.into());
        if begin {
            // A later stream's first paint continues the viewer's screen.
            let attach = mark.get("reason").and_then(Value::as_str) == Some("attach");
            let reason = match self.begins {
                0 => Some("attach"),
                _ if attach => Some("stitch"),
                _ => None,
            };
            if let Some(reason) = reason {
                mark.insert("reason".into(), reason.into());
            }
            self.begins += 1;
        }
        self.sent = self.sent.max(generation);
        Ok(Rewritten::Mark(serde_json::to_vec(&mark).expect("a mark serializes")))
    }

    /// `marks` and `bytes` rewritten here were dropped unread, voided by
    /// `gap`: the viewer reads it, saying what it missed, unless nothing it
    /// was painted is left to cut short (its next paint is then its attach).
    fn dropped(&mut self, mut gap: Value, bytes: usize, marks: &[Vec<u8>]) -> Option<Vec<u8>> {
        let mut missed = gap["droppedBytes"].as_u64().unwrap_or(0).saturating_add(bytes as u64);
        for mark in marks {
            let mark: Value = serde_json::from_slice(mark).unwrap_or_default();
            match mark["type"].as_str() {
                Some("paint_begin") => self.begins = self.begins.saturating_sub(1),
                // One it never read: this one says it all.
                Some("discontinuity") => missed = missed.saturating_add(mark["droppedBytes"].as_u64().unwrap_or(0)),
                _ => {}
            }
        }
        gap["droppedBytes"] = missed.into();
        (self.begins > 0).then(|| serde_json::to_vec(&gap).expect("a mark serializes"))
    }
}

/// Credit for one stream here that is fed by one stream there per line, in
/// bytes since it opened: `enqueued` into it (notices written here too),
/// `credited` by the viewer, and `boundary`, where the stream there now
/// began. Each stream there starts with a window of its own, so credit for
/// bytes before its boundary is not its to have: given anyway, every line
/// drop would widen the window there by what the viewer still owed.
#[derive(Default, Debug, PartialEq)]
pub(super) struct Ledger {
    enqueued: u64,
    credited: u64,
    boundary: u64,
}

impl Ledger {
    pub(super) fn enqueued(&mut self, bytes: usize) {
        self.enqueued += bytes as u64;
    }

    /// A new stream there: what is already queued here is not its.
    pub(super) fn switch(&mut self) {
        self.boundary = self.enqueued;
    }

    /// The viewer read `bytes` more: how many of them the stream there gets.
    pub(super) fn credited(&mut self, bytes: u32) -> u32 {
        let before = self.credited;
        self.credited += u64::from(bytes);
        self.credited.saturating_sub(self.boundary.max(before)) as u32
    }

    /// `bytes` queued here and never sent were dropped. The viewer won't
    /// credit them, so they leave the count; the share that came from the
    /// stream there now (the newest bytes are dropped first) goes back to it.
    pub(super) fn dropped(&mut self, bytes: usize) -> u32 {
        let bytes = (bytes as u64).min(self.enqueued);
        let own = bytes.min(self.enqueued - self.boundary);
        self.enqueued -= bytes;
        self.boundary -= bytes - own;
        own as u32
    }
}

#[cfg(test)]
mod tests {
    use super::{Ledger, Marks, Relay, Rewritten};
    use serde_json::{Value, json};

    fn rewrite(m: &mut Marks, mark: Value) -> Value {
        match m.rewrite(mark.to_string().as_bytes()).unwrap() {
            Rewritten::Mark(bytes) => serde_json::from_slice(&bytes).unwrap(),
            Rewritten::Gap(gap) => panic!("a gap: {gap}"),
        }
    }

    fn begin(reason: &str, generation: u64) -> Value {
        let colors = json!({"fg": [1, 2, 3], "bg": null, "cursor": null, "palette": [[0, 0, 0]], "dark": true});
        json!({"type": "paint_begin", "reason": reason, "paintGen": generation, "cols": 80, "rows": 24,
            "colors": colors, "future": {"kept": true}})
    }

    #[test]
    fn a_first_stream_passes_its_marks_as_they_are() {
        let mut m = Marks::default();
        assert_eq!(rewrite(&mut m, begin("attach", 1)), begin("attach", 1));
        assert_eq!(
            rewrite(&mut m, json!({"type": "paint_end", "paintGen": 1})),
            json!({"type": "paint_end", "paintGen": 1})
        );
        let gap = json!({"type": "discontinuity", "droppedBytes": 7, "future": 1});
        assert_eq!(m.rewrite(gap.to_string().as_bytes()), Ok(Rewritten::Gap(gap)), "the caller drops what it voids");
        assert_eq!(rewrite(&mut m, begin("resync", 2)), begin("resync", 2));
        let other = json!({"type": "later_kind", "x": [1]});
        assert_eq!(rewrite(&mut m, other.clone()), other, "the viewer decides what it can't follow");
        assert_eq!(m, Marks { sent: 2, offset: 0, begins: 2, told: false });
    }

    #[test]
    fn a_later_stream_counts_on_from_the_last_paint_and_stitches_its_first() {
        let mut m = Marks::default();
        rewrite(&mut m, begin("attach", 1));
        rewrite(&mut m, json!({"type": "paint_end", "paintGen": 1}));
        rewrite(&mut m, begin("resync", 2));
        m.switch();
        assert_eq!(rewrite(&mut m, begin("attach", 1)), begin("stitch", 3));
        assert_eq!(rewrite(&mut m, json!({"type": "paint_end", "paintGen": 1}))["paintGen"], 3);
        assert_eq!(rewrite(&mut m, begin("resync", 2)), begin("resync", 4));
        assert_eq!(m, Marks { sent: 4, offset: 2, begins: 4, told: false });
    }

    #[test]
    fn the_viewers_first_paint_is_its_attach_whatever_the_stream_there_says() {
        // A stream there that never painted before the next one began.
        let mut m = Marks::default();
        m.switch();
        assert_eq!(rewrite(&mut m, begin("attach", 1)), begin("attach", 1));
        let mut m = Marks::default();
        assert_eq!(rewrite(&mut m, begin("resync", 1)), begin("attach", 1));
    }

    #[test]
    fn a_mark_the_viewer_could_not_follow_ends_the_stream() {
        for bad in [
            json!({"type": "paint_begin", "reason": "attach", "cols": 80, "rows": 24}),
            json!({"type": "paint_end", "paintGen": "1"}),
            json!({"type": "paint_end", "paintGen": -1}),
            json!({"type": "paint_end", "paintGen": 1.5}),
            json!(["paint_end", 1]),
        ] {
            let mut m = Marks::default();
            assert_eq!(m.rewrite(bad.to_string().as_bytes()), Err(()), "{bad}");
            assert_eq!(m, Marks::default(), "{bad}");
        }
        assert_eq!(Marks::default().rewrite(b"{"), Err(()));
        let mut m = Marks { offset: u64::MAX, ..Marks::default() };
        assert_eq!(m.rewrite(br#"{"type":"paint_end","paintGen":1}"#), Err(()));
    }

    /// What `marks` and `bytes`, dropped unread for `gap`, leave the viewer to read.
    fn dropped(m: &mut Marks, gap: Value, bytes: usize, marks: &[Value]) -> Option<Value> {
        let marks: Vec<Vec<u8>> = marks.iter().map(|m| m.to_string().into_bytes()).collect();
        m.dropped(gap, bytes, &marks).map(|gap| serde_json::from_slice(&gap).unwrap())
    }

    #[test]
    fn a_drop_takes_back_the_paints_it_held_and_says_what_the_viewer_missed() {
        let mut m = Marks::default();
        rewrite(&mut m, begin("attach", 1));
        rewrite(&mut m, json!({"type": "paint_end", "paintGen": 1}));
        let queued = [begin("resync", 2), json!({"type": "paint_end", "paintGen": 2})];
        for mark in &queued {
            rewrite(&mut m, mark.clone());
        }
        let far = json!({"type": "discontinuity", "droppedBytes": 7, "future": 1});
        let read = dropped(&mut m, far, 100, &queued);
        assert_eq!(read, Some(json!({"type": "discontinuity", "droppedBytes": 107, "future": 1})));
        assert_eq!(m.begins, 1, "the first paint is the viewer's still");
        // A gap it never read is in the next one's count.
        let unread = [json!({"type": "discontinuity", "droppedBytes": 5}), rewrite(&mut m, begin("resync", 3))];
        assert_eq!(m.begins, 2);
        assert_eq!(
            dropped(&mut m, json!({"type": "discontinuity"}), 10, &unread),
            Some(json!({"type": "discontinuity", "droppedBytes": 15}))
        );
        assert_eq!(m.begins, 1);
    }

    #[test]
    fn a_drop_of_the_only_paint_leaves_no_gap_and_the_next_paint_its_attach() {
        let mut m = Marks::default();
        let first = begin("attach", 1);
        rewrite(&mut m, first.clone());
        assert_eq!(dropped(&mut m, json!({"type": "discontinuity"}), 30, &[first]), None);
        assert_eq!(dropped(&mut m, json!({"type": "discontinuity", "droppedBytes": 3}), 0, &[]), None);
        m.switch();
        assert_eq!(rewrite(&mut m, begin("attach", 1)), begin("attach", 2), "later than any it was given");
    }

    #[test]
    fn each_stream_there_ends_once() {
        let (frames, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut r = Relay::marked();
        assert!(r.ended(), "before the first: nothing queued yet");
        assert_eq!(r.dropped(json!({"type": "discontinuity"}), 0, &[]), (0, None), "and nothing to tell");
        r.switch(&frames, 9);
        assert!(r.ended(), "its line dropped");
        assert!(!r.ended(), "and the next reply came: told already");
        r.switch(&frames, 10);
        assert!(r.ended(), "its line dropped unheard: the next reply tells");
        let mut plain = Relay::default();
        plain.switch(&frames, 9);
        assert!(!plain.ended(), "an unmarked viewer is told in words");
    }

    #[test]
    fn a_dropped_backlog_gives_back_only_the_live_streams_share() {
        let (frames, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut r = Relay::marked();
        r.switch(&frames, 9);
        r.ledger.enqueued(1000);
        r.switch(&frames, 10);
        r.ledger.enqueued(300);
        let (back, gap) = r.dropped(json!({"type": "discontinuity"}), 500, &[]);
        assert_eq!((back, gap), (300, None), "nothing painted, and 200 of it the stream gone");
    }

    const NOTICE: usize = 40;

    #[test]
    fn a_reconnect_gives_the_new_stream_credit_only_for_its_own_bytes() {
        let mut l = Ledger::default();
        l.switch();
        l.enqueued(1000);
        assert_eq!(l.credited(600), 600);
        l.enqueued(NOTICE); // the line dropped
        l.switch();
        l.enqueued(300);
        assert_eq!(l.credited(400 + NOTICE as u32), 0, "the old stream's bytes and the notice");
        assert_eq!(l.credited(300), 300);
    }

    #[test]
    fn credit_spanning_the_boundary_gives_only_the_part_past_it() {
        let mut l = Ledger::default();
        l.enqueued(1000);
        l.switch();
        l.enqueued(500);
        assert_eq!(l.credited(900), 0);
        assert_eq!(l.credited(300), 200);
        assert_eq!(l.credited(300), 300);
    }

    #[test]
    fn reconnects_in_a_row_while_the_viewer_credits_nothing_give_no_stream_more() {
        let mut l = Ledger::default();
        l.enqueued(1000);
        for _ in 0..3 {
            l.enqueued(NOTICE);
            l.switch();
            l.enqueued(1000); // each new stream there spends its own window
        }
        assert_eq!(l.credited(3000 + 3 * NOTICE as u32), 0, "all of it before the last boundary");
        assert_eq!(l.credited(1000), 1000);
        assert_eq!(l.credited(5), 5, "a viewer may give more than it was sent, as it could directly");
    }

    #[test]
    fn dropped_bytes_leave_the_count_and_the_new_streams_share_goes_back_to_it() {
        // All dropped bytes came after the boundary.
        let mut l = Ledger::default();
        l.enqueued(1000);
        l.switch();
        l.enqueued(500);
        assert_eq!(l.dropped(200), 200);
        l.enqueued(100);
        assert_eq!(l.credited(1000 + 300 + 100), 400);
        // Some came before it, from a stream that is gone.
        let mut l = Ledger::default();
        l.enqueued(1000);
        assert_eq!(l.credited(400), 400);
        l.switch();
        l.enqueued(500);
        assert_eq!(l.dropped(800), 500);
        assert_eq!(l, Ledger { enqueued: 700, credited: 400, boundary: 700 });
        l.enqueued(50);
        assert_eq!(l.credited(300), 0, "the rest the viewer got before the boundary");
        assert_eq!(l.credited(50), 50);
    }
}
