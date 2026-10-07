//! Neutral catalog types: the scripts discovery found, their trust, and the files it could not
//! load. Discovery reads the filesystem and lives with the runtime.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::meta::{ArmMode, AutomationMeta, Trigger};

/// Part of every source digest, so a script approved for one interpreter is not trusted by
/// another.
pub const AUTOMATION_LANGUAGE_VERSION: u32 = 1;
pub const AUTOMATION_ABI_VERSION: u32 = 1;
pub const STEM_MISMATCH: &str = "the file name must equal meta.name";
pub const ALWAYS_OUTSIDE_USER_SCOPE: &str =
    "arm: \"always\" is allowed only in user scope; arm a project script by hand";
pub const ALWAYS_WITH_SCHEDULE: &str = "arm: \"always\" runs this schedule in every open session; \
     consider arming it by hand in one session instead";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trust {
    /// A user script, trusted where it lives.
    Location,
    /// A project script whose exact digest was approved.
    Approved,
    /// A project script whose digest still needs approval.
    Required,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CatalogEntry {
    pub meta: AutomationMeta,
    pub scope: Scope,
    pub path: PathBuf,
    /// The trust digest of the file: its exact bytes framed with the automation domain and the
    /// language and ABI versions, as caudra-storage's `TrustDomain::Automation` computes it.
    pub digest: String,
    pub trust: Trust,
    pub warnings: Vec<String>,
    /// The scopes whose script of the same name this one hides.
    pub shadowed: Vec<Scope>,
}

/// A file that stays listed with the reason it cannot load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidEntry {
    /// The file stem, since the header may not parse.
    pub name: String,
    pub scope: Scope,
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AutomationCatalog {
    /// One per name: a project script hides a user script of the same name, and lists its
    /// scope in `shadowed`.
    pub entries: Vec<CatalogEntry>,
    pub invalid: Vec<InvalidEntry>,
}

impl Trust {
    pub const fn assess(scope: Scope, digest_approved: bool) -> Self {
        match (scope, digest_approved) {
            (Scope::User, _) => Self::Location,
            (Scope::Project, true) => Self::Approved,
            (Scope::Project, false) => Self::Required,
        }
    }

    pub const fn is_trusted(self) -> bool {
        !matches!(self, Self::Required)
    }
}

impl AutomationCatalog {
    pub fn find(&self, name: &str) -> Option<&CatalogEntry> {
        self.entries.iter().find(|entry| entry.meta.name == name)
    }
}

/// The rules that tie a parsed header to its file and scope. `Err` holds the reason the file is
/// invalid; `Ok` holds warnings for a loadable script.
pub fn check_entry(stem: &str, scope: Scope, meta: &AutomationMeta) -> Result<Vec<String>, String> {
    if stem != meta.name {
        return Err(format!(
            "{STEM_MISMATCH}: the file is {stem:?} and meta.name is {:?}",
            meta.name
        ));
    }
    if meta.arm != ArmMode::Always {
        return Ok(Vec::new());
    }
    if scope != Scope::User {
        return Err(ALWAYS_OUTSIDE_USER_SCOPE.to_owned());
    }
    Ok(meta
        .triggers
        .iter()
        .any(|trigger| matches!(trigger, Trigger::Schedule(_)))
        .then(|| ALWAYS_WITH_SCHEDULE.to_owned())
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use test_case::test_case;

    use super::*;
    use crate::meta::{AutomationLimits, Cadence, CatchUp, MessagingCaps, Schedule};

    const NAME: &str = "ci-watch";
    const OTHER_NAME: &str = "ci-triage";
    const DESCRIPTION: &str = "Poll CI";
    const POLL_EVERY: Duration = Duration::from_mins(10);

    fn meta(name: &str, arm: ArmMode, triggers: Vec<Trigger>) -> AutomationMeta {
        AutomationMeta {
            name: name.to_owned(),
            description: DESCRIPTION.to_owned(),
            triggers,
            args: Vec::new(),
            limits: AutomationLimits::default(),
            network: Vec::new(),
            secrets: Vec::new(),
            messaging: MessagingCaps::default(),
            workflows: Vec::new(),
            timezone: None,
            arm,
        }
    }

    fn polling() -> Vec<Trigger> {
        vec![Trigger::Schedule(Schedule {
            cadence: Cadence::Every(POLL_EVERY),
            catch_up: CatchUp::Once,
        })]
    }

    #[test_case(Scope::User, false => Trust::Location; "user_scope")]
    #[test_case(Scope::User, true => Trust::Location; "approved_user_scope")]
    #[test_case(Scope::Project, true => Trust::Approved; "approved_project")]
    #[test_case(Scope::Project, false => Trust::Required; "unapproved_project")]
    fn trust_follows_scope_and_approval(scope: Scope, approved: bool) -> Trust {
        Trust::assess(scope, approved)
    }

    #[test_case(Trust::Location => true; "location")]
    #[test_case(Trust::Approved => true; "approved")]
    #[test_case(Trust::Required => false; "required")]
    fn only_required_trust_blocks(trust: Trust) -> bool {
        trust.is_trusted()
    }

    #[test]
    fn the_stem_must_equal_the_name() {
        let error = check_entry(
            OTHER_NAME,
            Scope::User,
            &meta(NAME, ArmMode::Manual, polling()),
        )
        .expect_err(STEM_MISMATCH);
        assert!(error.starts_with(STEM_MISMATCH), "{error}");
    }

    #[test_case(Scope::User, ArmMode::Manual, vec![Trigger::Armed] => Ok(Vec::new()); "manual_user")]
    #[test_case(Scope::Project, ArmMode::Manual, polling() => Ok(Vec::new()); "manual_project_schedule")]
    #[test_case(Scope::User, ArmMode::Always, vec![Trigger::Armed] => Ok(Vec::new()); "always_user")]
    #[test_case(Scope::User, ArmMode::Always, polling() => Ok(vec![ALWAYS_WITH_SCHEDULE.to_owned()]); "always_user_schedule")]
    #[test_case(Scope::Project, ArmMode::Always, vec![Trigger::Armed] => Err(ALWAYS_OUTSIDE_USER_SCOPE.to_owned()); "always_project")]
    fn arming_always_is_for_user_scope(
        scope: Scope,
        arm: ArmMode,
        triggers: Vec<Trigger>,
    ) -> Result<Vec<String>, String> {
        check_entry(NAME, scope, &meta(NAME, arm, triggers))
    }
}
