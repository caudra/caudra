//! Automations shipped in the binary: a provider such as pstack installs its scripts here while
//! it is on and removes them when it turns off. The catalog lists them in the bundled scope,
//! below the user's and the project's scripts, so a file of the same name overrides one.

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};

use arc_swap::ArcSwap;

use super::catalog::SCRIPT_EXTENSION;

/// Prefixes the path a bundled script shows in place of a file.
const PATH_SCHEME: &str = "builtin:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledAutomation {
    /// What installed the script, such as `pstack`.
    pub provider: &'static str,
    /// The script's name, which its `meta.name` must equal as a file's stem must.
    pub name: String,
    pub source: Cow<'static, str>,
}

impl BundledAutomation {
    /// Where the script comes from, as `builtin:<provider>/<name>.rhai`. Not a file.
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!(
            "{PATH_SCHEME}{}/{}.{SCRIPT_EXTENSION}",
            self.provider, self.name
        ))
    }
}

/// Sorted by provider, then name, so every scan lists them in one order.
static INSTALLED: LazyLock<ArcSwap<Vec<BundledAutomation>>> =
    LazyLock::new(|| ArcSwap::from_pointee(Vec::new()));

/// Replaces every script `provider` installed before with `scripts`, given as `(name, source)`.
pub fn install_bundled_automations(
    provider: &'static str,
    scripts: impl IntoIterator<Item = (&'static str, &'static str)>,
) {
    let scripts: Vec<BundledAutomation> = scripts
        .into_iter()
        .map(|(name, source)| BundledAutomation {
            provider,
            name: name.to_owned(),
            source: Cow::Borrowed(source),
        })
        .collect();
    INSTALLED.rcu(|installed| {
        let mut next: Vec<BundledAutomation> = installed
            .iter()
            .filter(|script| script.provider != provider)
            .cloned()
            .chain(scripts.iter().cloned())
            .collect();
        next.sort_by(|left, right| (left.provider, &left.name).cmp(&(right.provider, &right.name)));
        next
    });
}

pub fn remove_bundled_automations(provider: &str) {
    INSTALLED.rcu(|installed| {
        installed
            .iter()
            .filter(|script| script.provider != provider)
            .cloned()
            .collect::<Vec<_>>()
    });
}

/// What the next catalog scan lists in the bundled scope.
pub fn bundled_automations() -> Arc<Vec<BundledAutomation>> {
    INSTALLED.load_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDER: &str = "bundled-registry-test";
    const FIRST: &str = "first";
    const SECOND: &str = "second";
    const SOURCE: &str = "let meta = #{};";

    fn installed_names() -> Vec<String> {
        bundled_automations()
            .iter()
            .filter(|script| script.provider == PROVIDER)
            .map(|script| script.name.clone())
            .collect()
    }

    #[test]
    fn a_provider_replaces_and_removes_only_its_own_scripts() {
        install_bundled_automations(PROVIDER, [(SECOND, SOURCE), (FIRST, SOURCE)]);
        assert_eq!(installed_names(), [FIRST, SECOND]);

        install_bundled_automations(PROVIDER, [(FIRST, SOURCE)]);
        assert_eq!(installed_names(), [FIRST]);

        remove_bundled_automations(PROVIDER);
        assert!(installed_names().is_empty());
    }
}
