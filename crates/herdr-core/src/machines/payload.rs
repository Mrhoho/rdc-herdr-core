//! What a Nearshell hub installs on a machine, decided without IO: the payload
//! directory's manifest and which entry fits the machine, whether a file is the
//! one the manifest describes, the line that sends it there, and what the
//! remote core said once it put itself in place. `deliver.rs` does the IO.

use crate::api::BuildInfo;
use crate::rpc::RpcError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The manifest format `scripts/build-remote.py` writes.
pub const SCHEMA: u64 = 2;

/// The machines a remote core is built for, as `platform()` names them.
pub const TARGETS: [(&str, &str); 2] =
    [("linux x86_64", "x86_64-unknown-linux-musl"), ("linux aarch64", "aarch64-unknown-linux-musl")];

/// What the install line says when the stream ended early.
const INCOMPLETE: &str = "the copy arrived incomplete";

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    #[serde(default)]
    entries: Vec<Entry>,
}

/// One remote core in the directory: `file` beside manifest.json.
#[derive(Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub target: String,
    pub file: String,
    pub bytes: u64,
    pub sha256: String,
    pub build: BuildInfo,
}

/// What the remote core reported once it put itself in place.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Landed {
    /// The build installed there now (and running, when a core runs).
    pub build: BuildInfo,
    /// The build that was installed before; null for none, or one that did not run.
    pub was: Option<BuildInfo>,
    /// Whether the file there was replaced (false: it was already this build).
    pub replaced: bool,
    /// Whether a running core there was restarted onto it.
    pub restarted: bool,
}

fn invalid(dir: &str, why: String) -> RpcError {
    RpcError::new(
        "remote_payloads_invalid",
        format!("{dir}/manifest.json: {why}"),
        "use a payload directory written by scripts/build-remote.py",
    )
}

/// The manifest's entries, each well formed: known targets once each, plain
/// file names, sha256 in hex.
pub fn entries(manifest: &[u8], dir: &str) -> Result<Vec<Entry>, RpcError> {
    let m: Manifest = serde_json::from_slice(manifest).map_err(|e| invalid(dir, e.to_string()))?;
    if m.schema_version != SCHEMA {
        return Err(invalid(dir, format!("schemaVersion {}, this build reads {SCHEMA}", m.schema_version)));
    }
    for (i, e) in m.entries.iter().enumerate() {
        if !TARGETS.iter().any(|(_, t)| *t == e.target) {
            return Err(invalid(dir, format!("unknown target {:?}", e.target)));
        }
        if m.entries[..i].iter().any(|other| other.target == e.target) {
            return Err(invalid(dir, format!("two entries for {}", e.target)));
        }
        if e.file.is_empty()
            || e.file.starts_with('.')
            || e.file.contains(['/', '\\'])
            || e.file.contains(char::is_control)
        {
            return Err(invalid(dir, format!("file {:?} is not a plain name beside the manifest", e.file)));
        }
        if e.sha256.len() != 64 || !e.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(invalid(dir, format!("sha256 of {} is not 64 lowercase hex digits", e.file)));
        }
    }
    Ok(m.entries)
}

/// The target triple for a machine that `uname -sm` named `platform`.
pub fn target(platform: &str) -> Option<&'static str> {
    TARGETS.iter().find(|(p, _)| *p == platform).map(|(_, t)| *t)
}

pub fn unsupported(machine: &str, platform: &str) -> RpcError {
    if platform.starts_with("windows ") {
        return super::nearshell::windows_unsupported(machine);
    }
    RpcError::new(
        "unsupported_platform",
        format!("{machine} runs {platform}, which Nearshell does not support yet"),
        "add a Linux machine",
    )
}

/// The entry for `target`, built exactly as the remote core this hub expects.
pub fn choose(entries: Vec<Entry>, target: &str, expected: &BuildInfo, dir: &str) -> Result<Entry, RpcError> {
    let entry = entries.into_iter().find(|e| e.target == target).ok_or_else(|| {
        RpcError::new(
            "remote_payload_no_target",
            format!("{dir} has no remote core for {target}"),
            format!("add one built for {target} with scripts/build-remote.py"),
        )
    })?;
    if entry.build != *expected {
        return Err(not_expected(&entry.file, &entry.build, expected));
    }
    Ok(entry)
}

/// A payload whose build is not the one this hub expects there.
pub fn not_expected(file: &str, build: &BuildInfo, expected: &BuildInfo) -> RpcError {
    RpcError::new(
        "remote_payload_build",
        format!("{file} is {}, not {}", describe(build), describe(expected)),
        "use the remote cores built with this Nearshell",
    )
}

/// `release 5 (0123456789ab p2, dev:com.example:remote)`.
pub fn describe(b: &BuildInfo) -> String {
    format!("release {} ({}, {})", b.release_seq, super::nearshell::short(b), b.config)
}

/// The file is the one its entry describes: present, that long, that sha256.
pub fn check(entry: &Entry, file: Option<&[u8]>, shown: &str) -> Result<(), RpcError> {
    use sha2::Digest;
    let differs = |why: String| {
        RpcError::new(
            "remote_payload_mismatch",
            format!("{shown}: {why}"),
            "the payload directory is damaged: write it again with scripts/build-remote.py",
        )
    };
    let Some(file) = file else { return Err(differs("missing or unreadable".into())) };
    if file.len() as u64 != entry.bytes {
        return Err(differs(format!("{} bytes, the manifest says {}", file.len(), entry.bytes)));
    }
    let sha: String = sha2::Sha256::digest(file).iter().map(|b| format!("{b:02x}")).collect();
    if sha != entry.sha256 {
        return Err(differs(format!("sha256 {sha}, the manifest says {}", entry.sha256)));
    }
    Ok(())
}

/// The candidate's expected build as one shell-safe word (hex of its JSON).
fn expect_arg(b: &BuildInfo) -> String {
    serde_json::to_vec(b).expect("build serializes").iter().map(|b| format!("{b:02x}")).collect()
}

/// The build an install line expects (`--expect`), if it is one.
pub fn parse_expect(arg: &str) -> Option<BuildInfo> {
    let digits = arg.as_bytes();
    if !digits.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> =
        digits.chunks(2).map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()).collect();
    serde_json::from_slice(&bytes?).ok()
}

/// The line that takes `bytes` bytes on stdin into a private temp file in the
/// remote core's own bin folder, checks they all came, and runs that copy to
/// put itself in place (hidden `install`). Only `<root>/bin` is written: no
/// `hc` link, nothing in ~/.local/bin. The temp file goes on every way out
/// (once in place it is gone already); stale ones of failed attempts are
/// removed by a later install, under its lock. No single quote or backslash in
/// it: every login shell hands it to sh as is, and so does `--via`.
pub fn install_line(bundle: &str, bytes: u64, expect: &BuildInfo) -> String {
    let script = format!(
        "umask 077; unset HC_SESSION HC_SOCKET; d=\"{root}/bin\"; mkdir -p \"$d\" || exit 2; \
         t=\"$d/{staged}$$\"; c() {{ rm -f \"$t\"; }}; trap c EXIT; trap \"exit 1\" HUP INT TERM; \
         head -c {bytes} > \"$t\"; [ $(($(wc -c < \"$t\"))) -eq {bytes} ] || {{ echo \"{INCOMPLETE}\" >&2; exit 3; }}; \
         chmod 700 \"$t\" && \"$t\" --json install --from \"$t\" --expect {expect}",
        root = super::nearshell::remote_root(bundle),
        staged = crate::branding::STAGED_PREFIX,
        expect = expect_arg(expect),
    );
    format!("sh -c '{script}'")
}

/// What the install line's run says: the remote core's result, or its error
/// (a code of its own, kept), or why the line failed before it.
pub fn landed(machine: &str, ok: bool, stdout: &str, stderr: &str) -> Result<Landed, RpcError> {
    let failed = |why: &str| {
        RpcError::new(
            "install_failed",
            format!("on {machine}: {why}"),
            "retry; the remote core there is replaced only by a whole copy that checked itself",
        )
    };
    if ok {
        return serde_json::from_str(stdout).map_err(|_| failed("the remote core gave no install result"));
    }
    match serde_json::from_str::<Value>(stderr) {
        Ok(v) if v["error"]["kind"].is_string() => {
            let e = &v["error"];
            Err(RpcError::new(
                e["kind"].as_str().unwrap_or_default(),
                format!("on {machine}: {}", e["message"].as_str().unwrap_or_default()),
                e["hint"].as_str().unwrap_or_default(),
            ))
        }
        _ if stderr.is_empty() => Err(failed("the install ended without a word")),
        _ => Err(failed(stderr)),
    }
}

/// For people: what the install did there.
pub fn summary(l: &Landed) -> String {
    let now = l.build.release_seq;
    match (&l.was, l.replaced) {
        (_, false) => format!("its remote core is already current (release {now})"),
        (Some(was), true) => format!("updated its remote core from release {} to {now}", was.release_seq),
        (None, true) => format!("installed its remote core (release {now})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn build(seq: u64, commit: &str) -> BuildInfo {
        BuildInfo {
            commit: commit.into(),
            patchset: "p 'x\"$".into(),
            release_seq: seq,
            config: "dev:com.e:remote".into(),
        }
    }

    fn entry(target: &str, file: &str) -> Value {
        json!({"target": target, "file": file, "bytes": 3, "sha256": "a".repeat(64), "build": build(2, "c1"),
            "executed": false})
    }

    #[test]
    fn only_linux_on_x86_64_or_aarch64_has_a_remote_core() {
        let target = |uname: &str| target(&crate::machines::platform(uname));
        for (uname, triple) in [
            ("Linux x86_64", "x86_64-unknown-linux-musl"),
            ("Linux amd64", "x86_64-unknown-linux-musl"),
            ("Linux aarch64", "aarch64-unknown-linux-musl"),
            ("Linux arm64", "aarch64-unknown-linux-musl"),
        ] {
            assert_eq!(target(uname), Some(triple), "{uname}");
        }
        for uname in [
            "Darwin arm64",
            "MINGW64_NT-10.0 x86_64",
            "Linux armv7l",
            "Linux aarch64_be",
            "Linux i686",
            "FreeBSD amd64",
            "",
        ] {
            assert_eq!(target(uname), None, "{uname}");
        }
        assert_eq!(
            unsupported("box", "macos aarch64").message,
            "box runs macos aarch64, which Nearshell does not support yet"
        );
        assert_eq!(
            unsupported("box", "windows x86_64").message,
            "box runs Windows, which Nearshell does not support yet"
        );
        assert_eq!(unsupported("box", "linux i686").kind, "unsupported_platform");
    }

    #[test]
    fn a_manifest_names_each_target_once_with_a_plain_file_and_its_build() {
        let manifest = |entries: Vec<Value>| json!({"schemaVersion": 2, "entries": entries}).to_string();
        let ok =
            manifest(vec![entry("aarch64-unknown-linux-musl", "nhc-a"), entry("x86_64-unknown-linux-musl", "nhc-x")]);
        let all = entries(ok.as_bytes(), "/p").unwrap();
        let chosen = choose(all, "x86_64-unknown-linux-musl", &build(2, "c1"), "/p").unwrap();
        assert_eq!((chosen.file.as_str(), chosen.bytes), ("nhc-x", 3));
        // The earlier one-entry format, broken JSON, and entries that would read elsewhere.
        let kind = |text: String| entries(text.as_bytes(), "/p").unwrap_err().kind;
        let old = json!({"schemaVersion": 1, "entry": entry("x86_64-unknown-linux-musl", "nhc-x")}).to_string();
        assert_eq!(kind(old), "remote_payloads_invalid");
        assert_eq!(kind("{".into()), "remote_payloads_invalid");
        for bad in [
            entry("riscv64gc-unknown-linux-musl", "nhc"),
            entry("x86_64-unknown-linux-musl", "../nhc"),
            entry("x86_64-unknown-linux-musl", ".nhc"),
            entry("x86_64-unknown-linux-musl", ""),
            json!({"target": "x86_64-unknown-linux-musl", "file": "nhc", "bytes": 3, "sha256": "A".repeat(64), "build": build(2, "c1")}),
            json!({"target": "x86_64-unknown-linux-musl", "file": "nhc", "bytes": 3, "sha256": "a".repeat(64)}),
        ] {
            assert_eq!(kind(manifest(vec![bad.clone()])), "remote_payloads_invalid", "{bad}");
        }
        let twice = manifest(vec![entry("x86_64-unknown-linux-musl", "a"), entry("x86_64-unknown-linux-musl", "b")]);
        assert_eq!(kind(twice), "remote_payloads_invalid");
        // A missing target, and builds other than the expected one.
        let one = entries(manifest(vec![entry("x86_64-unknown-linux-musl", "nhc-x")]).as_bytes(), "/p").unwrap();
        let e = choose(one.clone(), "aarch64-unknown-linux-musl", &build(2, "c1"), "/p").unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str()),
            ("remote_payload_no_target", "/p has no remote core for aarch64-unknown-linux-musl")
        );
        let mut other = build(2, "c1");
        other.config = "dev:com.e:remote:scratch-test".into();
        for expected in [build(3, "c1"), build(2, "c2"), other] {
            let e = choose(one.clone(), "x86_64-unknown-linux-musl", &expected, "/p").unwrap_err();
            assert_eq!(e.kind, "remote_payload_build", "{expected:?}");
        }
    }

    #[test]
    fn a_file_must_be_exactly_what_its_entry_says() {
        use sha2::Digest;
        let sha: String = sha2::Sha256::digest(b"nhc").iter().map(|b| format!("{b:02x}")).collect();
        let e = Entry { target: "t".into(), file: "f".into(), bytes: 3, sha256: sha, build: build(1, "c") };
        assert!(check(&e, Some(b"nhc"), "/p/f").is_ok());
        for (file, why) in [
            (None, "/p/f: missing or unreadable"),
            (Some(&b"nh"[..]), "/p/f: 2 bytes, the manifest says 3"),
            (Some(&b"nhd"[..]), "/p/f: sha256 "),
        ] {
            let err = check(&e, file, "/p/f").unwrap_err();
            assert_eq!(err.kind, "remote_payload_mismatch");
            assert!(err.message.starts_with(why), "{}", err.message);
        }
    }

    #[test]
    fn the_install_line_writes_only_the_remote_bin_folder_and_lands_by_the_copy() {
        let b = build(4, "0123456789abcdef");
        let line = install_line("com.example.agent", 123, &b);
        let script = line.strip_prefix("sh -c '").and_then(|s| s.strip_suffix('\'')).unwrap();
        assert!(script.starts_with("umask 077; unset HC_SESSION HC_SOCKET; "), "{script}");
        assert!(script.contains("d=\"$HOME/.local/share/nearshell/com.example.agent/bin\"; mkdir -p \"$d\""));
        assert!(script.contains(&format!("t=\"$d/{}$$\"", crate::branding::STAGED_PREFIX)));
        assert!(script.contains("head -c 123 > \"$t\"; [ $(($(wc -c < \"$t\"))) -eq 123 ] || "));
        assert!(script.contains("trap c EXIT; trap \"exit 1\" HUP INT TERM;"));
        let tail = "\"$t\" --json install --from \"$t\" --expect ";
        let expect = &script[script.find(tail).unwrap() + tail.len()..];
        assert_eq!(parse_expect(expect), Some(b), "the patchset's quotes survive as hex");
        assert!(!script.contains(['\'', '\\']), "one word to every login shell: {script}");
        for never in [".local/bin", "ln ", "hc ", "curl", "wget"] {
            assert!(!script.contains(never), "{never}: {script}");
        }
        assert_eq!(parse_expect("zz"), None);
        assert_eq!(parse_expect("7b"), None);
    }

    #[test]
    fn the_remote_cores_word_is_kept_and_a_failed_line_says_why() {
        let l = Landed { build: build(5, "c5"), was: Some(build(4, "c4")), replaced: true, restarted: true };
        let ok = serde_json::to_string(&l).unwrap();
        assert_eq!(landed("box", true, &ok, ""), Ok(l.clone()));
        assert_eq!(summary(&l), "updated its remote core from release 4 to 5");
        assert_eq!(summary(&Landed { was: None, ..l.clone() }), "installed its remote core (release 5)");
        assert_eq!(summary(&Landed { replaced: false, ..l }), "its remote core is already current (release 5)");
        let refused = json!({"error": {"kind": "machine_newer", "message": "a newer one", "hint": "update Nearshell on this Mac"}});
        let e = landed("box", false, "", &refused.to_string()).unwrap_err();
        assert_eq!(
            (e.kind.as_str(), e.message.as_str(), e.hint.as_str()),
            ("machine_newer", "on box: a newer one", "update Nearshell on this Mac")
        );
        let e = landed("box", false, "", INCOMPLETE).unwrap_err();
        assert_eq!((e.kind.as_str(), e.message.as_str()), ("install_failed", "on box: the copy arrived incomplete"));
        assert_eq!(landed("box", true, "nhc 0.1.22", "").unwrap_err().kind, "install_failed");
    }
}
