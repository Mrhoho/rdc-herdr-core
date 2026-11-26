//! A machine's terminals as seen from here: replaced from each snapshot its
//! core sends, kept current by its events, and passed on to subscribers here.
//! `synced` on its line says the mirror is that core's whole set right now.

use super::{ADDRESS_KEYS, Line, localize_in, lock, map_addresses, tell};
use crate::machines::Machine;
use crate::runtime::{Core, State};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;

/// Asks the core there for everything that happens there (again, after a
/// refused ask): its reply is the snapshot, its channel the rest.
pub(super) fn subscribe(line: &Line) {
    let id = line.next.fetch_add(1, Ordering::Relaxed);
    lock(&line.mirror).0 = id;
    line.message(&json!({"jsonrpc": "2.0", "id": id, "method": "events.subscribe", "params": {}}));
}

/// The terminals in a reply to the mirror's subscription, when it is a
/// snapshot: a result with its terminals and the channel the rest comes on.
/// An error reply is none: it says nothing about the terminals there.
fn snapshot_of(reply: &Value) -> Option<Vec<Value>> {
    let result = reply.get("result").filter(|_| reply.get("error").is_none())?;
    result["channel"].as_u64()?;
    result["terminals"].as_array().cloned()
}

/// The reply to the mirror's subscription. A snapshot replaces the mirror;
/// anything else leaves it as it was and the line not synced, and the caller
/// asks again. Whether it was a snapshot.
pub(super) fn mirror_reply(core: &Core, m: &Machine, line: &Line, reply: &Value) -> bool {
    let Some(terminals) = snapshot_of(reply) else { return false };
    lock(&line.mirror).1 = reply["result"]["channel"].as_u64().map(|c| c as u32);
    mirror_replace(core, m, line, terminals);
    // Docs are not in terminal snapshots: readers must refetch after a gap.
    core.documents_missed(&mut core.lock());
    true
}

/// A machine's terminals now (after a connect, or after it says we missed
/// some): subscribers here hear what appeared, changed or went, and then
/// that the machine is synced.
pub(super) fn mirror_replace(core: &Core, m: &Machine, line: &Line, terminals: Vec<Value>) {
    let mut st = core.lock();
    let Some(name) = st.machines.iter().find(|current| current.id == m.id).map(|m| m.name.clone()) else { return };
    let fresh: std::collections::BTreeMap<String, Value> = {
        let granted = granted_by(&st, &m.id);
        terminals
            .into_iter()
            .map(|t| localize_in(core, &st, &name, &[], t))
            .filter_map(|mut t| admit(&mut t, &name, &granted).then_some(t))
            .map(|t| (t["address"].as_str().unwrap_or_default().to_string(), t))
            .collect()
    };
    // Not synced while the changes go out (told, for those who heard it was).
    line.synced.store(false, Ordering::Relaxed);
    tell(core, &mut st);
    // The new set in place first: one who falls behind meanwhile gets it whole in its snapshot.
    let old = st.mirrors.insert(m.id.clone(), fresh.clone()).unwrap_or_default();
    for (address, record) in old.iter().filter(|(a, _)| !fresh.contains_key(*a)) {
        core.deliver(&mut st, "removed", address, json!({}), record, Some(record));
    }
    for (address, record) in fresh.iter().filter(|(a, r)| old.get(*a) != Some(*r)) {
        core.deliver(&mut st, "created", address, record.clone(), record, None);
    }
    line.synced.store(true, Ordering::Relaxed);
    tell(core, &mut st);
}

pub fn rename_mirror(core: &Core, st: &mut State, id: &str, old: &str, new: &str) {
    // The machine under its new name first, then its terminals under it.
    tell(core, st);
    let Some(records) = st.mirrors.remove(id) else { return };
    let mut renamed = std::collections::BTreeMap::new();
    for record in records.values() {
        let mut record = record.clone();
        map_addresses(&mut record, &|s| match s.split_once('/') {
            Some((prefix, rest)) if prefix == old => format!("{new}/{rest}"),
            _ => s.to_owned(),
        });
        let address = record["address"].as_str().unwrap_or_default().to_owned();
        renamed.insert(address, record);
    }
    st.mirrors.insert(id.to_owned(), renamed.clone());
    for (address, record) in records {
        core.deliver(st, "removed", &address, json!({}), &record, Some(&record));
    }
    for (address, record) in renamed {
        core.deliver(st, "created", &address, record.clone(), &record, None);
    }
}

/// Event lines from a machine: applied to its mirror, passed on as seen from here.
pub(super) fn mirror_events(core: &Core, m: &Machine, line: &Line, payload: &[u8]) {
    let lines: Vec<Vec<u8>> = {
        let mut mirror = lock(&line.mirror);
        mirror.2.extend_from_slice(payload);
        let whole = mirror.2.iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1);
        let done: Vec<u8> = mirror.2.drain(..whole).collect();
        done.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(<[u8]>::to_vec).collect()
    };
    for l in lines {
        let Ok(msg) = serde_json::from_slice::<Value>(&l) else { continue };
        match msg["method"].as_str() {
            Some("event") => mirror_event(core, m, msg["params"].clone()),
            Some("missed") => {
                mirror_replace(core, m, line, msg["params"]["terminals"].as_array().cloned().unwrap_or_default());
                core.documents_missed(&mut core.lock());
            }
            _ => {}
        }
    }
}

pub(super) fn mirror_event(core: &Core, m: &Machine, p: Value) {
    let mut st = core.lock();
    let Some(name) = st.machines.iter().find(|current| current.id == m.id).map(|m| m.name.clone()) else { return };
    let mut p = localize_in(core, &st, &name, &[], p);
    if !matches!(p["type"].as_str(), Some("machine" | "doc")) && !admit_event(&mut p, &name, &granted_by(&st, &m.id)) {
        return;
    }
    let (kind, address, data) =
        (p["type"].as_str().unwrap_or_default(), p["terminal"].as_str().unwrap_or_default(), p["data"].clone());
    // Its own machines are its business: this core tells only of the ones it added.
    if kind == "machine" {
        return;
    }
    if kind == "doc" {
        // Only this machine, not docs it in turn watches on a third core.
        if p.get("machine").is_none() {
            let doc = data["name"].as_str().unwrap_or_default().to_string();
            core.doc_event(&mut st, &doc, data, Some(&name));
        }
        return;
    }
    let mirror = st.mirrors.entry(m.id.clone()).or_default();
    let (record, was) = match kind {
        "created" | "restarted" => {
            mirror.insert(address.into(), data.clone());
            (data.clone(), None)
        }
        "removed" => match mirror.remove(address) {
            Some(record) => (record.clone(), Some(record)),
            None => return,
        },
        _ => {
            let Some(record) = mirror.get_mut(address) else { return };
            let was = record.clone();
            apply(record, kind, &data);
            (record.clone(), Some(was))
        }
    };
    core.deliver(&mut st, kind, address, data, &record, was.as_ref());
}

/// A machine's answer to `terminal.list`, kept to its own terminals as its mirror is.
pub fn own_terminals(core: &Core, m: &Machine, records: Vec<Value>) -> Vec<Value> {
    let st = core.lock();
    let granted = granted_by(&st, &m.id);
    records.into_iter().filter_map(|mut r| admit(&mut r, &m.name, &granted).then_some(r)).collect()
}

/// Whether a terminal here (`t3`) let the machine `id` call back to it: one
/// of its runs went there (its `afar`).
fn granted_by<'a>(st: &'a State, id: &'a str) -> impl Fn(&str) -> bool + 'a {
    move |parent| {
        crate::terminals::parse_id(parent).and_then(|t| st.reg.terminals.get(&t)).is_some_and(|t| t.afar.contains(id))
    }
}

/// Whether a localized address is one of the terminals of the machine called
/// `name` here: `<name>/tN`.
fn its_own(name: &str, address: &str) -> bool {
    address.strip_prefix(name).and_then(|rest| rest.strip_prefix('/')).and_then(crate::terminals::parse_id).is_some()
}

/// A record from the machine called `name` (localized) is taken only for one of
/// its own terminals. Its other addresses, at any depth, are cleared unless
/// they are its own too, or its `parent` is a terminal here that `granted` that
/// machine a run: a core there cannot stand in for, or attach to, a terminal here.
fn admit(record: &mut Value, name: &str, granted: &dyn Fn(&str) -> bool) -> bool {
    if !record["address"].as_str().is_some_and(|a| its_own(name, a)) {
        return false;
    }
    scrub(record, name, Some(granted));
    true
}

/// An event from there (localized) is taken only about one of its own
/// terminals, with no record of its own making (one is added here, from the
/// mirror), and when it brings a record, that terminal's.
fn admit_event(p: &mut Value, name: &str, granted: &dyn Fn(&str) -> bool) -> bool {
    if !p["terminal"].as_str().is_some_and(|t| its_own(name, t)) {
        return false;
    }
    if let Some(data) = p["data"].as_object_mut() {
        data.remove("record");
    }
    match p["type"].as_str() {
        Some("created" | "restarted") => p["data"]["address"] == p["terminal"] && admit(&mut p["data"], name, granted),
        _ => {
            scrub(&mut p["data"], name, None);
            true
        }
    }
}

fn scrub(v: &mut Value, name: &str, granted: Option<&dyn Fn(&str) -> bool>) {
    match v {
        Value::Object(o) => {
            for (k, v) in o.iter_mut() {
                match v {
                    // Labels are a person's words, never addresses, whatever their keys.
                    _ if k == "labels" => {}
                    Value::String(s) if ADDRESS_KEYS.contains(&k.as_str()) => {
                        let parent_here = k == "parent" && granted.is_some_and(|granted| granted(s));
                        if !its_own(name, s) && !parent_here {
                            *v = Value::Null;
                        }
                    }
                    _ => scrub(v, name, None),
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|v| scrub(v, name, None)),
        _ => {}
    }
}

/// An event's change, applied to a terminal record (see schema/README.md).
fn apply(record: &mut Value, kind: &str, data: &Value) {
    let set = |record: &mut Value, keys: &[&str]| keys.iter().for_each(|k| record[*k] = data[*k].clone());
    match kind {
        "finished" | "lost" => {
            record["state"] = if kind == "lost" { "live" } else { "finished" }.into();
            if kind == "lost" {
                record["pid"] = Value::Null;
            }
            set(record, &["reason", "exit_code", "signal", "finished_ms"]);
            record.as_object_mut().map(|r| r.remove("foreground"));
        }
        "status" => set(record, &["status", "records", "reported"]),
        "turn" => set(record, &["turn"]),
        "title" => set(record, &["title"]),
        "labels" => set(record, &["labels"]),
        "keep" => set(record, &["keep"]),
        "size" => set(record, &["cols", "rows"]),
        "foreground" => record["foreground"] = data.clone(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::{admit, admit_event, snapshot_of};
    use serde_json::{Value, json};

    /// t5 here granted far a run; nothing else did.
    fn granted(parent: &str) -> bool {
        parent == "t5"
    }

    fn record(address: &str, parent: Value) -> Value {
        json!({"address": address, "parent": parent, "state": "live", "labels": {"k": "v", "from": "ci"}})
    }

    #[test]
    fn a_mirror_takes_only_the_machines_own_terminals() {
        let mut own = record("far/t1", json!("far/t2"));
        assert!(admit(&mut own, "far", &granted));
        assert_eq!(own, record("far/t1", json!("far/t2")), "its own, untouched");
        for forged in ["t1", "gondor/t1", "far/t1/x", "far/", "far/reviewer", "farther/t1", "", "f/t1"] {
            assert!(!admit(&mut record(forged, Value::Null), "far", &granted), "{forged}");
        }
        assert!(!admit(&mut json!({"state": "live"}), "far", &granted), "no address");
        // A parent here only where this core let far run something.
        let mut child = record("far/t1", json!("t5"));
        assert!(admit(&mut child, "far", &granted));
        assert_eq!(child["parent"], "t5");
        for parent in ["t3", "gondor/t2", "t5/x", "x"] {
            let mut forged = record("far/t1", json!(parent));
            assert!(admit(&mut forged, "far", &granted), "kept, without the parent");
            assert_eq!(forged["parent"], Value::Null, "{parent}");
        }
        // Any other address-like field, at any depth, names only its own.
        let mut nested = json!({"address": "far/t1", "from": "t3", "removed": "far/t2",
            "status": {"terminal": "t4", "parent": "t5"}, "list": [{"address": "t3"}, {"address": "far/t9"}]});
        assert!(admit(&mut nested, "far", &granted));
        assert_eq!(
            nested,
            json!({"address": "far/t1", "from": null, "removed": "far/t2",
            "status": {"terminal": null, "parent": null}, "list": [{"address": null}, {"address": "far/t9"}]})
        );
    }

    #[test]
    fn a_mirrored_event_is_about_the_machines_own_terminal_and_brings_only_its_record() {
        let event = |kind: &str, terminal: &str, data: Value| json!({"type": kind, "terminal": terminal, "data": data});
        let mut status =
            event("status", "far/t1", json!({"status": {"state": "idle"}, "record": record("t3", Value::Null)}));
        assert!(admit_event(&mut status, "far", &granted));
        assert_eq!(status["data"], json!({"status": {"state": "idle"}}), "a record is made here, not there");
        for (kind, terminal) in [("removed", "t3"), ("finished", "t3"), ("removed", "gondor/t1"), ("title", "far")] {
            assert!(!admit_event(&mut event(kind, terminal, json!({})), "far", &granted), "{kind} {terminal}");
        }
        let mut created = event("created", "far/t1", record("far/t1", json!("t3")));
        assert!(admit_event(&mut created, "far", &granted));
        assert_eq!(created["data"]["parent"], Value::Null);
        for data in [record("t3", Value::Null), record("far/t2", Value::Null), json!({})] {
            assert!(!admit_event(&mut event("created", "far/t1", data.clone()), "far", &granted), "{data}");
            assert!(!admit_event(&mut event("restarted", "far/t1", data), "far", &granted));
        }
        let mut finished = event("finished", "far/t1", json!({"reason": "exited", "from": "t2"}));
        assert!(admit_event(&mut finished, "far", &granted));
        assert_eq!(finished["data"], json!({"reason": "exited", "from": null}));
    }

    #[test]
    fn only_a_result_with_terminals_and_a_channel_is_a_snapshot() {
        let ok = json!({"id": 2, "result": {"seq": 1, "terminals": [{"address": "t1"}], "channel": 3}});
        assert_eq!(snapshot_of(&ok).unwrap().len(), 1);
        let empty = json!({"id": 2, "result": {"seq": 1, "terminals": [], "channel": 3}});
        assert_eq!(snapshot_of(&empty).unwrap().len(), 0, "an empty set is a snapshot too");
        let error = json!({"id": 2, "error": {"code": -32000, "message": "connection is closed",
            "data": {"kind": "connection_closed"}}});
        assert!(snapshot_of(&error).is_none(), "an error says nothing about the terminals there");
        assert!(snapshot_of(&json!({"id": 2, "result": {"seq": 1, "channel": 3}})).is_none());
        assert!(snapshot_of(&json!({"id": 2, "result": {"seq": 1, "terminals": []}})).is_none(), "no channel");
    }
}
