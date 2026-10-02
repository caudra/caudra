use super::{
    COMMAND_EXACT_PREFIX, ComposedAnswerError, ComposedRow, PermissionArgumentConstraint,
    PermissionLifetime, PermissionRequest, PermissionResourceConstraint,
    PermissionResourceSelector, PermissionRowGrant, PermissionRuleOption,
    StructuredPermissionEffect, StructuredPermissionRule, grade_command_pattern,
    permission_rule_covers_resource, resource_constraint, safe_summary,
};
use super::{COMMAND_TEMPLATE_PREFIX, PatternDefinition};
use crate::permissions::pattern_matching::CompiledPattern;

impl PermissionRequest {
    pub fn option_rule(
        &self,
        option_id: &str,
        lifetime: PermissionLifetime,
    ) -> Option<StructuredPermissionRule> {
        let option = self.options.iter().find(|option| option.id == option_id)?;
        if !option.allowed_lifetimes.contains(&lifetime) {
            return None;
        }
        let mut rule = option.rule.clone();
        rule.lifetime = lifetime;
        Some(rule)
    }
    /// One rule per remembered row, each for its row's own lifetime.
    ///
    /// Rows left `None` contribute nothing, which is what makes them this call
    /// only — the call itself proceeds on the answer, not on the rules.
    ///
    /// A written pattern is authority the request never offered, so it is
    /// admitted only against the command on its own row and only for a lifetime
    /// that row's offered rung still allows. That second gate is what keeps
    /// plan mode contained row by row without knowing anything about plans.
    pub fn composed_rules(
        &self,
        rows: &[Option<ComposedRow>],
    ) -> Result<Vec<StructuredPermissionRule>, ComposedAnswerError> {
        if rows.len() != self.resources.len() {
            return Err(ComposedAnswerError::RowCount {
                named: rows.len(),
                resources: self.resources.len(),
            });
        }
        let subsumed = self.subsumed_rows(rows);
        let mut rules = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let Some(row) = row else {
                continue;
            };
            if row.lifetime == PermissionLifetime::Once {
                return Err(ComposedAnswerError::RememberedOnce);
            }
            // Validated before pruning: a grant the request never offered is an
            // error whether or not it would have been kept.
            let (option, resources) = self.row_reach(index, &row.grant)?;
            if !option.allowed_lifetimes.contains(&row.lifetime) {
                return Err(ComposedAnswerError::LifetimeWithdrawn(option.id.clone()));
            }
            if subsumed[index].is_some() {
                continue;
            }
            let rule = StructuredPermissionRule {
                subject: self.subject.clone(),
                executor: self.executor.clone(),
                resources,
                arguments: PermissionArgumentConstraint::Unconstrained,
                lifetime: row.lifetime.clone(),
                effect: StructuredPermissionEffect::Allow,
                family: option.rule.family,
            };
            if !permission_rule_covers_resource(&rule, self, &self.resources[index]) {
                return Err(ComposedAnswerError::Uncovered);
            }
            rules.push(rule);
        }
        Ok(rules)
    }
    /// Whether a row's written pattern is broad enough to need confirming.
    pub fn broad_written_grant(&self, index: usize, grant: &PermissionRowGrant) -> bool {
        let PermissionRowGrant::Written(pattern) = grant else {
            return false;
        };
        self.resources.get(index).is_some_and(|resource| {
            grade_command_pattern(pattern, &resource.value)
                .is_ok_and(|grade| grade.confirmation.is_some())
        })
    }
    /// Plan mode keeps every broad rung to this conversation, and a written
    /// pattern rides the exact rung, so its own breadth is checked here.
    pub(crate) fn contain_rows_to_the_plan(
        &self,
        rows: &[Option<ComposedRow>],
    ) -> Result<(), ComposedAnswerError> {
        let outlives = rows.iter().enumerate().find(|(index, row)| {
            row.as_ref().is_some_and(|row| {
                row.lifetime > PermissionLifetime::Conversation
                    && self.broad_written_grant(*index, &row.grant)
            })
        });
        match outlives {
            Some((index, _)) => Err(ComposedAnswerError::LifetimeWithdrawn(format!(
                "{COMMAND_EXACT_PREFIX}{index}"
            ))),
            None => Ok(()),
        }
    }
    /// The option one row's grant rides on, and the constraints it names.
    ///
    /// Lifetime is deliberately not checked here: how far a grant reaches is a
    /// question about resources alone.
    pub(super) fn row_reach(
        &self,
        index: usize,
        grant: &PermissionRowGrant,
    ) -> Result<(&PermissionRuleOption, Vec<PermissionResourceConstraint>), ComposedAnswerError>
    {
        let offered = |id: &str| {
            self.options
                .iter()
                .find(|option| {
                    option.id == id
                        && option.rule.effect == StructuredPermissionEffect::Allow
                        && option.group.as_ref().and_then(|group| group.resource) == Some(index)
                })
                .ok_or_else(|| ComposedAnswerError::NotOffered(id.to_owned()))
        };
        let resource = self
            .resources
            .get(index)
            .ok_or(ComposedAnswerError::Uncovered)?;
        match grant {
            PermissionRowGrant::Offered(id) => {
                let option = offered(id)?;
                Ok((option, option.rule.resources.clone()))
            }
            PermissionRowGrant::Pattern {
                option_id,
                definition,
            } => {
                CompiledPattern::compile(definition)
                    .map_err(|error| ComposedAnswerError::Template(error.to_string()))?;
                let option = self.options.iter().find(|option| {
                    option.id == *option_id && option.id.starts_with(COMMAND_TEMPLATE_PREFIX)
                        && option.rule.effect == StructuredPermissionEffect::Allow
                        && option.group.as_ref().and_then(|group| group.resource) == Some(index)
                        && matches!(option.rule.resources.as_slice(), [constraint]
                            if matches!(&constraint.selector, PermissionResourceSelector::CommandTemplate { definition: original }
                                if preserves_template_structure(original, definition)))
                }).ok_or(ComposedAnswerError::TemplateNotOffered)?;
                let mut resources = option.rule.resources.clone();
                resources[0].selector = PermissionResourceSelector::CommandTemplate {
                    definition: definition.clone(),
                };
                Ok((option, resources))
            }
            PermissionRowGrant::Written(pattern) => {
                let option = offered(&format!("{COMMAND_EXACT_PREFIX}{index}"))?;
                grade_command_pattern(pattern, &resource.value).map_err(|fault| {
                    ComposedAnswerError::Pattern {
                        command: safe_summary(&resource.value),
                        fault,
                    }
                })?;
                Ok((
                    option,
                    vec![PermissionResourceConstraint {
                        selector: PermissionResourceSelector::CommandPattern {
                            pattern: pattern.clone(),
                        },
                        ..resource_constraint(resource)
                    }],
                ))
            }
        }
    }
    /// One row's grant as a rule of its own, for asking how far it reaches. The
    /// lifetime is the caller's to set; reach does not depend on it.
    pub(super) fn row_rule(
        &self,
        index: usize,
        grant: &PermissionRowGrant,
    ) -> Option<StructuredPermissionRule> {
        let (option, resources) = self.row_reach(index, grant).ok()?;
        Some(StructuredPermissionRule {
            subject: self.subject.clone(),
            executor: self.executor.clone(),
            resources,
            arguments: PermissionArgumentConstraint::Unconstrained,
            lifetime: PermissionLifetime::Once,
            effect: StructuredPermissionEffect::Allow,
            family: option.rule.family,
        })
    }
    /// For each row, the row whose grant already covers it.
    ///
    /// A grant that reaches no further than another row's, for no longer,
    /// contributes nothing: the prompt should not claim it does, and storage
    /// should not keep it. Row `j` dominates row `i` when `j` reaches resource
    /// `i` and lasts at least as long. Row `i` is subsumed by `j` when `j`
    /// dominates it and either `i` does not dominate `j` — a redundant narrower
    /// or shorter row, wherever it sits — or `j` comes first, which is what
    /// settles two rows that dominate each other so that exactly one survives.
    pub fn subsumed_rows(&self, rows: &[Option<ComposedRow>]) -> Vec<Option<usize>> {
        let reach: Vec<Option<(StructuredPermissionRule, &PermissionLifetime)>> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| {
                let row = row.as_ref()?;
                Some((self.row_rule(index, &row.grant)?, &row.lifetime))
            })
            .collect();
        let dominates = |row: usize, other: usize| {
            let (Some(Some((rule, lifetime))), Some(Some((_, other_lifetime))), Some(resource)) =
                (reach.get(row), reach.get(other), self.resources.get(other))
            else {
                return false;
            };
            lifetime >= other_lifetime && permission_rule_covers_resource(rule, self, resource)
        };
        (0..rows.len())
            .map(|index| {
                (0..rows.len()).find(|&other| {
                    other != index
                        && dominates(other, index)
                        && (!dominates(index, other) || other < index)
                })
            })
            .collect()
    }
}

fn preserves_template_structure(original: &PatternDefinition, edited: &PatternDefinition) -> bool {
    original.version == edited.version
        && original.context == edited.context
        && original.argv == edited.argv
        && original.slots.len() == edited.slots.len()
        && original.slots.iter().all(|slot| {
            edited
                .slots
                .iter()
                .any(|edited| slot.id == edited.id && slot.option_like == edited.option_like)
        })
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use crate::permissions::enforce::contain_authority_to_the_plan;
    use crate::permissions::structured::tests::{
        BROAD_ASK, BUILTIN_ALLOW_AUTHORITY, NARROW_ALLOW, command_resource, composed, covered_at,
        offered, remembered, twin_command_request, two_command_request,
    };
    use crate::permissions::structured::{
        ComposedAnswerError, ComposedRow, EXACT_COMMAND_CHIP, PatternFault,
        PermissionArgumentConstraint, PermissionLifetime, PermissionResourceSelector,
        PermissionRowGrant, RuleOrigin, permission_rule_covers_resource, resource_constraint,
    };

    fn row(id: &str, lifetime: PermissionLifetime) -> Option<ComposedRow> {
        Some(ComposedRow {
            grant: PermissionRowGrant::Offered(id.into()),
            lifetime,
        })
    }

    /// Two rows reaching the same pattern reach each other, so exactly one has
    /// to survive or the answer files the same rule twice.
    #[test]
    fn rows_that_reach_each_other_leave_only_the_first_standing() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&remembered(vec![
                offered("command_pattern_0"),
                offered("command_pattern_1")
            ])),
            vec![None, Some(0)]
        );
    }

    /// A row pinned to its own command contributes nothing under a row that
    /// reaches it, whichever way round they sit.
    #[test]
    fn a_narrower_row_is_subsumed_from_either_direction() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&remembered(vec![
                offered("command_exact_0"),
                offered("command_pattern_1")
            ])),
            vec![Some(1), None]
        );
        assert_eq!(
            request.subsumed_rows(&remembered(vec![
                offered("command_pattern_0"),
                offered("command_exact_1")
            ])),
            vec![None, Some(0)]
        );
    }

    /// A row is pruned only by one that covers it for at least as long, so a
    /// conversation-wide pattern never swallows a command kept for the project.
    #[test_case(PermissionLifetime::Conversation, PermissionLifetime::Project, vec![None, None]; "a_shorter_cover_keeps_the_row")]
    #[test_case(PermissionLifetime::Project, PermissionLifetime::Conversation, vec![None, Some(0)]; "a_longer_cover_prunes_it")]
    #[test_case(PermissionLifetime::Global, PermissionLifetime::Project, vec![None, Some(0)]; "all_projects_outlasts_one")]
    fn a_row_is_pruned_only_by_a_row_that_lasts_as_long(
        cover: PermissionLifetime,
        covered: PermissionLifetime,
        expected: Vec<Option<usize>>,
    ) {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&[
                row("command_pattern_0", cover),
                row("command_exact_1", covered)
            ]),
            expected
        );
    }

    /// Two rows reaching each other settle on the one that lasts longer, not on
    /// the one that comes first.
    #[test]
    fn rows_that_reach_each_other_keep_the_longer_lived() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&[
                row("command_pattern_0", PermissionLifetime::Conversation),
                row("command_pattern_1", PermissionLifetime::Project)
            ]),
            vec![Some(1), None]
        );
    }

    /// Narrowing the covering row hands the subsumed row its own choice back,
    /// so nothing has to be remembered across the change.
    #[test]
    fn narrowing_the_covering_row_leaves_nothing_subsumed() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&remembered(vec![
                offered("command_exact_0"),
                offered("command_exact_1")
            ])),
            vec![None, None]
        );
    }

    /// A row granting nothing neither subsumes nor is subsumed: it is not part
    /// of the answer at all.
    #[test]
    fn a_row_that_grants_nothing_stays_out_of_subsumption() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&remembered(vec![None, offered("command_pattern_1")])),
            vec![None, None]
        );
    }

    /// Each row's rule lasts as long as that row asked, so one answer can keep
    /// one command for the project and another for this conversation only.
    #[test]
    fn each_row_files_for_its_own_lifetime() {
        let request = two_command_request();

        let lifetimes: Vec<_> = request
            .composed_rules(&[
                row("command_exact_0", PermissionLifetime::Project),
                row("command_exact_1", PermissionLifetime::Conversation),
            ])
            .unwrap()
            .into_iter()
            .map(|rule| rule.lifetime)
            .collect();

        assert_eq!(
            lifetimes,
            [
                PermissionLifetime::Project,
                PermissionLifetime::Conversation
            ]
        );
    }

    /// "This time only" is a row left empty; a row that names it is malformed.
    #[test]
    fn a_row_lifetime_of_once_is_rejected() {
        let request = two_command_request();

        assert_eq!(
            request.composed_rules(&[row("command_exact_0", PermissionLifetime::Once), None]),
            Err(ComposedAnswerError::RememberedOnce)
        );
    }

    /// While planning, each row is held on its own to what a plan may keep: a
    /// narrow rung for the project, a broad one for the conversation, and none
    /// for all projects. One row past that refuses the whole answer even when
    /// its sibling stays in the conversation.
    #[test_case("command_exact_1", PermissionLifetime::Conversation, true; "conversation_rows_pass")]
    #[test_case("command_exact_1", PermissionLifetime::Project, true; "a_narrow_project_row_passes")]
    #[test_case("command_exact_1", PermissionLifetime::Global, false; "an_all_projects_row_is_refused")]
    #[test_case("command_prefix_1_1", PermissionLifetime::Conversation, true; "a_broad_conversation_row_passes")]
    #[test_case("command_prefix_1_1", PermissionLifetime::Project, false; "a_broad_project_row_is_refused")]
    fn plan_mode_contains_each_row_on_its_own(
        second: &str,
        lifetime: PermissionLifetime,
        accepted: bool,
    ) {
        let mut request = two_command_request();
        contain_authority_to_the_plan(&mut request);

        assert_eq!(
            request
                .composed_rules(&[
                    row("command_exact_0", PermissionLifetime::Conversation),
                    row(second, lifetime),
                ])
                .is_ok(),
            accepted
        );
    }

    /// Two rows reaching the same pattern once produced one rule holding the
    /// same constraint twice; as separate rules they would be twins that a
    /// single revoke could not remove together.
    #[test]
    fn rows_that_reach_each_other_file_one_rule() {
        let request = twin_command_request();

        let rules = composed(
            &request,
            vec![offered("command_pattern_0"), offered("command_pattern_1")],
        )
        .unwrap();

        assert_eq!(rules.len(), 1);
        assert!(
            request
                .resources
                .iter()
                .all(|resource| permission_rule_covers_resource(&rules[0], &request, resource))
        );
    }

    /// Each surviving row is a rule of its own, so one command can be revoked
    /// without touching the others.
    #[test]
    fn every_surviving_row_files_a_rule_of_its_own() {
        let request = two_command_request();

        let rules = composed(
            &request,
            vec![offered("command_exact_0"), offered("command_exact_1")],
        )
        .unwrap();

        assert_eq!(rules.len(), 2);
        for (index, rule) in rules.iter().enumerate() {
            assert_eq!(rule.resources.len(), 1);
            assert!(permission_rule_covers_resource(
                rule,
                &request,
                &request.resources[index]
            ));
            assert!(!permission_rule_covers_resource(
                rule,
                &request,
                &request.resources[1 - index]
            ));
        }
    }

    /// Coverage that expires sooner than the grant is not enough: pruning there
    /// would drop the grant and let the authority disappear with the session.
    #[test]
    fn a_row_covered_less_durably_than_the_grant_still_files() {
        let mut request = two_command_request();
        covered_at(&mut request, 0, RuleOrigin::Conversation, NARROW_ALLOW);

        assert_eq!(
            request
                .composed_rules(&[row("command_exact_0", PermissionLifetime::Project), None])
                .map(|rules| rules.len()),
            Ok(1)
        );
    }

    /// The builtin allowlist is a default consulted only where no rule speaks,
    /// so a grant over it is a real grant however durable it looks.
    #[test]
    fn a_row_covered_only_by_the_builtin_allowlist_still_files() {
        let mut request = two_command_request();
        covered_at(
            &mut request,
            0,
            RuleOrigin::Builtin,
            BUILTIN_ALLOW_AUTHORITY,
        );

        assert_eq!(
            request
                .composed_rules(&[row("command_exact_0", PermissionLifetime::Global), None])
                .map(|rules| rules.len()),
            Ok(1)
        );
    }

    /// Widening a covered row reaches commands the coverage may not, so it is
    /// a real grant even though this command was already allowed.
    #[test]
    fn widening_a_covered_row_still_files() {
        let mut request = two_command_request();
        covered_at(&mut request, 0, RuleOrigin::Project, EXACT_COMMAND_CHIP);
        let rows = [row("command_pattern_0", PermissionLifetime::Project), None];

        assert_eq!(
            request.composed_rules(&rows).map(|rules| rules.len()),
            Ok(1)
        );

        covered_at(&mut request, 0, RuleOrigin::Project, NARROW_ALLOW);
        assert_eq!(request.composed_rules(&rows).unwrap().len(), 1);
    }

    /// A pattern typed for one row still reaches the other, so it subsumes the
    /// same way an offered rung does.
    #[test]
    fn a_written_pattern_subsumes_the_row_it_reaches() {
        let request = twin_command_request();

        assert_eq!(
            request.subsumed_rows(&remembered(vec![
                Some(PermissionRowGrant::Written(BROAD_ASK.into())),
                offered("command_exact_1"),
            ])),
            vec![None, Some(0)]
        );
    }

    #[test]
    fn a_composed_answer_keeps_each_row_at_the_breadth_it_chose() {
        let request = two_command_request();
        let rules = composed(
            &request,
            vec![
                Some(PermissionRowGrant::Offered("command_pattern_0".into())),
                Some(PermissionRowGrant::Offered("command_exact_1".into())),
            ],
        )
        .unwrap();

        assert_eq!(rules.len(), 2);
        assert!(
            rules
                .iter()
                .all(|rule| matches!(rule.arguments, PermissionArgumentConstraint::Unconstrained))
        );
        assert!(matches!(
            &rules[0].resources[0].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "git status *"
        ));
        assert_eq!(
            rules[1].resources[0].selector,
            resource_constraint(&request.resources[1]).selector
        );
        // The widened row reaches beyond what was reviewed; the pinned one does not.
        assert!(permission_rule_covers_resource(
            &rules[0],
            &request,
            &command_resource("git status --porcelain", "/project")
        ));
        assert!(!permission_rule_covers_resource(
            &rules[1],
            &request,
            &command_resource("cargo test --lib", "/project")
        ));
    }

    /// A row left ungranted must contribute no constraint. An empty constraint
    /// list is an unrestricted rule, so the answer has to store nothing at all
    /// rather than store a rule that happens to name nothing.
    #[test]
    fn a_row_left_ungranted_is_absent_from_the_stored_rule() {
        let request = two_command_request();
        let rules = composed(
            &request,
            vec![
                Some(PermissionRowGrant::Offered("command_exact_0".into())),
                None,
            ],
        )
        .unwrap();

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].resources.len(), 1);
        assert!(permission_rule_covers_resource(
            &rules[0],
            &request,
            &request.resources[0]
        ));
        assert!(!permission_rule_covers_resource(
            &rules[0],
            &request,
            &request.resources[1]
        ));
        assert_eq!(composed(&request, vec![None, None]), Ok(Vec::new()));
    }

    #[test]
    fn a_written_pattern_is_admitted_only_against_its_own_command() {
        let request = two_command_request();
        let rules = composed(
            &request,
            vec![
                None,
                Some(PermissionRowGrant::Written("cargo test *".into())),
            ],
        )
        .unwrap();

        assert!(matches!(
            &rules[0].resources[0].selector,
            PermissionResourceSelector::CommandPattern { pattern } if pattern == "cargo test *"
        ));
        assert_eq!(
            composed(
                &request,
                vec![
                    Some(PermissionRowGrant::Written("cargo test *".into())),
                    None
                ],
            ),
            Err(ComposedAnswerError::Pattern {
                command: "git status --short".into(),
                fault: PatternFault::DoesNotMatch,
            })
        );
    }

    #[test]
    fn a_row_may_not_borrow_another_rows_rung() {
        let request = two_command_request();

        assert_eq!(
            composed(
                &request,
                vec![
                    Some(PermissionRowGrant::Offered("command_exact_1".into())),
                    None,
                ],
            ),
            Err(ComposedAnswerError::NotOffered("command_exact_1".into()))
        );
        assert_eq!(
            composed(&request, vec![None]),
            Err(ComposedAnswerError::RowCount {
                named: 1,
                resources: 2,
            })
        );
    }

    /// Withdrawing a row's lifetimes is how plan mode contains authority, and a
    /// written pattern must not be a way around it.
    #[test]
    fn a_written_pattern_inherits_the_lifetimes_its_row_still_allows() {
        let mut request = two_command_request();
        for option in &mut request.options {
            option.allowed_lifetimes.retain(|lifetime| {
                !matches!(
                    lifetime,
                    PermissionLifetime::Project | PermissionLifetime::Global
                )
            });
        }
        let written = |lifetime| {
            [
                None,
                Some(ComposedRow {
                    grant: PermissionRowGrant::Written("cargo test *".into()),
                    lifetime,
                }),
            ]
        };

        assert!(
            request
                .composed_rules(&written(PermissionLifetime::Conversation))
                .is_ok()
        );
        assert_eq!(
            request.composed_rules(&written(PermissionLifetime::Project)),
            Err(ComposedAnswerError::LifetimeWithdrawn(
                "command_exact_1".into()
            ))
        );
    }
    #[test_case(PermissionLifetime::Project; "project")]
    #[test_case(PermissionLifetime::Conversation; "conversation")]
    fn presentation_coverage_does_not_prune_selected_authority(lifetime: PermissionLifetime) {
        let mut request = two_command_request();
        covered_at(&mut request, 0, RuleOrigin::Project, NARROW_ALLOW);
        assert_eq!(
            request
                .composed_rules(&[row("command_exact_0", lifetime), None])
                .unwrap()
                .len(),
            1
        );
    }
}
