//! JavaScript's `Map` and `Set`, which upstream's charts depend on: they iterate in the order
//! their keys first went in, so the drawn lines come out in that order too.

use std::collections::HashMap;
use std::hash::Hash;

#[derive(Debug, Clone)]
pub struct OrderedMap<K, V> {
    entries: Vec<(K, V)>,
    index: HashMap<K, usize>,
}

impl<K, V> Default for OrderedMap<K, V> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl<K: Eq + Hash + Clone, V> OrderedMap<K, V> {
    pub fn get(&self, key: &K) -> Option<&V> {
        self.index.get(key).map(|&at| &self.entries[at].1)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.index.get(key).map(|&at| &mut self.entries[at].1)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.index.contains_key(key)
    }

    /// The value at `key`, put in last with `make` when the map lacks it.
    pub fn entry(&mut self, key: K, make: impl FnOnce() -> V) -> &mut V {
        let at = match self.index.get(&key) {
            Some(&at) => at,
            None => {
                self.index.insert(key.clone(), self.entries.len());
                self.entries.push((key, make()));
                self.entries.len() - 1
            }
        };
        &mut self.entries[at].1
    }

    /// Sets `key`, keeping its place when the map has it.
    pub fn insert(&mut self, key: K, value: V) {
        match self.index.get(&key) {
            Some(&at) => self.entries[at].1 = value,
            None => {
                self.index.insert(key.clone(), self.entries.len());
                self.entries.push((key, value));
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.entries.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.entries.iter().map(|(_, value)| value)
    }

    pub fn into_values(self) -> Vec<V> {
        self.entries.into_iter().map(|(_, value)| value).collect()
    }

    /// Drops every entry that `keep` refuses, keeping the order of the rest.
    pub fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.entries.retain(|(key, value)| keep(key, value));
        self.index = self
            .entries
            .iter()
            .enumerate()
            .map(|(at, (key, _))| (key.clone(), at))
            .collect();
    }
}

#[derive(Debug, Clone)]
pub struct OrderedSet<T> {
    items: OrderedMap<T, ()>,
}

impl<T> Default for OrderedSet<T> {
    fn default() -> Self {
        Self {
            items: OrderedMap::default(),
        }
    }
}

impl<T: Eq + Hash + Clone> OrderedSet<T> {
    /// Adds `item` last; returns whether the set lacked it.
    pub fn insert(&mut self, item: T) -> bool {
        let fresh = !self.items.contains_key(&item);
        if fresh {
            self.items.entry(item, || ());
        }
        fresh
    }

    pub fn contains(&self, item: &T) -> bool {
        self.items.contains_key(item)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.items.keys()
    }

    pub fn remove(&mut self, item: &T) {
        if self.contains(item) {
            self.items.retain(|kept, ()| kept != item);
        }
    }
}

impl<T: Eq + Hash + Clone> FromIterator<T> for OrderedSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(items: I) -> Self {
        let mut set = Self::default();
        for item in items {
            set.insert(item);
        }
        set
    }
}

impl<T: Eq + Hash + Clone> Extend<T> for OrderedSet<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, items: I) {
        for item in items {
            self.insert(item);
        }
    }
}
