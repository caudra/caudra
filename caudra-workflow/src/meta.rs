use std::collections::HashSet;

use serde::{Deserialize, Serialize};

pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_DESCRIPTION_BYTES: usize = 512;
pub const MAX_WHEN_TO_USE_BYTES: usize = 1024;
pub const MAX_PHASES: usize = 16;
pub const MAX_PHASE_TITLE_BYTES: usize = 64;
pub const MAX_PHASE_DETAIL_BYTES: usize = 256;

pub const META_VARIABLE: &str = "meta";

const FIELD_NAME: &str = "name";
const FIELD_DESCRIPTION: &str = "description";
const FIELD_WHEN_TO_USE: &str = "when_to_use";
const FIELD_PHASE_TITLE: &str = "phases[].title";
const FIELD_PHASE_DETAIL: &str = "phases[].detail";

/// The `let meta = #{ ... };` header every workflow script starts with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowMeta {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when_to_use: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phases: Vec<PhaseMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseMeta {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetaError {
    #[error("source is {bytes} bytes; the limit is {max}")]
    SourceTooLarge { bytes: usize, max: usize },
    #[error("script failed to parse: {0}")]
    Parse(String),
    #[error("first statement must be `let {META_VARIABLE} = #{{ ... }};`")]
    MetaNotFirst,
    #[error("meta may only contain string, array, and map literals ({position})")]
    NonLiteral { position: String },
    #[error("meta has an invalid shape: {0}")]
    InvalidShape(String),
    #[error("meta.{0} must be a non-empty string")]
    EmptyField(&'static str),
    #[error(
        "meta.name {0:?} must be kebab-case: lowercase ASCII letters or digits separated by single hyphens"
    )]
    InvalidName(String),
    #[error("meta.{field} must be at most {max} bytes (got {actual})")]
    TooLong {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    #[error("meta.phases may hold at most {MAX_PHASES} entries (got {0})")]
    TooManyPhases(usize),
    #[error("meta.phases repeats the title {0:?}")]
    DuplicatePhase(String),
}

/// Kebab-case (`^[a-z0-9]+(-[a-z0-9]+)*$`) and at most [`MAX_NAME_BYTES`] bytes.
pub fn is_valid_workflow_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name.split('-').all(|segment| {
            !segment.is_empty()
                && segment
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

impl WorkflowMeta {
    pub fn validate(&self) -> Result<(), MetaError> {
        require_non_empty(FIELD_NAME, &self.name)?;
        require_within(FIELD_NAME, &self.name, MAX_NAME_BYTES)?;
        if !is_valid_workflow_name(&self.name) {
            return Err(MetaError::InvalidName(self.name.clone()));
        }
        require_non_empty(FIELD_DESCRIPTION, &self.description)?;
        require_within(FIELD_DESCRIPTION, &self.description, MAX_DESCRIPTION_BYTES)?;
        if let Some(when_to_use) = &self.when_to_use {
            require_within(FIELD_WHEN_TO_USE, when_to_use, MAX_WHEN_TO_USE_BYTES)?;
        }
        if self.phases.len() > MAX_PHASES {
            return Err(MetaError::TooManyPhases(self.phases.len()));
        }
        let mut titles = HashSet::with_capacity(self.phases.len());
        for phase in &self.phases {
            require_non_empty(FIELD_PHASE_TITLE, &phase.title)?;
            require_within(FIELD_PHASE_TITLE, &phase.title, MAX_PHASE_TITLE_BYTES)?;
            if let Some(detail) = &phase.detail {
                require_within(FIELD_PHASE_DETAIL, detail, MAX_PHASE_DETAIL_BYTES)?;
            }
            if !titles.insert(phase.title.as_str()) {
                return Err(MetaError::DuplicatePhase(phase.title.clone()));
            }
        }
        Ok(())
    }
}

fn require_non_empty(field: &'static str, value: &str) -> Result<(), MetaError> {
    if value.trim().is_empty() {
        return Err(MetaError::EmptyField(field));
    }
    Ok(())
}

fn require_within(field: &'static str, value: &str, max: usize) -> Result<(), MetaError> {
    if value.len() > max {
        return Err(MetaError::TooLong {
            field,
            max,
            actual: value.len(),
        });
    }
    Ok(())
}

/// Reads the header without running the script: the first statement must be
/// `let meta = <literal>` built only from string, array, and map literals.
#[cfg(feature = "rhai")]
pub fn parse_meta(source: &str) -> Result<WorkflowMeta, MetaError> {
    use caudra_script::{HeaderError, SandboxLimits, ScalarKind, parse_header};

    use crate::run::EngineLimits;

    fn meta_error(error: HeaderError) -> MetaError {
        match error {
            HeaderError::Parse(error) => MetaError::Parse(error.to_string()),
            HeaderError::NotFirst => MetaError::MetaNotFirst,
            HeaderError::NonLiteral { position } | HeaderError::NonFinite { position } => {
                MetaError::NonLiteral {
                    position: position.to_string(),
                }
            }
        }
    }

    if source.len() > MAX_SOURCE_BYTES {
        return Err(MetaError::SourceTooLarge {
            bytes: source.len(),
            max: MAX_SOURCE_BYTES,
        });
    }
    let literal = parse_header(
        source,
        &SandboxLimits::from(&EngineLimits::default()),
        META_VARIABLE,
        &[ScalarKind::String],
    )
    .map_err(meta_error)?;
    let meta: WorkflowMeta = serde_json::from_value(literal)
        .map_err(|error| MetaError::InvalidShape(error.to_string()))?;
    meta.validate()?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    #[test_case("deep-research" => true; "kebab")]
    #[test_case("a1" => true; "alnum")]
    #[test_case("Deep_Research" => false; "uppercase_and_underscore")]
    #[test_case("-x" => false; "leading_hyphen")]
    #[test_case("x-" => false; "trailing_hyphen")]
    #[test_case("a--b" => false; "double_hyphen")]
    #[test_case("" => false; "empty")]
    fn workflow_name_validity(name: &str) -> bool {
        is_valid_workflow_name(name)
    }

    #[test]
    fn workflow_name_length_bound() {
        assert!(is_valid_workflow_name(&"a".repeat(MAX_NAME_BYTES)));
        assert!(!is_valid_workflow_name(&"a".repeat(MAX_NAME_BYTES + 1)));
    }

    #[test]
    fn validate_rejects_duplicate_phase_titles() {
        let meta = WorkflowMeta {
            name: "demo".into(),
            description: "d".into(),
            when_to_use: None,
            phases: vec![
                PhaseMeta {
                    title: "Scan".into(),
                    detail: None,
                },
                PhaseMeta {
                    title: "Scan".into(),
                    detail: Some("again".into()),
                },
            ],
        };
        assert_eq!(
            meta.validate(),
            Err(MetaError::DuplicatePhase("Scan".into()))
        );
    }
}

#[cfg(all(test, feature = "rhai"))]
mod parse_tests {
    use test_case::test_case;

    use super::*;

    fn script(meta_literal: &str) -> String {
        format!("let meta = {meta_literal};\nlet x = agent(\"hi\");\n")
    }

    #[test]
    fn parses_valid_meta_without_running_the_script() {
        let meta = parse_meta(&script(
            r#"#{
                name: "demo",
                description: "does things",
                when_to_use: "when asked",
                phases: [#{ title: "Scan" }, #{ title: "Fix", detail: "apply" }],
            }"#,
        ))
        .expect("valid meta");
        assert_eq!(meta.name, "demo");
        assert_eq!(meta.when_to_use.as_deref(), Some("when asked"));
        assert_eq!(meta.phases.len(), 2);
        assert_eq!(meta.phases[1].detail.as_deref(), Some("apply"));
    }

    #[test]
    fn comments_before_meta_are_allowed() {
        let source =
            "// header\n/* block\ncomment */\nlet meta = #{ name: \"n\", description: \"d\" };";
        assert_eq!(parse_meta(source).map(|meta| meta.name), Ok("n".into()));
    }

    #[test_case(r#"#{ description: "d" }"# => matches Err(MetaError::InvalidShape(_)); "missing_name")]
    #[test_case(r#"#{ name: "n" }"# => matches Err(MetaError::InvalidShape(_)); "missing_description")]
    #[test_case(r#"#{ name: "n", description: "d", extra: "x" }"# => matches Err(MetaError::InvalidShape(_)); "unknown_field")]
    #[test_case(r#"#{ name: "Deep_Research", description: "d" }"# => matches Err(MetaError::InvalidName(_)); "uppercase_name")]
    #[test_case(r#"#{ name: "-x", description: "d" }"# => matches Err(MetaError::InvalidName(_)); "leading_hyphen_name")]
    #[test_case(r#"#{ name: "", description: "d" }"# => Err(MetaError::EmptyField("name")); "empty_name")]
    #[test_case(r#"#{ name: "n", description: " " }"# => Err(MetaError::EmptyField("description")); "blank_description")]
    #[test_case(r#"#{ name: "n", description: "d", phases: [#{ title: "A" }, #{ title: "A" }] }"# => Err(MetaError::DuplicatePhase("A".into())); "duplicate_phases")]
    #[test_case(r#"#{ name: "n", description: "d" + "e" }"# => matches Err(MetaError::NonLiteral { .. }); "non_literal_value")]
    #[test_case(r#"#{ name: "n", description: 42 }"# => matches Err(MetaError::NonLiteral { .. }); "integer_value")]
    #[test_case(r#"#{ name: "n", description: args.x }"# => matches Err(MetaError::NonLiteral { .. }); "variable_value")]
    #[test_case(r#""just a string""# => matches Err(MetaError::InvalidShape(_)); "not_a_map")]
    fn rejects_bad_meta(meta_literal: &str) -> Result<WorkflowMeta, MetaError> {
        parse_meta(&script(meta_literal))
    }

    #[test_case("let x = 1;\nlet meta = #{ name: \"n\", description: \"d\" };"; "later_statement")]
    #[test_case("const meta = #{ name: \"n\", description: \"d\" };"; "const_binding")]
    #[test_case("let other = 1; let meta = 2;"; "not_a_map_binding_first")]
    #[test_case(""; "empty_source")]
    fn rejects_meta_not_first(source: &str) {
        assert_eq!(parse_meta(source), Err(MetaError::MetaNotFirst));
    }

    #[test]
    fn rejects_syntax_errors() {
        assert!(matches!(
            parse_meta("let meta = #{ name: \"n\", description: \"d\" }; fn {"),
            Err(MetaError::Parse(_))
        ));
    }

    #[test]
    fn rejects_oversized_source() {
        let padding = "x".repeat(MAX_SOURCE_BYTES);
        let source = format!("let meta = #{{ name: \"n\", description: \"d\" }};\n// {padding}");
        assert!(matches!(
            parse_meta(&source),
            Err(MetaError::SourceTooLarge {
                max: MAX_SOURCE_BYTES,
                ..
            })
        ));
    }

    #[test]
    fn rejects_oversized_fields_and_phase_counts() {
        let name = "a".repeat(MAX_NAME_BYTES + 1);
        assert!(matches!(
            parse_meta(&script(&format!(
                r#"#{{ name: "{name}", description: "d" }}"#
            ))),
            Err(MetaError::TooLong { field: "name", .. })
        ));
        let phases = vec![r#"#{ title: "p" }"#; MAX_PHASES + 1].join(",");
        assert_eq!(
            parse_meta(&script(&format!(
                r#"#{{ name: "n", description: "d", phases: [{phases}] }}"#
            ))),
            Err(MetaError::TooManyPhases(MAX_PHASES + 1))
        );
    }
}
