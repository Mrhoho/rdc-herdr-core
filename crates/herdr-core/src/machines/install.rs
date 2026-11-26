//! Running a command there for an install, bounded and watched, and copying
//! this hc there (the upstream installer; Nearshell's is `deliver`).

use super::{KEEPALIVE, Reach, WINDOWS, WINDOWS_HC, check_target, unreachable};
use crate::addr::check_name;
use crate::branding::COMMAND as CLI_NAME;
use crate::rpc::RpcError;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// How an install there is getting on, for a person watching it.
pub enum Stage<'a> {
    /// Finding out what runs there.
    Probing,
    /// It runs this (`linux x86_64`).
    Platform(&'a str),
    /// Bytes of the copy given to the link so far, of all of them.
    Sent(u64, u64),
    /// The whole copy is there: it is being put in place and started.
    Arrived,
}

/// What an install line prints there once the whole copy arrived (its
/// result comes after it, on the last line). No quote or `$` in it.
pub const ARRIVED: &str = "copy arrived";

/// How long a run there may take in all: a minute for a short command; for a
/// copy a minute more, plus its size at 16 kB/s (a slow link): 8 MB has 10
/// minutes. ssh itself gives up a link silent for a minute (`KEEPALIVE`).
fn limit(bytes: usize) -> Duration {
    match crate::tuning::ms("HC_INSTALL_LIMIT_MS", 0) {
        0 if bytes == 0 => Duration::from_secs(60),
        0 => Duration::from_secs(120 + bytes as u64 / 16_000),
        ms => Duration::from_millis(ms),
    }
}

fn timed_out(to: Reach, limit: Duration) -> RpcError {
    let check = match to.via {
        true => format!("`{} 'echo ok'` prints ok", to.target),
        false => format!("`ssh {}` answers", to.target),
    };
    RpcError::new(
        "install_timeout",
        format!("{} did not finish within {} s: the link is down or too slow", to.target, limit.as_secs()),
        format!("nothing is replaced there unless a whole copy arrived; check that {check}, then run it again"),
    )
}

/// `uname -sm` as this binary names platforms: `linux x86_64`, `macos aarch64`.
pub(super) fn platform(uname: &str) -> String {
    let mut words = uname.split_whitespace();
    let os = match words.next() {
        Some("Linux") => "linux",
        Some("Darwin") => "macos",
        Some(os) if os.starts_with("MINGW") || os.starts_with("MSYS") => "windows",
        other => other.unwrap_or("?"),
    };
    let arch = match words.next() {
        Some("arm64" | "aarch64") => "aarch64",
        Some("x86_64" | "amd64") => "x86_64",
        other => other.unwrap_or("?"),
    };
    format!("{os} {arch}")
}

/// Runs `command` there as its login shell takes it (riding the shared
/// login), feeding it `input`: the last line it printed.
fn run_there(
    paths: &crate::paths::Paths,
    to: Reach,
    command: &str,
    input: &[u8],
    watch: &mut dyn FnMut(Stage),
) -> Result<String, RpcError> {
    let out = run_raw(paths, to, command, input, watch)?;
    match out.status.success() {
        true => Ok(last_line(&out.stdout)),
        false => Err(RpcError::new(
            "install_failed",
            format!("on {}: {}", to.target, last_line(&out.stderr)),
            "nothing was changed there",
        )),
    }
}

pub(super) fn last_line(b: &[u8]) -> String {
    String::from_utf8_lossy(b).lines().rfind(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_string()
}

/// Runs `command` there (riding the shared login), feeding it `input`,
/// within its `limit`. The ssh and what it started end when the limit
/// passes, or when this process is interrupted (Ctrl-C, a closed terminal),
/// which then ends as it was asked to.
pub(super) fn run_raw(
    paths: &crate::paths::Paths,
    to: Reach,
    command: &str,
    input: &[u8],
    watch: &mut dyn FnMut(Stage),
) -> Result<std::process::Output, RpcError> {
    let mut options = vec!["BatchMode=yes", "ConnectTimeout=10"];
    options.extend(KEEPALIVE);
    let cmd = to.command(paths, &options, command)?;
    run_child(to, cmd, input, limit(input.len()), watch)
}

enum Fed {
    Sent(u64),
    Arrived,
}

fn run_child(
    to: Reach,
    mut cmd: std::process::Command,
    input: &[u8],
    limit: Duration,
    watch: &mut dyn FnMut(Stage),
) -> Result<std::process::Output, RpcError> {
    use std::io::Write;
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    crate::platform::own_group(&mut cmd);
    let interrupts = crate::platform::Interrupts::catch();
    let mut child = cmd.spawn().map_err(|e| unreachable(to, &format!("cannot run it: {e}")))?;
    let (tx, fed) = mpsc::channel();
    let (mut stdin, sent, chunks) = (child.stdin.take().expect("piped"), tx.clone(), input.to_vec());
    std::thread::spawn(move || {
        let mut done = 0;
        for chunk in chunks.chunks(64 * 1024) {
            if stdin.write_all(chunk).is_err() {
                return;
            }
            done += chunk.len() as u64;
            let _ = sent.send(Fed::Sent(done));
        }
    });
    let stdout = collect(child.stdout.take().expect("piped"), Some(tx));
    let stderr = collect(child.stderr.take().expect("piped"), None);
    let (total, deadline) = (input.len() as u64, Instant::now() + limit);
    let mut tell = |f: Fed| match f {
        Fed::Sent(n) => watch(Stage::Sent(n, total)),
        Fed::Arrived => watch(Stage::Arrived),
    };
    let status = loop {
        if let Ok(f) = fed.recv_timeout(Duration::from_millis(100)) {
            tell(f);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if interrupts.caught() || Instant::now() >= deadline => {
                crate::platform::end_group(&mut child);
                return Err(match interrupts.caught() {
                    true => RpcError::new("cancelled", "interrupted", ""),
                    false => timed_out(to, limit),
                });
            }
            Ok(None) => {}
            Err(e) => return Err(unreachable(to, &e.to_string())),
        }
    };
    // Bounded: something it started may hold its pipes open.
    let read = |r: mpsc::Receiver<Vec<u8>>| r.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    let (stdout, stderr) = (read(stdout), read(stderr));
    fed.try_iter().for_each(&mut tell);
    Ok(std::process::Output { status, stdout, stderr })
}

/// All a pipe gives, once it closes; `arrived` hears the line `ARRIVED`.
fn collect(
    mut pipe: impl std::io::Read + Send + 'static,
    arrived: Option<mpsc::Sender<Fed>>,
) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut all, mut chunk, mut arrived) = (Vec::new(), [0u8; 8192], arrived);
        while let Ok(n @ 1..) = pipe.read(&mut chunk) {
            all.extend_from_slice(&chunk[..n]);
            if arrived.is_some() && says_arrived(&all) {
                let _ = arrived.take().expect("some").send(Fed::Arrived);
            }
        }
        let _ = tx.send(all);
    });
    rx
}

/// Whether a whole line of `out` is `ARRIVED`.
fn says_arrived(out: &[u8]) -> bool {
    let whole = &out[..out.iter().rposition(|b| *b == b'\n').map_or(0, |at| at + 1)];
    whole.split(|b| *b == b'\n').any(|l| l.strip_suffix(b"\r").unwrap_or(l) == ARRIVED.as_bytes())
}

/// Copies this very hc to `target` (same OS and CPU only), swaps it in at
/// ~/.local/bin/herdr-core (adding `hc` beside it only if there is none),
/// and restarts hc there in `session`, so terminals there keep running.
/// Returns what the installed one says it is.
pub fn install(
    paths: &crate::paths::Paths,
    to: Reach,
    session: Option<&str>,
    watch: &mut dyn FnMut(Stage),
) -> Result<String, RpcError> {
    crate::branding::upstream_distribution()?;
    check_target(to)?;
    let target = to.target;
    if let Some(s) = session {
        check_name(s)?;
    }
    let here = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    watch(Stage::Probing);
    let probe = run_raw(paths, to, "uname -sm", b"", watch)?;
    let there = platform(&last_line(&probe.stdout));
    let windows = if probe.status.success() {
        there.starts_with("windows ")
    } else {
        let native = run_raw(paths, to, "cmd /d /c \"echo herdr-core-windows\"", b"", watch)?;
        native.status.success() && last_line(&native.stdout) == "herdr-core-windows"
    };
    if windows || probe.status.success() {
        watch(Stage::Platform(if windows { "windows" } else { &there }));
    }
    if windows {
        if to.via {
            return Err(RpcError::new(
                "install_failed",
                format!("{target} runs Windows"),
                format!("install {CLI_NAME} there yourself"),
            ));
        }
        WINDOWS.lock().unwrap_or_else(|p| p.into_inner()).push(target.to_string());
        return install_windows(paths, target, session, watch);
    }
    if !probe.status.success() {
        return Err(RpcError::new(
            "install_failed",
            format!("on {target}: {}", last_line(&probe.stderr)),
            "nothing was changed there",
        ));
    }
    // Same OS and CPU: this very hc. Else this version's released build for it.
    let scratch = crate::releases::Scratch::new()?;
    let macos = there.starts_with("macos ");
    let bundle = !macos || run_there(paths, to, "id -u", b"", watch)? != "0";
    let current = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|e| RpcError::internal(format!("cannot find this hc: {e}")))?;
    let bin = if there == here && (bundle || !current.ends_with("Herdr Core.app/Contents/MacOS/herdr-core")) {
        current
    } else {
        let version = env!("CARGO_PKG_VERSION");
        let not_released = |_| {
            RpcError::new(
                "install_failed",
                format!("{target} is {there}, and {CLI_NAME} {version} has no release to download"),
                format!("install {CLI_NAME} there yourself"),
            )
        };
        let m = crate::releases::manifest(Some(version), &scratch.0).map_err(not_released)?;
        crate::releases::download(&m, &there.replace(' ', "-"), &scratch.0, bundle)?
    };
    let (exe, bundled) = install_payload(&bin, &scratch.0)?;
    let n = exe.len();
    let session = session.map(|s| format!("HC_SESSION={s} ")).unwrap_or_default();
    if bundled {
        let script = format!(
            "set -e; d=$HOME/.local/bin; mkdir -p \"$d\"; t=$(mktemp -d); trap \"rm -rf \\\"$t\\\"\" EXIT; \
             head -c {n} > \"$t/core.tar\"; [ $(wc -c < \"$t/core.tar\") -eq {n} ]; echo {ARRIVED}; \
             tar -xf \"$t/core.tar\" -C \"$t\"; b=\"$t/Herdr Core.app/Contents/MacOS/herdr-core\"; \
             \"$b\" --hc-install-bundle \"$b\" \"$d/herdr-core\"; \
             {session}\"$d/herdr-core\" server restart > /dev/null; \"$d/herdr-core\" --version"
        );
        return run_there(paths, to, &format!("sh -c '{script}'"), &exe, watch);
    }
    let guard = if macos {
        "case $(readlink \"$d/herdr-core\" || true) in */Contents/MacOS/herdr-core) \
         echo \"a bundled remote core needs a complete bundle; run hc update on that machine\" >&2; exit 5;; esac; "
    } else {
        ""
    };
    // Into a temp file beside the target, checked whole and running, then swapped.
    let script = format!(
        "set -e; d=$HOME/.local/bin; mkdir -p $d; {guard}rm -f $d/.herdr-core.new.*; t=$d/.herdr-core.new.$$; \
         head -c {n} > $t; [ $(($(wc -c < $t))) -eq {n} ] || {{ rm -f $t; echo \"the copy arrived incomplete\" >&2; exit 3; }}; \
         echo {ARRIVED}; chmod 755 $t; $t --version > /dev/null || {{ rm -f $t; echo \"this {CLI_NAME} does not run there\" >&2; exit 4; }}; \
         mv -f $t $d/herdr-core; command -v hc > /dev/null 2>&1 || [ -e $d/hc ] || ln -s herdr-core $d/hc; \
         {session}$d/herdr-core server restart > /dev/null; $d/herdr-core --version"
    );
    run_there(paths, to, &format!("sh -c '{script}'"), &exe, watch)
}

fn install_payload(bin: &Path, scratch: &Path) -> Result<(Vec<u8>, bool), RpcError> {
    let bundled = bin.ends_with("Herdr Core.app/Contents/MacOS/herdr-core");
    let path = if bundled {
        let root = bin.parent().unwrap().parent().unwrap().parent().unwrap();
        let archive = scratch.join("core-bundle.tar");
        let status = std::process::Command::new("tar")
            .arg("-cf")
            .arg(&archive)
            .arg("-C")
            .arg(root.parent().unwrap())
            .arg("Herdr Core.app")
            .env("COPYFILE_DISABLE", "1")
            .status()
            .map_err(|e| RpcError::internal(e.to_string()))?;
        if !status.success() {
            return Err(RpcError::internal("cannot pack the core bundle"));
        }
        archive
    } else {
        bin.to_path_buf()
    };
    let bytes = std::fs::read(path).map_err(|e| RpcError::internal(e.to_string()))?;
    Ok((bytes, bundled))
}

/// Windows there: this version's release zip and the installer go to its
/// home folder, and the installer runs from them (no download there). It
/// swaps an hc already there the planned way, so its terminals keep running.
fn install_windows(
    paths: &crate::paths::Paths,
    target: &str,
    session: Option<&str>,
    watch: &mut dyn FnMut(Stage),
) -> Result<String, RpcError> {
    let scratch = crate::releases::Scratch::new()?;
    let version = env!("CARGO_PKG_VERSION");
    let m = crate::releases::manifest(Some(version), &scratch.0).map_err(|_| {
        RpcError::new(
            "install_failed",
            format!("{target} runs Windows, and {CLI_NAME} {version} has no release to download"),
            format!("install {CLI_NAME} there yourself"),
        )
    })?;
    let zip = crate::releases::fetch(&m, "windows-x86_64", &scratch.0)?;
    let script = scratch.0.join("hc-install.ps1");
    std::fs::write(&script, include_str!("../../../../distribution/install.ps1"))
        .map_err(|e| RpcError::internal(e.to_string()))?;
    std::fs::copy(&zip, scratch.0.join("hc-install.zip")).map_err(|e| RpcError::internal(e.to_string()))?;
    let mut scp = std::process::Command::new("scp");
    scp.arg("-q");
    #[cfg(unix)]
    scp.arg("-o")
        .arg(format!("ControlPath={}", super::control_path(paths, target).display()))
        .args(["-o", "ControlMaster=no"]);
    scp.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
    for o in KEEPALIVE {
        scp.args(["-o", o]);
    }
    scp.arg(scratch.0.join("hc-install.zip")).arg(&script).arg(format!("{target}:"));
    let to = Reach { target, via: false };
    let size = std::fs::metadata(&zip).map_or(0, |m| m.len());
    watch(Stage::Sent(0, size));
    let out = run_child(to, scp, &[], limit(size as usize), &mut |_| {})?;
    watch(Stage::Arrived);
    if !out.status.success() {
        return Err(RpcError::new(
            "install_failed",
            format!("copying {CLI_NAME} to {target}: {}", String::from_utf8_lossy(&out.stderr).trim()),
            "nothing was changed there",
        ));
    }
    // No quotes or $ in it: cmd and PowerShell read it the same.
    let session = session.map(|s| format!(" {s}")).unwrap_or_default();
    run_there(
        paths,
        to,
        &format!("powershell -NoProfile -ExecutionPolicy Bypass -File hc-install.ps1 hc-install.zip{session}"),
        b"",
        watch,
    )?;
    run_there(paths, to, &format!("{WINDOWS_HC} --version"), b"", watch)
}
