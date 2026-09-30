//! The `caudra-workflow-dev` builtin skill: the authoring guide bundled with
//! `caudra-workflow`, offered through the native `skill` tool so the model can
//! write a workflow for the session it is in.

use caudra_workflow::WORKFLOW_SKILL;

use crate::tools::native::skill::{BuiltinSkill, parse_frontmatter};

const NAME_FIELD: &str = "name";
const DESCRIPTION_FIELD: &str = "description";
const FALLBACK_NAME: &str = "caudra-workflow-dev";

/// The frontmatter names the skill, so the guide is the single source for how
/// it is listed. The body is static, so resolution never touches the disk.
pub fn workflow_dev_skill() -> BuiltinSkill {
    let (frontmatter, body) = parse_frontmatter(WORKFLOW_SKILL);
    let body = body.to_owned();
    BuiltinSkill {
        name: frontmatter
            .get(NAME_FIELD)
            .cloned()
            .unwrap_or_else(|| FALLBACK_NAME.to_owned()),
        description: frontmatter
            .get(DESCRIPTION_FIELD)
            .cloned()
            .unwrap_or_default(),
        resolve: Box::new(move || (body.clone(), None)),
        pages: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESCRIBED_MSG: &str = "the skill needs a description for the model to pick it by";
    const HEADER_LEAKED_MSG: &str = "the frontmatter must not be part of the body";

    #[test]
    fn the_skill_is_named_and_described_by_its_frontmatter() {
        let skill = workflow_dev_skill();
        assert_eq!(skill.name, FALLBACK_NAME);
        assert!(!skill.description.is_empty(), "{DESCRIBED_MSG}");

        let (body, reference) = (skill.resolve)();
        assert!(reference.is_none());
        assert!(
            body.starts_with("# Writing Caudra workflows"),
            "{HEADER_LEAKED_MSG}"
        );
        assert!(
            !body.contains("name: caudra-workflow-dev"),
            "{HEADER_LEAKED_MSG}"
        );
    }
}
