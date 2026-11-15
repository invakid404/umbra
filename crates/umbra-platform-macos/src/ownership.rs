//! Test witnesses travel with the owning value, including moved containers.
use std::ops::{Deref, DerefMut};
#[cfg(any(test, feature = "retirement-witness"))]
use std::{cell::RefCell, sync::Arc};

#[derive(Default)]
pub(crate) struct Witness {
    #[cfg(any(test, feature = "retirement-witness"))]
    _alive: Arc<()>,
}
#[cfg(any(test, feature = "retirement-witness"))]
type Observation = (u64, &'static str, Box<dyn Fn() -> bool>);
#[cfg(any(test, feature = "retirement-witness"))]
thread_local! { static OWNERS: RefCell<Vec<Observation>> = RefCell::default(); }
impl Witness {
    pub(crate) fn new(pid: u64, kind: &'static str) -> Self {
        let value = Self::default();
        #[cfg(any(test, feature = "retirement-witness"))]
        {
            let weak = Arc::downgrade(&value._alive);
            OWNERS.with(|owners| {
                owners
                    .borrow_mut()
                    .push((pid, kind, Box::new(move || weak.upgrade().is_some())))
            });
        }
        #[cfg(not(any(test, feature = "retirement-witness")))]
        let _ = (pid, kind);
        value
    }
}
#[cfg(any(test, feature = "retirement-witness"))]
pub(crate) fn task<T: 'static>(pid: u64, task: &Arc<T>) {
    let weak = Arc::downgrade(task);
    OWNERS.with(|owners| {
        owners
            .borrow_mut()
            .push((pid, "task", Box::new(move || weak.upgrade().is_some())))
    });
}
#[cfg(any(test, feature = "retirement-witness"))]
pub(crate) fn remaining(pid: u64) -> Vec<&'static str> {
    OWNERS.with(|owners| {
        owners
            .borrow()
            .iter()
            .filter(|(owner, _, alive)| *owner == pid && alive())
            .map(|(_, kind, _)| *kind)
            .collect()
    })
}

/// Fields stay private so extraction must transfer the witness with ownership.
pub(crate) struct Owned<T> {
    value: T,
    witness: Witness,
    pid: u64,
    kind: &'static str,
}
impl<T> Owned<T> {
    pub(crate) fn new(value: T, pid: u64, kind: &'static str) -> Self {
        Self {
            value,
            witness: Witness::new(pid, kind),
            pid,
            kind,
        }
    }
}
impl<T: Default> Owned<T> {
    pub(crate) fn take(&mut self) -> Self {
        let replacement = Self::new(T::default(), self.pid, self.kind);
        std::mem::replace(self, replacement)
    }
}
impl<T> Deref for Owned<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}
impl<T> DerefMut for Owned<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}
pub(crate) struct OwnedIter<I> {
    iter: I,
    _witness: Witness,
}
impl<I: Iterator> Iterator for OwnedIter<I> {
    type Item = I::Item;
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.next()
    }
}
impl<K: Ord, V> Owned<std::collections::BTreeMap<K, V>> {
    pub(crate) fn into_values(self) -> OwnedIter<std::collections::btree_map::IntoValues<K, V>> {
        OwnedIter {
            iter: self.value.into_values(),
            _witness: self.witness,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retirement_map_witness_follows_move_and_iterator() {
        let mut map = Owned::new(
            std::collections::BTreeMap::from([(1, vec![0u8; 4096])]),
            274,
            "pending",
        );
        let moved = map.take();
        assert_eq!(remaining(274), ["pending", "pending"]);
        drop(map);
        assert_eq!(remaining(274), ["pending"]);
        let iter = moved.into_values();
        assert_eq!(remaining(274), ["pending"]);
        drop(iter);
        assert!(remaining(274).is_empty());
    }
    #[test]
    fn retirement_task_witness_counts_both_owners() {
        let session = Arc::new(vec![1; 128]);
        let registry = session.clone();
        task(275, &session);
        drop(session);
        assert_eq!(remaining(275), ["task"]);
        drop(registry);
        assert!(remaining(275).is_empty());
    }
}
