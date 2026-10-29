#![cfg(unix)]
mod common;
use common::*;
use serde_json::{Value, json};

#[test]
fn local_identity_never_resolves_paths_or_starts_a_server() {
    let s = Session::new();
    let out = s.cmd().env("HC_SESSION", "invalid/session").args(["build-info", "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let local: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(local["commit"].is_string());
    assert!(local["patchset"].is_string());
    assert!(local["releaseSeq"].is_u64());
    assert!(local["config"].is_string());
    assert!(!s.socket().exists());
    assert!(!s.root.join("state/herdr-core/test").exists());
    let status = s.json(&["server", "status"]);
    let hello = s.connect().call(1, "hello", json!({}));
    assert_eq!(status["build"], local);
    assert_eq!(hello["result"]["build"], local);
    assert!(hello["result"]["features"].as_array().unwrap().contains(&json!("build-info")));
}
