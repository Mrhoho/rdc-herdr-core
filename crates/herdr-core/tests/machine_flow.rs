//! Adding, logging in to and updating a machine from a terminal, as a person
//! does: asked, shown each step, told the outcome, held open with --hold.
//! And an install's ssh: cut off at its limit or with the CLI, never left.
#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use serde_json::json;
use std::os::unix::process::ExitStatusExt;
use std::time::{Duration, Instant};

/// A fake ssh to `stall` where no remote core answers and every other line
/// hangs, leaving `<root>/child.pid` for what it started there.
fn stalling(s: &mut Session) {
    let root = s.root.display();
    let ssh = executable(
        s,
        &format!(
            "#!/bin/sh\ncase \"$*\" in *' connect'*) echo 'herdr-core is not installed here' >&2; exit 127 ;; esac\n\
             sleep 300 &\necho $! > {root}/child.pid\nwait\n"
        ),
    );
    s.env.push(("HC_SSH".into(), ssh));
}

fn child_left(s: &Session) -> i64 {
    std::fs::read_to_string(s.root.join("child.pid")).unwrap().trim().parse().unwrap()
}

#[test]
fn an_install_line_that_hangs_is_cut_off_at_its_limit_with_nothing_left() {
    let mut s = Session::new();
    stalling(&mut s);
    s.env.push(("HC_INSTALL_LIMIT_MS".into(), "1500".into()));
    let started = Instant::now();
    let (code, e) = s.err(&["--json", "machine", "add", "stall", "--install"]);
    assert_eq!(e["error"]["kind"], "install_timeout", "{e}");
    assert!(code != 0 && started.elapsed() < Duration::from_secs(10), "{code} {:?}", started.elapsed());
    assert!(e["error"]["message"].as_str().unwrap().starts_with("stall did not finish within 1 s"), "{e}");
    assert!(e["error"]["hint"].as_str().unwrap().contains("`ssh stall` answers"), "{e}");
    let child = child_left(&s);
    wait_until("what ssh started there ended", || !pid_alive(child));
    assert!(s.json(&["machine", "ls"])["machines"].as_array().unwrap().is_empty());
}

#[test]
fn an_install_ends_its_ssh_when_the_cli_is_interrupted_or_its_terminal_closes() {
    for signal in ["INT", "HUP", "TERM"] {
        let mut s = Session::new();
        stalling(&mut s);
        s.json(&["server", "start"]);
        let mut cli = s.cmd().args(["--json", "machine", "add", "stall", "--install"]).spawn().unwrap();
        wait_until("the install line started", || s.root.join("child.pid").exists());
        let child = child_left(&s);
        std::process::Command::new("kill").args([&format!("-{signal}"), &cli.id().to_string()]).status().unwrap();
        let status = cli.wait().unwrap();
        let number = match signal {
            "INT" => libc::SIGINT,
            "HUP" => libc::SIGHUP,
            _ => libc::SIGTERM,
        };
        assert_eq!(status.signal(), Some(number), "{signal}: it ends as it was asked to: {status:?}");
        wait_until(&format!("{signal}: what ssh started ended with it"), || !pid_alive(child));
    }
}

/// `sh -c <line>; echo status-$?` in a terminal of `s`, kept open.
fn in_a_terminal(s: &Session, line: &str) -> String {
    run(s, &["sh", "-c", &format!("{line}; echo status-$?; sleep 100")])
}

fn screen_until(s: &Session, t: &str, what: &str) -> String {
    wait_until(what, || text(s, t).contains(what));
    text(s, t)
}

/// No hc on `bare` (until one is installed there), rohan with one.
fn worlds() -> (Session, Session, Session) {
    let mut bare = Session::new();
    bare.bin = bare.root.join("bin/herdr-core");
    let rohan = Session::new();
    let mut s = Session::new();
    s.env.push(fake_ssh(&s, &[("bare", &bare), ("rohan", &rohan)]));
    (bare, rohan, s)
}

#[test]
fn a_person_adds_a_machine_by_answering_and_sees_each_step_of_its_install() {
    let (_bare, _rohan, s) = worlds();
    s.json(&["machine", "add", "rohan"]);
    let t = in_a_terminal(&s, &format!("{BIN} machine add --install --hold"));
    screen_until(&s, &t, "ssh target (");
    s.json(&["send", &t, "--", "-oProxyCommand=x"]);
    screen_until(&s, &t, "bad ssh target");
    s.json(&["send", &t, "bare"]);
    screen_until(&s, &t, "name for it here [bare]:");
    s.json(&["send", &t, "rohan"]);
    screen_until(&s, &t, "\"rohan\" is already a machine here");
    s.json(&["keys", &t, "enter"]);
    let screen = screen_until(&s, &t, "press Enter to close");
    let platform = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    let total = std::fs::metadata(BIN).unwrap().len();
    let mb = format!("{:.1} of {:.1} MB, arrived", total as f64 / 1e6, total as f64 / 1e6);
    for step in [
        "logging in to bare (ssh may ask for a password or a code)\nlogged in\n",
        "looking for hc there... not installed\n",
        &format!("finding out what bare runs... {platform}\n"),
        &format!("sending this hc... {mb}"),
        "starting it there... done\n",
        "adding it here... done\n",
        "waiting for the line... connected\n",
        "added bare: installed hc ",
    ] {
        assert!(screen.contains(step), "{step:?} in:\n{screen}");
    }
    assert!(!screen.contains("status-"), "held:\n{screen}");
    s.json(&["keys", &t, "enter"]);
    screen_until(&s, &t, "status-0");
    let m = s.json(&["machine", "ls"])["machines"].as_array().unwrap().iter().find(|m| m["name"] == "bare").cloned();
    assert_eq!(m.unwrap()["state"], "connected");
}

#[test]
fn an_update_shows_its_steps_and_hold_keeps_the_exit_status() {
    let (_bare, _rohan, s) = worlds();
    s.json(&["machine", "add", "bare", "--install"]);
    let t = in_a_terminal(&s, &format!("{BIN} machine update bare --hold"));
    let screen = screen_until(&s, &t, "press Enter to close");
    for step in [
        "logged in\n",
        "finding out what bare runs... ",
        "sending this hc... ",
        ", arrived\nstarting it there... done\n",
        "waiting for the line... connected\n",
        "bare now runs hc ",
        "; its terminals kept running",
    ] {
        assert!(screen.contains(step), "{step:?} in:\n{screen}");
    }
    s.json(&["keys", &t, "enter"]);
    screen_until(&s, &t, "status-0");

    // A failure: said with what to do, held, and its status kept.
    let (code, _) = s.err(&["--json", "machine", "add", "nohc-box", "--install"]);
    assert_ne!(code, 0);
    let t = in_a_terminal(&s, &format!("{BIN} machine add nohc-box --install --hold"));
    let screen = screen_until(&s, &t, "press Enter to close");
    assert!(screen.contains("\nerror: ") && screen.contains("\nhint: "), "{screen}");
    assert!(!screen.contains("status-"));
    s.json(&["keys", &t, "enter"]);
    screen_until(&s, &t, &format!("status-{code}"));
    // A login by its id, held too.
    let id = s.json(&["machine", "ls"])["machines"][0]["id"].as_str().unwrap().to_string();
    let t = in_a_terminal(&s, &format!("{BIN} machine login {id} --hold"));
    let screen = screen_until(&s, &t, "press Enter to close");
    assert!(screen.contains("logged in to bare: connected"), "{screen}");
    s.json(&["keys", &t, "enter"]);
    screen_until(&s, &t, "status-0");
}

#[test]
fn without_a_terminal_hold_waits_for_nothing_and_add_still_needs_a_target() {
    let (_bare, _rohan, s) = worlds();
    let started = Instant::now();
    let out = s.hc(&["machine", "add", "rohan", "--hold"]);
    assert!(out.status.success() && started.elapsed() < Duration::from_secs(20), "{out:?}");
    let (code, e) = s.err(&["--json", "machine", "add"]);
    assert_eq!(code, 2);
    assert_eq!(
        e,
        json!({"error": {"kind": "usage", "hint": "see `hc --help`",
            "message": "the following required arguments were not provided: <TARGET>"}})
    );
    // A terminal, but nobody to answer (stdin is not one): clap's own usage error, as always.
    let t = in_a_terminal(&s, &format!("{BIN} machine add < /dev/null"));
    let screen = screen_until(&s, &t, "status-");
    assert!(screen.contains("the following required arguments were not provided") && screen.contains("Usage: "));
    assert!(screen.contains("status-2") && !screen.contains("ssh target"), "{screen}");
    let out = s.hc(&["machine", "login", "rohan", "--hold"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("logging in needs a terminal"), "{out:?}");
}
