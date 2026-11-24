//! A managed shutdown is complete only after an accepted reply, stream EOF,
//! and release of the pinned service lock. Never resend the lifecycle request.
use crate::{client, paths::Paths, platform, rpc::RpcError};
use serde_json::Value;
use std::fs::File;
use std::io::{self, Read};
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(30);

fn unconfirmed() -> RpcError {
    RpcError::new(
        "managed_shutdown_unconfirmed",
        "shutdown acknowledgement was not confirmed",
        "inspect the service state before deciding the next owner operation",
    )
}
fn incomplete() -> RpcError {
    RpcError::new(
        "managed_shutdown_incomplete",
        "shutdown was acknowledged but the service lock was not released",
        "retain the managed files and inspect the service state; do not repeat shutdown blindly",
    )
}
fn transport_failed() -> RpcError {
    RpcError::new(
        "managed_shutdown_transport",
        "shutdown was acknowledged and the service lock was released, but the transport did not close cleanly",
        "service departure was observed; do not resend shutdown",
    )
}

/// Permission errors remain permission errors; a lost/malformed reply is an
/// unknown effect, not proof of a stopped service or grounds to retry it.
pub(super) fn request(
    paths: &Paths,
    stream: &platform::Stream,
    method: &str,
    params: Value,
) -> Result<Value, RpcError> {
    let completion = Completion::prepare(paths, stream)?;
    let result = client::call(stream, method, params)
        .map_err(|error| if error.kind == "unavailable" { unconfirmed() } else { error })?;
    if result["stopped"] != true {
        return Err(unconfirmed());
    }
    completion.wait(stream, TIMEOUT)?;
    Ok(result)
}

/// No reachable listener is not enough: a starting/departing service may still
/// own core.lock. No request was acknowledged in this branch, so report unknown.
pub(super) fn require_absent(paths: &Paths) -> Result<(), RpcError> {
    match File::options().write(true).open(paths.state.join("core.lock")) {
        Ok(service) if platform::try_lock(&service) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        _ => Err(unconfirmed()),
    }
}

struct Completion {
    service: File,
}
impl Completion {
    fn prepare(paths: &Paths, stream: &platform::Stream) -> Result<Self, RpcError> {
        // Configure BEFORE sending stop/restart. On Darwin, setsockopt after
        // peer close can return EINVAL even though read would return clean EOF.
        stream.set_read_timeout(Some(TIMEOUT)).map_err(|_| unconfirmed())?;
        // Pin the existing inode before the operation. Missing/replaced state
        // must not be interpreted as a new, unlocked service lock.
        let service = File::options().write(true).open(paths.state.join("core.lock")).map_err(|_| unconfirmed())?;
        Ok(Self { service })
    }

    fn wait(self, stream: &platform::Stream, timeout: Duration) -> Result<(), RpcError> {
        let deadline = Instant::now() + timeout;
        // No further bytes are part of the shutdown protocol. One read gives
        // EOF/error without allocating a buffer that a broken peer can grow.
        let transport = (&*stream).read(&mut [0]);
        let departed = loop {
            if platform::try_lock(&self.service) {
                break true;
            }
            if Instant::now() >= deadline {
                break false;
            }
            // This observes completion; it does not resend a stop or restart.
            std::thread::sleep(Duration::from_millis(5));
        };
        completion_result(transport, departed)
        // Drop releases the service lease before a subsequent owner start.
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        platform::unlock(&self.service);
    }
}

fn completion_result(transport: io::Result<usize>, departed: bool) -> Result<(), RpcError> {
    if !departed {
        return Err(incomplete());
    }
    match transport {
        Ok(0) => Ok(()),
        _ => Err(transport_failed()),
    }
}

#[cfg(test)]
mod tests;
