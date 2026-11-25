//! The managed-test core of one test: a scratch root, an owner lock and token,
//! and a guardian that stops it whatever happens to the test.
#![allow(dead_code)] // Each test binary uses part of it.
use super::guardian::Guardian;
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

pub struct Scratch(pub PathBuf, pub String, pub Option<std::fs::File>, pub Guardian);
impl Scratch {
    pub fn new() -> Self {
        let root = PathBuf::from(format!("/var/tmp/nc-{}", herdr_core::platform::random_hex()));
        std::fs::create_dir_all(root.join("config/herdr-core")).unwrap();
        let lock =
            std::fs::File::options().create(true).truncate(false).write(true).open(root.join("owner.lock")).unwrap();
        assert!(herdr_core::platform::try_lock(&lock));
        let token = herdr_core::platform::random_hex() + &herdr_core::platform::random_hex();
        let guardian = Guardian::new(&root, std::path::Path::new(env!("CARGO_BIN_EXE_nhc")), Some(&token));
        Self(root, token, Some(lock), guardian)
    }
    pub fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nhc"));
        c.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("NEARSHELL_HC_TEST_ROOT", &self.0)
            .env("NEARSHELL_TEST_SESSION_ID", &self.3.nonce)
            .env("HOME", self.0.join("wrong-home"))
            .env("XDG_CONFIG_HOME", self.0.join("wrong-xdg"))
            .env("XDG_STATE_HOME", self.0.join("wrong-state"))
            .env("XDG_RUNTIME_DIR", self.0.join("wrong-run"));
        c
    }
    pub fn config(&self, text: &str) {
        std::fs::write(
            self.0.join("config/herdr-core/config.toml"),
            format!("owner_token_sha256 = '{}'\n{text}", herdr_core::owner::verifier(&self.1)),
        )
        .unwrap();
        Guardian::register(&self.0, std::path::Path::new(env!("CARGO_BIN_EXE_nhc")), Some(&self.1));
    }
    pub fn owner_cmd(&self) -> Command {
        let mut cmd = self.cmd();
        cmd.args(["--owner-token", &self.1]);
        cmd
    }
    pub fn start(&self) -> Value {
        value(self.owner_cmd().args(["server", "start"]).output().unwrap())
    }
    pub fn socket(&self) -> PathBuf {
        self.0.join("run/c.sock")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        self.2.take(); // Guardian acquires the owner lease before authorized stop.
        let clean = self.3.finish();
        if clean {
            let _ = std::fs::remove_dir_all(&self.0);
        }
        assert!(clean || std::thread::panicking(), "test_guardian_cleanup_unconfirmed");
    }
}
pub fn value(out: Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}
pub fn error(out: Output, kind: &str) {
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["error"]["kind"], kind, "{v}");
}

pub fn rpc(s: &Scratch, method: &str, params: Value) -> Result<Value, herdr_core::rpc::RpcError> {
    let stream = herdr_core::platform::connect(&s.socket()).unwrap();
    herdr_core::client::call(&stream, method, params)
}
