//! A machine as clients see it, what a line that is down tells a call, and the
//! `machine` events that keep unfiltered subscribers' list of machines current.

use super::LineState;
use crate::branding::COMMAND as CLI_NAME;
use crate::machines::{Machine, Refusal};
use crate::rpc::RpcError;
use crate::runtime::{Core, State};
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

/// A machine as `machine ls` and `ls` show it: its profile and its line now.
#[derive(Serialize, JsonSchema, Clone)]
pub struct MachineView {
    #[serde(flatten)]
    pub machine: Machine,
    /// connecting, connected, needs_login, host_key_unknown, host_key_changed,
    /// needs_update, different_core or unreachable; on Nearshell builds also
    /// machine_newer, build_conflict and incompatible.
    pub state: String,
    /// Its terminals here are exactly the ones there now: connected, and every
    /// change from the last snapshot taken over this line delivered.
    pub synced: bool,
    /// Since when it is not connected (ms since the epoch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

pub fn view(st: &State, m: &Machine) -> MachineView {
    let (state, since_ms, why) = match st.lines.get(&m.id) {
        None | Some(LineState::Connecting) => ("connecting", None, None),
        Some(LineState::Connected(_)) => ("connected", None, None),
        Some(LineState::NeedsLogin { since_ms }) => ("needs_login", Some(*since_ms), None),
        Some(LineState::HostKey { since_ms, changed: false }) => ("host_key_unknown", Some(*since_ms), None),
        Some(LineState::HostKey { since_ms, changed: true }) => ("host_key_changed", Some(*since_ms), None),
        Some(LineState::NeedsUpdate { since_ms }) => ("needs_update", Some(*since_ms), None),
        Some(LineState::DifferentCore { since_ms, why }) => ("different_core", Some(*since_ms), Some(why.clone())),
        Some(LineState::Unreachable { since_ms, why }) => ("unreachable", Some(*since_ms), Some(why.clone())),
        Some(LineState::Held { since_ms, refusal, why }) => (refusal.state(), Some(*since_ms), Some(why.clone())),
    };
    let synced = matches!(st.lines.get(&m.id), Some(LineState::Connected(line)) if line.synced.load(Ordering::Relaxed));
    MachineView { machine: m.clone(), state: state.into(), synced, since_ms, why }
}

/// Every machine here, in order, as snapshots carry them.
pub fn views(st: &State) -> Vec<MachineView> {
    st.machines.iter().map(|m| view(st, m)).collect()
}

impl LineState {
    /// Since when it is down; `None` while connecting or connected.
    pub(super) fn since_ms(&self) -> Option<u64> {
        match self {
            LineState::Connecting | LineState::Connected(_) => None,
            LineState::NeedsLogin { since_ms }
            | LineState::HostKey { since_ms, .. }
            | LineState::NeedsUpdate { since_ms }
            | LineState::DifferentCore { since_ms, .. }
            | LineState::Unreachable { since_ms, .. }
            | LineState::Held { since_ms, .. } => Some(*since_ms),
        }
    }

    /// The line after a dial that failed with `e`, down since `since_ms`.
    pub(super) fn after(e: &RpcError, since_ms: u64) -> LineState {
        if let Some(refusal) = Refusal::of(e) {
            return LineState::Held { since_ms, refusal, why: e.message.clone() };
        }
        match e.kind.as_str() {
            "needs_login" => LineState::NeedsLogin { since_ms },
            "host_key_unknown" => LineState::HostKey { since_ms, changed: false },
            "host_key_changed" => LineState::HostKey { since_ms, changed: true },
            "incompatible" => LineState::NeedsUpdate { since_ms },
            "different_core" => LineState::DifferentCore { since_ms, why: e.message.clone() },
            _ => LineState::Unreachable { since_ms, why: e.message.clone() },
        }
    }

    /// What a call to `name` gets while its line is not up.
    pub(super) fn refusal(&self, name: &str) -> RpcError {
        match self {
            LineState::NeedsLogin { .. } => RpcError::new(
                "unavailable",
                format!("{name} needs a login typed by a person"),
                if cfg!(windows) {
                    format!("set up its SSH keys, then: {CLI_NAME} machine login {name}")
                } else {
                    format!("in a terminal: {CLI_NAME} machine login {name}")
                },
            ),
            LineState::HostKey { changed: false, .. } => RpcError::new(
                "unavailable",
                format!("{name}'s host key is not known here"),
                format!("in a terminal, `{CLI_NAME} machine login {name}` shows it to check and accept"),
            ),
            LineState::HostKey { changed: true, .. } => RpcError::new(
                "unavailable",
                format!("{name}'s host key is not the one known here"),
                "check why with whoever runs it; only you can fix known_hosts",
            ),
            LineState::NeedsUpdate { .. } => RpcError::new(
                "needs_update",
                format!("{name}'s {CLI_NAME} speaks another protocol generation"),
                format!("{CLI_NAME} machine update {name}"),
            ),
            LineState::DifferentCore { why, .. } => RpcError::new(
                "unavailable",
                format!("{name}: {why}"),
                format!("its terminals there are gone for this core: {CLI_NAME} machine rm {name}, then add it again"),
            ),
            LineState::Unreachable { why, .. } => RpcError::new(
                "unavailable",
                format!("{name} is unreachable: {why}"),
                format!("{CLI_NAME} machine ls shows when it comes back"),
            ),
            LineState::Held { refusal, why, .. } => refusal.unavailable(name, why),
            LineState::Connecting | LineState::Connected(_) => {
                RpcError::new("unavailable", format!("still connecting to {name}"), "try again in a moment")
            }
        }
    }
}

/// What is news about machines: `told` is what subscribers last heard of each
/// (by id), `now` every machine's view in order. The events to send, and what
/// subscribers will have heard then. A new `since_ms` alone is no news: it
/// changes only with the state, which is.
fn news(told: &BTreeMap<String, Value>, now: Vec<(String, Value)>) -> (Vec<Value>, BTreeMap<String, Value>) {
    let bare = |v: &Value| {
        let mut v = v.clone();
        v.as_object_mut().map(|o| o.remove("since_ms"));
        v
    };
    let mut events: Vec<Value> = now
        .iter()
        .filter(|(id, v)| told.get(id).is_none_or(|was| bare(was) != bare(v)))
        .map(|(_, v)| v.clone())
        .collect();
    for (id, was) in told.iter().filter(|(id, _)| !now.iter().any(|(n, _)| n == *id)) {
        events.push(json!({"id": id, "name": was["name"], "removed": true}));
    }
    (events, now.into_iter().collect())
}

/// Tells unfiltered subscribers about each machine whose view changed since
/// they last heard (and each one removed): call after anything that may have
/// changed one. Before its terminals' events, so a client knows the machine
/// (and its name) first.
pub fn tell(core: &Core, st: &mut State) {
    let now = st.machines.iter().map(|m| (m.id.clone(), json!(view(st, m)))).collect();
    let (events, told) = news(&st.events.machines, now);
    st.events.machines = told;
    for data in events {
        core.machine_event(st, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine(id: &str, name: &str, state: &str, since_ms: Option<u64>) -> (String, Value) {
        let mut v = json!({"id": id, "name": name, "state": state, "synced": false});
        if let Some(since_ms) = since_ms {
            v["since_ms"] = since_ms.into();
        }
        (id.into(), v)
    }

    #[test]
    fn only_a_changed_view_is_news() {
        let (events, told) = news(&BTreeMap::new(), vec![machine("a", "rohan", "connecting", None)]);
        assert_eq!(events, [json!({"id": "a", "name": "rohan", "state": "connecting", "synced": false})], "added");
        let (events, told) = news(&told, vec![machine("a", "rohan", "connecting", None)]);
        assert!(events.is_empty(), "nothing changed: {events:?}");
        let (events, told) = news(&told, vec![machine("a", "rohan", "unreachable", Some(5))]);
        assert_eq!(events.len(), 1, "a new state");
        let (events, told) = news(&told, vec![machine("a", "rohan", "unreachable", Some(9))]);
        assert!(events.is_empty(), "since_ms alone is no news: {events:?}");
        assert_eq!(told["a"]["since_ms"], 9, "though what was heard follows it");
        let (events, told) = news(&told, vec![machine("a", "gondor", "unreachable", Some(9))]);
        assert_eq!(events[0]["name"], "gondor", "renamed");
        let (events, told) = news(&told, vec![machine("b", "osgiliath", "connecting", None)]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["id"], "b", "an added one before one removed");
        assert_eq!(events[1], json!({"id": "a", "name": "gondor", "removed": true}));
        assert_eq!(told.keys().collect::<Vec<_>>(), ["b"]);
    }
}
