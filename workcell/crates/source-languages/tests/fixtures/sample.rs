//! Catalog inventory tracking.
use std::collections::BTreeMap;

pub const DEFAULT_PAGE_SIZE: usize = 25;

pub trait Restockable {
    fn restock(&mut self, units: u32);
}

pub struct Catalog {
    pub items: BTreeMap<String, u32>,
}

impl Catalog {
    pub fn new() -> Self {
        Self { items: BTreeMap::new() }
    }

    pub fn add(&mut self, sku: &str, units: u32) -> u32 {
        let normalized = Self::normalize(sku);
        let entry = self.items.entry(normalized).or_insert(0);
        *entry = entry.saturating_add(units);
        *entry
    }

    fn normalize(sku: &str) -> String {
        sku.trim().to_ascii_uppercase()
    }

    pub fn total(&self) -> u32 {
        self.items.values().copied().sum()
    }
}

impl Restockable for Catalog {
    fn restock(&mut self, units: u32) {
        self.add("DEFAULT", units);
    }
}

pub fn build_catalog() -> Catalog {
    let mut catalog = Catalog::new();
    catalog.add("a-1", DEFAULT_PAGE_SIZE as u32);
    catalog
}
