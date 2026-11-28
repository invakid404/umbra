//! Awake-time deadlines; the test seam models sleep without suspending the host.
use std::time::Instant;
/// Same awake-time epoch used by managed role tickets.
pub(crate) fn boot_nanos() -> umbra_core::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: writable timespec, with no process-local epoch.
    if unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut time) } != 0 {
        return Err(crate::error(
            "admission clock",
            std::io::Error::last_os_error(),
        ));
    }
    let seconds = u64::try_from(time.tv_sec)
        .map_err(|_| crate::error("admission clock", "negative clock"))?;
    let nanos = u64::try_from(time.tv_nsec)
        .map_err(|_| crate::error("admission clock", "negative clock fraction"))?;
    if nanos >= 1_000_000_000 {
        return Err(crate::error("admission clock", "invalid clock fraction"));
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|s| s.checked_add(nanos))
        .ok_or_else(|| crate::error("admission clock", "clock overflow"))
}

pub(crate) fn now() -> Instant {
    #[cfg(test)]
    if let Some((awake, _continuous)) = SCRIPT.with(|v| v.get()) {
        return awake;
    }
    Instant::now()
}
/// Convert a shared absolute cutoff conservatively into the local clock.
pub(crate) fn clamp_deadline(
    deadline: Instant,
    cutoff: Option<u64>,
) -> umbra_core::Result<Instant> {
    let Some(cutoff) = cutoff else {
        return Ok(deadline);
    };
    // Sample Instant first so conversion cannot add time to the shared cutoff.
    let local = now();
    let remaining = cutoff.saturating_sub(boot_nanos()?);
    Ok(local
        .checked_add(std::time::Duration::from_nanos(remaining))
        .map_or(deadline, |shared| deadline.min(shared)))
}
/// Cleanup never receives extra wait time when its shared clock cannot be read.
pub(crate) fn cleanup_limit(
    deadline: Instant,
    ledger: &umbra_platform::shutdown::Ledger,
) -> Instant {
    ledger
        .admission_cutoff()
        .and_then(|cutoff| clamp_deadline(deadline, cutoff))
        .unwrap_or_else(|_| now())
}
#[cfg(test)]
thread_local! { pub(crate) static SCRIPT:std::cell::Cell<Option<(Instant,Instant)>>=const{std::cell::Cell::new(None)}; }
