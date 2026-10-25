//! A value that records when it was last borrowed mutably, so a reader can
//! tell an unchanged value from a changed one without comparing it.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_STAMP: AtomicU64 = AtomicU64::new(1);

fn next_stamp() -> u64 {
    NEXT_STAMP.fetch_add(1, Ordering::Relaxed)
}

/// Stamps come from one process-wide sequence, so a stamp never repeats: a
/// replaced value, or one in another owner, never matches a stamp read from
/// an earlier one. Any mutable borrow takes a new stamp, changed or not.
#[derive(Debug)]
pub struct Tracked<T> {
    value: T,
    stamp: u64,
}

impl<T> Tracked<T> {
    pub fn new(value: T) -> Self {
        Self {
            value,
            stamp: next_stamp(),
        }
    }

    pub fn stamp(&self) -> u64 {
        self.stamp
    }
}

impl<T: Default> Default for Tracked<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: Clone> Clone for Tracked<T> {
    fn clone(&self) -> Self {
        Self::new(self.value.clone())
    }
}

impl<T> Deref for Tracked<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<'a, T> IntoIterator for &'a Tracked<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.value.into_iter()
    }
}

impl<T> DerefMut for Tracked<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.stamp = next_stamp();
        &mut self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_keep_the_stamp_and_writes_replace_it() {
        let mut tracked = Tracked::new(vec![1]);
        let stamp = tracked.stamp();
        assert_eq!(tracked.len(), 1);
        assert_eq!(tracked.stamp(), stamp);
        tracked.push(2);
        assert!(tracked.stamp() > stamp);
    }

    #[test]
    fn a_fresh_value_never_reuses_a_stamp() {
        let first = Tracked::new(0);
        let replacement = Tracked::new(0);
        assert_ne!(first.stamp(), replacement.stamp());
        assert_ne!(first.clone().stamp(), first.stamp());
    }
}
