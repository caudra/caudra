mod bundled;
mod convert;
mod current;
mod refresh;

#[cfg(test)]
use caudra_models::{ModelEntry, Snapshot, validate};

#[cfg(test)]
pub(crate) use current::install;
pub use current::{Current, current, initialize_bundled};
pub use refresh::{RefreshError, RefreshOutcome, refresh, start_refresh};

#[cfg(test)]
pub(crate) fn edit_model(slug: &str, id: &str, edit: impl FnOnce(&mut ModelEntry)) {
    let mut snapshot = bundled::snapshot();
    let provider = snapshot
        .payload
        .providers
        .iter_mut()
        .find(|provider| provider.slug == slug)
        .unwrap();
    let entry = provider
        .models
        .iter_mut()
        .find(|entry| entry.prefixes.iter().any(|prefix| prefix == id))
        .unwrap();
    edit(entry);
    let snapshot = Snapshot::stamp(snapshot.payload, None, snapshot.updated_at);
    validate(&snapshot).unwrap();
    install(snapshot);
}
