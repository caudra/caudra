use caudra_providers::model::Model;
use serde::Serialize;

use crate::methods::{effort_levels, split_spec};

pub(crate) const MODELS_LIST: &str = "_caudra/models/list";
pub(crate) const MODELS_CHANGED: &str = "_caudra/models/changed";

/// Every model a client can open a session on, before it opens one.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelCatalog {
    pub(crate) models: Vec<CatalogModel>,
    pub(crate) default_model: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CatalogModel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) provider: String,
    pub(crate) context_window: u32,
    /// The `effort` option's values on this model, empty when it cannot reason.
    pub(crate) efforts: Vec<String>,
    pub(crate) fast: bool,
}

impl ModelCatalog {
    /// `specs` in order, the default first when the list misses it, as the
    /// model option lists the current model. A spec that no longer resolves
    /// is left out.
    pub(crate) fn new(specs: &[String], default: &Model) -> Self {
        let default_model = default.spec();
        let listed = specs.contains(&default_model);
        let models = (!listed)
            .then(|| CatalogModel::of(default))
            .into_iter()
            .chain(
                specs
                    .iter()
                    .filter_map(|spec| Model::from_spec(spec).ok())
                    .map(|model| CatalogModel::of(&model)),
            )
            .collect();
        Self {
            models,
            default_model,
        }
    }
}

impl CatalogModel {
    fn of(model: &Model) -> Self {
        let id = model.spec();
        let (provider, name) = split_spec(&id);
        Self {
            name: name.to_owned(),
            provider: provider.to_owned(),
            context_window: model.context_window,
            efforts: effort_levels(model)
                .iter()
                .map(ToString::to_string)
                .collect(),
            fast: model.supports_fast(),
            id,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::test_support::{FAST_SPEC, OFFLINE_SPEC, SELECTED_SPEC};

    fn catalog(specs: &[&str], default: &str) -> Value {
        let specs: Vec<String> = specs.iter().map(|spec| (*spec).to_owned()).collect();
        serde_json::to_value(ModelCatalog::new(
            &specs,
            &Model::from_spec(default).unwrap(),
        ))
        .unwrap()
    }

    fn ids(catalog: &Value) -> Vec<&str> {
        catalog["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn a_model_carries_what_a_picker_shows() {
        let catalog = catalog(&[FAST_SPEC], FAST_SPEC);
        let model = Model::from_spec(FAST_SPEC).unwrap();
        let entry = &catalog["models"][0];

        assert_eq!(catalog["defaultModel"], FAST_SPEC);
        assert_eq!(entry["id"], FAST_SPEC);
        let (provider, name) = split_spec(FAST_SPEC);
        assert_eq!(entry["provider"], provider);
        assert_eq!(entry["name"], name);
        assert_eq!(entry["contextWindow"], model.context_window);
        assert_eq!(entry["fast"], true);
        let efforts: Vec<String> = effort_levels(&model)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert!(!efforts.is_empty());
        assert_eq!(entry["efforts"], serde_json::json!(efforts));
    }

    #[test]
    fn the_default_leads_a_list_that_misses_it() {
        assert_eq!(
            ids(&catalog(&[OFFLINE_SPEC], SELECTED_SPEC)),
            [SELECTED_SPEC, OFFLINE_SPEC]
        );
        assert_eq!(
            ids(&catalog(&[OFFLINE_SPEC, SELECTED_SPEC], SELECTED_SPEC)),
            [OFFLINE_SPEC, SELECTED_SPEC]
        );
    }
}
