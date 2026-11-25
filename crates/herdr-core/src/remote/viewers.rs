//! Viewer streams relayed to other cores, including reattachment.

use super::*;
use std::time::Instant;

/// A viewer here on a terminal there. It outlives a dropped line: when the
/// line is back, it is attached again on the same channel here.
#[derive(Clone)]
pub struct Relayed {
    pub automatic_input: bool,
    pub line: Arc<Line>,
    pub there: u32,
    pub conn: Arc<Conn>,
    pub machine: String,
    pub terminal: String,
    pub params: Value,
    pub active_at: Instant,
    pub size_pending: bool,
    pub(crate) relay: relay::Shared,
}

/// Why an opted-in stream was refused: a hop on the way lost resize support.
pub(super) const RESIZE_LOST: &str = "resize notifications lost";

/// Viewers left waiting by a dropped line to `m` get their terminal back.
pub(super) async fn reattach(core: Arc<Core>, m: Machine, line: Arc<Line>) {
    let mut waiting: Vec<_> = core
        .lock()
        .relayed
        .iter()
        .filter(|(_, r)| r.machine == m.id && !Arc::ptr_eq(&r.line, &line))
        .map(|(k, r)| (*k, r.active_at))
        .collect();
    // Resumed viewers tie there; the last attached wins unless another viewer stayed active.
    waiting.sort_by_key(|(_, active_at)| *active_at);
    for (key @ (_, here), was_active_at) in waiting {
        let Some(r) = core.lock().relayed.get(&key).cloned() else { continue };
        // The core there no longer paints marks: nothing it sends is a paint this viewer can follow.
        if r.params["marks"] == true && !paint_marks(&line) {
            core.lock().relayed.remove(&key);
            r.conn.with(|out| out.close(here, b""));
            continue;
        }
        // The screen is painted again; the viewer already has its history.
        let mut params = r.params.clone();
        params["history"] = 0.into();
        params["resumed"] = true.into();
        negotiate_resize(&line, &mut params);
        let deadline = deadline_for(&core, "terminal.attach", &params);
        let sink = ((r.conn.clone(), here, r.relay.clone()), r.params["resize_notifications"] == true);
        let reply = line.call_into(&m.name, "terminal.attach", params, Some(sink), deadline).await;
        let lost_resize = matches!(&reply, Err(e) if e.message == RESIZE_LOST);
        let mut st = core.lock();
        // Gone meanwhile (the viewer detached): nothing to resume.
        let admitted = r.conn.admit(&st).is_ok();
        let Some(current) = st.relayed.get(&key).filter(|v| admitted && Arc::ptr_eq(&v.line, &r.line)) else {
            if let Ok(reply) = reply {
                close_remote_viewer(&line, &reply);
            }
            continue;
        };
        // Its line went meanwhile, whatever it answered: the next line brings it back.
        if !lost_resize && went(&st, &m, &line) {
            continue;
        }
        let reply = reply.ok();
        if r.automatic_input && reply.as_ref().is_some_and(|reply| !automatic_input(&line, reply)) {
            st.relayed.remove(&key);
            close_remote_viewer(&line, reply.as_ref().unwrap());
            r.conn.with(|out| out.close(here, b""));
            continue;
        }
        match reply.as_ref().and_then(|reply| reply["channel"].as_u64()) {
            Some(there) => {
                let there = there as u32;
                let terminal =
                    reply.as_ref().and_then(|r| r["terminal"]["address"].as_str()).unwrap_or(&current.terminal);
                let missed = current.active_at != was_active_at;
                let newer = missed
                    && st.relayed.values().any(|v| {
                        Arc::ptr_eq(&v.line, &line)
                            && v.terminal == terminal
                            && v.params["observe"] != true
                            && v.active_at > current.active_at
                    });
                let terminal = terminal.to_owned();
                let current = st.relayed.get_mut(&key).expect("still attached");
                current.line = line.clone();
                current.there = there;
                current.terminal = terminal;
                current.size_pending =
                    current.params["cols"] != r.params["cols"] || current.params["rows"] != r.params["rows"];
                // A resize also claims size control. Replay only missed activity,
                // and leave a newer viewer on the same terminal in control.
                if !newer && (missed || current.size_pending) {
                    resend_size(current);
                    current.size_pending = false;
                }
                if current.params["display"] != r.params["display"] {
                    let _ = line.frames.send(wire::encode(
                        there,
                        wire::FACTS,
                        current.params["display"].to_string().as_bytes(),
                    ));
                }
            }
            None => {
                st.relayed.remove(&key);
                // A downgraded path must not deliver paints sized for the old
                // contract; any other failure still hands over what arrived.
                if lost_resize {
                    r.conn.with(|out| out.close(here, b""));
                } else {
                    r.conn.with(|out| out.finish(here, b""));
                }
            }
        }
    }
}

/// A frame on a stream there that a viewer here is relayed from.
pub(super) fn relay_out(core: &Core, line: &Line, there: u32, kind: u8, payload: &[u8]) {
    // Taken out before anything else is locked: elsewhere the core's state
    // is locked first and a line's streams after it.
    let sink = match kind {
        wire::CLOSE => lock(&line.streams).remove(&there),
        _ => lock(&line.streams).get(&there).cloned(),
    };
    let Some(sink) = sink else { return };
    let (conn, here) = (&sink.0, sink.1);
    let open = match kind {
        wire::DATA => deliver(&sink, payload),
        wire::MARK => mark(&sink, payload),
        // A marked viewer was told no grid comes: each paint says its own.
        wire::RESIZE if payload.len() == 4 && lock(&sink.2).marks.is_none() => {
            conn.with(|out| out.ordered_resize(here, payload));
            true
        }
        wire::CLOSE => {
            core.lock().relayed.remove(&(conn_key(conn), here));
            conn.with(|out| out.finish(here, payload));
            true
        }
        _ => true,
    };
    // Closed here (all locks above released): it ends there too.
    if !open {
        core.lock().relayed.remove(&(conn_key(conn), here));
        line.close_stream(there);
    }
}

/// Viewers here hear that the line is gone and stay open; the programs keep
/// running there, and the viewers get them back with the line.
pub(super) fn lost_line(line: &Line, machine: &str) {
    let notice = lost_notice(machine);
    let sinks: Vec<Sink> = lock(&line.streams).drain().map(|(_, sink)| sink).collect();
    for sink in &sinks {
        deliver(sink, notice.as_bytes());
    }
}

/// Bytes into a relayed viewer's stream, counted for its credit. False: it
/// was closed here.
fn deliver((conn, here, relay): &Sink, bytes: &[u8]) -> bool {
    let mut relay = lock(relay);
    let marked = relay.marks.is_some();
    let (mut back, mut open) = (0, true);
    conn.with(|out| {
        if !out.has_stream(*here) {
            return;
        }
        let queued = out.queued(*here);
        // Too far behind, it drops what it had not sent; nothing here paints
        // it again, but the stream there gets back what was its share. A
        // marked one would be left inside a paint it can't finish: it ends.
        let fits = out.data(*here, bytes);
        if !fits && marked {
            out.close(*here, wire::PAINT_LIMIT);
            open = false;
            return;
        }
        let dropped = if fits { 0 } else { queued + bytes.len() };
        relay.ledger.enqueued(bytes.len());
        back = relay.ledger.dropped(dropped);
    });
    if back > 0
        && let Some((frames, there)) = &relay.to
    {
        let _ = frames.send(wire::encode(*there, wire::CREDIT, &back.to_le_bytes()));
    }
    open
}

/// A paint mark into a marked viewer's stream, as the viewer is to read it
/// (an unmarked one never asked for them). False: it was closed here.
fn mark((conn, here, relay): &Sink, payload: &[u8]) -> bool {
    let mut relay = lock(relay);
    let Some(marks) = &mut relay.marks else { return true };
    let mut open = true;
    conn.with(|out| {
        if !out.has_stream(*here) {
            return;
        }
        open = match marks.rewrite(payload) {
            Ok(Some(mark)) => out.mark(*here, &mark),
            Ok(None) => true,
            Err(()) => {
                out.close(*here, b"");
                false
            }
        };
    });
    open
}

/// Whether `line` is no longer the one to the machine: closed, its writer
/// gone, or another dialed meanwhile (an abandoned line may never answer).
fn went(st: &State, m: &Machine, line: &Arc<Line>) -> bool {
    line.closed.load(Ordering::Relaxed)
        || line.frames.is_closed()
        || st.lines.get(&m.id).is_some_and(|now| !matches!(now, LineState::Connected(l) if Arc::ptr_eq(l, line)))
}

fn resend_size(viewer: &Relayed) {
    let cols = viewer.params["cols"].as_u64().unwrap_or_default() as u16;
    let rows = viewer.params["rows"].as_u64().unwrap_or_default() as u16;
    let size = [cols.to_le_bytes(), rows.to_le_bytes()].concat();
    let _ = viewer.line.frames.send(wire::encode(viewer.there, wire::RESIZE, &size));
}

fn close_remote_viewer(line: &Line, reply: &Value) {
    if let Some(there) = reply["channel"].as_u64() {
        lock(&line.streams).remove(&(there as u32));
        let _ = line.frames.send(wire::encode(there as u32, wire::CLOSE, b""));
    }
}

pub(super) fn automatic_input(line: &Line, reply: &Value) -> bool {
    line.features.iter().any(|f| f == "automatic_input") && reply["automatic_input"] == true
}

fn paint_marks(line: &Line) -> bool {
    line.features.iter().any(|f| f == "paint-marks")
}

pub(super) fn negotiate_resize(line: &Line, params: &mut Value) {
    if params["resize_notifications"] == true && !line.features.iter().any(|f| f == "resize_notifications") {
        params["resize_notifications"] = false.into();
    }
}

/// Opens a viewer here on a terminal there: the stream channel there is
/// relayed to a new channel on this connection, both ways.
pub async fn attach(core: &Core, conn: &Arc<Conn>, m: &Machine, mut params: Value) -> Result<Value, RpcError> {
    conn.admit(&core.lock())?;
    let line = line(core, m).await?;
    let marked = params.get("marks").and_then(Value::as_bool) == Some(true);
    // Before the call: a viewer there would already be painting unmarked.
    if marked && !paint_marks(&line) {
        return Err(RpcError::new(
            "marks_local_only",
            format!("the core on {} does not support paint marks", m.name),
            "update the core there, or attach without marks",
        ));
    }
    negotiate_resize(&line, &mut params);
    if marked {
        // The core there sends a marked viewer no RESIZE, whatever it confirms.
        params["resize_notifications"] = false.into();
    }
    let here = conn.new_channel();
    let relay = relay::Shared::new(Mutex::new(if marked { relay::Relay::marked() } else { Default::default() }));
    {
        let st = core.lock();
        conn.admit(&st)?;
        conn.with(|out| out.open_paused(here));
    }
    let deadline = deadline_for(core, "terminal.attach", &params);
    match line
        .call_into(
            &m.name,
            "terminal.attach",
            params.clone(),
            Some(((conn.clone(), here, relay.clone()), false)),
            deadline,
        )
        .await
    {
        Ok(mut r) => {
            let there = r["channel"].as_u64().unwrap_or_default() as u32;
            let enabled = params["observe"] == true
                && params["resize_notifications"] == true
                && r["resize_notifications"] == true;
            r["resize_notifications"] = enabled.into();
            params["resize_notifications"] = enabled.into();
            let automatic_input = automatic_input(&line, &r);
            r["automatic_input"] = automatic_input.into();
            {
                let mut st = core.lock();
                if let Err(error) = conn.admit(&st) {
                    line.close_stream(there);
                    return Err(error);
                }
                let viewer = Relayed {
                    automatic_input,
                    line,
                    there,
                    conn: conn.clone(),
                    machine: m.id.clone(),
                    terminal: r["terminal"]["address"]
                        .as_str()
                        .or_else(|| params["terminal"].as_str())
                        .unwrap_or_default()
                        .into(),
                    params,
                    active_at: Instant::now(),
                    size_pending: false,
                    relay,
                };
                st.relayed.insert((conn_key(conn), here), viewer);
            }
            r["channel"] = here.into();
            Ok(localize(core, &m.name, &[], r))
        }
        Err(e) => {
            conn.with(|out| out.close(here, b""));
            Err(e)
        }
    }
}

/// A viewer's frame for a relayed stream goes there. False: not relayed.
pub fn relay_in(core: &Core, conn: &Arc<Conn>, here: u32, kind: u8, payload: &[u8]) -> bool {
    let mut st = core.lock();
    let Some(r) = st.relayed.get_mut(&(conn_key(conn), here)) else { return false };
    let resize = kind == wire::RESIZE && payload.len() == 4;
    if r.params["observe"] != true
        && (kind == wire::DATA || resize || (kind == wire::FOCUS && payload.first() == Some(&1)))
    {
        r.active_at = Instant::now();
        if r.size_pending && !resize {
            resend_size(r);
            r.size_pending = false;
        }
    }
    // What a reattach after a dropped line brings back.
    if resize {
        r.params["cols"] = u16::from_le_bytes([payload[0], payload[1]]).into();
        r.params["rows"] = u16::from_le_bytes([payload[2], payload[3]]).into();
        r.size_pending = false;
    } else if kind == wire::FACTS
        && let Ok(display) = serde_json::from_slice::<Value>(payload)
    {
        r.params["display"] = display;
    }
    // Read here: our stream to the viewer may send more, and so may the one
    // there to us, by what was its own.
    if kind == wire::CREDIT && payload.len() == 4 {
        let bytes = u32::from_le_bytes(payload.try_into().unwrap());
        conn.with(|out| out.credit(here, bytes as usize));
        let mut relay = lock(&r.relay);
        let more = relay.ledger.credited(bytes);
        if more > 0
            && let Some((frames, there)) = &relay.to
        {
            let _ = frames.send(wire::encode(*there, wire::CREDIT, &more.to_le_bytes()));
        }
        return true;
    }
    // While the line is down this goes nowhere: typing into a gap is lost, not queued.
    let _ = r.line.frames.send(wire::encode(r.there, kind, payload));
    true
}

/// A viewer's connection closed: its relayed streams there end.
pub fn conn_gone(core: &Core, conn: &Arc<Conn>) {
    let key = conn_key(conn);
    let mut st = core.lock();
    let gone: Vec<(u32, Arc<Line>, u32)> = st
        .relayed
        .iter()
        .filter(|((k, _), _)| *k == key)
        .map(|((_, here), r)| (*here, r.line.clone(), r.there))
        .collect();
    for (here, line, there) in gone {
        st.relayed.remove(&(key, here));
        lock(&line.streams).remove(&there);
        let _ = line.frames.send(wire::encode(there, wire::CLOSE, b""));
    }
}
