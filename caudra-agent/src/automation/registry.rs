//! The automation runtimes open in this process, by session, so the agent loop and the
//! `automation` tool reach a session's runtime without threading it through their parameters.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};

use caudra_storage::id::CaudraId;

use super::handle::Shared;

type Runtimes = HashMap<CaudraId, Weak<Shared>>;

static RUNTIMES: OnceLock<Mutex<Runtimes>> = OnceLock::new();

pub(super) fn register(shared: &Arc<Shared>) {
    let mut runtimes = runtimes();
    runtimes.retain(|_, runtime| runtime.strong_count() > 0);
    runtimes.insert(shared.session_id, Arc::downgrade(shared));
}

/// Forgets `shared`, unless another runtime of the session replaced it since.
pub(super) fn unregister(shared: &Arc<Shared>) {
    let mut runtimes = runtimes();
    if runtimes
        .get(&shared.session_id)
        .is_some_and(|runtime| Weak::ptr_eq(runtime, &Arc::downgrade(shared)))
    {
        runtimes.remove(&shared.session_id);
    }
}

pub(super) fn lookup(session_id: CaudraId) -> Option<Arc<Shared>> {
    runtimes().get(&session_id).and_then(Weak::upgrade)
}

fn runtimes() -> MutexGuard<'static, Runtimes> {
    RUNTIMES
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
