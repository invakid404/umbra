//! Run isolation for `umbra run` in launchd's session (#193).
//!
//! A run started in launchd's session (SID 1) is moved into a fresh session,
//! so no exit inside the run can be counted against a group in launchd's
//! session (the orphaned-process-group SIGHUP of #186/#193). In every other
//! session, including every terminal session, this crate does nothing.
//!
//! Topology (README.md): the process the caller started stays where it is as
//! the **relay**; a **guardian** leads the new session S and runs the
//! **supervisor** (the same `umbra` binary and arguments) inside it. Cleanup of
//! S follows the #193 arc's T1 protocol (an external arc record, graph audit
//! `dg-56gcxqyw`, not in this repository); README.md carries its XNU citations.
//!
//! This is the only umbra crate in the CLI's tree that permits `unsafe`; `umbra-cli`
//! keeps `#![forbid(unsafe_code)]` and calls [`guardian_entry`] and
//! [`isolate_run`].

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]

pub mod frame;
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

/// Set in the supervisor's environment by the guardian: the run's session id.
/// Providers inherit it; `umbra-platform-macos` adds `--kill-on-error` to
/// debugserver only when it equals the provider's own `getsid(0)`.
pub const ENV_RUN_ISOLATED: &str = "UMBRA_RUN_ISOLATED";

/// Call first in `main`. In a guardian (the relay spawned this process with
/// [`ENV_GUARDIAN`]), runs the guardian and never returns. Otherwise returns.
pub fn guardian_entry() {
    #[cfg(target_os = "macos")]
    if let Some(spec) = std::env::var_os(ENV_GUARDIAN) {
        guardian::run(&spec);
    }
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
