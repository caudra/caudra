//! Sets one `[decisions.features]` mode in the global `caudra.toml`, keeping the rest as written.

use std::path::Path;

use thiserror::Error;
use toml_edit::{Item, Table, Value};

use crate::decisions::{DecisionFeatures, FeatureMode};
use crate::global_file::{GlobalDocument, GlobalFileError};
use crate::toml_document::set_item_value;

const DECISIONS_KEY: &str = "decisions";
const FEATURES_KEY: &str = "features";
const UNEXPECTED_LAYOUT: &str = "[decisions] must hold `features` as a table";

#[derive(Debug, Error)]
pub enum DecisionFeatureWriteError {
    #[error("no decision feature is called `{0}`")]
    UnknownFeature(String),
    #[error("decisions.features.{key} does not take `{mode}`")]
    UnsupportedMode { key: String, mode: &'static str },
    #[error(transparent)]
    File(#[from] GlobalFileError),
}

/// Writes `key = "<mode>"` under `[decisions.features]` of the global config at `path`,
/// creating the table when it is absent. Refuses a mode the feature does not take.
pub fn set_decision_feature_mode(
    path: &Path,
    key: &str,
    mode: &FeatureMode,
) -> Result<(), DecisionFeatureWriteError> {
    let feature = DecisionFeatures::ALL
        .iter()
        .find(|feature| feature.name == key)
        .ok_or_else(|| DecisionFeatureWriteError::UnknownFeature(key.into()))?;
    if !feature.modes.contains(mode) {
        return Err(DecisionFeatureWriteError::UnsupportedMode {
            key: key.into(),
            mode: mode.as_str(),
        });
    }
    let file = GlobalDocument::load(path)?;
    let mut document = file.document.clone();
    let mut decisions = Table::new();
    decisions.set_implicit(true);
    let written = document
        .entry(DECISIONS_KEY)
        .or_insert(Item::Table(decisions))
        .as_table_like_mut()
        .is_some_and(|decisions| {
            set_item_value(
                decisions
                    .entry(FEATURES_KEY)
                    .or_insert(Item::Table(Table::new())),
                key,
                Value::from(mode.as_str()),
            )
        });
    if !written {
        return Err(GlobalFileError::Invalid {
            path: path.to_path_buf(),
            message: UNEXPECTED_LAYOUT.into(),
        }
        .into());
    }
    file.commit(&document)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;
    use crate::config_file::{CONFIG_FILE, load_global_config};
    use crate::decisions::DecisionsConfig;
    use crate::own_writes;

    const FEATURE: &str = "memory_search";
    const COMMENTED: &str = "# my settings\n[decisions.features]\n# keep this\nmemory_search = \"off\" # trailing\nhistory_search = \"enforce\"\n";
    const INLINE: &str = "[decisions]\nlog = true\nfeatures = { history_search = \"enforce\" }\n";
    const DOTTED: &str = "decisions.features.history_search = \"enforce\"\n";
    const SCALAR_FEATURES: &str = "[decisions]\nfeatures = 1\n";

    fn written(contents: Option<&str>) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        if let Some(contents) = contents {
            fs::write(&path, contents).unwrap();
        }
        (dir, path)
    }

    fn decisions(path: &Path) -> DecisionsConfig {
        load_global_config(path)
            .unwrap()
            .settings
            .into_config(false)
            .unwrap()
            .decisions
    }

    #[test_case(None; "a_new_file")]
    #[test_case(Some(COMMENTED); "a_commented_table")]
    #[test_case(Some(INLINE); "an_inline_table")]
    #[test_case(Some(DOTTED); "dotted_keys")]
    fn sets_the_mode_and_keeps_the_rest(original: Option<&str>) {
        let (_dir, path) = written(original);

        set_decision_feature_mode(&path, FEATURE, &FeatureMode::Enforce).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        let config = decisions(&path);
        assert_eq!(config.features.memory_search, FeatureMode::Enforce);
        if let Some(original) = original {
            assert_eq!(config.features.history_search, FeatureMode::Enforce);
            for line in original
                .lines()
                .filter(|line| !line.contains(FEATURE) && !line.starts_with(FEATURES_KEY))
            {
                assert!(text.contains(line), "{line:?} lost from {text}");
            }
        }
        assert!(own_writes::is_own(&path, text.as_bytes()));
    }

    #[test]
    fn an_inline_table_keeps_its_closing_space() {
        let (_dir, path) = written(Some(INLINE));

        set_decision_feature_mode(&path, FEATURE, &FeatureMode::Enforce).unwrap();

        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("{ history_search = \"enforce\", memory_search = \"enforce\" }")
        );
    }

    #[test]
    fn a_commented_line_keeps_its_comments() {
        let (_dir, path) = written(Some(COMMENTED));

        set_decision_feature_mode(&path, FEATURE, &FeatureMode::Enforce).unwrap();

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            COMMENTED.replace("memory_search = \"off\"", "memory_search = \"enforce\"")
        );
    }

    #[test_case("tool_search", FeatureMode::Off; "unknown_feature")]
    #[test_case(FEATURE, FeatureMode::Advise; "unsupported_mode")]
    fn refuses_what_the_config_would_reject(key: &str, mode: FeatureMode) {
        let (_dir, path) = written(Some(COMMENTED));

        let error = set_decision_feature_mode(&path, key, &mode).unwrap_err();

        assert!(matches!(
            error,
            DecisionFeatureWriteError::UnknownFeature(_)
                | DecisionFeatureWriteError::UnsupportedMode { .. }
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), COMMENTED);
    }

    #[test]
    fn a_features_value_that_is_no_table_is_refused() {
        let (_dir, path) = written(Some(SCALAR_FEATURES));

        assert!(set_decision_feature_mode(&path, FEATURE, &FeatureMode::Shadow).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), SCALAR_FEATURES);
    }
}
