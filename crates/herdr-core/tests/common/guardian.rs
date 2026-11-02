//! Test support only. A detached guardian survives SIGKILL of the test binary.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/test_session_guardian.py");

pub struct Guardian {
    root: PathBuf,
    pub nonce: String,
}
impl Guardian {
    pub fn new(root: &Path, binary: &Path, token: Option<&str>) -> Self {
        let nonce = Self::register(root, binary, token);
        Self { root: root.into(), nonce }
    }
    pub fn register(root: &Path, binary: &Path, token: Option<&str>) -> String {
        let python = "/usr/bin/python3";
        let mut cleanup = vec![
            python.to_string(),
            SCRIPT.into(),
            if token.is_some() { "managed-stop".into() } else { "upstream-stop".into() },
            root.display().to_string(),
            binary.display().to_string(),
        ];
        if let Some(token) = token {
            cleanup.push(token.into());
        }
        let spec = serde_json::json!({"root":root, "owner":std::process::id(), "cleanup":cleanup,
            "marked":true, "environment":{"PATH":std::env::var("PATH").unwrap_or_default()}});
        let mut child = Command::new(python)
            .args(["-B", SCRIPT, "register"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("guardian spawn");
        child.stdin.take().unwrap().write_all(spec.to_string().as_bytes()).expect("guardian registration");
        let output = child.wait_with_output().expect("guardian readiness");
        assert!(output.status.success(), "test_guardian_registration_failed");
        String::from_utf8(output.stdout).unwrap().trim().strip_prefix("READY ").expect("guardian READY").into()
    }
    pub fn finish(&self) -> bool {
        Command::new("/usr/bin/python3")
            .args(["-B", SCRIPT, "finish"])
            .arg(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}
