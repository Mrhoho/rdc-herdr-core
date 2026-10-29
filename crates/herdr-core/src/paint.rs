//! Frozen, credit-driven paint producer. No live Core lock is held while the
//! formatter runs or waits. A cancelled queue drops both channels and the worker
//! exits at its next write; every allocation of the clone shares the queue budget.
use crate::{runtime::State, wire::Conn};
use ghostty_vt::budget::{Budget, Bytes, Reservation};
use serde::Serialize;
use std::{
    io::{self, Write},
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc,
    },
};

pub const CHUNK: usize = 16 * 1024;
const STACK: usize = 512 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Attach,
    Resync,
    Stitch,
}
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Mark {
    PaintBegin {
        reason: Reason,
        #[serde(rename = "paintGen")]
        generation: u64,
        cols: u16,
        rows: u16,
        #[serde(skip_serializing_if = "Option::is_none")]
        colors: Option<Box<ColorDefaults>>,
    },
    PaintEnd {
        #[serde(rename = "paintGen")]
        generation: u64,
    },
    Discontinuity {
        #[serde(rename = "droppedBytes")]
        dropped: u64,
    },
}
impl Mark {
    pub fn bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("paint mark serializes")
    }
}
#[derive(Default)]
pub struct Metrics {
    pub generated: AtomicU64,
    pub workers: AtomicUsize,
}

pub struct Source {
    snapshot: Bytes,
    tail: Bytes,
    pub size: (u16, u16),
    pub colors: ColorDefaults,
    history: usize,
    continuation: bool,
    budget: Arc<Budget>,
    metrics: Arc<Metrics>,
}
impl Source {
    pub fn capture(
        st: &State,
        id: u32,
        history: usize,
        continuation: bool,
        budget: &Arc<Budget>,
        metrics: &Arc<Metrics>,
    ) -> io::Result<Self> {
        let screen = &st.screens[&id];
        let snapshot = screen.snapshot_budget(budget)?;
        let mut tail = Bytes::new(budget);
        let terminal = &st.reg.terminals[&id];
        tail.write_all(if terminal.win32_input { b"\x1b[?9001h" } else { b"\x1b[?9001l" })?;
        let root = terminal.status.iter().map(|s| ("", s));
        for (rid, status) in root.chain(terminal.records.iter().map(|(k, s)| (k.as_str(), s))) {
            tail.write_all(status.encode(rid).as_bytes())?;
        }
        Ok(Self {
            snapshot,
            tail,
            size: screen.size(),
            colors: ColorDefaults(screen.default_colors()),
            history,
            continuation,
            budget: budget.clone(),
            metrics: metrics.clone(),
        })
    }
}
pub struct Data {
    pub bytes: Vec<u8>,
    pub reservation: Reservation,
}
struct Permit {
    n: usize,
    reservation: Reservation,
}
enum Reply {
    Data(Data),
    End(bool),
}
pub enum Poll {
    Pending,
    Data(Data),
    End,
    Failed,
}

#[derive(Default)]
struct Progress {
    generated: u64,
    delivered: u64,
    cancelled: bool,
}

pub struct Job {
    progress: Arc<std::sync::Mutex<Progress>>,
    source: Option<Source>,
    budget: Arc<Budget>,
    conn: Weak<Conn>,
    permits: Option<mpsc::SyncSender<Permit>>,
    replies: Option<mpsc::Receiver<Reply>>,
    outstanding: bool,
    stack: Option<Reservation>,
}
impl Job {
    pub fn new(source: Source, conn: Weak<Conn>) -> io::Result<Self> {
        let stack = source.budget.reserve(STACK + 4096).ok_or_else(|| io::Error::other("paint_limit"))?;
        Ok(Self {
            progress: Default::default(),
            budget: source.budget.clone(),
            source: Some(source),
            conn,
            permits: None,
            replies: None,
            outstanding: false,
            stack: Some(stack),
        })
    }
    pub fn cancel(&self) -> u64 {
        let mut p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        p.cancelled = true;
        p.generated.saturating_sub(p.delivered)
    }
    pub fn poll(&mut self, credit: usize) -> Poll {
        if let Some(replies) = &self.replies {
            match replies.try_recv() {
                Ok(Reply::Data(data)) => {
                    self.progress.lock().unwrap_or_else(|e| e.into_inner()).delivered += data.bytes.len() as u64;
                    self.outstanding = false;
                    return Poll::Data(data);
                }
                Ok(Reply::End(true)) => return Poll::End,
                Ok(Reply::End(false)) | Err(mpsc::TryRecvError::Disconnected) => return Poll::Failed,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if credit == 0 || self.outstanding {
            return Poll::Pending;
        }
        let n = credit.min(CHUNK);
        // One bounded output chunk plus the wire encoding's temporary copy.
        let Some(reservation) = self.budget.reserve(n * 2 + crate::wire::HEADER) else {
            return Poll::Failed;
        };
        if let Some(source) = self.source.take() {
            let (tx, rx) = mpsc::sync_channel(1);
            let (out, incoming) = mpsc::sync_channel(1);
            self.permits = Some(tx);
            self.replies = Some(incoming);
            let conn = self.conn.clone();
            let progress = self.progress.clone();
            let stack = self.stack.take().expect("unstarted job has its stack allowance");
            if std::thread::Builder::new()
                .name("core-paint".into())
                .stack_size(STACK)
                .spawn(move || {
                    let _stack = stack;
                    source.metrics.workers.fetch_add(1, Ordering::Relaxed);
                    struct Active(Arc<Metrics>);
                    impl Drop for Active {
                        fn drop(&mut self) {
                            self.0.workers.fetch_sub(1, Ordering::Relaxed);
                        }
                    }
                    let _active = Active(source.metrics.clone());
                    let mut sink = Sink { rx, out, conn, progress, buffer: None, metrics: source.metrics.clone() };
                    // Do not decode or format before the first actual CREDIT grant.
                    let result = (|| {
                        sink.permit()?;
                        let mut frozen =
                            ghostty_vt::Terminal::restore_budget(source.snapshot.as_slice(), &source.budget)
                                .ok_or_else(|| io::Error::other("paint_limit"))?;
                        if source.continuation {
                            frozen.paint_with_colors_to(source.history, &mut sink)?;
                        } else {
                            frozen.paint_to(source.history, &mut sink)?;
                        }
                        sink.write_all(source.tail.as_slice())?;
                        if source.continuation {
                            frozen.continuation_to(&mut sink)?;
                        }
                        sink.send_buffer()?;
                        Ok::<(), io::Error>(())
                    })();
                    let _ = sink.out.send(Reply::End(result.is_ok()));
                    sink.wake();
                })
                .is_err()
            {
                return Poll::Failed;
            }
        }
        if self.permits.as_ref().expect("started job").try_send(Permit { n, reservation }).is_err() {
            return Poll::Failed;
        }
        self.outstanding = true;
        Poll::Pending
    }
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct Sink {
    progress: Arc<std::sync::Mutex<Progress>>,
    rx: mpsc::Receiver<Permit>,
    out: mpsc::SyncSender<Reply>,
    conn: Weak<Conn>,
    buffer: Option<(Data, usize)>,
    metrics: Arc<Metrics>,
}
impl Sink {
    fn wake(&self) {
        if let Some(conn) = self.conn.upgrade() {
            conn.wake.notify_one();
        }
    }
    fn permit(&mut self) -> io::Result<()> {
        if self.buffer.is_none() {
            let permit = self.rx.recv().map_err(|_| io::Error::other("paint_cancelled"))?;
            self.buffer =
                Some((Data { bytes: Vec::with_capacity(permit.n), reservation: permit.reservation }, permit.n));
        }
        Ok(())
    }
    fn send_buffer(&mut self) -> io::Result<()> {
        if let Some((data, _)) = self.buffer.take().filter(|(data, _)| !data.bytes.is_empty()) {
            self.out.send(Reply::Data(data)).map_err(|_| io::Error::other("paint_cancelled"))?;
            self.wake();
        }
        Ok(())
    }
}
impl Write for Sink {
    fn write(&mut self, mut bytes: &[u8]) -> io::Result<usize> {
        let count = bytes.len();
        while !bytes.is_empty() {
            self.permit()?;
            let (data, limit) = self.buffer.as_mut().expect("a permit owns a buffer");
            let n = bytes.len().min(*limit - data.bytes.len());
            {
                let mut p = self.progress.lock().unwrap_or_else(|e| e.into_inner());
                if p.cancelled {
                    return Err(io::Error::other("paint_cancelled"));
                }
                data.bytes.extend_from_slice(&bytes[..n]);
                p.generated += n as u64;
            }
            bytes = &bytes[n..];
            self.metrics.generated.fetch_add(n as u64, Ordering::Relaxed);
            if data.bytes.len() == *limit {
                self.send_buffer()?;
            }
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Serialize, schemars::JsonSchema)]
pub struct Stats {
    pub limit: usize,
    pub used: usize,
    pub peak: usize,
    pub denied: usize,
    pub generated_paint_bytes: u64,
    pub paint_workers: usize,
}
impl Stats {
    pub fn new(budget: &Budget, metrics: &Metrics) -> Self {
        Self {
            limit: budget.limit(),
            used: budget.used(),
            peak: budget.peak(),
            denied: budget.denied(),
            generated_paint_bytes: metrics.generated.load(Ordering::Relaxed),
            paint_workers: metrics.workers.load(Ordering::Relaxed),
        }
    }
}

/// MARK default layer. OSC in the following paint restores only the program
/// overrides. Arrays are RGB bytes, palette always has exactly 256 entries.
#[derive(Clone)]
pub struct ColorDefaults(pub ghostty_vt::Colors);
impl serde::Serialize for ColorDefaults {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let c = &self.0;
        let mut s = serializer.serialize_struct("ColorDefaults", 5)?;
        s.serialize_field("fg", &c.fg)?;
        s.serialize_field("bg", &c.bg)?;
        s.serialize_field("cursor", &c.cursor)?;
        s.serialize_field("palette", c.palette.as_slice())?;
        s.serialize_field("dark", &c.dark)?;
        s.end()
    }
}
