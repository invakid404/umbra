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
use crate::transport::{CompoundReply, ConnectionEpoch, ConnectionState, TransportResult};

use super::decode::{self, ReplyBudget};
use super::sys;

/// What a libnfs callback reported for one call.
pub(super) enum Completion {
    /// A COMPOUND reply, already decoded into owned Rust values.
    Reply(Box<TransportResult<CompoundReply>>),
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
}

/// One in-flight call: its id, its PDU, and the arguments C is reading.
pub(super) struct CallSlot {
    /// Registry id, also the value behind the public `CallToken`.
    pub(super) id: u64,
    /// PDU ownership state.
    pub(super) dispatch: Dispatch,
    /// Argument memory, owned until the call completes or is withdrawn.
    ///
    /// Dropping the slot drops the arena. That ordering is the whole point:
    /// libnfs references the WRITE payload from the PDU's iovector, so the
    /// arena must not be released while a PDU can still reach it.
    pub(super) _arena: super::args::CallArena,
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
        })
    }

    pub(super) fn state(&self) -> ConnectionState {
        self.state
    }

    pub(super) fn epoch(&self) -> ConnectionEpoch {
        self.epoch
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
            dispatch: Dispatch::Queued(pdu),
            _arena: arena,
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
                return ServiceOutcome::Disconnected;
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
                return ServiceOutcome::Disconnected;
            }
            if ready == 0 {
                continue;
            }

            // SAFETY: `revents` came from the poll above for this context's fd.
            // Completion callbacks run inside this call.
            if unsafe { sys::rpc_service(self.rpc, c_int::from(descriptor.revents)) } < 0 {
                self.state = ConnectionState::Broken(self.epoch);
                return ServiceOutcome::Disconnected;
            }
        }
    }

    /// Withdraw a call from libnfs and report whether the pump is proven idle.
    ///
    /// Returns `true` when no callback for this call can run any more. That is
    /// the case when `rpc_cancel_pdu` removed the PDU (libnfs frees it without
    /// invoking the callback) or when a completion had already been observed.
    pub(super) fn retire(&mut self, slot: &mut CallSlot) -> bool {
        let drained = match slot.dispatch.take_for_cancel() {
            // No PDU is owed a cancellation: libnfs already finished with it.
            None => true,
            Some(pdu) => {
                // SAFETY: `Dispatch::Queued` is only ever set from a live PDU
                // pointer, and `take_for_cancel` consumes it, so libnfs still
                // owned this PDU and no second cancellation can occur.
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
