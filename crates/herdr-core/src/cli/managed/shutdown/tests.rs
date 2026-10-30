use super::*;
use serde_json::json;
use std::io::{BufRead, Write};

struct Fixture {
    root: std::path::PathBuf,
    paths: Paths,
    service: Option<File>,
}
impl Fixture {
    fn new() -> Self {
        let root = std::path::PathBuf::from(format!("/var/tmp/nc-stop-{}", platform::random_hex()));
        std::fs::create_dir(&root).unwrap();
        let paths = Paths {
            state: root.clone(),
            config: root.join("config.toml"),
            socket: root.join("c.sock"),
            holder_socket: root.join("h.sock"),
        };
        let service = File::options().create_new(true).write(true).open(root.join("core.lock")).unwrap();
        assert!(platform::try_lock(&service));
        Self { root, paths, service: Some(service) }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.service.take();
        std::fs::remove_dir_all(&self.root).unwrap();
    }
}

// This is the original red regression, now using preparation before the RPC.
// The reply is accepted, then joining forces peer close BEFORE completion.
#[test]
fn acknowledged_stop_with_already_closed_peer_does_not_deny_owner() {
    let mut f = Fixture::new();
    let (stream, mut peer) = platform::Stream::pair().unwrap();
    let completion = Completion::prepare(&f.paths, &stream).unwrap();
    let service = f.service.take().unwrap();
    let server = std::thread::spawn(move || {
        let mut request = String::new();
        std::io::BufReader::new(&peer).read_line(&mut request).unwrap();
        let request: Value = serde_json::from_str(&request).unwrap();
        assert_eq!(request["method"], "server.stop");
        peer.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"stopped\":true}}\n").unwrap();
        drop(peer);
        drop(service);
    });
    assert_eq!(client::call(&stream, "server.stop", json!({})).unwrap()["stopped"], true);
    server.join().unwrap();
    completion.wait(&stream, TIMEOUT).expect("completed stop must not become an authorization error");
}

#[test]
fn eof_is_not_a_service_departure_barrier() {
    let f = Fixture::new();
    let (stream, peer) = platform::Stream::pair().unwrap();
    let completion = Completion::prepare(&f.paths, &stream).unwrap();
    drop(peer);
    let error = completion.wait(&stream, Duration::ZERO).unwrap_err();
    assert_eq!(error.kind, "managed_shutdown_incomplete");
    assert!(!platform::try_lock(&File::options().write(true).open(f.root.join("core.lock")).unwrap()));
}

#[test]
fn replacing_the_lock_path_cannot_forge_departure() {
    let f = Fixture::new();
    let (stream, peer) = platform::Stream::pair().unwrap();
    let completion = Completion::prepare(&f.paths, &stream).unwrap();
    std::fs::rename(f.root.join("core.lock"), f.root.join("old.lock")).unwrap();
    File::create(f.root.join("core.lock")).unwrap();
    drop(peer);
    assert_eq!(completion.wait(&stream, Duration::ZERO).unwrap_err().kind, "managed_shutdown_incomplete");
}

#[test]
fn completion_waits_for_the_pinned_service_lock_and_releases_its_lease() {
    let mut f = Fixture::new();
    let (stream, peer) = platform::Stream::pair().unwrap();
    let completion = Completion::prepare(&f.paths, &stream).unwrap();
    drop(peer);
    let (send, recv) = std::sync::mpsc::channel();
    let waiting = std::thread::spawn(move || send.send(completion.wait(&stream, TIMEOUT)).unwrap());
    assert!(recv.recv_timeout(Duration::from_millis(25)).is_err());
    drop(f.service.take());
    recv.recv_timeout(Duration::from_secs(2)).unwrap().unwrap();
    waiting.join().unwrap();
    assert!(platform::try_lock(&File::options().write(true).open(f.root.join("core.lock")).unwrap()));
}

#[test]
fn transport_failure_after_ack_distinguishes_departed_from_still_running() {
    for kind in [io::ErrorKind::ConnectionReset, io::ErrorKind::TimedOut, io::ErrorKind::WouldBlock] {
        assert_eq!(completion_result(Err(kind.into()), true).unwrap_err().kind, "managed_shutdown_transport");
        assert_eq!(completion_result(Err(kind.into()), false).unwrap_err().kind, "managed_shutdown_incomplete");
    }
    assert_eq!(completion_result(Ok(1), true).unwrap_err().kind, "managed_shutdown_transport");
}

#[test]
fn permission_rejection_and_unconfirmed_reply_are_not_completed_or_retried() {
    for (reply, expected) in [
        (json!({"error":{"code":-32000,"message":"denied","data":{"kind":"managed_by_owner"}}}), "managed_by_owner"),
        (json!({"result":{"stopped":false}}), "managed_shutdown_unconfirmed"),
        (json!(null), "managed_shutdown_unconfirmed"),
    ] {
        let f = Fixture::new();
        let (stream, mut peer) = platform::Stream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut line = String::new();
            std::io::BufReader::new(&peer).read_line(&mut line).unwrap();
            if !reply.is_null() {
                writeln!(peer, "{reply}").unwrap();
            }
            // Let the client observe EOF, but keep the read half to count requests.
            peer.shutdown(std::net::Shutdown::Write).unwrap();
            let mut extra = Vec::new();
            peer.read_to_end(&mut extra).unwrap();
            assert!(extra.is_empty(), "a rejected or unconfirmed lifecycle request was resent");
        });
        assert_eq!(request(&f.paths, &stream, "server.stop", json!({})).unwrap_err().kind, expected);
        drop(stream);
        server.join().unwrap();
    }
}

#[test]
fn an_unreachable_listener_does_not_prove_service_departure() {
    let mut f = Fixture::new();
    assert_eq!(require_absent(&f.paths).unwrap_err().kind, "managed_shutdown_unconfirmed");
    drop(f.service.take());
    require_absent(&f.paths).unwrap();
    std::fs::remove_file(f.root.join("core.lock")).unwrap();
    require_absent(&f.paths).unwrap();
    assert!(!f.root.join("core.lock").exists(), "observing absence must not create a service lock");
}
