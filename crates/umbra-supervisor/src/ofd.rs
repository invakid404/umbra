//! Open file descriptions for routed descriptors.
//!
//! POSIX puts a descriptor's file position in its *open file description*, which
//! `fork` and `posix_spawn` share between parent and child. Only the descriptor
//! number and its close-on-exec bit belong to the descriptor itself (XBD 3.241;
//! XNU `struct fileproc` / `struct fileglob`). `ProcessContext::fds` is a
//! per-process map of values, so cloning it on fork copied every position, and
//! the parent's next write landed where its own last write ended (#172, #190).
//!
//! This table is what the processes share. It is runtime-only, like
//! `ProcessTree`, and it holds the one per-OFD field that ever changes after an
//! open: the position. Every other per-OFD field (`object`, `logical_path`,
//! `directory`, `flags`) is fixed at the open, because no routed operation
//! rewrites it and `F_SETFL` is not routed. Those fields can therefore stay
//! copied in each process's [`umbra_core::FdState`] view without any observable
//! difference. The views' `offset` is kept equal to the position by the
//! supervisor, which writes every advance through to every bound view.
//!
//! The table changes only where the supervisor applies an *observed* outcome,
//! which is the rule `FdState::offset` states, and only while no other routed
//! operation is in flight (`Supervisor::admit_during_routed_effect`).
//!
//! **What this shares, and what it does not.** It shares the regular-file
//! position. It is not the whole of a POSIX open file description:
//! - a routed directory's enumeration cursor is still kept per process by the
//!   overlay, keyed by task and exec generation (#194);
//! - `O_CLOFORK` is not decoded, so a close-on-fork routed descriptor is still
//!   inherited (#195);
//! - the dup family is not routed at all (`dup`, `dup2` and `fcntl` reach the
//!   kernel and answer `EBADF`). [`OfdTable::share`] is what a routed dup would
//!   call.
use std::collections::{BTreeMap, BTreeSet};

use umbra_core::{TaskId, TracedFd};

/// One routed descriptor: a process and the number it holds.
pub(crate) type Binding = (TaskId, TracedFd);

/// The identity of one open file description, unique for the run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OfdId(u64);

#[derive(Clone, Debug)]
struct Ofd {
    /// Bytes from the start of the object, advanced by observed transfers.
    position: u64,
    /// Every descriptor that refers to this description. The last one released
    /// frees it.
    references: BTreeSet<Binding>,
}

/// The run's open file descriptions and the descriptors bound to them.
#[derive(Clone, Debug, Default)]
pub(crate) struct OfdTable {
    next: u64,
    ofds: BTreeMap<OfdId, Ofd>,
    bindings: BTreeMap<Binding, OfdId>,
}

impl OfdTable {
    /// A routed `open` succeeded: a new description, referred to by `binding`.
    pub(crate) fn open(&mut self, binding: Binding, position: u64) -> Result<(), &'static str> {
        if self.bindings.contains_key(&binding) {
            return Err(
                "a routed open bound a descriptor that already has an open file description",
            );
        }
        let id = OfdId(self.next);
        self.next += 1;
        self.ofds.insert(
            id,
            Ofd {
                position,
                references: BTreeSet::from([binding]),
            },
        );
        self.bindings.insert(binding, id);
        Ok(())
    }

    /// `to` now refers to the description `from` refers to (fork, spawn, and
    /// the shape a future routed `dup` would use).
    pub(crate) fn share(&mut self, from: Binding, to: Binding) -> Result<(), &'static str> {
        let id = *self
            .bindings
            .get(&from)
            .ok_or("an inherited routed descriptor has no open file description")?;
        if self.bindings.contains_key(&to) {
            return Err("an inheriting descriptor already has an open file description");
        }
        self.ofds
            .get_mut(&id)
            .expect("a binding names a live description")
            .references
            .insert(to);
        self.bindings.insert(to, id);
        Ok(())
    }

    /// `binding` no longer refers to its description (close, exec sweep).
    pub(crate) fn release(&mut self, binding: Binding) -> Result<(), &'static str> {
        let id = self
            .bindings
            .remove(&binding)
            .ok_or("a released routed descriptor has no open file description")?;
        let ofd = self
            .ofds
            .get_mut(&id)
            .expect("a binding names a live description");
        ofd.references.remove(&binding);
        if ofd.references.is_empty() {
            self.ofds.remove(&id);
        }
        Ok(())
    }

    /// Every descriptor `task` holds is released (process exit).
    pub(crate) fn release_task(&mut self, task: TaskId) {
        let held = self
            .bindings
            .keys()
            .filter(|(t, _)| *t == task)
            .copied()
            .collect::<Vec<_>>();
        for binding in held {
            self.release(binding)
                .expect("collected from the bindings map");
        }
    }

    /// An observed transfer of `count` bytes on `binding`'s description. Returns
    /// the new position and every descriptor whose view must be set to it.
    pub(crate) fn advance(
        &mut self,
        binding: Binding,
        count: u64,
    ) -> Result<(u64, Vec<Binding>), &'static str> {
        let id = self
            .bindings
            .get(&binding)
            .ok_or("a routed transfer advanced a descriptor with no open file description")?;
        let ofd = self
            .ofds
            .get_mut(id)
            .expect("a binding names a live description");
        ofd.position = ofd.position.saturating_add(count);
        Ok((ofd.position, ofd.references.iter().copied().collect()))
    }

    #[cfg(test)]
    pub(crate) fn references(&self, binding: Binding) -> Option<usize> {
        let id = self.bindings.get(&binding)?;
        Some(self.ofds[id].references.len())
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.ofds.is_empty() && self.bindings.is_empty()
    }
}
