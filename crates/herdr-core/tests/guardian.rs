#![cfg(unix)]
mod common;
use common::*;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn guardian_child_fixture() {
    let Ok(ready) = std::env::var("HC_GUARDIAN_PROBE_READY") else { return };
    let s = Session::new();
    let t = run(&s, &["/bin/sh", "-c", "while :; do sleep 0.05; done"]);
    let shell = get(&s, &t)["pid"].as_i64().unwrap();
    let status = s.json(&["server", "status"]);
    let mut pids = vec![status["pid"].as_i64().unwrap(), shell];
    for holder in status["holders"].as_array().unwrap() {
        pids.push(holder["pid"].as_i64().unwrap());
    }
    if std::env::var_os("HC_GUARDIAN_REMOVE_ROOT").is_some() {
        std::fs::remove_dir_all(&s.root).unwrap();
    }
    std::fs::write(ready, serde_json::to_vec(&pids).unwrap()).unwrap();
    loop {
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn killed_test_binary_reaps_its_core_and_shell_without_stopping_parallel_session() {
    let other = Session::new();
    let terminal = run(&other, &["/bin/sleep", "60"]);
    let original = get(&other, &terminal)["pid"].clone();
    for deleted in [false, true] {
        let ready = other.root.join(if deleted { "deleted-ready" } else { "ready" });
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", "guardian_child_fixture", "--nocapture"])
            .env("HC_GUARDIAN_PROBE_READY", &ready)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if deleted {
            cmd.env("HC_GUARDIAN_REMOVE_ROOT", "1");
        }
        let mut child = cmd.spawn().unwrap();
        wait_until("guardian child ready", || ready.exists());
        let pids: Vec<i64> = serde_json::from_slice(&std::fs::read(&ready).unwrap()).unwrap();
        let start = Instant::now();
        child.kill().unwrap();
        child.wait().unwrap();
        wait_until("registered core and PTY gone after SIGKILL", || pids.iter().all(|p| !pid_alive(*p)));
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(get(&other, &terminal)["pid"], original);
        eprintln!("guardian_exit_ms={} root_deleted={deleted}", start.elapsed().as_millis());
    }
}
