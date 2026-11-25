//! Nearshell builds reach only their own remote core: the line names it by its
//! absolute path (never PATH, never the user's hc), and a gate decides from the
//! two build identities whether the core that answered may be used. Decisions
//! only: `machines.rs` dials and `remote.rs` keeps the line.

use crate::api::BuildInfo;
use crate::rpc::RpcError;
use serde_json::Value;
use std::cmp::Ordering;

/// This build's bundle ID: the remote core there was built with the same one.
pub const BUNDLE: &str = env!("NEARSHELL_CORE_BUNDLE_ID");

/// Where the line looks, from the home an SSH session starts in.
fn remote_bin(bundle: &str) -> String {
    format!("$HOME/.local/share/nearshell/{bundle}/bin/nhc")
}

/// The line for a Unix machine. The bundle ID is letters, digits, `.` and `-`
/// (checked at build time), so it needs no quoting of its own. A login profile
/// may export the user's own hc session: unset before nhc refuses it.
pub fn unix_line(bundle: &str, not_here: &str) -> String {
    format!(
        "sh -c 'p=\"{}\"; [ -x \"$p\" ] || {{ echo \"{not_here}\" >&2; exit 127; }}; unset HC_SESSION HC_SOCKET; exec \"$p\" connect'",
        remote_bin(bundle)
    )
}

/// The probe for Windows (cmd and PowerShell read it alike). Only cmd expands
/// %OS%: a Unix shell that echoes the line it could not run never prints it.
pub const WINDOWS_PROBE: &str = "cmd /d /c \"echo nearshell-windows-%OS% 1>&2 & exit /b 127\"";
pub const WINDOWS_SEEN: &str = "nearshell-windows-Windows_NT";

pub fn windows_unsupported(target: &str) -> RpcError {
    RpcError::new(
        "unsupported_platform",
        format!("{target} runs Windows, which Nearshell does not support yet"),
        "add a Linux machine",
    )
}

/// The remote core a hub of `config` expects: `<kind>:<bundle>`, `:remote`,
/// then `:scratch-test` for test builds. A remote build acting as hub expects
/// its own config, so this is idempotent.
pub fn expected_remote(config: &str) -> String {
    let (base, test) = match config.strip_suffix(":scratch-test") {
        Some(base) => (base, ":scratch-test"),
        None => (config, ""),
    };
    format!("{}:remote{test}", base.strip_suffix(":remote").unwrap_or(base))
}

/// Why the gate turned the core there away. Its name is the error code of a
/// refused dial and the machine's state until `machine.connect` dials again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Refusal {
    /// An older release there: an install can update it.
    NeedsUpdate,
    /// A newer release there: this hub's Nearshell needs the update.
    MachineNewer,
    /// Another build of the same release, another config, or no build at all.
    BuildConflict,
    /// Another protocol generation (`hello` refused us).
    Incompatible,
}

impl Refusal {
    pub fn state(self) -> &'static str {
        match self {
            Refusal::NeedsUpdate => "needs_update",
            Refusal::MachineNewer => "machine_newer",
            Refusal::BuildConflict => "build_conflict",
            Refusal::Incompatible => "incompatible",
        }
    }

    /// A refused dial, in a nearshell build; upstream redials everything.
    pub fn of(e: &RpcError) -> Option<Refusal> {
        let all = [Refusal::NeedsUpdate, Refusal::MachineNewer, Refusal::BuildConflict, Refusal::Incompatible];
        all.into_iter().find(|r| cfg!(feature = "nearshell") && r.state() == e.kind)
    }

    /// What a call routed to a refused machine gets. Only needs_update keeps
    /// its own code (as upstream); the others are unavailable until resolved.
    pub fn unavailable(self, name: &str, why: &str) -> RpcError {
        let kind = if self == Refusal::NeedsUpdate { "needs_update" } else { "unavailable" };
        RpcError::new(kind, format!("{name}: {why}"), self.hint(&format!("nhc machine update {name}")))
    }

    pub(super) fn hint(self, update: &str) -> String {
        match self {
            Refusal::NeedsUpdate => format!("update its remote core: {update}"),
            Refusal::MachineNewer => "update Nearshell on this Mac".into(),
            Refusal::BuildConflict => format!(
                "not handled automatically: compare `nhc build-info` here with `{} build-info` there",
                remote_bin(BUNDLE).replace("$HOME", "~")
            ),
            Refusal::Incompatible => {
                "not handled automatically: make this Mac's Nearshell and the remote core there the same release".into()
            }
        }
    }
}

/// The gate: the remote core must be built for this hub (config) and be the
/// same release and build. Semver is not looked at.
pub fn gate(here: &BuildInfo, there: Option<&BuildInfo>) -> Result<(), Refusal> {
    let Some(there) = there.filter(|t| t.config == expected_remote(&here.config)) else {
        return Err(Refusal::BuildConflict);
    };
    match there.release_seq.cmp(&here.release_seq) {
        Ordering::Less => Err(Refusal::NeedsUpdate),
        Ordering::Greater => Err(Refusal::MachineNewer),
        Ordering::Equal if (&there.commit, &there.patchset) == (&here.commit, &here.patchset) => Ok(()),
        Ordering::Equal => Err(Refusal::BuildConflict),
    }
}

/// The gate on a dialed core's `hello`, as the dial's error. `add` says how
/// the machine is added, for the update hint.
pub fn admit(here: &BuildInfo, hello: &Value, target: &str, add: &str) -> Result<(), RpcError> {
    let there: Option<BuildInfo> = serde_json::from_value(hello["build"].clone()).ok();
    let Err(refusal) = gate(here, there.as_ref()) else { return Ok(()) };
    let short = |b: &BuildInfo| format!("{} {}", b.commit.get(..12).unwrap_or(&b.commit), b.patchset);
    let expected = expected_remote(&here.config);
    let message = match &there {
        None => format!("{target} answers without a build identity: not a Nearshell remote core"),
        Some(t) if t.config != expected => format!("{target}'s core is built as {}, not {expected}", t.config),
        Some(t) if refusal == Refusal::BuildConflict => {
            format!(
                "{target} runs another build of release {} ({}; this core: {})",
                t.release_seq,
                short(t),
                short(here)
            )
        }
        Some(t) => format!(
            "{target} runs {} Nearshell remote core (release {}; this core: release {})",
            if refusal == Refusal::NeedsUpdate { "an older" } else { "a newer" },
            t.release_seq,
            here.release_seq
        ),
    };
    Err(RpcError::new(refusal.state(), message, refusal.hint(&format!("{add} --install"))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build(commit: &str, patchset: &str, release_seq: u64, config: &str) -> BuildInfo {
        BuildInfo { commit: commit.into(), patchset: patchset.into(), release_seq, config: config.into() }
    }

    #[test]
    fn the_expected_remote_config_follows_the_hub_kind_and_is_idempotent() {
        use crate::build_identity::config_identity as id;
        // Managed hub, managed-test hub, remote build as hub, remote test build as hub.
        assert_eq!(expected_remote("release:com.nearshell.agent"), "release:com.nearshell.agent:remote");
        assert_eq!(expected_remote("dev:com.example.dev:scratch-test"), "dev:com.example.dev:remote:scratch-test");
        assert_eq!(expected_remote("dev:com.example.dev:remote"), "dev:com.example.dev:remote");
        assert_eq!(
            expected_remote("dev:com.example.dev:remote:scratch-test"),
            "dev:com.example.dev:remote:scratch-test"
        );
        for (remote, test) in [(false, false), (false, true), (true, false), (true, true)] {
            let hub = id("dev", "com.example.dev", remote, test);
            assert_eq!(expected_remote(&hub), id("dev", "com.example.dev", true, test), "{hub}");
            assert_eq!(expected_remote(&expected_remote(&hub)), expected_remote(&hub));
        }
    }

    #[test]
    fn the_gate_compares_build_identities_not_versions() {
        let here = build("c1", "p1", 5, "dev:com.example:scratch-test");
        let there = |commit, patchset, seq| build(commit, patchset, seq, "dev:com.example:remote:scratch-test");
        assert_eq!(gate(&here, Some(&there("c1", "p1", 5))), Ok(()));
        assert_eq!(gate(&here, Some(&there("c0", "p1", 4))), Err(Refusal::NeedsUpdate));
        assert_eq!(gate(&here, Some(&there("c2", "p1", 6))), Err(Refusal::MachineNewer));
        assert_eq!(gate(&here, Some(&there("c9", "p1", 5))), Err(Refusal::BuildConflict));
        assert_eq!(gate(&here, Some(&there("c1", "p9", 5))), Err(Refusal::BuildConflict));
        // Built for another hub: the managed config itself, another bundle, another kind.
        for config in
            ["dev:com.example:scratch-test", "dev:com.other:remote:scratch-test", "release:com.example:remote"]
        {
            assert_eq!(gate(&here, Some(&build("c1", "p1", 5, config))), Err(Refusal::BuildConflict), "{config}");
        }
        // Even an older release built for another hub is no update target.
        assert_eq!(gate(&here, Some(&build("c0", "p1", 4, "upstream"))), Err(Refusal::BuildConflict));
        assert_eq!(gate(&here, None), Err(Refusal::BuildConflict));
    }

    #[test]
    fn a_hello_without_a_build_is_a_conflict_and_codes_are_the_states() {
        let here = build("c1", "p1", 5, "release:com.example");
        let hello = |b: Value| json!({"version": "0.1.22", "build": b});
        let ok = json!({"commit":"c1","patchset":"p1","releaseSeq":5,"config":"release:com.example:remote"});
        assert!(admit(&here, &hello(ok.clone()), "box", "nhc machine add box").is_ok());
        let kind = |h: Value| admit(&here, &h, "box", "nhc machine add box").unwrap_err();
        // An upstream hc at that path, or a build too old to send one.
        let missing = kind(json!({"version": "0.1.22"}));
        assert_eq!(missing.kind, "build_conflict");
        assert!(missing.message.contains("without a build identity"), "{}", missing.message);
        assert_eq!(kind(hello(json!({"commit": "c1"}))).kind, "build_conflict");
        let mut older = ok.clone();
        older["releaseSeq"] = 4.into();
        let older = kind(hello(older));
        assert_eq!(
            (older.kind.as_str(), older.hint.as_str()),
            ("needs_update", "update its remote core: nhc machine add box --install")
        );
        let mut newer = ok;
        newer["releaseSeq"] = 6.into();
        assert_eq!(kind(hello(newer)).hint, "update Nearshell on this Mac");
        for r in [Refusal::NeedsUpdate, Refusal::MachineNewer, Refusal::BuildConflict, Refusal::Incompatible] {
            let e = RpcError::new(r.state(), "", "");
            assert_eq!(Refusal::of(&e), cfg!(feature = "nearshell").then_some(r));
        }
        assert_eq!(Refusal::of(&RpcError::new("unreachable", "", "")), None);
        assert_eq!(Refusal::NeedsUpdate.unavailable("box", "older").kind, "needs_update");
        assert_eq!(Refusal::MachineNewer.unavailable("box", "newer").kind, "unavailable");
    }

    #[test]
    fn the_line_names_the_remote_core_by_its_absolute_path() {
        assert_eq!(
            unix_line("com.example.agent", "not here"),
            "sh -c 'p=\"$HOME/.local/share/nearshell/com.example.agent/bin/nhc\"; [ -x \"$p\" ] || { echo \"not here\" >&2; exit 127; }; unset HC_SESSION HC_SOCKET; exec \"$p\" connect'"
        );
        assert!(WINDOWS_PROBE.contains("nearshell-windows-%OS%") && !WINDOWS_PROBE.contains(WINDOWS_SEEN));
    }
}
