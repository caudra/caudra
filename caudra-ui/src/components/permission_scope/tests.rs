use caudra_storage::permission_patterns::{
    ArgumentDomain, ArgumentRole, OptionLikePolicy, PATTERN_SCHEMA_VERSION, PatternContext,
    PatternDefinition, PatternSlot, PatternToken, SlotCombinations, SlotId,
};
use caudra_storage::permission_state::{
    PermissionArgumentConstraint, PermissionExecutorKind, PermissionLifetime,
    PermissionResourceAccess, PermissionResourceConstraint, PermissionResourceKind,
    PermissionResourceSelector, PermissionReview, PermissionReviewResource, PermissionReviewSource,
    PermissionRuleRecord, PermissionSubject, RemotePermissionIdentity, StructuredPermissionEffect,
    StructuredPermissionRule,
};
use caudra_workspace::{
    AuthenticatedPrincipalId, AuthorityIdentity, ProjectIdentity, ProjectKey, SourceTrustAnchor,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use std::collections::BTreeMap;
use std::fs::OpenOptions;
#[cfg(unix)]
use std::fs::{self, Permissions};
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::Arc;
use tempfile::{Builder, TempDir};
use test_case::test_case;
use unicode_width::UnicodeWidthStr;

use super::model::{
    ANY_RESOURCES, FIXED_VALUE, PROTECTED_RISK, SOURCE_RISK, ScopeModel, UNCONSTRAINED_INPUT,
    UNKNOWN_ROLE_RISK, rule_kind,
};
use super::view::{ALLOWED_COMBINATIONS, Disclosure, ScopeControl, ScopeView};
use crate::components::buffer_text;
use crate::components::permission_prompt::{assert_plain, buffer_rows};
use crate::theme;

const WORKDIR: &str = "/work/repository";
const DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DISPLAY_ONLY: &str = "/unverified/review/label";
const VALUE: &str = "two words";
const EMPTY_LITERAL: &str = "\"\"";
const SECRET: &str = "never-show-sensitive-payload";
const HEIGHT: u16 = 24;
const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const LONG_ARGV_COUNT: usize = 24;
const SHORT_SCOPE_HEIGHT: u16 = 10;
const EXECUTE_TARGET: &str = "1 Command/execute";
const PAGE_UP: &str = "[PgUp]";
const PAGE_DOWN: &str = "[PgDn]";
const ALL_OF_TARGET: &str = "Target 1 · Command · all of:";
const ANY_OF_TARGETS: &str = "Targets (1) · any of:";

fn mixed_risk_record() -> PermissionRuleRecord {
    let mut record = record(true);
    record.rule.arguments = PermissionArgumentConstraint::Unconstrained;
    record.rule.resources[0].protected = None;
    let PermissionResourceSelector::CommandTemplate { definition } =
        &mut record.rule.resources[0].selector
    else {
        unreachable!()
    };
    definition.argv[1] = PatternToken::Exact {
        value: "-n".into(),
        role: ArgumentRole::Unknown,
    };
    record
}

#[test_case(40; "narrow")]
#[test_case(80; "normal")]
#[test_case(140; "wide")]
fn scope_risks_are_complete_separate_visible_rows(width: u16) {
    for name in ["ayu_dark", "ayu_light"] {
        let mut unrestricted = record(false);
        unrestricted.rule.resources.clear();
        unrestricted.rule.arguments = PermissionArgumentConstraint::Unconstrained;
        for (record, expected) in [
            (
                mixed_risk_record(),
                vec![
                    UNCONSTRAINED_INPUT,
                    PROTECTED_RISK,
                    SOURCE_RISK,
                    UNKNOWN_ROLE_RISK,
                ],
            ),
            (
                unrestricted,
                vec![ANY_RESOURCES, UNCONSTRAINED_INPUT, SOURCE_RISK],
            ),
        ] {
            let model = ScopeModel::record(Arc::new(record));
            let theme = theme::load_by_name(name).unwrap();
            let buffer = render(&model, &mut ScopeView::default(), width, name);
            let mut warning_rows = Vec::new();
            for warning in expected {
                let y = (buffer.area.y..buffer.area.bottom())
                    .find(|y| {
                        let row: String = (buffer.area.x..buffer.area.right())
                            .map(|x| buffer[(x, *y)].symbol())
                            .collect();
                        row.trim_end() == warning
                    })
                    .unwrap();
                assert!(!warning_rows.contains(&y));
                warning_rows.push(y);
                for x in 0..warning.width() as u16 {
                    assert_eq!(buffer[(x, y)].fg, theme.tool_warning.fg.unwrap());
                }
            }
            assert!(warning_rows.windows(2).all(|rows| rows[0] < rows[1]));
        }
    }
}

#[test_case(40; "narrow")]
#[test_case(80; "normal")]
#[test_case(140; "wide")]
fn all_of_selection_does_not_break_repeated_slot_equality(width: u16) {
    let model = ScopeModel::record(Arc::new(record(true)));
    for name in ["ayu_dark", "ayu_light"] {
        let mut view = ScopeView::default();
        view.activate(ScopeControl::Slot(SlotId(1)));
        let slots = render(&model, &mut view, width, name);
        let linked: Vec<_> = view
            .hits
            .iter()
            .filter(|hit| hit.control == ScopeControl::Slot(SlotId(1)))
            .map(|hit| hit.area)
            .collect();
        assert_eq!(linked.len(), 2);
        view.activate(ScopeControl::Disclosure(Disclosure::Conditions));
        let conditions = render(&model, &mut view, width, name);
        assert!(buffer_text(&conditions).contains(ALL_OF_TARGET));
        assert_eq!(view.slot, Some(SlotId(1)));
        for area in linked {
            for x in area.x..area.right() {
                assert_eq!(slots[(x, area.y)], conditions[(x, area.y)]);
            }
        }
        view.activate(ScopeControl::Slot(SlotId(1)));
        assert_eq!(render(&model, &mut view, width, name), slots);
    }
}

pub(super) fn template() -> PatternDefinition {
    PatternDefinition {
        version: PATTERN_SCHEMA_VERSION,
        name: "Search paths".into(),
        context: PatternContext {
            tool_identity: "native:workcell:shell.execution.v1".into(),
            executable_identity: "rg".into(),
            effective_workdir: WORKDIR.into(),
            path_binding: WORKDIR.into(),
            analysis_version: "fixture-v1".into(),
        },
        argv: vec![
            PatternToken::Exact {
                value: "rg".into(),
                role: ArgumentRole::Executable,
            },
            PatternToken::Exact {
                value: "-n".into(),
                role: ArgumentRole::Flag,
            },
            PatternToken::Slot {
                id: SlotId(1),
                role: ArgumentRole::Data,
            },
            PatternToken::Exact {
                value: String::new(),
                role: ArgumentRole::Data,
            },
            PatternToken::Slot {
                id: SlotId(1),
                role: ArgumentRole::Data,
            },
            PatternToken::Slot {
                id: SlotId(2),
                role: ArgumentRole::Data,
            },
        ],
        slots: vec![
            PatternSlot {
                id: SlotId(1),
                label: "query".into(),
                domain: ArgumentDomain::ObservedSet {
                    values: [VALUE.into(), "error".into()].into(),
                },
                option_like: OptionLikePolicy::Reject,
            },
            PatternSlot {
                id: SlotId(2),
                label: "path".into(),
                domain: ArgumentDomain::Glob {
                    pattern: "src/**".into(),
                },
                option_like: OptionLikePolicy::Reject,
            },
        ],
        combinations: SlotCombinations::ObservedTuples {
            tuples: [
                BTreeMap::from([(SlotId(1), VALUE.into()), (SlotId(2), "src/main.rs".into())]),
                BTreeMap::from([
                    (SlotId(1), "error".into()),
                    (SlotId(2), "src/lib.rs".into()),
                ]),
            ]
            .into(),
        },
    }
}

pub(super) fn record(template_rule: bool) -> PermissionRuleRecord {
    let selector = if template_rule {
        PermissionResourceSelector::CommandTemplate {
            definition: Box::new(template()),
        }
    } else {
        PermissionResourceSelector::Digest {
            digest: DIGEST.into(),
        }
    };
    let rule = StructuredPermissionRule {
        subject: PermissionSubject::Native {
            owner: "workcell".into(),
            contract: "shell.execution.v1".into(),
        },
        executor: PermissionExecutorKind::Native,
        resources: vec![PermissionResourceConstraint {
            kind: PermissionResourceKind::Command,
            selector,
            access: Some(PermissionResourceAccess::Execute),
            protected: Some(false),
            attributes: BTreeMap::from([(
                "workdir".into(),
                PermissionResourceSelector::Digest {
                    digest: DIGEST.into(),
                },
            )]),
        }],
        arguments: PermissionArgumentConstraint::Exact {
            digest: DIGEST.into(),
        },
        lifetime: PermissionLifetime::Conversation,
        effect: StructuredPermissionEffect::Allow,
        family: None,
    };
    let mut record = PermissionRuleRecord::conversation(StructuredPermissionRule {
        resources: Vec::new(),
        ..rule.clone()
    })
    .unwrap();
    record.rule = rule;
    record.id = "fixture-record".into();
    record.created_at = 1;
    record.review = Some(PermissionReview {
        tool: "shell".into(),
        authority: "Display evidence only".into(),
        input: None,
        resources: vec![PermissionReviewResource {
            index: 0,
            value: Some(DISPLAY_ONLY.into()),
            attributes: BTreeMap::new(),
        }],
        source: PermissionReviewSource::Recovered,
    });
    record
}

fn render(model: &ScopeModel, view: &mut ScopeView, width: u16, name: &str) -> Buffer {
    let area = Rect::new(0, 0, width, HEIGHT);
    let mut buffer = Buffer::empty(area);
    view.render(
        model,
        area,
        &mut buffer,
        &theme::load_by_name(name).unwrap(),
    );
    buffer
}

#[test_case(40, "ayu_dark"; "narrow_dark")]
#[test_case(80, "ayu_dark"; "normal_dark")]
#[test_case(140, "ayu_dark"; "wide_dark")]
#[test_case(40, "ayu_light"; "narrow_light")]
#[test_case(80, "ayu_light"; "normal_light")]
#[test_case(140, "ayu_light"; "wide_light")]
fn typed_chips_preserve_empty_arguments_and_linked_slot_styles(width: u16, name: &str) {
    let model = ScopeModel::record(Arc::new(record(true)));
    let mut view = ScopeView::default();
    view.slot = Some(SlotId(1));
    let buffer = render(&model, &mut view, width, name);
    let text = buffer_text(&buffer);
    assert!(text.contains(EMPTY_LITERAL));
    let linked: Vec<_> = view
        .hits
        .iter()
        .filter(|hit| hit.control == ScopeControl::Slot(SlotId(1)))
        .collect();
    assert_eq!(linked.len(), 2);
    for hit in linked {
        for x in hit.area.x..hit.area.right() {
            assert_eq!(
                buffer[(x, hit.area.y)].bg,
                theme::load_by_name(name).unwrap().item_selected.bg.unwrap()
            );
        }
    }
    let repeated = render(&model, &mut view.clone(), width, name);
    assert_eq!(buffer, repeated);
}

#[test_case(40; "narrow")]
#[test_case(80; "normal")]
#[test_case(140; "wide")]
fn scope_rows_and_pager_keep_distinct_readable_cells(width: u16) {
    let model = ScopeModel::record(Arc::new(record(true)));
    for name in ["ayu_dark", "ayu_light"] {
        let theme = theme::load_by_name(name).unwrap();
        let mut view = ScopeView::default();
        let full = render(&model, &mut view, width, name);
        assert!(buffer_text(&full).contains(EXECUTE_TARGET));
        let area = Rect::new(0, 0, width, SHORT_SCOPE_HEIGHT);
        let mut buffer = Buffer::empty(area);
        view.render(&model, area, &mut buffer, &theme);
        let pagers: Vec<_> = view
            .hits
            .iter()
            .filter(|hit| matches!(hit.control, ScopeControl::Scroll(_)))
            .collect();
        assert_eq!(pagers.len(), 2);
        assert!(pagers[0].area.intersection(pagers[1].area).is_empty());
        for (hit, label) in pagers.iter().zip([PAGE_UP, PAGE_DOWN]) {
            assert_eq!(hit.area.y, area.bottom() - 1);
            let text = (hit.area.x..hit.area.right())
                .map(|x| buffer[(x, hit.area.y)].symbol())
                .collect::<String>();
            assert_eq!(text, label);
        }
        assert_eq!(
            buffer[(pagers[0].area.right(), area.bottom() - 1)].symbol(),
            " "
        );
        assert_eq!(buffer[(area.right() - 1, area.bottom() - 1)].symbol(), " ");
        assert!(
            view.hits
                .iter()
                .all(|hit| hit.area.intersection(area) == hit.area)
        );
    }
}

#[test_case(false; "exact_is_not_review_preimage")]
#[test_case(true; "template_is_not_exact_resource")]
fn authority_is_typed_not_reconstructed_from_review(template_rule: bool) {
    let record = record(template_rule);
    let kind = rule_kind(&record.rule);
    assert_eq!(kind.contains("TEMPLATE"), template_rule);
    let model = ScopeModel::record(Arc::new(record));
    let text = buffer_text(&render(&model, &mut ScopeView::default(), 80, "ayu_dark"));
    assert!(!text.contains(DISPLAY_ONLY));
    if !template_rule {
        assert!(text.contains(FIXED_VALUE));
    }
    let mut view = ScopeView::default();
    view.disclosure = Some(Disclosure::Evidence);
    assert!(buffer_text(&render(&model, &mut view, 80, "ayu_dark")).contains(DISPLAY_ONLY));
}

#[test_case(40; "narrow")]
#[test_case(80; "normal")]
#[test_case(140; "wide")]
fn scope_views_never_show_internal_terms(width: u16) {
    for (fixture, record) in [
        ("fixed", record(false)),
        ("template", record(true)),
        ("risks", mixed_risk_record()),
        ("remote", remote_record()),
    ] {
        let model = ScopeModel::record(Arc::new(record));
        for name in ["ayu_dark", "ayu_light"] {
            for disclosure in [
                None,
                Some(Disclosure::Conditions),
                Some(Disclosure::Combinations),
                Some(Disclosure::Identity),
                Some(Disclosure::Evidence),
            ] {
                let mut view = ScopeView::default();
                view.disclosure = disclosure;
                let buffer = render(&model, &mut view, width, name);
                assert_plain(&buffer_rows(&buffer), fixture);
            }
        }
    }
}

#[test_case(false; "same_target_click")]
#[test_case(true; "reflow_cancels_click")]
fn mouse_release_requires_same_current_geometry(resize: bool) {
    let model = ScopeModel::record(Arc::new(record(true)));
    let mut view = ScopeView::default();
    render(&model, &mut view, 80, "ayu_dark");
    let hit = view
        .hits
        .iter()
        .find(|hit| hit.control == ScopeControl::Slot(SlotId(1)))
        .unwrap()
        .clone();
    let event = |kind| MouseEvent {
        kind,
        column: hit.area.x,
        row: hit.area.y,
        modifiers: KeyModifiers::NONE,
    };
    view.handle_mouse(event(MouseEventKind::Down(MouseButton::Left)));
    if resize {
        render(&model, &mut view, 40, "ayu_dark");
    }
    view.handle_mouse(event(MouseEventKind::Up(MouseButton::Left)));
    assert_eq!(view.slot, if resize { None } else { Some(SlotId(1)) });
}

#[test_case(KeyCode::Enter; "enter_inspects")]
#[test_case(KeyCode::Char(' '); "space_inspects")]
fn keyboard_inspects_first_typed_slot(code: KeyCode) {
    let model = ScopeModel::record(Arc::new(record(true)));
    let mut view = ScopeView::default();
    render(&model, &mut view, 80, "ayu_dark");
    view.handle_key(KeyEvent::from(code));
    assert_eq!(view.slot, Some(SlotId(1)));
    view.disclosure = Some(Disclosure::Combinations);
    let text = buffer_text(&render(&model, &mut view, 80, "ayu_dark"));
    assert!(text.contains(ANY_OF_TARGETS));
    assert!(text.contains(ALLOWED_COMBINATIONS));
    assert!(text.contains("src/main.rs"));
}

#[test_case(ArgumentRole::Sensitive; "sensitive_token")]
#[test_case(ArgumentRole::Payload; "payload_token")]
fn sensitive_tokens_are_not_disclosed(role: ArgumentRole) {
    let mut record = record(true);
    if let PermissionResourceSelector::CommandTemplate { definition } =
        &mut record.rule.resources[0].selector
    {
        definition.argv.push(PatternToken::Exact {
            value: SECRET.into(),
            role,
        });
    }
    let model = ScopeModel::record(Arc::new(record));
    let mut view = ScopeView::default();
    for disclosure in [None, Some(Disclosure::Identity)] {
        view.disclosure = disclosure;
        assert!(!buffer_text(&render(&model, &mut view, 140, "ayu_dark")).contains(SECRET));
    }
}

fn remote_record() -> PermissionRuleRecord {
    let authority = AuthorityIdentity::new(
        SourceTrustAnchor::new("fixture-ca").unwrap(),
        "fixture-host",
        "fixture-workspace",
        "fixture-root",
        "fixture-server",
    )
    .unwrap();
    let identity = RemotePermissionIdentity {
        principal: AuthenticatedPrincipalId::new(authority.clone(), "fixture-principal").unwrap(),
        project: ProjectIdentity::new(
            authority.clone(),
            ProjectKey::new("fixture-project").unwrap(),
        ),
        authority,
    };
    let mut record = record(false);
    record.rule.subject = PermissionSubject::RemoteNative {
        identity: identity.clone(),
        owner: "workcell".into(),
        contract: "file.read.v1".into(),
    };
    record.rule.resources[0].kind = PermissionResourceKind::RemoteFile {
        identity: identity.clone(),
    };
    record.rule.resources[0].selector = PermissionResourceSelector::RemoteSubtree {
        identity,
        scope: vec!["project".into(), "src/one component".into()],
    };
    record.rule.resources[0].access = Some(PermissionResourceAccess::Read);
    record
}

#[test_case(40; "narrow")]
#[test_case(80; "normal")]
#[test_case(140; "wide")]
fn changing_target_counts_keeps_header_and_controls_in_bounds(width: u16) {
    let mut record = record(false);
    let target = record.rule.resources[0].clone();
    let mut view = ScopeView::default();
    for count in [1, 24, 2, 0, 240] {
        record.rule.resources = vec![target.clone(); count];
        let model = ScopeModel::record(Arc::new(record.clone()));
        let buffer = render(&model, &mut view, width, "ayu_dark");
        for hit in &view.hits {
            assert_eq!(hit.area.intersection(buffer.area), hit.area);
        }
        assert_eq!(buffer[(0, 0)].symbol(), "[");
    }
}

#[test_case(40; "narrow")]
#[test_case(80; "normal")]
#[test_case(140; "wide")]
fn slots_beyond_clipped_chips_remain_mouse_reachable(width: u16) {
    let mut record = record(true);
    let PermissionResourceSelector::CommandTemplate { definition } =
        &mut record.rule.resources[0].selector
    else {
        panic!("expected template");
    };
    definition.argv.splice(
        1..1,
        (0..LONG_ARGV_COUNT).map(|_| PatternToken::Exact {
            value: VALUE.into(),
            role: ArgumentRole::Data,
        }),
    );
    let model = ScopeModel::record(Arc::new(record));
    let mut view = ScopeView::default();
    for id in [SlotId(2), SlotId(1)] {
        render(&model, &mut view, width, "ayu_dark");
        let hit = view
            .hits
            .iter()
            .find(|hit| hit.control == ScopeControl::Slot(id))
            .unwrap()
            .clone();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            view.handle_mouse(MouseEvent {
                kind,
                column: hit.area.x,
                row: hit.area.y,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(view.slot, Some(id));
    }
}

pub(super) fn visual_directory(prefix: &str) -> TempDir {
    let directory = Builder::new().prefix(prefix).tempdir_in("/tmp").unwrap();
    #[cfg(unix)]
    fs::set_permissions(directory.path(), Permissions::from_mode(DIRECTORY_MODE)).unwrap();
    directory
}

pub(super) fn write_visual_buffer(directory: &Path, stem: &str, buffer: &Buffer) {
    for (extension, contents) in [
        ("txt", buffer_text(buffer)),
        ("cells", format!("{buffer:#?}")),
    ] {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(FILE_MODE);
        options
            .open(directory.join(format!("{stem}.{extension}")))
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
    }
}

#[test]
#[ignore = "private whole-cell visual artifacts; run after serialized build and review before acceptance"]
fn export_scope_visual_review() {
    let directory = visual_directory("caudra-scope-phase3-");
    let mut alternatives = record(false);
    alternatives
        .rule
        .resources
        .push(PermissionResourceConstraint {
            kind: PermissionResourceKind::Url,
            selector: PermissionResourceSelector::UrlOriginDigest {
                digest: DIGEST.into(),
            },
            access: Some(PermissionResourceAccess::Connect),
            protected: Some(true),
            attributes: BTreeMap::from([(
                "principal".into(),
                PermissionResourceSelector::Digest {
                    digest: DIGEST.into(),
                },
            )]),
        });
    let fixtures = [
        ("opaque-exact", record(false)),
        ("linked-template", record(true)),
        ("combined-risks", mixed_risk_record()),
        ("alternatives", alternatives),
        ("remote", remote_record()),
    ];
    for name in ["ayu_dark", "ayu_light"] {
        for width in [40, 80, 140] {
            for (fixture, record) in &fixtures {
                let model = ScopeModel::record(Arc::new(record.clone()));
                for disclosure in [
                    None,
                    Some(Disclosure::Combinations),
                    Some(Disclosure::Identity),
                    Some(Disclosure::Evidence),
                ] {
                    let mut view = ScopeView::default();
                    view.disclosure = disclosure.clone();
                    let buffer = render(&model, &mut view, width, name);
                    let stem = format!("{name}-{width}x{HEIGHT}-{fixture}-{disclosure:?}");
                    write_visual_buffer(directory.path(), &stem, &buffer);
                }
            }
        }
    }
    println!("{}", directory.keep().display());
}
