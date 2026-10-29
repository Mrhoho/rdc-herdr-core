#![cfg(all(unix, feature = "managed-test"))]
use serde_json::Value;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = PathBuf::from(format!("/var/tmp/nc-{}", herdr_core::platform::random_hex()));
        std::fs::create_dir_all(root.join("config/herdr-core")).unwrap();
        Self(root)
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_nearshell-hc"));
        c.env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("NEARSHELL_HC_TEST_ROOT", &self.0)
            .env("HOME", self.0.join("wrong-home"))
            .env("XDG_CONFIG_HOME", self.0.join("wrong-xdg"))
            .env("XDG_STATE_HOME", self.0.join("wrong-state"))
            .env("XDG_RUNTIME_DIR", self.0.join("wrong-run"));
        c
    }
    fn config(&self, text: &str) {
        std::fs::write(self.0.join("config/herdr-core/config.toml"), text).unwrap();
    }
    fn socket(&self) -> PathBuf {
        self.0.join("run/c.sock")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if self.socket().exists() {
            let _ = self.cmd().args(["server", "stop"]).output();
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn value(out: Output) -> Value {
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}
fn error(out: Output, kind: &str) {
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stderr).unwrap();
    assert_eq!(v["error"]["kind"], kind, "{v}");
}
#[test]
fn managed_addressing_fails_closed_and_ignores_shell_xdg() {
    let s = Scratch::new();
    error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    for text in ["[broken", "headless = '80x24'", "socket = 'relative.sock'"] {
        s.config(text);
        error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    }
    s.config(&format!("socket = '{}'", s.socket().display()));
    for var in ["HC_SESSION", "HC_SOCKET"] {
        for val in ["", "other"] {
            error(s.cmd().env(var, val).args(["server", "status"]).output().unwrap(), "managed_address_override");
        }
    }
    error(s.cmd().args(["connect", "--session", "other"]).output().unwrap(), "managed_address_override");
    assert!(!s.socket().exists());
    let local = value(s.cmd().args(["build-info", "--json"]).output().unwrap());
    let status = value(s.cmd().args(["server", "status"]).output().unwrap());
    assert_eq!(status["build"], local);
    assert!(s.socket().exists());
    assert!(s.0.join("state/herdr-core/default/core.json").exists());
    assert!(!s.0.join("wrong-xdg").exists());
    assert!(!s.0.join("wrong-home").exists());
}
#[test]
fn managed_cli_uses_its_own_name_and_keeps_payloads_opaque() {
    let s = Scratch::new();
    for args in [&["--help"][..], &["--skill"], &["run", "--help"], &["--json", "--help"]] {
        let out = s.cmd().args(args).output().unwrap();
        assert!(out.status.success());
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("nearshell-hc"), "{text}");
        assert!(!text.split_whitespace().any(|w| w == "hc"), "{text}");
        assert!(!text.contains("Tests need their own `HC_SESSION`"));
    }
    let out = s.cmd().arg("--version").output().unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "nearshell-hc 0.1.13");
    s.config(&format!("socket = '{}'", s.socket().display()));
    let payload = "hc send t1 -- hc";
    let v = value(s.cmd().args(["--json", "exec", "printf", "%s", payload]).output().unwrap());
    assert_eq!(v["stdout"], "aGMgc2VuZCB0MSAtLSBoYw==");
}

#[test]
fn managed_socket_directory_must_be_private_and_not_a_symlink() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let s = Scratch::new();
    let run = s.0.join("run");
    std::fs::create_dir(&run).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    s.config(&format!("socket = '{}'", s.socket().display()));
    error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    assert_eq!(std::fs::metadata(&run).unwrap().permissions().mode() & 0o777, 0o755);
    std::fs::remove_dir(&run).unwrap();
    let target = s.0.join("elsewhere");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, &run).unwrap();
    error(s.cmd().args(["server", "status"]).output().unwrap(), "managed_config");
    assert!(!target.join("c.sock").exists());
}

#[test]
fn scratch_build_has_no_production_fallback_and_cannot_install_upstream() {
    let s = Scratch::new();
    error(s.cmd().env_remove("NEARSHELL_HC_TEST_ROOT").args(["server", "status"]).output().unwrap(), "managed_config");
    // Offline identity bypasses even intentionally hostile addressing overrides.
    let build = value(
        s.cmd()
            .env("HC_SOCKET", "ignored")
            .env("HC_SESSION", "ignored")
            .env("NEARSHELL_CORE_COMMIT", "runtime-must-not-change-identity")
            .args(["build-info", "--json"])
            .output()
            .unwrap(),
    );
    assert_ne!(build["commit"], "runtime-must-not-change-identity");
    assert!(build["config"].as_str().unwrap().ends_with(":scratch-test"));
    s.config(&format!("socket = '{}'", s.socket().display()));
    error(s.cmd().args(["update"]).output().unwrap(), "managed_distribution");
    let paths = herdr_core::paths::Paths {
        socket: s.socket(),
        holder_socket: s.0.join("h.sock"),
        state: s.0.join("state"),
        config: s.0.join("config"),
    };
    let result = herdr_core::machines::install(
        &paths,
        herdr_core::machines::Reach { target: "example.invalid", via: false },
        None,
    );
    assert_eq!(result.unwrap_err().kind, "managed_distribution");
}
