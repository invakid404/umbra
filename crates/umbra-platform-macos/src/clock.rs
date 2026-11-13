//! Awake-time deadlines; the test seam models sleep without suspending the host.
use std::time::Instant;
pub(crate) fn now() -> Instant {
    #[cfg(test)]
    if let Some((awake, _continuous)) = SCRIPT.with(|v| v.get()) {
        return awake;
    }
    Instant::now()
}
#[cfg(test)]
thread_local! { pub(crate) static SCRIPT:std::cell::Cell<Option<(Instant,Instant)>>=const{std::cell::Cell::new(None)}; }
