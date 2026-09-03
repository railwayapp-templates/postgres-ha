//! Stop-signal forwarding for the haproxy entrypoint.
//!
//! This binary is PID 1 in the image (`ENTRYPOINT ["/entrypoint"]`) and a
//! container stop delivers its signal to PID 1 only. A PID 1 with no handler
//! drops the signal, so until this module existed every `stop`/`restart` of
//! the edge waited out the whole grace period and ended in SIGKILL (measured
//! on the published image: 15s, exit 137) — haproxy never soft-stopped and
//! every client connection through it was cut mid-flight.
//!
//! The signal is relayed to haproxy raw so the runtime's choice keeps its
//! meaning: SIGUSR1 (the `STOPSIGNAL` inherited from the upstream image) is
//! haproxy's soft stop, SIGTERM/SIGINT its hard stop, SIGHUP its state dump.
//! The generated config bounds the soft stop with `hard-stop-after` so a drain
//! can never outlive the grace period.

use nix::libc;
use nix::sys::signal::{signal, SigHandler, Signal};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// haproxy's pid, written once before the handlers are installed.
static HAPROXY_PID: AtomicI32 = AtomicI32::new(0);
/// Set once a stop-class signal has been relayed, so the exit that follows is
/// reported as the requested stop it is rather than an unexpected death.
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn forward(sig: libc::c_int) {
    // Async-signal-safe: atomic ops and a raw kill only.
    if sig != libc::SIGHUP {
        STOP_REQUESTED.store(true, Ordering::Relaxed);
    }
    let pid = HAPROXY_PID.load(Ordering::Relaxed);
    if pid > 0 {
        unsafe {
            libc::kill(pid, sig);
        }
    }
}

/// Install the forwarders for the spawned haproxy process.
pub fn install_forwarding(haproxy_pid: u32) {
    HAPROXY_PID.store(haproxy_pid as i32, Ordering::Relaxed);
    for sig in [
        Signal::SIGUSR1,
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGHUP,
    ] {
        // SAFETY: the handler is async-signal-safe (atomic load/store + kill).
        unsafe {
            let _ = signal(sig, SigHandler::Handler(forward));
        }
    }
}

/// Whether a stop signal has been relayed to haproxy.
pub fn stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::Relaxed)
}

/// Exit code mirroring haproxy's own exit status: its code when it exited, the
/// conventional 128+N when a signal killed it.
pub fn exit_code_for(code: Option<i32>, signal: Option<i32>) -> i32 {
    match (code, signal) {
        (Some(code), _) => code,
        (None, Some(signal)) => 128 + signal,
        (None, None) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::exit_code_for;

    #[test]
    fn exit_code_mirrors_haproxy() {
        assert_eq!(exit_code_for(Some(0), None), 0);
        assert_eq!(exit_code_for(Some(1), None), 1);
        assert_eq!(exit_code_for(None, Some(9)), 137);
        assert_eq!(exit_code_for(None, None), 1);
    }
}
