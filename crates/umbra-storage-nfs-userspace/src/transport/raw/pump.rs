//! The event pump: one libnfs RPC context, one poll loop, one call registry.
//!
//! # Why the registry is global and keyed by an integer
//!
//! The audited spike handed C a pointer to a stack `CallState`
//! (`experiments/nfs-userspace-spike/src/raw4.rs:646–687`) and could return on a
//! pump deadline without cancelling, leaving libnfs able to write through that
//! pointer later. Here, the only `private_data` libnfs ever receives is a `u64`
//! call id cast to `*mut c_void`. A completion resolves that id against a
//! process-wide registry; an id that has been withdrawn resolves to nothing and
//! the completion is dropped. **C is never given the address of a Rust value**,
//! so the dangling-pointer shape is not reachable rather than merely avoided.
//!
//! The registry is global rather than per-pump because the callback receives
//! nothing but the id: making it global is what lets the pump be `Send` without
//! a thread-local that would break when a transport moves between threads.

use core::ffi::{c_char, c_int, c_void};
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use crate::error::TransportError;
use crate::pdu::{ownership, DisposalGeneration, Ownership, ServiceFailure};
use crate::transport::{ConnectionEpoch, ConnectionState, TransportResult};

use super::decode::{self, ReplyBudget};
use super::sys;

/// What a libnfs callback reported for one call.
pub(super) enum Completion {
    /// A COMPOUND reply, already decoded into owned Rust values.
    Reply(Box<TransportResult<decode::DecodedReply>>),
    /// The connect attempt succeeded.
    Connected,
    /// libnfs reported an error; the string is its own diagnostic.
    Failed(String),
    /// libnfs reported the call cancelled.
    Cancelled,
}

struct Entry {
    max_reply_bytes: usize,
    completion: Option<Completion>,
}

fn registry() -> &'static Mutex<BTreeMap<u64, Entry>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<u64, Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Call ids are process-unique, so an id identifies both the pump and the call
/// and a completion can never be delivered to the wrong transport.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<u64, Entry>> {
    registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Reserve a call id before anything is handed to libnfs.
fn register(max_reply_bytes: usize) -> u64 {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    lock().insert(
        id,
        Entry {
            max_reply_bytes,
            completion: None,
        },
    );
    id
}

/// Take a delivered completion, leaving the registration in place.
fn take(id: u64) -> Option<Completion> {
    lock()
        .get_mut(&id)
        .and_then(|entry| entry.completion.take())
}

/// Remove a registration. A completion delivered after this is discarded.
fn withdraw(id: u64) -> Option<Completion> {
    lock().remove(&id).and_then(|entry| entry.completion)
}

/// Deliver a completion, ignoring it if the call was already withdrawn.
fn deliver(id: u64, completion: Completion) {
    if let Some(entry) = lock().get_mut(&id) {
        entry.completion = Some(completion);
    }
}

/// The reply budget a call was registered with, or `None` if it is retired.
fn budget_for(id: u64) -> Option<usize> {
    lock().get(&id).map(|entry| entry.max_reply_bytes)
}

/// Where one dispatched call's PDU pointer lives.
///
/// This type exists to make the stale-pointer hazard unreachable.
/// `rpc_cancel_pdu` dereferences the pointer it is handed *before* checking
/// whether libnfs still owns it (`lib/pdu.c`: `rpc_find_pdu(rpc, pdu->xid)`),
/// so cancelling an already-completed PDU is undefined behaviour. The pointer
/// can only be obtained by [`Dispatch::take_for_cancel`], which consumes it: a
/// second cancellation, or a cancellation after a completion was observed,
/// gets `None` instead of a pointer.
pub(super) enum Dispatch {
    /// libnfs owns the PDU and it can still be cancelled.
    Queued(*mut sys::rpc_pdu),
    /// A completion was observed; libnfs has freed the PDU.
    Completed,
    /// The registration has been withdrawn.
    Withdrawn,
}

impl Dispatch {
    /// Take the PDU pointer for exactly one cancellation, if one is owed.
    ///
    /// Always leaves `self` in a state that yields `None` next time.
    pub(super) fn take_for_cancel(&mut self) -> Option<*mut sys::rpc_pdu> {
        let previous = core::mem::replace(self, Dispatch::Withdrawn);
        match previous {
            Dispatch::Queued(pdu) => Some(pdu),
            _ => None,
        }
    }

    /// Record that libnfs finished with this PDU.
    pub(super) fn mark_completed(&mut self) {
        *self = Dispatch::Completed;
    }

    /// Whether libnfs may still be holding this PDU.
    pub(super) fn is_queued(&self) -> bool {
        matches!(self, Dispatch::Queued(_))
    }
}

/// One in-flight call: its id, its PDU, and the arguments C is reading.
pub(super) struct CallSlot {
    /// Registry id, also the value behind the public `CallToken`.
    pub(super) id: u64,
    /// Disposal generation this call was queued in.
    ///
    /// **R1-009.** A connection-wide disposal frees every outstanding PDU at
    /// once, so "may this pointer still be cancelled" is a question about the
    /// context's history and not only about this call. See [`crate::pdu`].
    pub(super) queued_in: DisposalGeneration,
    /// PDU ownership state.
    pub(super) dispatch: Dispatch,
    /// Argument memory, owned until the call completes or is withdrawn.
    ///
    /// Dropping the slot drops the arena. That ordering is the whole point:
    /// libnfs references the WRITE payload from the PDU's iovector, so the
    /// arena must not be released while a PDU can still reach it.
    pub(super) arena: super::args::CallArena,
    /// Whether a fault plan asked for this call's reply to be discarded.
    pub(super) drop_reply: bool,
}

// SAFETY: the PDU pointer is libnfs's, recorded here and never dereferenced by
// Rust; it is handed back to libnfs exactly once through `Dispatch`. The arena
// is `Send` for the reason given at its definition. A slot is reachable only
// through `&mut` from the pump that owns it.
unsafe impl Send for CallSlot {}

/// The libnfs completion callback for a COMPOUND.
///
/// # Safety
///
/// Invoked by libnfs from inside `rpc_service`. `data` is a live
/// `COMPOUND4res` when `status` is `RPC_STATUS_SUCCESS` and a C string
/// otherwise. `private_data` is the `u64` call id this module put there.
unsafe extern "C" fn on_compound(
    _rpc: *mut sys::rpc_context,
    status: c_int,
    data: *mut c_void,
    private_data: *mut c_void,
) {
    let id = private_data as u64;
    // A retired id has no budget, which is also the proof that nothing about
    // this call is still owned. Returning here is the safe-by-construction path
    // for a late or duplicated completion.
    let Some(max_reply_bytes) = budget_for(id) else {
        return;
    };

    let completion = match status as u32 {
        sys::RPC_STATUS_SUCCESS => {
            let mut budget = ReplyBudget::new(max_reply_bytes);
            Completion::Reply(Box::new(decode::compound_reply(
                data.cast::<sys::COMPOUND4res>(),
                &mut budget,
            )))
        }
        sys::RPC_STATUS_CANCEL => Completion::Cancelled,
        _ => Completion::Failed(error_text(data)),
    };
    deliver(id, completion);
}

/// The libnfs completion callback for a connect attempt.
///
/// # Safety
///
/// Same contract as [`on_compound`]; `data` is only ever a C string here.
unsafe extern "C" fn on_connect(
    _rpc: *mut sys::rpc_context,
    status: c_int,
    data: *mut c_void,
    private_data: *mut c_void,
) {
    let id = private_data as u64;
    if budget_for(id).is_none() {
        return;
    }
    let completion = match status as u32 {
        sys::RPC_STATUS_SUCCESS => Completion::Connected,
        sys::RPC_STATUS_CANCEL => Completion::Cancelled,
        _ => Completion::Failed(error_text(data)),
    };
    deliver(id, completion);
}

/// Read a libnfs diagnostic string without retaining the pointer.
///
/// # Safety
///
/// `data` is either null or a NUL-terminated string owned by libnfs.
unsafe fn error_text(data: *mut c_void) -> String {
    if data.is_null() {
        return "libnfs reported an error with no detail".into();
    }
    CStr::from_ptr(data.cast::<c_char>())
        .to_string_lossy()
        .into_owned()
}

/// One libnfs RPC context and the single poll loop that drives it.
pub(super) struct EventPump {
    rpc: *mut sys::rpc_context,
    auth: *mut sys::AUTH,
    server: CString,
    port: u16,
    epoch: ConnectionEpoch,
    state: ConnectionState,
    /// How many times libnfs has disposed of every outstanding PDU (**R1-009**).
    disposals: DisposalGeneration,
}

// SAFETY: `rpc` and `auth` are owned exclusively by this value and are only
// ever touched through `&mut EventPump`; libnfs client contexts are not shared
// between threads by the library itself. Completions are routed through the
// mutex-guarded global registry rather than thread-local state, so moving a
// pump to another thread does not strand an in-flight call.
unsafe impl Send for EventPump {}

impl EventPump {
    /// Create a context and attach AUTH_SYS credentials.
    ///
    /// No connection is made here, so a construction failure cannot leave a
    /// half-open socket behind.
    pub(super) fn new(server: &str, port: u16) -> TransportResult<Self> {
        let server = CString::new(server).map_err(|_| {
            TransportError::Connect("server address contains an interior NUL byte".into())
        })?;

        // SAFETY: no arguments; returns an owned context or null.
        let rpc = unsafe { sys::rpc_init_context() };
        if rpc.is_null() {
            return Err(TransportError::Connect(
                "libnfs could not allocate an RPC context".into(),
            ));
        }

        // AUTH_SYS is the only authorised flavour. The host string is
        // informational; uid/gid come from the running process.
        let host = CString::new("umbra").expect("literal has no NUL");
        // SAFETY: `host` outlives the call; libnfs copies what it needs.
        let auth = unsafe {
            sys::libnfs_authunix_create(
                host.as_ptr(),
                current_uid(),
                current_gid(),
                0,
                core::ptr::null_mut(),
            )
        };
        if auth.is_null() {
            // SAFETY: `rpc` is a live context this function owns.
            unsafe { sys::rpc_destroy_context(rpc) };
            return Err(TransportError::Connect(
                "libnfs could not build AUTH_SYS credentials".into(),
            ));
        }
        // SAFETY: both pointers are live and owned here. libnfs takes ownership
        // of `auth`, which is why `Drop` does not free it separately.
        unsafe { sys::rpc_set_auth(rpc, auth) };

        Ok(Self {
            rpc,
            auth,
            server,
            port,
            epoch: ConnectionEpoch(0),
            state: ConnectionState::Idle,
            disposals: DisposalGeneration::START,
        })
    }

    pub(super) fn state(&self) -> ConnectionState {
        self.state
    }

    pub(super) fn epoch(&self) -> ConnectionEpoch {
        self.epoch
    }

    /// Record that libnfs has errored and freed every outstanding PDU.
    ///
    /// **R1-009.** Called on every path where `rpc_reconnect_requeue` can have
    /// run: a negative `rpc_service`, a lost file descriptor, a poll failure, and
    /// the deliberate `rpc_disconnect`. After this, no PDU pointer queued before
    /// the call may be handed to `rpc_cancel_pdu`.
    fn note_disposal(&mut self) {
        self.disposals = self.disposals.disposed();
    }

    /// Number of PDUs libnfs currently holds, used for backpressure.
    pub(super) fn queue_length(&mut self) -> u32 {
        // SAFETY: `self.rpc` is live for the lifetime of this value.
        let length = unsafe { sys::rpc_queue_length(self.rpc) };
        u32::try_from(length).unwrap_or(0)
    }

    /// The last libnfs diagnostic for this context.
    pub(super) fn error(&mut self) -> String {
        // SAFETY: `rpc_get_error` returns a NUL-terminated string owned by the
        // context; it is copied before it can be invalidated.
        unsafe {
            let text = sys::rpc_get_error(self.rpc);
            if text.is_null() {
                "libnfs reported no detail".into()
            } else {
                CStr::from_ptr(text).to_string_lossy().into_owned()
            }
        }
    }

    /// Connect, bounded by `deadline_millis`, and open a new generation.
    pub(super) fn connect(&mut self, deadline_millis: u64) -> TransportResult<ConnectionEpoch> {
        let id = register(0);

        // SAFETY: `server` outlives the call; `id` is an integer, not a
        // pointer into Rust memory.
        let queued = unsafe {
            sys::rpc_connect_async(
                self.rpc,
                self.server.as_ptr(),
                c_int::from(self.port),
                Some(on_connect),
                id as *mut c_void,
            )
        };
        if queued != 0 {
            withdraw(id);
            let detail = self.error();
            self.state = ConnectionState::Broken(self.epoch);
            return Err(TransportError::Connect(detail));
        }

        let outcome = self.service_until(id, deadline_millis, false);
        let completion = withdraw(id);

        match (outcome, completion) {
            (ServiceOutcome::Completed, Some(Completion::Connected)) => {
                self.epoch = ConnectionEpoch(self.epoch.0 + 1);
                self.state = ConnectionState::Connected(self.epoch);
                Ok(self.epoch)
            }
            (_, Some(Completion::Failed(detail))) => {
                self.state = ConnectionState::Broken(self.epoch);
                Err(TransportError::Connect(detail))
            }
            (ServiceOutcome::TimedOut, _) => {
                self.state = ConnectionState::Broken(self.epoch);
                Err(TransportError::Connect(format!(
                    "connect to {}:{} did not complete within {deadline_millis} ms",
                    self.server.to_string_lossy(),
                    self.port
                )))
            }
            _ => {
                let detail = self.error();
                self.state = ConnectionState::Broken(self.epoch);
                Err(TransportError::Connect(detail))
            }
        }
    }

    /// Drop the transport connection. Protocol state is not this pump's to
    /// revalidate; callers treat a new epoch as invalidating prior state.
    pub(super) fn disconnect(&mut self) {
        let reason = CString::new("umbra: transport reconnect").expect("literal has no NUL");
        // SAFETY: `self.rpc` is live; `reason` outlives the call.
        unsafe { sys::rpc_disconnect(self.rpc, reason.as_ptr()) };
        // rpc_disconnect errors and frees every outstanding PDU, exactly as a
        // socket failure does.
        self.note_disposal();
        self.state = ConnectionState::Broken(self.epoch);
    }

    /// Queue a COMPOUND and return the slot that owns it.
    ///
    /// The arena is moved into the returned slot, so from this point the
    /// argument memory's lifetime is the slot's lifetime.
    pub(super) fn dispatch(
        &mut self,
        mut arena: super::args::CallArena,
        max_reply_bytes: usize,
    ) -> TransportResult<CallSlot> {
        let id = register(max_reply_bytes);
        let args = arena.compound_ptr();
        let private = id as *mut c_void;

        // SAFETY: `arena` owns every buffer `args` points at and is moved into
        // the returned slot, so it outlives the PDU. The READ destination and
        // WRITE source are the same arena's buffers, which libnfs references
        // rather than copies.
        let pdu = unsafe {
            match arena.kind() {
                super::args::DispatchKind::Compound => {
                    sys::rpc_nfs4_compound_task(self.rpc, Some(on_compound), args, private)
                }
                super::args::DispatchKind::Read => {
                    let (buffer, length) = arena.io_buffer().expect("a READ arena has a buffer");
                    sys::rpc_nfs4_read_task(
                        self.rpc,
                        Some(on_compound),
                        buffer.cast::<c_void>(),
                        length,
                        args,
                        private,
                    )
                }
                super::args::DispatchKind::Write => {
                    let (buffer, length) = arena.io_buffer().expect("a WRITE arena has a buffer");
                    sys::rpc_nfs4_write_task(
                        self.rpc,
                        Some(on_compound),
                        buffer.cast::<c_void>(),
                        length,
                        args,
                        private,
                    )
                }
            }
        };

        if pdu.is_null() {
            withdraw(id);
            let detail = self.error();
            return Err(TransportError::Disconnected {
                epoch: self.epoch,
                detail,
            });
        }

        Ok(CallSlot {
            id,
            queued_in: self.disposals,
            dispatch: Dispatch::Queued(pdu),
            arena,
            drop_reply: false,
        })
    }

    /// Drive the poll loop until `id` completes or the deadline elapses.
    ///
    /// `discard` models a fault plan that dropped the reply: the completion is
    /// consumed and thrown away so the call goes on to reach its deadline with
    /// the request already on the wire.
    pub(super) fn service_until(
        &mut self,
        id: u64,
        deadline_millis: u64,
        discard: bool,
    ) -> ServiceOutcome {
        let start = Instant::now();

        loop {
            if has_completion(id) {
                if !discard {
                    return ServiceOutcome::Completed;
                }
                // Consume and drop it; the PDU is finished either way.
                let _ = take(id);
                return ServiceOutcome::Discarded;
            }

            let elapsed = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            if elapsed >= deadline_millis {
                return ServiceOutcome::TimedOut;
            }
            let remaining = (deadline_millis - elapsed).min(i32::MAX as u64) as c_int;

            // SAFETY: `self.rpc` is live for the lifetime of this value.
            let fd = unsafe { sys::rpc_get_fd(self.rpc) };
            if fd < 0 {
                // R2-005: a missing descriptor is observed *here*, not reported by
                // libnfs. Nothing in C has been called, so nothing has been freed.
                return self.stopped(ServiceFailure::Local, id, discard);
            }
            // SAFETY: as above.
            let events = unsafe { sys::rpc_which_events(self.rpc) };

            let mut descriptor = sys::pollfd {
                fd,
                events: events as core::ffi::c_short,
                revents: 0,
            };
            // SAFETY: exactly one descriptor is passed, and the timeout is
            // always bounded by the caller's deadline.
            let ready = unsafe { sys::poll(&mut descriptor, 1, remaining) };
            if ready < 0 {
                let errno = std::io::Error::last_os_error();
                if errno.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // R2-005: `poll` failing is this process's problem. libnfs was not
                // called, so every PDU is still C-owned and every argument arena
                // is still referenced by one.
                return self.stopped(ServiceFailure::Local, id, discard);
            }
            if ready == 0 {
                continue;
            }

            // SAFETY: `revents` came from the poll above for this context's fd.
            // Completion callbacks run inside this call.
            if unsafe { sys::rpc_service(self.rpc, c_int::from(descriptor.revents)) } < 0 {
                self.state = ConnectionState::Broken(self.epoch);
                // libnfs reported this one, so `rpc_reconnect_requeue` has already
                // errored and freed every outstanding PDU.
                return self.stopped(ServiceFailure::LibnfsReported, id, discard);
            }
        }
    }

    /// Settle a bounded service run that stopped without a reply for `id`.
    ///
    /// **R1-009.** Reconcile first, whatever stopped it: `rpc_reconnect_requeue`
    /// calls every outstanding completion on its way out, so a connection failure
    /// can carry a settled answer, and reporting `Disconnected` over it discards
    /// one. A completion that *is* waiting also means libnfs already freed that
    /// call's PDU, which [`ownership`] reads without any generation bookkeeping.
    ///
    /// **R2-005.** What happens next depends on who reported the failure, and the
    /// previous version got this wrong by treating every stop alike. A negative
    /// `rpc_service` is proven disposal, so the generation advances and every
    /// outstanding pointer becomes unreachable. A local `poll` error or a missing
    /// descriptor proves nothing: libnfs was never called, so every PDU is still
    /// C-owned and still referencing its argument arena. Advancing the generation
    /// there made `retire` withdraw the registration without cancelling, and
    /// `cancel` then dropped the slot and its arena while C still held a pointer
    /// into it — a use-after-free in the opposite direction from R1-009's.
    ///
    /// So a local failure is *made* proven instead of assumed: `rpc_disconnect`
    /// errors and frees every outstanding PDU exactly as the requeue path does,
    /// and only then is the disposal recorded. The alternative the review allows —
    /// cancelling while the PDU is live — is what an ordinary deadline already
    /// does; this path is aborting the connection, so disconnecting is both
    /// simpler and stronger.
    fn stopped(&mut self, failure: ServiceFailure, id: u64, discard: bool) -> ServiceOutcome {
        if let Some(settled) = self.take_settled(id, discard) {
            // A completion is already in hand. The connection is not touched: for
            // a local failure it may well still be usable, and for a reported one
            // the caller learns from the completion itself.
            if failure.disposed_pdus() {
                self.note_disposal();
            }
            return settled;
        }
        if failure.disposed_pdus() {
            self.note_disposal();
        } else {
            // Turn an unproven state into a proven one before anything is
            // released. `disconnect` frees every outstanding PDU and records the
            // disposal itself.
            self.disconnect();
        }
        ServiceOutcome::Disconnected
    }

    /// Take a delivered completion for `id`, if one is waiting.
    fn take_settled(&mut self, id: u64, discard: bool) -> Option<ServiceOutcome> {
        if !has_completion(id) {
            return None;
        }
        if discard {
            let _ = take(id);
            Some(ServiceOutcome::Discarded)
        } else {
            Some(ServiceOutcome::Completed)
        }
    }

    /// Withdraw a call from libnfs and report whether the pump is proven idle.
    ///
    /// Returns `true` when no callback for this call can run any more. That is
    /// the case when `rpc_cancel_pdu` removed the PDU (libnfs frees it without
    /// invoking the callback) or when a completion had already been observed.
    pub(super) fn retire(&mut self, slot: &mut CallSlot) -> bool {
        // R1-009: decide ownership *before* reaching for the pointer. A call that
        // has a completion waiting, or that was outstanding across a
        // connection-wide disposal, is one libnfs has already freed; asking for
        // its pointer at all would be the use-after-free.
        if slot.dispatch.is_queued()
            && ownership(slot.queued_in, self.disposals, has_completion(slot.id))
                == Ownership::Freed
        {
            // Make the pointer unreachable rather than merely unused.
            slot.dispatch.mark_completed();
            withdraw(slot.id);
            // libnfs is provably done with this call: no callback for it can run
            // again, which is exactly what `drained` reports.
            return true;
        }
        let drained = match slot.dispatch.take_for_cancel() {
            // No PDU is owed a cancellation: libnfs already finished with it.
            None => true,
            Some(pdu) => {
                // SAFETY: `Dispatch::Queued` is only ever set from a live PDU
                // pointer, and `take_for_cancel` consumes it, so libnfs still
                // owned this PDU and no second cancellation can occur. The
                // ownership check above has additionally ruled out both ways
                // libnfs can have freed it first (R1-009).
                let removed = unsafe { sys::rpc_cancel_pdu(self.rpc, pdu) };
                // 0 means the PDU was found and freed without a callback.
                // -ENOENT means it had already been serviced, in which case any
                // completion is already sitting in the registry.
                removed == 0 || has_completion(slot.id)
            }
        };
        withdraw(slot.id);
        drained
    }
}

impl Drop for EventPump {
    fn drop(&mut self) {
        // SAFETY: `self.rpc` is live and owned. Destroying the context frees any
        // PDU still queued; a callback fired during teardown resolves its id
        // against the registry, finds nothing, and returns.
        unsafe { sys::rpc_destroy_context(self.rpc) };
        let _ = self.auth; // owned by the context since `rpc_set_auth`.
    }
}

/// Outcome of one bounded pump run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServiceOutcome {
    /// A completion for the requested id is waiting.
    Completed,
    /// A completion arrived and was deliberately discarded.
    Discarded,
    /// The deadline elapsed first.
    TimedOut,
    /// The connection failed while pumping.
    Disconnected,
}

fn has_completion(id: u64) -> bool {
    lock()
        .get(&id)
        .map(|entry| entry.completion.is_some())
        .unwrap_or(false)
}

/// Take the completion recorded for a call.
pub(super) fn take_completion(id: u64) -> Option<Completion> {
    take(id)
}

fn current_uid() -> u32 {
    extern "C" {
        fn getuid() -> u32;
    }
    // SAFETY: `getuid` takes no arguments and cannot fail.
    unsafe { getuid() }
}

fn current_gid() -> u32 {
    extern "C" {
        fn getgid() -> u32;
    }
    // SAFETY: `getgid` takes no arguments and cannot fail.
    unsafe { getgid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pdu_pointer_can_only_be_taken_for_one_cancellation() {
        let mut dispatch = Dispatch::Queued(0x1234 as *mut sys::rpc_pdu);
        assert!(dispatch.take_for_cancel().is_some());
        // The second attempt cannot produce a pointer, which is what makes
        // cancelling an already-freed PDU unreachable.
        assert!(dispatch.take_for_cancel().is_none());
    }

    #[test]
    fn a_completed_dispatch_never_yields_a_pointer_to_cancel() {
        let mut dispatch = Dispatch::Queued(0x1234 as *mut sys::rpc_pdu);
        dispatch.mark_completed();
        assert!(dispatch.take_for_cancel().is_none());
    }

    /// **R1-009.** A slot queued before a connection-wide disposal is never
    /// cancelled, because libnfs has already freed its PDU.
    ///
    /// This is the pointer-level half of the rule; the decision itself is
    /// `crate::pdu::ownership`, which is tested in the default build. Together
    /// they cover the review's trace: queued PDU -> socket error -> callback
    /// records Failed -> libnfs frees the PDU -> `rpc_service` returns -1 ->
    /// (previously) the slot stayed `Queued` and retirement dereferenced the
    /// freed pointer.
    #[test]
    fn r1_009_a_slot_outstanding_across_a_disposal_is_not_cancellable() {
        let queued_in = DisposalGeneration::START;
        let after = queued_in.disposed();

        // The pointer is deliberately bogus: the point of the assertion is that
        // nothing ever reaches for it. If the rule regressed, this test would
        // hand `0xDEAD` to `rpc_cancel_pdu`.
        let mut slot_dispatch = Dispatch::Queued(0xDEAD as *mut sys::rpc_pdu);
        assert!(slot_dispatch.is_queued());
        assert_eq!(
            ownership(queued_in, after, false),
            Ownership::Freed,
            "a disposal after dispatch means libnfs freed this PDU"
        );

        // What `retire` does with that verdict: make the pointer unreachable.
        slot_dispatch.mark_completed();
        assert!(!slot_dispatch.is_queued());
        assert!(
            slot_dispatch.take_for_cancel().is_none(),
            "a freed PDU's pointer must be unreachable, not merely unused"
        );
    }

    /// **R2-005.** A local failure must not advance the disposal generation on its
    /// own; it disconnects first, which is what actually frees the PDUs.
    ///
    /// Driven against a real `rpc_context` so the branch is exercised at the C
    /// boundary rather than modelled. No connection is made: `rpc_init_context`
    /// allocates the context, and `rpc_disconnect` on an unconnected context is
    /// the same call the poll-error path makes.
    #[test]
    fn r2_005_a_local_failure_disconnects_before_recording_disposal() {
        let mut pump = EventPump::new("127.0.0.1", 12112).expect("a context allocates");
        let before = pump.disposals;

        // No completion is registered for this id, so this is the "nothing in
        // hand" branch the poll error takes.
        let outcome = pump.stopped(ServiceFailure::Local, u64::MAX, false);
        assert_eq!(outcome, ServiceOutcome::Disconnected);
        assert!(
            pump.disposals > before,
            "the generation advances, but only because `disconnect` made disposal true"
        );
        assert!(
            matches!(pump.state(), ConnectionState::Broken(_)),
            "a local failure that disconnected must not leave the connection usable"
        );
    }

    /// **R2-005.** A libnfs-reported failure records disposal without an extra
    /// disconnect: `rpc_reconnect_requeue` already freed everything.
    #[test]
    fn r2_005_a_reported_failure_records_disposal_directly() {
        let mut pump = EventPump::new("127.0.0.1", 12112).expect("a context allocates");
        let before = pump.disposals;
        let outcome = pump.stopped(ServiceFailure::LibnfsReported, u64::MAX, false);
        assert_eq!(outcome, ServiceOutcome::Disconnected);
        assert_eq!(
            pump.disposals.get(),
            before.get() + 1,
            "exactly one disposal is recorded for one reported failure"
        );
    }

    /// **R1-009.** Without a disposal, an outstanding call is still cancellable —
    /// the deadline path must keep working.
    #[test]
    fn r1_009_an_ordinary_deadline_still_cancels_its_pdu() {
        let generation = DisposalGeneration::START;
        assert_eq!(
            ownership(generation, generation, false),
            Ownership::Cancellable
        );
        let mut dispatch = Dispatch::Queued(0x1234 as *mut sys::rpc_pdu);
        assert!(dispatch.take_for_cancel().is_some());
    }

    #[test]
    fn a_completion_for_a_withdrawn_call_is_discarded() {
        let id = register(4096);
        assert!(withdraw(id).is_none());
        // Delivery after withdrawal is exactly the late-callback case.
        deliver(id, Completion::Cancelled);
        assert!(take(id).is_none());
        assert!(budget_for(id).is_none());
    }

    #[test]
    fn registered_calls_carry_their_own_reply_budget() {
        let small = register(16);
        let large = register(1 << 20);
        assert_eq!(budget_for(small), Some(16));
        assert_eq!(budget_for(large), Some(1 << 20));
        assert_ne!(small, large);
        withdraw(small);
        withdraw(large);
    }
}
