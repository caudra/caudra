use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_path_to_error::Segment;
use thiserror::Error;

use crate::{DecisionModel, MAX_BYTES, Payload, Provider};

pub const INDEX: &str = "providers/index.json";
pub const DECISIONS: &str = "decision-models.json";
pub const IMPORT: &str = "import/models-dev.json";
pub const DIST: &str = "dist/v1/models.json";
const SLUG_MESSAGE: &str =
    "slug must contain only lowercase ASCII letters, digits, hyphens, or underscores";

#[derive(Debug, Error)]
#[error("{path}: {pointer}: {message}")]
pub struct RepoError {
    pub path: PathBuf,
    pub pointer: String,
    pub message: String,
}

#[derive(Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct DecisionFile {
    #[serde(rename = "$schema", default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    pub decision_models: Vec<DecisionModel>,
}

impl RepoError {
    pub fn new(path: &Path, pointer: &str, message: impl ToString) -> Self {
        Self {
            path: path.to_owned(),
            pointer: pointer.to_owned(),
            message: message.to_string(),
        }
    }
}

pub fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T, RepoError> {
    let bytes = fs::read(path).map_err(|error| RepoError::new(path, "", error))?;
    if bytes.len() > MAX_BYTES {
        return Err(RepoError::new(
            path,
            "",
            format!("file exceeds {MAX_BYTES} bytes"),
        ));
    }
    let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
    let value = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let pointer: String = error
            .path()
            .iter()
            .map(|segment| {
                let token = match segment {
                    Segment::Seq { index } => index.to_string(),
                    Segment::Map { key } => key.clone(),
                    Segment::Enum { variant } => variant.clone(),
                    Segment::Unknown => String::new(),
                };
                format!("/{}", token.replace('~', "~0").replace('/', "~1"))
            })
            .collect();
        RepoError::new(path, &pointer, error.inner())
    })?;
    deserializer
        .end()
        .map_err(|error| RepoError::new(path, "", error))?;
    Ok(value)
}

pub fn load_providers(dir: &Path) -> Result<Vec<Provider>, RepoError> {
    let index_path = dir.join(INDEX);
    let slugs: Vec<String> = read_json(&index_path)?;
    let mut indexed = BTreeSet::new();
    for (index, slug) in slugs.iter().enumerate() {
        if slug.is_empty()
            || !slug.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            return Err(RepoError::new(
                &index_path,
                &format!("/{index}"),
                SLUG_MESSAGE,
            ));
        }
        if !indexed.insert(slug) {
            return Err(RepoError::new(
                &index_path,
                &format!("/{index}"),
                "duplicate provider slug",
            ));
        }
    }
    let providers_dir = dir.join("providers");
    let entries =
        fs::read_dir(&providers_dir).map_err(|error| RepoError::new(&providers_dir, "", error))?;
    for entry in entries {
        let path = entry
            .map_err(|error| RepoError::new(&providers_dir, "", error))?
            .path();
        if path.extension().is_some_and(|ext| ext == "json") && path != index_path {
            let slug = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or_default();
            if !slugs.iter().any(|indexed| indexed == slug) {
                return Err(RepoError::new(
                    &path,
                    "/slug",
                    "provider file is not listed in providers/index.json",
                ));
            }
        }
    }
    let mut providers = Vec::with_capacity(slugs.len());
    for slug in slugs {
        let path = dir.join(format!("providers/{slug}.json"));
        let provider: Provider = read_json(&path)?;
        if provider.slug != slug {
            return Err(RepoError::new(
                &path,
                "/slug",
                format!("expected index slug {slug:?}"),
            ));
        }
        providers.push(provider);
    }
    Ok(providers)
}

pub fn load(dir: &Path) -> Result<Payload, RepoError> {
    let mut providers = load_providers(dir)?;
    let authored = providers.clone();
    for provider in &mut providers {
        let Some(slug) = provider.models_from.take() else {
            continue;
        };
        let path = dir.join(format!("providers/{}.json", provider.slug));
        if !provider.models.is_empty()
            || !provider.generations.is_empty()
            || !provider.price_periods.is_empty()
            || !provider.availability.is_empty()
        {
            return Err(RepoError::new(
                &path,
                "/models_from",
                "models_from cannot be combined with models, generations, price_periods, or availability",
            ));
        }
        let source = authored
            .iter()
            .find(|source| source.slug == slug)
            .ok_or_else(|| {
                RepoError::new(&path, "/models_from", format!("unknown provider {slug:?}"))
            })?;
        if source.models_from.is_some() {
            return Err(RepoError::new(
                &path,
                "/models_from",
                "referenced provider must not itself use models_from",
            ));
        }
        provider.models.clone_from(&source.models);
        provider.generations.clone_from(&source.generations);
        provider.price_periods.clone_from(&source.price_periods);
        provider.availability.clone_from(&source.availability);
    }
    Ok(Payload {
        providers,
        decision_models: read_json::<DecisionFile>(&dir.join(DECISIONS))?.decision_models,
        catalog: read_json(&dir.join(IMPORT))?,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::fs;

    use tempfile::TempDir;
    use test_case::test_case;

    use super::{DECISIONS, DecisionFile, IMPORT, INDEX, load, read_json};
    use crate::{Provider, tests::fixture};

    pub(crate) fn repo_fixture() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("providers")).unwrap();
        fs::create_dir(dir.path().join("import")).unwrap();
        let payload = fixture().payload;
        fs::write(dir.path().join(INDEX), br#"["example"]"#).unwrap();
        fs::write(
            dir.path().join("providers/example.json"),
            serde_json::to_vec(&payload.providers[0]).unwrap(),
        )
        .unwrap();
        fs::write(
            dir.path().join(DECISIONS),
            serde_json::to_vec(&DecisionFile {
                schema: None,
                decision_models: payload.decision_models,
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            dir.path().join(IMPORT),
            serde_json::to_vec(&payload.catalog).unwrap(),
        )
        .unwrap();
        dir
    }

    #[test]
    fn loads_authoring_repo_without_changing_payload() {
        let dir = repo_fixture();
        assert_eq!(load(dir.path()).unwrap(), fixture().payload);
    }

    #[test]
    fn malformed_field_names_its_authoring_file_and_pointer() {
        let dir = repo_fixture();
        let path = dir.path().join("providers/example.json");
        let mut value: serde_json::Value = read_json(&path).unwrap();
        value["models"][0]["context_window"] = serde_json::json!("invalid");
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        let error = load(dir.path()).unwrap_err();
        assert_eq!(error.path, path);
        assert_eq!(error.pointer, "/models/0/context_window");
    }

    #[test_case(false; "own_tables")]
    #[test_case(true; "chained_reference")]
    fn models_from_rejects_ambiguous_authoring(chained: bool) {
        let dir = repo_fixture();
        let path = dir.path().join("providers/example.json");
        let mut provider: Provider = read_json(&path).unwrap();
        if chained {
            provider.models.clear();
            provider.generations.clear();
            provider.price_periods.clear();
            provider.availability.clear();
            provider.models_from = Some("copy".to_owned());
            fs::write(&path, serde_json::to_vec(&provider).unwrap()).unwrap();
            provider.slug = "copy".to_owned();
            provider.models_from = Some("example".to_owned());
            fs::write(
                dir.path().join("providers/copy.json"),
                serde_json::to_vec(&provider).unwrap(),
            )
            .unwrap();
            fs::write(dir.path().join(INDEX), br#"["example","copy"]"#).unwrap();
        } else {
            provider.models_from = Some("example".to_owned());
            fs::write(&path, serde_json::to_vec(&provider).unwrap()).unwrap();
        }
        let error = load(dir.path()).unwrap_err();
        assert_eq!(error.pointer, "/models_from");
        assert_eq!(error.path, path);
    }

    #[test_case("copy", true; "expands")]
    #[test_case("missing", false; "unknown")]
    #[test_case("example", false; "self_reference")]
    fn models_from_expands_only_direct_references(target: &str, valid: bool) {
        let dir = repo_fixture();
        let path = dir.path().join("providers/example.json");
        let mut provider: Provider = read_json(&path).unwrap();
        let original = provider.clone();
        provider.slug = "copy".to_owned();
        fs::write(
            dir.path().join("providers/copy.json"),
            serde_json::to_vec(&provider).unwrap(),
        )
        .unwrap();
        fs::write(dir.path().join(INDEX), br#"["example","copy"]"#).unwrap();
        provider = original.clone();
        provider.models_from = Some(target.to_owned());
        provider.models.clear();
        provider.generations.clear();
        provider.price_periods.clear();
        provider.availability.clear();
        fs::write(&path, serde_json::to_vec(&provider).unwrap()).unwrap();
        if valid {
            assert_eq!(load(dir.path()).unwrap().providers[0], original);
        } else {
            assert_eq!(load(dir.path()).unwrap_err().pointer, "/models_from");
        }
    }

    #[test_case(br#"[]"#, "providers/example.json"; "unindexed_file")]
    #[test_case(br#"["example","missing"]"#, "providers/missing.json"; "missing_file")]
    #[test_case(br#"["example","example"]"#, INDEX; "duplicate_slug")]
    #[test_case(br#"["../escape"]"#, INDEX; "path_traversal")]
    fn index_mismatch_names_the_file(index: &[u8], file: &str) {
        let dir = repo_fixture();
        fs::write(dir.path().join(INDEX), index).unwrap();
        assert_eq!(load(dir.path()).unwrap_err().path, dir.path().join(file));
    }
}
