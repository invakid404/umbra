//! Managed process custody and legacy launchd-session isolation.
//!
//! The macOS `managed` module supplies the CLI and registered executable
//! callers with a relay, guardian, worker and cleanup owner. Terminal callers
//! keep their controlling session; launchd callers get a fresh guardian session.
//!
//! The legacy [`isolate_run`] entry moves a run started in launchd's session
//! (SID 1) into a fresh session,
//! so no exit inside the run can be counted against a group in launchd's
//! session (the orphaned-process-group SIGHUP of #186/#193). That legacy entry
//! does nothing in other sessions.
//!
//! Topology (README.md): the process the caller started stays where it is as
//! the **relay**; a **guardian** leads the new session S and runs the
//! **supervisor** (the same `umbra` binary and arguments) inside it. Cleanup of
//! S follows the #193 arc's T1 protocol (an external arc record, graph audit
//! `dg-56gcxqyw`, not in this repository); README.md carries its XNU citations.
//!
//! This crate permits `unsafe` for native process and descriptor ownership.
//! `umbra-cli` keeps `#![forbid(unsafe_code)]` and enters managed roles through
//! the supervisor's safe registration API. The legacy [`guardian_entry`] and
//! [`isolate_run`] remain available separately.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod frame;
#[cfg(target_os = "macos")]
pub mod managed;
pub mod record;
pub mod sweep;

#[cfg(target_os = "macos")]
pub mod census;
#[cfg(target_os = "macos")]
mod guardian;
#[cfg(target_os = "macos")]
mod relay;
#[cfg(target_os = "macos")]
pub mod spawn;

/// Set in the guardian's environment by the relay: the guardian's three
/// control descriptors (`up,down,life`). Never reaches the supervisor.
pub const ENV_GUARDIAN: &str = "UMBRA_ISOLATION_GUARDIAN";

/// Call first in `main`. In a guardian (the relay spawned this process with
/// [`ENV_GUARDIAN`]), runs the guardian and never returns. Otherwise returns.
pub fn guardian_entry() {
    #[cfg(target_os = "macos")]
    if let Some(spec) = std::env::var_os(ENV_GUARDIAN) {
        guardian::run(&spec);
    }
}

/// Apply the legacy end-of-run handling only when this process owns the session.
/// Managed parents must collect their actual children before calling this.
#[cfg(target_os = "macos")]
pub fn finish_managed_session() -> Option<sweep::Outcome> {
    guardian::finish_managed_session()
}

/// Mirror an actual worker wait status after the caller has finished cleanup.
/// This terminates the calling process; external library callers must not use it.
#[cfg(target_os = "macos")]
pub fn mirror_worker_status(status: i32) -> ! {
    relay::mirror(status)
}

/// Whether this process is in launchd's session (SID 1).
pub fn in_launchd_session() -> bool {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: getsid takes no memory arguments and cannot fail for pid 0.
        unsafe { libc::getsid(0) == 1 }
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Test seam: run the relay against an explicit guardian executable and argv
/// and an explicit record directory, regardless of session. Never returns.
/// Used only by this crate's process-level tests (a scripted guardian).
#[cfg(target_os = "macos")]
#[doc(hidden)]
pub fn __relay_for_tests(
    exe: std::path::PathBuf,
    args: Vec<std::ffi::OsString>,
    dir: std::path::PathBuf,
) -> ! {
    relay::run_with(exe, args, dir)
}

/// Call before starting a supervised run. In launchd's session, becomes the
/// relay and never returns: the run executes in a guardian-led fresh session
/// and this process exits with its reported status. Anywhere else, returns
/// immediately and changes nothing.
pub fn isolate_run() {
    #[cfg(target_os = "macos")]
    if in_launchd_session() {
        relay::run();
    }
}

#[cfg(all(feature = "shutdown-witness", target_os = "macos"))]
#[path = "../../../experiments/fixtures/umbra-fix-299b-performance/observation.rs"]
#[allow(dead_code)] // This role uses record_as; provider uses record.
mod performance_observation;
