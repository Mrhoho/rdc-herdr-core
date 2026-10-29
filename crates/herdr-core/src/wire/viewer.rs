//! Ordered DATA / MARK queue. Paint is a lazy DATA producer in this same queue.
use super::*;
use crate::paint::{self, Job, Mark, Reason, Source};
use ghostty_vt::budget::{Budget, Reservation};
use std::sync::{Arc, Weak};

// Charge queue nodes and allocator overhead as well as payload. Linked nodes
// release capacity with each item; the base allowance is retained until close.
const BASE: usize = 4096;
const ITEM: usize = 256;
enum Item {
    Data { bytes: Vec<u8>, at: usize, _lease: Reservation },
    Mark { bytes: Vec<u8>, _lease: Reservation },
    Paint(Box<Job>),
}
pub(super) struct ViewerQueue {
    items: std::collections::LinkedList<Item>,
    budget: Arc<Budget>,
    _base: Reservation,
    pub marks: bool,
    generation: u64,
    bytes: usize,
    dropped: u64,
    painting: usize,
    // Once an in-progress paint was replaced, suppress live tail and coalesce
    // it into one subsequent snapshot. The recovery paint can now finish even
    // under continuous output. No unbounded series of frozen sources is kept.
    protected: bool,
    retry: bool,
    pub needs: bool,
}
impl ViewerQueue {
    pub fn new(marks: bool, budget: Arc<Budget>) -> Option<Self> {
        let base = budget.reserve(BASE)?;
        Some(Self {
            items: std::collections::LinkedList::new(),
            budget,
            _base: base,
            marks,
            generation: 0,
            bytes: 0,
            dropped: 0,
            painting: 0,
            protected: false,
            retry: false,
            needs: false,
        })
    }
    pub fn needs_retry(&self) -> bool {
        self.needs || self.retry
    }
    pub fn empty(&self) -> bool {
        self.items.is_empty() && !self.needs
    }
    pub fn ready(&self) -> bool {
        self.needs && self.items.is_empty()
    }
    pub fn queued(&self) -> usize {
        self.bytes + usize::from(!self.items.is_empty() || self.needs)
    }
    pub fn mark(&mut self, mark: Mark) -> Result<(), ()> {
        if !self.marks {
            return Ok(());
        }
        let bytes = mark.bytes();
        let lease = self.budget.reserve(bytes.len() * 2 + ITEM).ok_or(())?;
        self.items.push_back(Item::Mark { bytes, _lease: lease });
        Ok(())
    }
    pub fn data(&mut self, bytes: &[u8]) -> bool {
        if bytes.is_empty() {
            return true;
        }
        if self.needs || (self.painting > 0 && self.protected) || self.retry {
            self.dropped = self.dropped.saturating_add(bytes.len() as u64);
            self.retry = true;
            return true;
        }
        let lease = (self.bytes.saturating_add(bytes.len()) <= MAX_PENDING)
            .then(|| self.budget.reserve(bytes.len() * 2 + ITEM))
            .flatten();
        let Some(lease) = lease else {
            self.reset(bytes.len() as u64);
            self.needs = true;
            return false;
        };
        self.bytes += bytes.len();
        self.items.push_back(Item::Data { bytes: bytes.to_vec(), at: 0, _lease: lease });
        true
    }
    pub fn reset(&mut self, extra: u64) {
        for item in &self.items {
            if let Item::Paint(job) = item {
                self.dropped = self.dropped.saturating_add(job.cancel());
            }
        }
        self.protected |= self.painting > 0;
        self.dropped = self.dropped.saturating_add(self.bytes as u64).saturating_add(extra);
        self.items = std::collections::LinkedList::new();
        self.bytes = 0;
        self.painting = 0;
        self.retry = false;
    }
    /// The only way to enqueue synthetic output. Call with Core State held so
    /// capture, insertion and subsequent live output have one total order.
    pub fn enqueue(
        &mut self,
        source: Source,
        conn: Weak<Conn>,
        reason: Reason,
        replace: bool,
        loss: u64,
    ) -> Result<(), ()> {
        if replace {
            self.reset(loss);
        } else {
            self.dropped = self.dropped.saturating_add(loss);
        }
        self.generation = self.generation.checked_add(1).ok_or(())?;
        let generation = self.generation;
        let (cols, rows) = source.size;
        let job = Job::new(source, conn).map_err(|_| ())?;
        if self.dropped > 0 || reason == Reason::Resync || (replace && reason == Reason::Stitch) {
            self.mark(Mark::Discontinuity { dropped: self.dropped })?;
        }
        self.dropped = 0;
        self.needs = false;
        self.retry = false;
        self.mark(Mark::PaintBegin { reason, generation, cols, rows })?;
        self.items.push_back(Item::Paint(Box::new(job)));
        self.mark(Mark::PaintEnd { generation })?;
        self.painting += 1;
        Ok(())
    }
    pub fn next(&mut self, channel: u32, credit: &mut usize) -> Result<Option<Vec<u8>>, ()> {
        loop {
            let Some(front) = self.items.front_mut() else { return Ok(None) };
            match front {
                Item::Mark { bytes, .. } => {
                    let frame = encode(channel, MARK, bytes);
                    self.items.pop_front();
                    return Ok(Some(frame));
                }
                Item::Data { bytes, at, .. } => {
                    let n = (bytes.len() - *at).min(*credit).min(CHUNK);
                    if n == 0 {
                        return Ok(None);
                    }
                    let frame = encode(channel, DATA, &bytes[*at..*at + n]);
                    *credit -= n;
                    self.bytes -= n;
                    *at += n;
                    if *at == bytes.len() {
                        self.items.pop_front();
                    }
                    return Ok(Some(frame));
                }
                Item::Paint(job) => match job.poll(*credit) {
                    paint::Poll::Pending => return Ok(None),
                    paint::Poll::Failed => return Err(()),
                    paint::Poll::Data(data) => {
                        *credit -= data.bytes.len();
                        let frame = encode(channel, DATA, &data.bytes);
                        // data's lease also covers this bounded encoding copy.
                        drop(data);
                        return Ok(Some(frame));
                    }
                    paint::Poll::End => {
                        self.items.pop_front();
                        self.painting -= 1;
                        if self.painting == 0 {
                            self.protected = false;
                        }
                        if self.retry {
                            self.needs = true;
                        }
                    }
                },
            }
        }
    }
}
