//! The kernel-refusal contract, driven end to end: a real `Overlay` over real
//! `LocalStorage`, inside a real `Supervisor`, through
//! `Supervisor::with_namespace`.
//!
//! `umbra-supervisor/src/events.rs` has a namespace double (`Journaling`) whose
//! job is the supervisor's half of that contract: that the exit path sends the
//! *observed* fact as the abort reason, propagates whatever verdict it gets, and
//! fails closed on a refusal. It deliberately does not model `Overlay::abort`'s
//! own rule. This file is where that rule is pinned, against the object that
//! owns it, so the two copies of it that drifted in #53's and #69's reviews
//! become one copy with a compiler behind it
//! ([#57](https://github.com/invakid404/umbra/issues/57)).
//!
//! **Deleting this file un-pins `Overlay::abort`'s rule.** Nothing else in the
//! tree wires the two crates together, and the double no longer restates the
//! rule to fall back on.
//!
//! Scope: this is an end-to-end *transaction-verdict* test, not an end-to-end
//! *run* test. The journal is in-memory and cannot fail, so nothing here says
//! anything about durability ordering — see `support/journal.rs`.
//!
//! Only write-mode `Open` and `Fchownat` are reachable end to end today, which
//! is why every test below is driven by one of the two: `Overlay::resolve`
//! refuses `Chmod`, `Write` and `Link` outright, answers `Emulate` for
//! `Symlink`, `Mkdir` and `Unlink` — which the supervisor's entry path rejects
//! as an unsupported capability before any exit can be driven — and `Rename`
//! resolves to a two-path `Rewrite` the supervisor's single-path rewrite surface
//! refuses.

mod support;

use std::{fs, os::unix::fs::PermissionsExt};

use support::Harness;
use umbra_core::{
    BytePath, ChownFlags, DirRef, Errno, ErrorKind, FsOp, OpenFlags, ResumeCommand, ResumeMode,
};
use umbra_supervisor::RunLifecycle;

const EPERM: Errno = Errno(1);
const ENOSPC: Errno = Errno(28);

fn bytes(value: &[u8]) -> BytePath {
    BytePath::new(value.to_vec()).unwrap()
}

/// A creating write `open`, the one `Materialise` operand whose prepare records
/// an undo on `Pending.rollback`.
fn create_open(path: &[u8]) -> FsOp {
    FsOp::Open {
        dir: DirRef::Cwd,
        path: bytes(path),
        flags: OpenFlags {
            write: true,
            create: true,
            ..OpenFlags::default()
        },
        mode: 0o640,
    }
}

/// Both IDs are `Some` deliberately: `Overlay::resolve` refuses the
/// unchanged-ID form on a base object, because copy-up cannot preserve the
/// ownership the sentinel promises to leave alone.
fn chown(path: &[u8]) -> FsOp {
    FsOp::Fchownat {
        dir: DirRef::Cwd,
        path: bytes(path),
        uid: Some(0),
        gid: Some(0),
        flags: ChownFlags { follow: true },
    }
}

// The load-bearing case: the one reachable operand that is mutating,
// metadata-shaped and rollback-*empty*, so it isolates `Overlay::abort`'s
// corroboration gate from the undo that runs after it. `Fchownat`'s prepare is a
// `copy_up` and nothing else, and `copy_up` deliberately records no rollback
// entry, so a reconciled abort here must not over-undo the copy.
#[test]
fn a_kernel_refused_chown_reconciles_through_a_real_overlay_and_resumes_the_tracee() {
    let mut h = Harness::new(&[(b"file", b"base bytes")]);
    let thread = h.thread();
    let mark = h.prepared_entry(chown(b"file"), "file");

    h.exit(EPERM).expect("a corroborated refusal reconciles");

    // The run continues: no poison latch and no recovery lifecycle.
    assert!(!h.supervisor.is_poisoned());
    assert_eq!(h.supervisor.state().lifecycle, RunLifecycle::Running);
    // The rewritten syscall already ran and its return register already carries
    // the kernel's errno, so the exit resumes it and installs nothing. A
    // `set_registers` or an `emulate` here would mean the supervisor had
    // synthesised a verdict of its own.
    assert_eq!(h.order_since(mark), vec!["resume"]);
    assert!(h.set_register_threads_since(mark).is_empty());
    assert_eq!(
        h.resumes_since(mark),
        vec![ResumeCommand {
            thread,
            mode: ResumeMode::Syscall,
            signal: None,
        }]
    );
    // The operation slot was released, asserted through the public door: a
    // still-held slot is exactly what the next entry is rejected for, with
    // "entry received while an operation is still awaiting its exit".
    assert!(
        h.entry(chown(b"file")).is_ok(),
        "the slot must have been released, or this entry is rejected"
    );
    // `copy_up` records nothing on `Pending.rollback`, so the reconciled abort
    // had nothing to undo and must have left the copy standing.
    let copied = h.shadow_root.join("file");
    assert!(
        copied.exists(),
        "a reconciled abort must not over-undo a copy-up"
    );
    assert_eq!(fs::read(&copied).unwrap(), b"base bytes");
}

// The corroboration gate itself, reached the only way a driven exit can reach
// it: the namespace recorded a different kernel verdict from the one the exit
// path claims. `syscall_exit` observes and then aborts with the same errno, so
// without the skew the gate can never fail from here.
#[test]
fn an_uncorroborated_abort_claim_poisons_the_run_through_a_real_overlay() {
    // The namespace records EPERM; the supervisor observes and claims ENOSPC.
    let mut h = Harness::with_skewed_outcome(&[], EPERM);
    let mark = h.prepared_entry(create_open(b"fresh"), "fresh");

    let failure = h.exit(ENOSPC).unwrap_err();

    assert_eq!(failure.kind, ErrorKind::InvalidState);
    // The context, not the operation, is what distinguishes this from the other
    // `InvalidState`s the overlay returns ("result already observed", "unknown
    // operation"): `umbra-overlay`'s engine stamps every one of its own errors
    // with the operation `"overlay"`. The `unreconcilable()` helper in
    // `events.rs` spells `"overlay.abort"` instead, which is the double's own
    // string and not the real one — one more reason the rule belongs here.
    assert_eq!(failure.operation, "overlay");
    assert_eq!(failure.context, "aborted effects require reconciliation");
    assert!(h.supervisor.is_poisoned());
    assert_eq!(
        h.supervisor.state().lifecycle,
        RunLifecycle::RecoveryRequired
    );
    // Not "the resume log is empty": the entry already resumed once. The exit
    // added nothing.
    assert!(
        h.order_since(mark).is_empty(),
        "a run that could not reconcile must resume nothing"
    );
    // The gate returns before the rollback loop, so the prepare-time creation is
    // deliberately left standing for recovery to find.
    assert!(
        h.shadow_root.join("fresh").exists(),
        "an uncorroborated abort refuses before it undoes anything"
    );
}

// The mode the `Journaling` double cannot model at all: an undo the storage
// backend refuses. Its error is the backend's own, not the engine's, which is
// the divergence the double used to describe in prose and this test replaces
// with a compiler-checked fact.
#[test]
fn a_rollback_the_backend_refuses_poisons_the_run_with_the_backends_own_kind() {
    let mut h = Harness::new(&[]);
    let mark = h.prepared_entry(create_open(b"fresh"), "fresh");

    // r-x: the directory stays searchable, so the object is still reachable and
    // `stat`-able; only the `unlink` is refused.
    let restore = fs::metadata(&h.shadow_root).unwrap().permissions();
    fs::set_permissions(&h.shadow_root, fs::Permissions::from_mode(0o500)).unwrap();
    let aborted = h.exit(ENOSPC);
    // Restored before the result is examined, not after: an unrestored `0o500`
    // would defeat `TempDir::drop` and leak the directory on every path out of
    // this test, including the early return below.
    fs::set_permissions(&h.shadow_root, restore).unwrap();
    // Probed rather than unwrapped, and probed rather than tested with
    // `geteuid`: under uid 0 or `CAP_DAC_OVERRIDE` the mode does not stop
    // `remove_file`, so the rollback succeeds and there is no refusal to assert
    // against. Asking the result covers every capability that bypasses the mode,
    // which a uid check would miss, and needs no new dependency.
    //
    // Note for whoever reads a CI log looking for this: libtest captures a
    // passing test's stderr unless `--nocapture` is passed, so the line below is
    // a breadcrumb for an operator who goes looking, not an alarm that raises
    // itself. Both of this repo's CI environments are non-elevated, so the
    // branch is not taken there today.
    let Err(failure) = aborted else {
        eprintln!(
            "SKIPPED a_rollback_the_backend_refuses_poisons_the_run_with_the_backends_own_kind: \
             this environment bypasses the directory mode (effective uid 0 or CAP_DAC_OVERRIDE), \
             so the storage backend cannot be made to refuse the rollback and the error kind it \
             reports is left unpinned here"
        );
        return;
    };

    // `LocalStorage::io_error` maps `PermissionDenied` to `Denied`; `Io` is its
    // `_ =>` fallback, which is what `umbra-overlay`'s injected-failure siblings
    // assert because they inject `Io` rather than provoking a real `EACCES`.
    assert_eq!(failure.kind, ErrorKind::Denied);
    assert_ne!(
        failure.kind,
        ErrorKind::InvalidState,
        "a failed rollback reports the backend's error, not the engine's"
    );
    // Which storage call refused, so this cannot pass for some other reason:
    // the journal is in-memory and cannot fail, so the rollback's `unlink` is
    // the only call in the exit path that can produce this at all.
    assert_eq!(failure.operation, "unlink");
    assert!(h.supervisor.is_poisoned());
    assert_eq!(
        h.supervisor.state().lifecycle,
        RunLifecycle::RecoveryRequired
    );
    assert!(
        h.order_since(mark).is_empty(),
        "a run that could not reconcile must resume nothing"
    );
    assert!(
        h.shadow_root.join("fresh").exists(),
        "the object the rollback could not remove is still standing, which is \
         why the session had to poison"
    );
}

// The last property the double structurally cannot make: its `undo_refused` was
// a session constant, while the real verdict is decided per transaction. Two
// transactions on one session, with different verdicts, in order.
#[test]
fn two_transactions_in_one_session_are_reconciled_independently() {
    let mut h = Harness::new(&[]);
    let thread = h.thread();

    // T1: the parent is absent, so `prepare` materialises a directory over
    // nothing and records it *and* the file, in walk order.
    let mark = h.prepared_entry(create_open(b"newdir/fresh"), "newdir/fresh");
    h.exit(ENOSPC).expect("a corroborated refusal reconciles");

    assert!(!h.supervisor.is_poisoned());
    assert_eq!(h.supervisor.state().lifecycle, RunLifecycle::Running);
    assert_eq!(h.order_since(mark), vec!["resume"]);
    assert!(h.set_register_threads_since(mark).is_empty());
    assert_eq!(
        h.resumes_since(mark),
        vec![ResumeCommand {
            thread,
            mode: ResumeMode::Syscall,
            signal: None,
        }]
    );
    assert!(!h.shadow_root.join("newdir/fresh").exists());
    // The ancestor is the assertion that earns this half: a rollback that
    // unlinked only the file would leave a phantom directory the tracee was told
    // does not exist.
    assert!(
        !h.shadow_root.join("newdir").exists(),
        "the reverse walk must remove the materialised ancestor too"
    );

    // T2: same supervisor, same thread, a verdict of its own. T1's rollback list
    // must not leak into it, and a reconciled T1 must have left the session
    // usable enough to run it.
    let mark = h.prepared_entry(create_open(b"second"), "second");
    let restore = fs::metadata(&h.shadow_root).unwrap().permissions();
    fs::set_permissions(&h.shadow_root, fs::Permissions::from_mode(0o500)).unwrap();
    let aborted = h.exit(EPERM);
    // Restored before the result is examined, and probed rather than unwrapped,
    // for the same two reasons as the sibling above.
    fs::set_permissions(&h.shadow_root, restore).unwrap();
    let Err(failure) = aborted else {
        eprintln!(
            "SKIPPED two_transactions_in_one_session_are_reconciled_independently (T2): \
             this environment bypasses the directory mode (effective uid 0 or CAP_DAC_OVERRIDE), \
             so the storage backend cannot be made to refuse the rollback and T2's poisoning half \
             is left unpinned here"
        );
        return;
    };

    assert_eq!(failure.kind, ErrorKind::Denied);
    assert_ne!(
        failure.kind,
        ErrorKind::InvalidState,
        "a failed rollback reports the backend's error, not the engine's"
    );
    assert!(h.supervisor.is_poisoned());
    assert_eq!(
        h.supervisor.state().lifecycle,
        RunLifecycle::RecoveryRequired
    );
    assert!(
        h.order_since(mark).is_empty(),
        "a run that could not reconcile must resume nothing"
    );
    assert!(
        h.shadow_root.join("second").exists(),
        "the object the rollback could not remove is still standing"
    );
}
