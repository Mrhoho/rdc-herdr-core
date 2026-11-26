//! Why ssh refused a dial, read from what it printed. Only what OpenSSH
//! prints word for word is read; anything else is left as it was (the
//! caller reports it as unreachable, with ssh's last line).

use super::Reach;
use crate::rpc::RpcError;

/// ssh refused us a login only a person can give (code, password, key).
pub fn wants_login(err: &str) -> bool {
    // Too many keys offered (sshd's MaxAuthTries) fails auth the same way; retrying fast gets banned.
    err.contains("Too many authentication failures")
        || err.contains("Permission denied")
            && ["publickey", "keyboard-interactive", "password"].iter().any(|k| err.contains(k))
}

/// ssh refused the host's key: `host_key_unknown` when none is known for it
/// here (and BatchMode leaves nobody to accept one), `host_key_changed` when
/// it differs from the one known. A revoked key, a DNS warning or any other
/// warning banner is neither.
pub fn host_key(err: &str) -> Option<&'static str> {
    if !err.contains("Host key verification failed.") {
        return None;
    }
    if err.contains("WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!") {
        return Some("host_key_changed");
    }
    // Unknown prints nothing else (BatchMode), or one line when checking is strict.
    let unknown = err.lines().map(str::trim).filter(|l| !l.is_empty()).all(|l| {
        l == "Host key verification failed."
            || l.starts_with("No ")
                && l.contains(" host key is known for ")
                && l.ends_with("you have requested strict checking.")
    });
    unknown.then_some("host_key_unknown")
}

/// ssh refused the host's key (`host_key`): only a person can settle it.
pub fn host_key_refused(to: Reach, kind: &str) -> RpcError {
    let target = to.target;
    match kind {
        "host_key_unknown" => RpcError::new(
            kind,
            format!("{target}'s host key is not known here"),
            format!("in a terminal, `{}` shows it to check and accept", to.add()),
        ),
        _ => RpcError::new(
            kind,
            format!("{target}'s host key is not the one known here: ssh refuses it"),
            "check why with whoever runs it; only you can fix known_hosts",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What OpenSSH prints (sshconnect.c), as captured from OpenSSH 9.x.
    const UNKNOWN_BATCH: &str = "Host key verification failed.\r\n";
    const UNKNOWN_STRICT: &str = "No ED25519 host key is known for rohan.lan and you have requested strict checking.\r\n\
        Host key verification failed.\r\n";
    const CHANGED: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
        @    WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED!     @\r\n\
        @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
        IT IS POSSIBLE THAT SOMEONE IS DOING SOMETHING NASTY!\r\n\
        Someone could be eavesdropping on you right now (man-in-the-middle attack)!\r\n\
        It is also possible that a host key has just been changed.\r\n\
        The fingerprint for the ED25519 key sent by the remote host is\r\n\
        SHA256:0123456789abcdefghijklmnopqrstuvwxyzABCDEFG.\r\n\
        Please contact your system administrator.\r\n\
        Add correct host key in /home/user/.ssh/known_hosts to get rid of this message.\r\n\
        Offending ED25519 key in /home/user/.ssh/known_hosts:3\r\n\
        Host key for rohan.lan has changed and you have requested strict checking.\r\n\
        Host key verification failed.\r\n";
    const REVOKED: &str = "@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
        @       WARNING: REVOKED HOST KEY DETECTED!               @\r\n\
        @@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@@\r\n\
        The ED25519 host key for rohan.lan is marked as revoked.\r\n\
        This could mean that a stolen key is being used to\r\n\
        impersonate this host.\r\n\
        Host key verification failed.\r\n";

    #[test]
    fn a_host_key_refusal_is_told_apart_from_what_ssh_printed() {
        assert_eq!(host_key(UNKNOWN_BATCH), Some("host_key_unknown"), "BatchMode: nobody to ask");
        assert_eq!(host_key(UNKNOWN_STRICT), Some("host_key_unknown"), "StrictHostKeyChecking=yes");
        assert_eq!(host_key(CHANGED), Some("host_key_changed"));
        assert_eq!(host_key(REVOKED), None, "revoked is neither: it stays unreachable");
        assert_eq!(host_key("ssh: connect to host rohan port 22: Connection refused\r\n"), None);
        assert_eq!(host_key("rohan: Permission denied (publickey).\r\n"), None);
        let unknown_and_more =
            "kex_exchange_identification: read: Connection reset by peer\r\nHost key verification failed.\r\n";
        assert_eq!(host_key(unknown_and_more), None, "anything unrecognised stays what it is");
    }

    #[test]
    fn a_login_refusal_is_one_a_person_can_give() {
        assert!(wants_login("rohan: Permission denied (publickey,keyboard-interactive).\r\n"));
        assert!(wants_login("Received disconnect from 10.0.0.2 port 22:2: Too many authentication failures\r\n"));
        assert!(!wants_login(UNKNOWN_BATCH));
        assert!(!wants_login("ssh: Could not resolve hostname rohan: Name or service not known\r\n"));
    }
}
