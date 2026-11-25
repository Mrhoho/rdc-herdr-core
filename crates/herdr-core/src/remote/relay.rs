//! What a relayed viewer stream decides on its own, apart from any IO: how
//! much of the viewer's credit the stream there may have, across lines.

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
}

impl Relay {
    pub(super) fn switch(&mut self, frames: &mpsc::UnboundedSender<Vec<u8>>, there: u32) {
        self.ledger.switch();
        self.to = Some((frames.clone(), there));
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
    use super::Ledger;

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
