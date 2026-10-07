use super::{
    Confirmation, Control, Focus, Manager, ReadSurface, SandboxAction, SandboxView, SnapshotState,
    StoreTicket, TextEditor, editor_action,
    form::{CIDR_HELP, DOMAIN_HELP, LEASE_HELP, network_list_key, parse_cidrs, parse_domains},
    image, key,
    view::hover_style,
};
use crate::{
    sandbox::{
        LiveOperation, LiveRequest, MAX_LIVE_PREVIEW_BYTES, RULE_TEST_NOTICE,
        SandboxInstanceSnapshot, SandboxInstanceState, SandboxSnapshotRequest,
    },
    theme,
};
use caudra_config::sandbox::{LeaseSeconds, SandboxName};
use caudra_sandbox::{
    CreateReview, LifecycleAction, Ownership,
    dto::{OperationStatus, Policy},
    local_admin::{AdminOperation, AdminRequest, ImageProbe, ProbedImage},
};
use caudra_storage::sandbox_auth::{SandboxApiKey, SandboxCredentialRef};
use caudra_workbench::text_field::{self, EditCommand, FieldKind, TextCommand};
use caudra_workspace::WorkspacePath;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    widgets::{Block, Borders, Paragraph},
};
use std::path::{Path, PathBuf};

const DEFAULT_LEASE: &str = "3600";
pub(super) const LEASE_FIELD: &str = "Lease seconds";
const NO_EXPIRY_REVIEW: &str = "New running lease: no expiry. The VM keeps running, and using host resources, until it is paused or deleted, including after Caudra exits.";
const EXTEND_REVIEW: &str = "EXTEND: lengthen the running lease, counted from now. Extend never shortens a lease, so one with no expiry keeps it. Conditional on the reviewed execution and revision. Saved profiles remain unchanged.";
const DESTROY_BORROWED: &str = "Destroy borrowed VM/disk (type DELETE; empty detaches)";
const DELETE_APPROVAL: &str = "DELETE";
const TRANSITION: &str = "Attach opens a NEW sandbox session after Workcell verification and session gates. The local session is saved unchanged. No conversation, grants, pending prompts, files or secrets are copied. Closing/exiting detaches; it never deletes or pauses the VM.";
const MIB_BYTES: u64 = 1024 * 1024;
const LIVE_FIELD_ROWS: u16 = 6;
const RECOVERY_REQUIRED: &str = "Pending or unknown outcome: Inspect / Reconcile first; acknowledging failure is separate from retrying.";
const PERSISTENT_REQUIRED: &str =
    "Requires a persistent disk. Stop never silently deletes an ephemeral instance.";
const LIVE_KEYS: &str = "Ctrl+Enter previews (does not execute). Esc goes back one level and keeps this draft; F6 discards it. Tab changes field; Ctrl+G/Ctrl+L selects first/last field. Network F4 evaluates rules only.";
const DRAFT_NEW: &str = "Live action draft.";
const DRAFT_RESTORED: &str = "Retained action draft; nothing was submitted while it was set aside.";
const DRAFT_KEPT: &str =
    "Action draft kept; nothing submitted. Reopening the same action restores it.";
const DRAFT_DISCARDED: &str = "Action draft discarded. No action submitted.";
const PAUSE_REVIEW: &str = "PAUSE / STOP: stop execution, preserve the persistent disk. Memory/process state is not saved; Resume is a cold boot. The running lease ends; paused-disk retention still applies. When attached, save conversations and drain active work before detaching; the active TUI then closes.";
const RESTART_REVIEW: &str = "RESTART: stop then cold boot the SAME persistent disk with the reviewed new lease. No delete or recreate. Memory/process state is lost. If stop or boot is uncertain, reconcile; never automatically retry. When attached, save conversations and drain active work before detaching. Only after verified success, reconstruct the verified sandbox runtime and restore the same saved conversations; never switch to a new local workspace.";
const ACK_REVIEW: &str = "Acknowledge FAILURE only. No remote action or automatic resume/retry. Inspect / Reconcile first when possible; a later Resume requires a fresh, separate review. Attached sessions exit detached.";
pub(super) const INSTANCE_ACTIONS: &[(&str, Option<Kind>)] = &[
    ("Inspect · read-only, no request", None),
    (
        "Reconcile · look up outcome, never retry",
        Some(Kind::Reconcile),
    ),
    (
        "Acknowledge failure · explicit local recovery",
        Some(Kind::AcknowledgeFailure),
    ),
    ("Resume · cold boot preserved disk", Some(Kind::Resume)),
    ("Pause / Stop · preserve disk", Some(Kind::Pause)),
    ("Restart · cold boot same disk", Some(Kind::Restart)),
    ("Extend running lease", Some(Kind::Extend)),
    ("Attach · new sandbox session", Some(Kind::Attach)),
    ("Network · review live policy", Some(Kind::Network)),
    (
        "Cancel create · explicit remote cancellation",
        Some(Kind::CancelCreate),
    ),
    ("Detach record · keep VM and disk", Some(Kind::Detach)),
    (
        "Delete · destructive (borrowed: detach by default)",
        Some(Kind::Delete),
    ),
];

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Kind {
    Create,
    Attach,
    Pause,
    Resume,
    Restart,
    Extend,
    Delete,
    Detach,
    Reconcile,
    AcknowledgeFailure,
    CancelCreate,
    Network,
    Doctor,
    Credential,
    ImportImage,
    Build,
    Gc,
    InspectImage,
}

pub(super) struct LiveField {
    pub label: &'static str,
    pub editor: TextEditor,
    pub choices: &'static [&'static str],
}

impl LiveField {
    pub(super) fn secret(&self) -> bool {
        self.editor.is_secret()
    }
}

pub(super) struct LiveForm {
    pub(super) kind: Kind,
    pub fields: Vec<LiveField>,
    pub focus: usize,
    target: Option<SandboxInstanceSnapshot>,
    pub picker: Option<image::HostPicker>,
    pub probe: Option<ProbedImage>,
    pub(super) origin: Option<usize>,
}

pub(super) struct RetainedLive {
    view: SandboxView,
    entry: String,
    form: LiveForm,
}

impl LiveForm {
    pub(super) fn install_probe(&mut self, probe: ProbedImage) {
        if self.kind != Kind::ImportImage
            || Path::new(&self.field(image::SOURCE)) != probe.source_path
            || Path::new(&self.field(image::QEMU)) != probe.qemu_img
        {
            return;
        }
        for field in &mut self.fields {
            if field.label == image::SHA256 {
                field.editor.set_text(probe.sha256.as_str().into());
            }
            if matches!(field.label, "Minimum disk MiB" | "Default disk MiB")
                && field.editor.text().is_empty()
            {
                field.editor.set_text(
                    probe
                        .image
                        .virtual_size_bytes
                        .div_ceil(MIB_BYTES)
                        .to_string(),
                );
            }
        }
        self.probe = Some(probe);
    }
    pub(super) fn field(&self, label: &str) -> String {
        self.fields
            .iter()
            .find(|field| field.label == label)
            .map(|field| field.editor.text())
            .unwrap_or_default()
    }

    fn policy(&self) -> Result<Policy, String> {
        let policy = Policy {
            mode: self.field("TLS mode"),
            domains: parse_domains(&self.field("Domains (one per line)"))?,
            cidrs: parse_cidrs(&self.field("CIDRs (one per line)"))?,
        };
        policy.validate().map_err(|error| error.to_string())?;
        Ok(policy)
    }
}

impl Manager {
    pub(super) fn selected_instance(&self) -> Option<&SandboxInstanceSnapshot> {
        let selected = self.entries().get(self.selected)?.clone();
        match &self.snapshot.as_ref()?.instances {
            SnapshotState::Ready(rows) if self.view == SandboxView::Instances => {
                rows.iter().find(|row| row.id == selected)
            }
            _ => None,
        }
    }

    pub(super) fn instance_action_error(&self, kind: &Kind) -> Option<String> {
        if self.dirty() || self.pending.is_some() || self.live_pending.is_some() {
            return Some("Save or discard edits and wait for pending operations.".into());
        }
        instance_action_error(kind, self.selected_instance()).map(str::to_owned)
    }

    pub(super) fn open_instance_actions(&mut self) {
        self.cancel_selections();
        self.instance_action = Some(0);
        self.reference_scroll = 0;
        self.reveal_reference = true;
        self.status = "Choose an action to draft and review; selection never executes. Disabled actions explain why. Inspect and Reconcile do not retry; acknowledgement does not resume.".into();
    }

    pub(super) fn choose_instance_action(&mut self, index: usize) -> SandboxAction {
        let Some((_, kind)) = INSTANCE_ACTIONS.get(index) else {
            return SandboxAction::None;
        };
        self.instance_action = Some(index);
        if let Some(kind) = kind {
            if let Some(reason) = self.instance_action_error(kind) {
                self.status = reason;
                return SandboxAction::None;
            }
            self.open_live(kind.clone())
        } else {
            self.instance_action = None;
            self.detail = true;
            self.focus = Focus::Detail;
            self.status = "Inspect only: no lifecycle request, acknowledgement or retry.".into();
            SandboxAction::None
        }
    }

    pub(super) fn instance_actions_key(&mut self, event: KeyEvent) -> SandboxAction {
        let Some(index) = self.instance_action else {
            return SandboxAction::None;
        };
        let delta = match event.code {
            KeyCode::Esc | KeyCode::F(3) => {
                self.instance_action = None;
                return SandboxAction::None;
            }
            KeyCode::Enter => return self.choose_instance_action(index),
            KeyCode::Up | KeyCode::BackTab => -1,
            KeyCode::Down | KeyCode::Tab => 1,
            KeyCode::PageUp => -(self.references_area.height.max(1) as isize),
            KeyCode::PageDown => self.references_area.height.max(1) as isize,
            KeyCode::Home => -isize::MAX,
            KeyCode::End => isize::MAX,
            _ => return SandboxAction::None,
        };
        self.instance_action = Some(
            index
                .saturating_add_signed(delta)
                .min(INSTANCE_ACTIONS.len() - 1),
        );
        self.reveal_reference = true;
        SandboxAction::None
    }

    pub(super) fn open_live(&mut self, kind: Kind) -> SandboxAction {
        if INSTANCE_ACTIONS
            .iter()
            .any(|(_, action)| action.as_ref() == Some(&kind))
            && let Some(reason) = self.instance_action_error(&kind)
        {
            self.status = reason;
            return SandboxAction::None;
        }
        if self.dirty() || self.pending.is_some() || self.live_pending.is_some() {
            self.status = "Save or discard configuration edits and wait for pending acknowledgments before live actions.".into();
            return SandboxAction::None;
        }
        let selected = self
            .entries()
            .get(self.selected)
            .cloned()
            .unwrap_or_default();
        let origin = self.instance_action.take();
        let view = self.view.clone();
        if let Some(retained) = self.retained_live.take_if(|retained| {
            retained.form.kind == kind && retained.view == view && retained.entry == selected
        }) {
            self.live_form = Some(LiveForm {
                origin,
                ..retained.form
            });
            self.show_live(DRAFT_RESTORED);
            return SandboxAction::None;
        }
        let target = self
            .snapshot
            .as_ref()
            .and_then(|snapshot| match &snapshot.instances {
                SnapshotState::Ready(rows) if self.view == SandboxView::Instances => {
                    rows.iter().find(|row| row.id == selected).cloned()
                }
                _ => None,
            });
        let provider = target
            .as_ref()
            .map(|row| row.provider.to_string())
            .or_else(|| {
                self.form.as_ref().map(|form| {
                    if self.view == SandboxView::Providers {
                        form.text("name")
                    } else {
                        form.text("provider")
                    }
                })
            })
            .or_else(|| {
                selected
                    .split_once(": ")
                    .map(|(provider, _)| provider.to_owned())
            })
            .or_else(|| self.draft.providers.keys().next().map(ToString::to_string))
            .unwrap_or_default();
        let mut fields = Vec::new();
        let mut field = |label, value: String, secret| {
            let mut editor = TextEditor::new();
            editor.set_text(value);
            editor.set_secret(secret);
            fields.push(LiveField {
                label,
                editor,
                choices: &[],
            });
        };
        match kind {
            Kind::Create => {
                field("Saved profile", selected, false);
                field("New instance name", String::new(), false);
                field(
                    "Initial seed local root (optional absolute)",
                    String::new(),
                    false,
                );
                field("Initial seed remote root", ".".into(), false);
            }
            Kind::Attach if target.as_ref().is_some_and(|row| row.record.is_none()) => {
                field("Borrowed instance name", String::new(), false);
                field("Guest cwd", ".".into(), false);
            }
            Kind::Delete
                if target
                    .as_ref()
                    .and_then(|row| row.record.as_ref())
                    .is_some_and(|record| record.ownership == Ownership::Borrowed) =>
            {
                field(DESTROY_BORROWED, String::new(), false)
            }
            Kind::Resume | Kind::Restart | Kind::Extend => field(
                LEASE_FIELD,
                target
                    .as_ref()
                    .and_then(|row| row.effective.as_ref())
                    .map(|launch| {
                        launch
                            .configuration()
                            .profile
                            .value()
                            .running_ttl_seconds
                            .get()
                            .to_string()
                    })
                    .unwrap_or_else(|| DEFAULT_LEASE.into()),
                false,
            ),
            Kind::Network => {
                let policy = target
                    .as_ref()
                    .and_then(|row| row.live.as_ref())
                    .and_then(|instance| instance.egress.policy.as_ref());
                field(
                    "TLS mode",
                    policy
                        .map(|policy| policy.mode.clone())
                        .unwrap_or_else(|| "sni-only".into()),
                    false,
                );
                field(
                    "Domains (one per line)",
                    policy
                        .map(|policy| policy.domains.join("\n"))
                        .unwrap_or_default(),
                    false,
                );
                field(
                    "CIDRs (one per line)",
                    policy
                        .map(|policy| policy.cidrs.join("\n"))
                        .unwrap_or_default(),
                    false,
                );
                field("Test destination (bare host or IP)", String::new(), false);
            }
            Kind::Doctor => field("Provider", provider, false),
            Kind::Credential => {
                field(
                    "Credential reference",
                    self.form
                        .as_ref()
                        .map(|form| form.text("credential_ref"))
                        .unwrap_or_default(),
                    false,
                );
                field("API key (masked; never exported)", String::new(), true);
            }
            Kind::ImportImage | Kind::Build | Kind::Gc | Kind::InspectImage => {
                let template = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| {
                        snapshot
                            .providers
                            .iter()
                            .find(|(name, _)| name.as_str() == provider)
                    })
                    .and_then(|(_, provider)| provider.doctor.as_ref())
                    .and_then(|doctor| {
                        doctor
                            .templates
                            .iter()
                            .find(|template| selected.contains(template.revision.as_str()))
                    });
                fields = image::fields(&kind, provider, template);
            }
            _ => {}
        }
        if kind == Kind::Network
            && let Some(field) = fields.iter_mut().find(|field| field.label == "TLS mode")
        {
            field.choices = &["sni-only", "mitm"];
        }
        self.live_form = Some(LiveForm {
            kind,
            fields,
            focus: 0,
            target,
            picker: None,
            probe: None,
            origin,
        });
        self.show_live(DRAFT_NEW);
        SandboxAction::None
    }

    fn show_live(&mut self, lead: &str) {
        self.detail_scroll = 0;
        self.status = if self.live_form.as_ref().is_some_and(|form| {
            matches!(
                form.kind,
                Kind::ImportImage | Kind::Build | Kind::Gc | Kind::InspectImage
            )
        }) {
            image::ADMIN_NOTICE.into()
        } else {
            format!("{lead} {LIVE_KEYS}")
        };
    }

    fn leave_live(&mut self, retain: bool) {
        let Some(form) = self.live_form.take() else {
            return;
        };
        self.instance_action = form.origin;
        self.reveal_reference = true;
        self.detail_scroll = 0;
        let entry = self
            .entries()
            .get(self.selected)
            .cloned()
            .unwrap_or_default();
        self.retained_live = retain.then(|| RetainedLive {
            view: self.view.clone(),
            entry,
            form,
        });
        self.status = if retain { DRAFT_KEPT } else { DRAFT_DISCARDED }.into();
    }

    fn prepare_live(&self) -> Result<(LiveOperation, String), String> {
        let form = self.live_form.as_ref().ok_or("No live action draft")?;
        let baseline = self.baseline.as_ref().ok_or("Load configuration first")?;
        let name =
            |label| SandboxName::parse(&form.field(label)).map_err(|error| error.to_string());
        let (operation, preview) = match form.kind {
            Kind::Create => {
                let profile = name("Saved profile")?;
                let name = name("New instance name")?;
                let saved = baseline
                    .saved()
                    .configuration()
                    .profiles
                    .get(&profile)
                    .ok_or("Missing saved profile")?;
                let provider = self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.providers.get(&saved.provider))
                    .ok_or("Provider unavailable; use Doctor")?;
                let SnapshotState::Ready(catalog) = &provider.catalog else {
                    return Err("Catalog unavailable".into());
                };
                let launch = baseline
                    .saved()
                    .resolve_launch(&profile, &provider.capabilities, catalog)
                    .map_err(|error| error.to_string())?;
                let doctor = provider
                    .doctor
                    .as_ref()
                    .ok_or("Live provider authority unavailable; refresh")?;
                let pinned = &launch.configuration().template;
                let template = doctor
                    .templates
                    .iter()
                    .find(|template| {
                        template.manifest.id == pinned.id && template.revision == pinned.revision
                    })
                    .ok_or("Immutable image metadata unavailable")?;
                let review = CreateReview {
                    owner_id: doctor.discovery.owner_id.clone(),
                    image_sha256: template.image_sha256.clone(),
                    launch_revision: launch.revision().clone(),
                };
                let local = form.field("Initial seed local root (optional absolute)");
                let seed = if local.is_empty() {
                    None
                } else {
                    if launch.configuration().transfer.value().initial_seed
                        != caudra_config::sandbox::InitialSeed::Ask
                    {
                        return Err(
                            "This profile disables initial seed; use an explicit Transfer later"
                                .into(),
                        );
                    }
                    let local = PathBuf::from(local);
                    if !local.is_absolute() {
                        return Err("Initial seed local root must be absolute".into());
                    }
                    Some((
                        local,
                        WorkspacePath::new(form.field("Initial seed remote root"))
                            .map_err(|error| error.to_string())?,
                    ))
                };
                let preview = format!(
                    "CREATE {name}\nOwner {}\n{}\nImage / recipe / capabilities:\n{}\nCreates a VM with these immutable saved defaults; does NOT attach or copy local files.",
                    review.owner_id,
                    pretty(&launch)?,
                    pretty(template)?
                );
                (
                    LiveOperation::Create {
                        profile,
                        name,
                        review,
                        seed,
                    },
                    preview,
                )
            }
            Kind::Doctor => {
                let provider = name("Provider")?;
                (
                    LiveOperation::Doctor {
                        provider: provider.clone(),
                    },
                    format!(
                        "Doctor {provider}: authenticated discovery/catalog and local KVM access inspection only. No install/start/probe."
                    ),
                )
            }
            Kind::Credential => {
                let reference = form
                    .field("Credential reference")
                    .parse::<SandboxCredentialRef>()
                    .map_err(|error| error.to_string())?;
                let key = SandboxApiKey::new(form.field("API key (masked; never exported)"))
                    .map_err(|error| error.to_string())?;
                if key.expose_secret().len() < 32 {
                    return Err("API key requires at least 32 bytes".into());
                }
                let preview = format!(
                    "Replace {reference} in the owner-only lifecycle purpose store. Secret omitted. Existing provider identity may change; refresh and verify it before attachment. No Workcell bearer or configuration export contains this secret."
                );
                (LiveOperation::Credential { reference, key }, preview)
            }
            Kind::ImportImage | Kind::Build | Kind::Gc | Kind::InspectImage => {
                let provider = name("Provider")?;
                let operation = image::operation(form)?;
                if let AdminOperation::Import(request) = &operation {
                    let probe = form
                        .probe
                        .as_ref()
                        .ok_or("F4: review and run the pinned qemu-img probe before importing")?;
                    if request.source_path != probe.source_path
                        || request.expected_sha256 != probe.sha256
                        || Path::new(&form.field(image::QEMU)) != probe.qemu_img
                        || probe.image.virtual_size_bytes
                            > u64::from(request.manifest.minimum.disk_size_mb) * MIB_BYTES
                    {
                        return Err("Image path, digest, qemu-img or minimum disk differs from probed metadata; review F4 again".into());
                    }
                }
                let request = AdminRequest {
                    helper: image::helper(form),
                    operation,
                };
                let preview = request
                    .command(
                        baseline
                            .saved()
                            .configuration()
                            .providers
                            .get(&provider)
                            .ok_or("Provider missing")?,
                    )
                    .and_then(|command| command.preview())
                    .map_err(|error| error.to_string())?;
                (LiveOperation::Admin { provider, request }, preview)
            }
            _ => {
                let target = form.target.as_ref().ok_or("Select an instance first")?;
                if let Some(reason) = instance_action_error(&form.kind, Some(target)) {
                    return Err(reason.into());
                }
                if form.kind == Kind::Attach && target.record.is_none() {
                    let instance = target.live.clone().ok_or("Live instance unavailable")?;
                    let name = name("Borrowed instance name")?;
                    let cwd = WorkspacePath::new(form.field("Guest cwd"))
                        .map_err(|error| error.to_string())?;
                    let preview = format!(
                        "BORROW {} as {name}\n{}\n{TRANSITION}",
                        target.id,
                        pretty(&instance)?
                    );
                    (
                        LiveOperation::Borrow {
                            provider: target.provider.clone(),
                            instance,
                            name,
                            cwd,
                        },
                        preview,
                    )
                } else {
                    let record = target
                        .record
                        .as_ref()
                        .ok_or("Attach this provider instance as borrowed before controlling it")?;
                    let revision = record.revision().map_err(|error| error.to_string())?;
                    let name = record.name.clone();
                    if !matches!(
                        form.kind,
                        Kind::Reconcile
                            | Kind::CancelCreate
                            | Kind::Detach
                            | Kind::AcknowledgeFailure
                    ) && (target.live.is_none() || target.live != record.instance)
                    {
                        return Err("Live instance changed/unavailable. Reconcile, then review a fresh action.".into());
                    }
                    let lease = || {
                        form.field(LEASE_FIELD)
                            .parse::<LeaseSeconds>()
                            .map_err(|_| LEASE_HELP.to_owned())
                    };
                    let operation = match form.kind {
                        Kind::Attach => LiveOperation::Attach { name, revision },
                        Kind::Reconcile => LiveOperation::Reconcile { name },
                        Kind::CancelCreate => LiveOperation::CancelCreate { name, revision },
                        Kind::Restart => LiveOperation::Restart {
                            name,
                            revision,
                            lease_seconds: lease()?,
                        },
                        Kind::AcknowledgeFailure => {
                            LiveOperation::AcknowledgeFailure { name, revision }
                        }
                        _ => {
                            let action = match form.kind {
                                Kind::Pause => LifecycleAction::Pause,
                                Kind::Resume => LifecycleAction::Resume {
                                    lease_seconds: lease()?,
                                },
                                Kind::Extend => LifecycleAction::Extend {
                                    lease_seconds: lease()?,
                                },
                                Kind::Network => {
                                    let policy = form.policy()?;
                                    let doctor = self.snapshot.as_ref().and_then(|snapshot| snapshot.providers.get(&target.provider)).and_then(|provider| provider.doctor.as_ref()).ok_or("Refresh provider TLS capabilities before review")?;
                                    policy.validate_for(&doctor.discovery, &record.template.manifest, target.live.as_ref()).map_err(|error| error.to_string())?;
                                    LifecycleAction::ApplyPolicy { policy }
                                },
                                Kind::Delete => LifecycleAction::Delete {
                                    destroy_borrowed: match form.field(DESTROY_BORROWED).as_str() {
                                        "" => false,
                                        DELETE_APPROVAL => true,
                                        _ => return Err("Type DELETE exactly to destroy a borrowed disk, or clear the field to detach".into()),
                                    },
                                },
                                Kind::Detach => LifecycleAction::Detach,
                                _ => return Err("Unsupported action".into()),
                            };
                            LiveOperation::Control {
                                name,
                                revision,
                                action,
                            }
                        }
                    };
                    let effect = match &operation {
                        LiveOperation::Attach { .. } => TRANSITION.into(),
                        LiveOperation::AcknowledgeFailure { .. } => format!("{ACK_REVIEW}\n{}", record.lifecycle_failure_review().map_err(|error| error.to_string())?),
                        LiveOperation::Restart { lease_seconds, .. } => format!("{RESTART_REVIEW}\n{}", lease_review(*lease_seconds)),
                        LiveOperation::Control { action: LifecycleAction::Pause, .. } => PAUSE_REVIEW.into(),
                        LiveOperation::Control { action: LifecycleAction::Resume { lease_seconds }, .. } => format!("RESUME: cold boot the preserved disk; no memory/process restoration or disk deletion. {} Reconnect only after verified success.", lease_review(*lease_seconds)),
                        LiveOperation::Control { action: LifecycleAction::Extend { lease_seconds }, .. } => format!("{EXTEND_REVIEW}\n{}", lease_review(*lease_seconds)),
                        LiveOperation::Control { action: LifecycleAction::Delete { destroy_borrowed: false }, .. } if record.ownership == Ownership::Borrowed => "DETACH borrowed instance only. VM and disk are NOT deleted.".into(),
                        LiveOperation::Control { action: LifecycleAction::Detach, .. } => "DETACH local record only. VM and disk are NOT deleted.".into(),
                        LiveOperation::Control { action: LifecycleAction::Delete { .. }, .. } => "PERMANENTLY DELETE this VM AND DISK. Cannot be undone.".into(),
                        LiveOperation::Control { action: LifecycleAction::ApplyPolicy { policy }, .. } => {
                            let doctor = self.snapshot.as_ref().and_then(|snapshot| snapshot.providers.get(&target.provider)).and_then(|provider| provider.doctor.as_ref()).ok_or("Provider TLS metadata unavailable")?;
                            format!("Apply host-only rules (no port/method policy). Empty domains AND CIDRs deny all. Saved profiles remain unchanged. MITM terminates TLS and can break pinning.\n{}\nDiscovered TLS modes: {:?}; live mode change: {}; reviewed image guest CA: {}; instance guest CA ready: {}", pretty(policy)?, doctor.discovery.tls_modes, doctor.discovery.capabilities.live_tls_mode_change, record.template.manifest.guest_ca, target.live.as_ref().is_some_and(|instance| instance.egress.guest_ca_ready))
                        }
                        LiveOperation::CancelCreate { .. } => "CANCEL CREATE: explicit remote cancellation; may race completion. Reconcile if outcome is unknown. Escape/close alone never requests this.".into(),
                        _ => "RECONCILE by lookup only; never replay Create or implicitly resume.".into(),
                    };
                    let preview = format!(
                        "{effect}\nProvider {} / local record {}\n{}\nBlockers: {}\nIf this is the current runtime: save, quiesce and detach before control. Other holders still block. Only verified success may reconnect; pause/delete/detach/acknowledgement exits detached. Failure exits detached and recoverable; never local fallback.",
                        target.provider,
                        record.name,
                        pretty(record)?,
                        target.blockers.join("; ")
                    );
                    (operation, preview)
                }
            }
        };
        if preview.len() > MAX_LIVE_PREVIEW_BYTES {
            return Err("Complete confirmation exceeds the review bound; nothing was sent".into());
        }
        Ok((operation, preview))
    }

    pub(super) fn live_key(&mut self, event: KeyEvent) -> SandboxAction {
        if let Some(form) = self.live_form.as_mut()
            && let Some(picker) = form.picker.as_mut()
        {
            if event.code == KeyCode::Esc {
                form.picker = None;
            } else {
                match picker.key(event.code) {
                    Ok(Some(path)) => {
                        if let Some(field) = form
                            .fields
                            .iter_mut()
                            .find(|field| field.label == image::SOURCE)
                        {
                            field.editor.set_text(path.to_string_lossy().into_owned());
                        }
                        form.probe = None;
                        form.picker = None;
                        self.status = "Host image selected. F4 reviews the pinned qemu-img probe; selection alone executes nothing.".into();
                    }
                    Ok(None) => {}
                    Err(error) => self.status = error,
                }
            }
            return SandboxAction::None;
        }
        if matches!(event.code, KeyCode::Esc | KeyCode::F(6)) {
            self.leave_live(event.code == KeyCode::Esc);
            return SandboxAction::None;
        }
        if event.code == KeyCode::Enter && event.modifiers.contains(KeyModifiers::CONTROL) {
            match self.prepare_live() {
                Ok((operation, preview)) => {
                    self.confirmation = Some(Confirmation::Live {
                        operation: Box::new(operation),
                        preview,
                        choice: 0,
                    });
                    self.detail_scroll = 0;
                }
                Err(error) => self.status = error,
            }
            return SandboxAction::None;
        }
        let Some(form) = self.live_form.as_mut() else {
            return SandboxAction::None;
        };
        if event.code == KeyCode::F(2) && form.kind == Kind::ImportImage {
            match image::HostPicker::open(&form.field(image::SOURCE)) {
                Ok(picker) => form.picker = Some(picker),
                Err(error) => self.status = error,
            }
            return SandboxAction::None;
        }
        if event.code == KeyCode::F(4) && form.kind == Kind::ImportImage {
            let prepare = || {
                let provider = SandboxName::parse(&form.field(image::PROVIDER))
                    .map_err(|error| error.to_string())?;
                let saved = self.baseline.as_ref().ok_or("Load configuration first")?;
                let provider = saved
                    .saved()
                    .configuration()
                    .providers
                    .get(&provider)
                    .ok_or("Provider missing")?;
                let probe = ImageProbe::prepare(
                    provider,
                    form.field(image::SOURCE).into(),
                    form.field(image::QEMU).into(),
                )
                .map_err(|error| error.to_string())?;
                let preview = probe.preview().map_err(|error| error.to_string())?;
                Ok::<_, String>((probe, preview))
            };
            match prepare() {
                Ok((probe, preview)) => {
                    self.confirmation = Some(Confirmation::Live {
                        operation: Box::new(LiveOperation::ProbeImage(probe)),
                        preview,
                        choice: 0,
                    });
                    self.detail_scroll = 0;
                }
                Err(error) => self.status = error,
            }
            return SandboxAction::None;
        }
        if event.code == KeyCode::F(4) && form.kind == Kind::Network {
            self.status = match form.policy().and_then(|policy| {
                policy
                    .test_destination(&form.field("Test destination (bare host or IP)"))
                    .map_err(|error| error.to_string())
            }) {
                Ok(matched) => format!(
                    "Draft rule match: {}. {RULE_TEST_NOTICE}",
                    if matched { "ALLOW" } else { "DENY (default)" }
                ),
                Err(error) => error,
            };
            return SandboxAction::None;
        }
        if form.fields.is_empty() {
            return SandboxAction::None;
        }
        if key::SANDBOX_FIRST_FIELD.matches(event) {
            form.focus = 0;
            return SandboxAction::None;
        }
        if key::SANDBOX_LAST_FIELD.matches(event) {
            form.focus = form.fields.len() - 1;
            return SandboxAction::None;
        }
        if matches!(event.code, KeyCode::Tab | KeyCode::BackTab) {
            form.focus = if event.code == KeyCode::BackTab {
                (form.focus + form.fields.len() - 1) % form.fields.len()
            } else {
                (form.focus + 1) % form.fields.len()
            };
            return SandboxAction::None;
        }
        let field = &mut form.fields[form.focus];
        if form.kind == Kind::Network
            && matches!(
                field.label,
                "Domains (one per line)" | "CIDRs (one per line)"
            )
            && network_list_key(&mut field.editor, event, MAX_LIVE_PREVIEW_BYTES)
        {
            return SandboxAction::None;
        }
        if event.code == KeyCode::F(3) && !field.choices.is_empty() {
            let next = field
                .choices
                .iter()
                .position(|choice| *choice == field.editor.text())
                .map_or(0, |index| (index + 1) % field.choices.len());
            field.editor.set_text(field.choices[next].into());
            return SandboxAction::None;
        }
        if field.secret() && unfit_for_secret(event) {
            return SandboxAction::None;
        }
        let Ok(result) = field
            .editor
            .handle_key_bounded(event, MAX_LIVE_PREVIEW_BYTES)
        else {
            self.status = super::TOO_LARGE.into();
            return SandboxAction::None;
        };
        if field.secret() {
            SandboxAction::None
        } else {
            editor_action(result)
        }
    }

    pub(super) fn start_operation(&mut self, operation: LiveOperation) -> SandboxAction {
        let Some(saved) = self.baseline.clone() else {
            return SandboxAction::None;
        };
        if self.pending.is_some() || self.live_pending.is_some() || self.dirty() {
            return SandboxAction::None;
        }
        self.operation += 1;
        let ticket = StoreTicket {
            session: self.session,
            operation: self.operation,
            draft_revision: self.revision,
        };
        let scope = SandboxSnapshotRequest {
            conversation: self.conversation,
            manager_session: self.session,
            configuration_revision: saved.saved().revision().clone(),
            configuration_epoch: self.configuration_epoch,
        };
        self.live_pending = Some((ticket.clone(), scope.clone()));
        if !matches!(operation, LiveOperation::ProbeImage(_)) {
            self.live_form = None;
        }
        self.live_report("Action accepted; awaiting admission / lifecycle result.\nEsc closes the manager, NOT the operation. Do not resubmit Create; reconcile an unknown outcome.".into());
        SandboxAction::Live(Box::new(LiveRequest {
            ticket,
            scope,
            saved,
            operation,
        }))
    }

    pub(super) fn view_live(&mut self, frame: &mut Frame, area: Rect) {
        let Some(form) = self.live_form.as_mut() else {
            return;
        };
        if let Some(picker) = &mut form.picker {
            picker.view(frame, area);
            return;
        }
        if form.fields.is_empty() {
            self.readers[ReadSurface::Body as usize].view(frame, area, "Ctrl+Enter reviews this action. No action runs until the separate confirmation is accepted. Escape goes back and keeps the action draft; F6 discards it.".into());
            return;
        }
        let help = if form.kind == Kind::Network {
            match form.fields[form.focus].label {
                "Domains (one per line)" => DOMAIN_HELP,
                "CIDRs (one per line)" => CIDR_HELP,
                "TLS mode" => {
                    "F3 switches TLS mode. MITM needs a guest CA and can break certificate pinning. Live changes require provider support."
                }
                _ => {
                    "Bare hostname or IP only, not a URL. F4 evaluates the draft rules locally; this is not a connectivity or TLS test."
                }
            }
        } else {
            ""
        };
        let help_rows = if help.is_empty() {
            0
        } else {
            3.min(area.height / 3)
        };
        let [list, editor, help_area] = Layout::vertical([
            Constraint::Length((form.fields.len() as u16).min(LIVE_FIELD_ROWS)),
            Constraint::Min(1),
            Constraint::Length(help_rows),
        ])
        .areas(area);
        self.readers[ReadSurface::Help as usize].view(frame, help_area, help.into());
        self.fields_area = list;
        if self.live_scroll_focus != form.focus {
            self.live_scroll = self.live_scroll.min(form.focus);
            if form.focus >= self.live_scroll + list.height as usize {
                self.live_scroll = form
                    .focus
                    .saturating_sub(list.height.saturating_sub(1) as usize);
            }
            self.live_scroll_focus = form.focus;
        }
        self.live_scroll = self
            .live_scroll
            .min(form.fields.len().saturating_sub(list.height as usize));
        let start = self.live_scroll;
        for (row, (index, field)) in form
            .fields
            .iter()
            .enumerate()
            .skip(start)
            .take(list.height as usize)
            .enumerate()
        {
            let area = Rect {
                y: list.y + row as u16,
                height: 1,
                ..list
            };
            frame.render_widget(
                Paragraph::new(format!(
                    "{} {}{}",
                    if form.focus == index { ">" } else { " " },
                    field.label,
                    if field.choices.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}; F3]", field.choices.join(" | "))
                    }
                ))
                .style(hover_style(
                    theme::current().tool_dim,
                    self.hovered == Some(Control::LiveField(index)),
                )),
                area,
            );
            self.hits.push((area, Control::LiveField(index)));
        }
        self.fields_bar
            .draw(frame, list, form.fields.len() as u32, start as u32);
        let count = form.fields.len();
        let field = &mut form.fields[form.focus];
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(hover_style(
                Style::default(),
                self.hovered == Some(Control::Editor),
            ))
            .title(format!("{} ({}/{})", field.label, form.focus + 1, count));
        self.editor_area = block.inner(editor);
        frame.render_widget(block, editor);
        if field.secret() {
            field.editor.view(frame, self.editor_area);
        } else {
            field.editor.view_json(frame, self.editor_area);
        }
    }
}

/// Whether `event` would put more than printable ASCII into a secret, which is
/// one line of it.
fn unfit_for_secret(event: KeyEvent) -> bool {
    match text_field::decode(event, FieldKind::Document) {
        Some(TextCommand::Edit(EditCommand::Insert(character))) => !character.is_ascii_graphic(),
        Some(TextCommand::Edit(EditCommand::Newline | EditCommand::IndentedNewline)) => true,
        _ => false,
    }
}

fn instance_action_error(
    kind: &Kind,
    target: Option<&SandboxInstanceSnapshot>,
) -> Option<&'static str> {
    let Some(target) = target else {
        return Some("Select an instance first; refresh if unavailable.");
    };
    let record = target.record.as_ref();
    let pending = record
        .and_then(|record| record.lifecycle.as_ref())
        .is_some_and(|intent| intent.is_pending());
    match kind {
        Kind::Reconcile => {
            return record
                .is_none()
                .then_some("Attach as borrowed before reconciling a local record.");
        }
        Kind::AcknowledgeFailure => {
            return (!pending).then_some("No unresolved lifecycle intent to acknowledge.");
        }
        Kind::CancelCreate => {
            return (pending
                || !record
                    .and_then(|record| record.create.as_ref())
                    .and_then(|intent| intent.operation.as_ref())
                    .is_some_and(|operation| {
                        operation.status == OperationStatus::Creating && !operation.cancel_requested
                    }))
            .then_some("Only an unresolved create can be cancelled; Reconcile first.");
        }
        _ => {}
    }
    if pending
        || matches!(
            target.state,
            SandboxInstanceState::Creating
                | SandboxInstanceState::Recovering
                | SandboxInstanceState::Unknown
        )
    {
        return Some(RECOVERY_REQUIRED);
    }
    if *kind != Kind::Attach && record.is_none() {
        return Some("Attach as borrowed before controlling this instance.");
    }
    if *kind == Kind::Detach {
        return None;
    }
    let Some(instance) = &target.live else {
        return Some("Live state unavailable: Inspect / Reconcile first.");
    };
    if record.is_some_and(|record| record.instance.as_ref() != Some(instance)) {
        return Some("Live state changed: Reconcile, then review a fresh action.");
    }
    if matches!(kind, Kind::Pause | Kind::Resume | Kind::Restart) && !instance.persistent {
        return Some(PERSISTENT_REQUIRED);
    }
    match kind {
        Kind::Resume if target.state != SandboxInstanceState::Paused => {
            Some("Resume requires a paused instance; Reconcile if the state is stale.")
        }
        Kind::Pause | Kind::Restart | Kind::Extend | Kind::Attach | Kind::Network
            if target.state != SandboxInstanceState::Running =>
        {
            Some("Requires a running instance; Resume a paused disk separately.")
        }
        _ => None,
    }
}

fn lease_review(lease: LeaseSeconds) -> String {
    match lease.finite() {
        Some(_) => format!("New running lease: {lease}."),
        None => NO_EXPIRY_REVIEW.into(),
    }
}

fn pretty(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::{
        ACK_REVIEW, EXTEND_REVIEW, INSTANCE_ACTIONS, LEASE_FIELD, LEASE_HELP, NO_EXPIRY_REVIEW,
        PAUSE_REVIEW, PERSISTENT_REQUIRED, RECOVERY_REQUIRED, RESTART_REVIEW,
        instance_action_error,
    };
    use super::{Kind, LIVE_KEYS, LiveField, LiveForm, TextEditor, image, key};
    #[cfg(unix)]
    use crate::components::sandbox_manager::{
        Confirmation, SandboxAction, SnapshotState,
        tests::{fixture, live_instance},
    };
    #[cfg(unix)]
    use crate::sandbox::{LiveOperation, SandboxInstanceState};
    #[cfg(unix)]
    use caudra_config::sandbox::LeaseSeconds;
    #[cfg(unix)]
    use caudra_sandbox::{LifecycleAction, dto::InstanceState};
    use caudra_sandbox::{
        dto::{PROTOCOL_VERSION, TRANSFER_PROTOCOL},
        local_admin::AdminOperation,
    };
    #[cfg(unix)]
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use serde_json::json;
    use test_case::test_case;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const RAW: &str = "Raw operation JSON override (optional)";
    #[cfg(unix)]
    const PROFILE_LEASE: LeaseSeconds = LeaseSeconds::new(3600);
    const STALE_KEYS: &str = "the live form key line names a key that is no longer bound";

    #[test]
    fn live_key_line_names_the_bound_field_jumps() {
        for bind in [key::SANDBOX_FIRST_FIELD, key::SANDBOX_LAST_FIELD] {
            assert!(
                LIVE_KEYS.contains(bind.label),
                "{STALE_KEYS}: {}",
                bind.label
            );
        }
    }

    #[cfg(unix)]
    #[test_case(Kind::Pause, InstanceState::Running, true; "running_pause")]
    #[test_case(Kind::Restart, InstanceState::Running, true; "running_restart")]
    #[test_case(Kind::Resume, InstanceState::Running, false; "running_resume_disabled")]
    #[test_case(Kind::Resume, InstanceState::Paused, true; "paused_resume")]
    #[test_case(Kind::Pause, InstanceState::Paused, false; "paused_stop_disabled")]
    #[test_case(Kind::Extend, InstanceState::Paused, false; "paused_extend_disabled")]
    #[test_case(Kind::Resume, InstanceState::Deleted, false; "deleted_resume_disabled")]
    #[test_case(Kind::Pause, InstanceState::Deleted, false; "deleted_stop_disabled")]
    #[test_case(Kind::Restart, InstanceState::Deleted, false; "deleted_restart_disabled")]
    #[test_case(Kind::Attach, InstanceState::Deleted, false; "deleted_attach_disabled")]
    #[test_case(Kind::Pause, InstanceState::Pausing, false; "pending_pause_disabled")]
    #[test_case(Kind::Reconcile, InstanceState::Pausing, true; "pending_reconcile")]
    #[test_case(Kind::CancelCreate, InstanceState::Running, false; "completed_create_cannot_cancel")]
    fn action_eligibility(kind: Kind, state: InstanceState, enabled: bool) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let mut target = manager
            .state
            .as_ref()
            .unwrap()
            .selected_instance()
            .unwrap()
            .clone();
        target.state = SandboxInstanceState::from(&state);
        target.live.as_mut().unwrap().state = state;
        target.record.as_mut().unwrap().instance = target.live.clone();
        assert_eq!(
            instance_action_error(&kind, Some(&target)).is_none(),
            enabled
        );
    }

    #[cfg(unix)]
    #[test_case(Kind::Pause; "pause")]
    #[test_case(Kind::Restart; "restart")]
    fn persistent_actions_review_without_implicit_delete(kind: Kind) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        assert!(matches!(state.open_live(kind.clone()), SandboxAction::None));
        assert!(state.live_pending.is_none());
        state.live_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        let Some(Confirmation::Live {
            operation,
            preview,
            choice,
        }) = &state.confirmation
        else {
            panic!("missing review")
        };
        assert_eq!(*choice, 0);
        if kind == Kind::Restart {
            assert!(matches!(
                **operation,
                LiveOperation::Restart { lease_seconds, .. } if lease_seconds == PROFILE_LEASE
            ));
            assert!(preview.contains(RESTART_REVIEW));
        } else {
            assert!(matches!(
                **operation,
                LiveOperation::Control {
                    action: LifecycleAction::Pause,
                    ..
                }
            ));
            assert!(preview.contains(PAUSE_REVIEW));
        }
        assert!(matches!(state.confirm(0), SandboxAction::None));
        assert!(state.live_pending.is_none());
    }

    #[cfg(unix)]
    #[test_case("0", Some(LeaseSeconds::NO_EXPIRY); "no_expiry")]
    #[test_case("forever", None; "not_whole_seconds")]
    fn extend_reviews_a_lease_with_no_expiry(text: &str, expected: Option<LeaseSeconds>) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        assert!(matches!(state.open_live(Kind::Extend), SandboxAction::None));
        set(state.live_form.as_mut().unwrap(), LEASE_FIELD, text);
        state.live_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        let Some(expected) = expected else {
            assert!(state.confirmation.is_none());
            assert_eq!(state.status, LEASE_HELP);
            return;
        };
        let Some(Confirmation::Live {
            operation, preview, ..
        }) = &state.confirmation
        else {
            panic!("missing review")
        };
        assert!(matches!(
            **operation,
            LiveOperation::Control {
                action: LifecycleAction::Extend { lease_seconds },
                ..
            } if lease_seconds == expected
        ));
        assert!(preview.contains(EXTEND_REVIEW));
        assert!(preview.contains(NO_EXPIRY_REVIEW));
    }

    #[cfg(unix)]
    #[test_case(false; "ephemeral_disk")]
    #[test_case(true; "pending_intent")]
    fn disabled_picker_and_shortcut_never_dispatch(pending: bool) {
        let (_directory, _store, mut manager) = fixture();
        live_instance(&mut manager, false);
        let state = manager.state.as_mut().unwrap();
        let SnapshotState::Ready(rows) = &mut state.snapshot.as_mut().unwrap().instances else {
            panic!("missing instance")
        };
        let row = &mut rows[0];
        if pending {
            let record = row.record.as_mut().unwrap();
            record.lifecycle = Some(serde_json::from_value(json!({"action":"pause", "expected":record.instance.as_ref().unwrap().expected(), "lease_seconds":null, "observed_revision":null, "policy":null, "policy_revision":null, "minimum_lease_deadline":null, "allow_equal_revision":false, "failure_acknowledged":false})).unwrap());
        } else {
            row.live.as_mut().unwrap().persistent = false;
            row.record.as_mut().unwrap().instance = row.live.clone();
        }
        let index = INSTANCE_ACTIONS
            .iter()
            .position(|(_, kind)| *kind == Some(Kind::Pause))
            .unwrap();
        state.open_instance_actions();
        assert!(matches!(
            state.choose_instance_action(index),
            SandboxAction::None
        ));
        assert_eq!(
            state.status,
            if pending {
                RECOVERY_REQUIRED
            } else {
                PERSISTENT_REQUIRED
            }
        );
        assert!(state.instance_action.is_some());
        assert!(matches!(state.open_live(Kind::Pause), SandboxAction::None));
        state.instance_action = None;
        assert!(matches!(
            state.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE)),
            SandboxAction::None
        ));
        assert_eq!(
            state.status,
            if pending {
                RECOVERY_REQUIRED
            } else {
                PERSISTENT_REQUIRED
            }
        );
        assert!(state.live_form.is_none());
        assert!(state.live_pending.is_none());
        assert!(state.confirmation.is_none());
        if pending {
            state.instance_action = None;
            state.open_live(Kind::AcknowledgeFailure);
            state.live_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
            let Some(Confirmation::Live {
                operation,
                preview,
                choice,
            }) = &state.confirmation
            else {
                panic!("missing acknowledgement review")
            };
            assert_eq!(*choice, 0);
            assert!(preview.contains(ACK_REVIEW));
            assert!(matches!(
                **operation,
                LiveOperation::AcknowledgeFailure { .. }
            ));
            let SandboxAction::Live(request) = state.confirm(1) else {
                panic!("missing explicit acknowledgement")
            };
            assert!(matches!(
                request.operation,
                LiveOperation::AcknowledgeFailure { .. }
            ));
        }
    }

    #[test_case(" API.Example.com \n\n", " 10.20.30.40/24 ", "mitm", true; "normalizes_both_lists_preserves_tls")]
    #[test_case("\n", "", "sni-only", true; "deny_all")]
    #[test_case("https://api.example.com", "", "sni-only", false; "rejects_url")]
    #[test_case("api.example.com", "10.0.0.0/99", "sni-only", false; "rejects_cidr")]
    fn network_policy_fields(domains: &str, cidrs: &str, mode: &str, valid: bool) {
        let mut form = form(Kind::Network);
        form.fields = [
            ("Domains (one per line)", domains),
            ("CIDRs (one per line)", cidrs),
            ("TLS mode", mode),
        ]
        .into_iter()
        .map(|(label, value)| {
            let mut editor = TextEditor::new();
            editor.set_text(value.into());
            LiveField {
                label,
                editor,
                choices: &[],
            }
        })
        .collect();
        let policy = form.policy();
        assert_eq!(policy.is_ok(), valid);
        if let Ok(policy) = policy {
            assert_eq!(policy.mode, mode);
            if domains.trim().is_empty() {
                assert!(policy.domains.is_empty());
                assert!(policy.cidrs.is_empty());
                assert!(!policy.test_destination("api.example.com").unwrap());
            } else {
                assert_eq!(policy.domains, ["api.example.com"]);
                assert_eq!(policy.cidrs, ["10.20.30.0/24"]);
            }
        }
    }

    fn form(kind: Kind) -> LiveForm {
        let fields = image::fields(&kind, "local".into(), None);
        LiveForm {
            kind,
            fields,
            focus: 0,
            target: None,
            picker: None,
            probe: None,
            origin: None,
        }
    }

    fn set(form: &mut LiveForm, label: &str, value: &str) {
        form.fields
            .iter_mut()
            .find(|field| field.label == label)
            .unwrap()
            .editor
            .set_text(value.into());
    }

    fn manifest_fields(form: &mut LiveForm) {
        for (label, value) in [
            (image::HELPER, "/usr/local/bin/e2b-locald"),
            (image::QEMU, "/usr/bin/qemu-img"),
            (image::DATABASE, "/srv/e2b/database"),
            (image::CATALOG, "/srv/e2b/catalog"),
            ("Template ID", "rust-image"),
            ("Expected current revision (empty for NEW template)", ""),
            ("Manifest schema version", "1"),
            ("Architecture", "x86_64"),
            ("Machine", "q35"),
            ("Minimum CPUs", "2"),
            ("Default CPUs", "4"),
            ("Minimum memory MiB", "1024"),
            ("Default memory MiB", "2048"),
            ("Minimum disk MiB", "8192"),
            ("Default disk MiB", "16384"),
            ("Network topology", "slirp-enforced"),
            (
                "Guest CA compatible (operator reviewed; NOT automatic)",
                "false",
            ),
            ("Workcell version", "0.1.0"),
            ("Workcell binary SHA-256", DIGEST),
            ("Workcell protocol", PROTOCOL_VERSION),
            ("Workcell transfer contract", TRANSFER_PROTOCOL),
            ("Remote workspace feature", "true"),
            ("Workspace snapshots feature", "true"),
            ("Reviewed transfer feature", "true"),
            ("Guest workspace root", "/workspace"),
            ("Guest snapshot root", "/var/lib/workcell-mcp/snapshots"),
            ("Guest transfer root", "/var/lib/workcell-mcp/transfers"),
            ("Recipe SHA-256 (required for build)", DIGEST),
        ] {
            set(form, label, value);
        }
    }

    fn import() -> LiveForm {
        let mut form = form(Kind::ImportImage);
        manifest_fields(&mut form);
        set(&mut form, image::SOURCE, "/images/source with spaces.qcow2");
        set(&mut form, image::SHA256, DIGEST);
        set(&mut form, "Manifest build recipe", "import");
        set(&mut form, "Manifest source revision (optional)", DIGEST);
        form
    }

    #[test]
    fn every_native_import_field_serializes_without_any_template_seed_or_json() {
        let mut form = import();
        set(
            &mut form,
            "Guest CA compatible (operator reviewed; NOT automatic)",
            "true",
        );
        assert!(form.field(RAW).is_empty());
        let AdminOperation::Import(request) = image::operation(&form).unwrap() else {
            panic!("expected import")
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "sourcePath":"/images/source with spaces.qcow2", "expectedSHA256":DIGEST, "expectedRevision":"",
                "manifest": {"schemaVersion":1, "id":"rust-image", "architecture":"x86_64", "machine":"q35",
                    "minimum":{"cpuCount":2,"memoryMB":1024,"diskSizeMB":8192}, "defaults":{"cpuCount":4,"memoryMB":2048,"diskSizeMB":16384},
                    "networkTopology":"slirp-enforced", "guestCA":true,
                    "workcell":{"version":"0.1.0","sha256":DIGEST,"protocolVersion":PROTOCOL_VERSION,"transferProtocol":TRANSFER_PROTOCOL,
                        "remoteWorkspace":true,"workspaceSnapshots":true,"reviewedTransfer":true,"workspaceRoot":"/workspace","snapshotRoot":"/var/lib/workcell-mcp/snapshots","transferRoot":"/var/lib/workcell-mcp/transfers"},
                    "build":{"recipe":"import","recipeSHA256":DIGEST,"sourceRevision":DIGEST}}
            })
        );
        let helper = serde_json::to_value(image::helper(&form)).unwrap();
        assert_eq!(
            helper,
            json!({"executable":"/usr/local/bin/e2b-locald","qemu_img":"/usr/bin/qemu-img","database":"/srv/e2b/database","catalog_dir":"/srv/e2b/catalog"})
        );
        let raw = serde_json::to_string(&request).unwrap();
        set(&mut form, RAW, &raw);
        assert_eq!(
            serde_json::to_value(image::operation(&form).unwrap()).unwrap()["input"],
            serde_json::to_value(request).unwrap()
        );
    }

    #[test_case("Architecture", "aarch64"; "unsupported_architecture")]
    #[test_case("Machine", "virt"; "unsupported_machine")]
    #[test_case("Manifest schema version", "2"; "unsupported_schema")]
    #[test_case("Minimum CPUs", "5"; "minimum_exceeds_default")]
    #[test_case("Default CPUs", "0"; "zero_resources")]
    #[test_case("Workcell protocol", "old"; "protocol")]
    #[test_case("Workcell transfer contract", "other"; "transfer")]
    #[test_case("Remote workspace feature", "false"; "contradictory_features")]
    #[test_case("Reviewed transfer feature", "yes"; "boolean")]
    #[test_case("Guest snapshot root", "/workspace/snapshots"; "overlapping_roots")]
    #[test_case("Guest transfer root", "/workspace/transfers"; "transfer_overlaps_workspace")]
    #[test_case("Guest transfer root", "/var/lib/workcell-mcp/snapshots"; "transfer_overlaps_snapshots")]
    #[test_case("Guest workspace root", "/workspace/../private"; "guest_traversal")]
    #[test_case("Expected current revision (empty for NEW template)", "latest"; "mutable_revision")]
    #[test_case("Template ID", "Uppercase"; "template_name")]
    #[test_case(image::SOURCE, "../image.qcow2"; "host_traversal")]
    fn native_import_rejects_invalid_typed_fields(label: &str, value: &str) {
        let mut form = import();
        set(&mut form, label, value);
        assert!(image::operation(&form).is_err());
    }

    #[test_case("base"; "base_recipe")]
    #[test_case("egress"; "egress_recipe")]
    #[test_case("caudra"; "caudra_recipe")]
    fn native_build_variants_validate_all_input_references_and_paths(recipe: &str) {
        let mut form = form(Kind::Build);
        manifest_fields(&mut form);
        set(&mut form, "Recipe variant", recipe);
        set(
            &mut form,
            "Recipe scripts directory (absolute)",
            "/opt/e2b/scripts",
        );
        if recipe == "base" {
            for (label, value) in [
                ("Network topology", "slirp-unrestricted"),
                ("Workspace snapshots feature", "false"),
                ("Reviewed transfer feature", "true"),
                ("Guest workspace root", "/workspace"),
                ("Guest snapshot root", ""),
                (
                    "Container proxy binary (base only; absolute)",
                    "/opt/e2b/container-proxy",
                ),
                ("Container proxy SHA-256 (base only)", DIGEST),
            ] {
                set(&mut form, label, value);
            }
        } else {
            set(&mut form, "Source template ID (derived builds)", "base");
            set(
                &mut form,
                "Source template revision (derived builds)",
                DIGEST,
            );
        }
        if recipe != "egress" {
            set(
                &mut form,
                "Workcell binary (base/caudra; absolute)",
                "/opt/e2b/workcell",
            );
        }
        let AdminOperation::Build(request) = image::operation(&form).unwrap() else {
            panic!("expected build")
        };
        assert_eq!(request.manifest.build.recipe, recipe);
        assert_eq!(request.scripts_dir.to_str().unwrap(), "/opt/e2b/scripts");
        assert_eq!(
            request.manifest.build.source_revision,
            request.source_revision
        );
        assert_eq!(serde_json::to_value(&request).unwrap()["recipe"], recipe);
        set(
            &mut form,
            "Recipe scripts directory (absolute)",
            "./scripts",
        );
        assert!(image::operation(&form).is_err());
        set(
            &mut form,
            "Recipe scripts directory (absolute)",
            "/opt/e2b/scripts",
        );
        set(
            &mut form,
            "Source template revision (derived builds)",
            "unreviewed-head",
        );
        assert!(image::operation(&form).is_err());
    }
}
