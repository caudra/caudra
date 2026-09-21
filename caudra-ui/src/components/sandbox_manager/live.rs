use super::{
    Confirmation, Control, Manager, SandboxAction, SandboxView, SnapshotState, StoreTicket,
    TextEditor, editor_action, image,
};
use crate::{
    sandbox::{
        LiveOperation, LiveRequest, MAX_LIVE_PREVIEW_BYTES, RULE_TEST_NOTICE,
        SandboxInstanceSnapshot, SandboxSnapshotRequest,
    },
    theme,
};
use caudra_config::sandbox::SandboxName;
use caudra_sandbox::{
    CreateReview, LifecycleAction, Ownership,
    dto::Policy,
    local_admin::{AdminOperation, AdminRequest, ImageProbe, ProbedImage},
};
use caudra_storage::sandbox_auth::{SandboxApiKey, SandboxCredentialRef};
use caudra_workspace::WorkspacePath;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::path::{Path, PathBuf};

const DEFAULT_LEASE: &str = "3600";
const DESTROY_BORROWED: &str = "Destroy borrowed VM/disk (type DELETE; empty detaches)";
const DELETE_APPROVAL: &str = "DELETE";
const TRANSITION: &str = "Attach opens a NEW sandbox session after Workcell verification and session gates. The local session is saved unchanged. No conversation, grants, pending prompts, files or secrets are copied. Closing/exiting detaches; it never deletes or pauses the VM.";
const MIB_BYTES: u64 = 1024 * 1024;
const LIVE_FIELD_ROWS: u16 = 6;

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Kind {
    Create,
    Attach,
    Pause,
    Resume,
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
    pub secret: bool,
    pub choices: &'static [&'static str],
}

pub(super) struct LiveForm {
    pub(super) kind: Kind,
    pub fields: Vec<LiveField>,
    pub focus: usize,
    target: Option<SandboxInstanceSnapshot>,
    pub picker: Option<image::HostPicker>,
    pub probe: Option<ProbedImage>,
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
            domains: self
                .field("Domains (one per line)")
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_owned())
                .collect(),
            cidrs: self
                .field("CIDRs (one per line)")
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_owned())
                .collect(),
        };
        policy.validate().map_err(|error| error.to_string())?;
        Ok(policy)
    }
}

impl Manager {
    pub(super) fn open_live(&mut self, kind: Kind) -> SandboxAction {
        if self.dirty() || self.pending.is_some() || self.live_pending.is_some() {
            self.status = "Save or discard configuration edits and wait for pending acknowledgments before live actions.".into();
            return SandboxAction::None;
        }
        let selected = self
            .entries()
            .get(self.selected)
            .cloned()
            .unwrap_or_default();
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
            fields.push(LiveField {
                label,
                editor,
                secret,
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
            Kind::Resume | Kind::Extend => field(
                "Lease seconds",
                target
                    .as_ref()
                    .and_then(|row| row.effective.as_ref())
                    .map(|launch| {
                        launch
                            .configuration()
                            .profile
                            .value()
                            .running_ttl_seconds
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
        });
        self.detail_scroll = 0;
        self.status = "Live action draft. Ctrl+Enter previews (does not execute). Esc closes and retains this draft; F6 explicitly discards it. Tab changes field. Network F4 evaluates rules only.".into();
        if self.live_form.as_ref().is_some_and(|form| {
            matches!(
                form.kind,
                Kind::ImportImage | Kind::Build | Kind::Gc | Kind::InspectImage
            )
        }) {
            self.status = image::ADMIN_NOTICE.into();
        }
        SandboxAction::None
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
                let template = doctor
                    .templates
                    .iter()
                    .find(|template| {
                        template.manifest.id == saved.template
                            && template.revision == saved.template_revision
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
                    let operation = match form.kind {
                        Kind::Attach => LiveOperation::Attach { name, revision },
                        Kind::Reconcile => LiveOperation::Reconcile { name },
                        Kind::CancelCreate => LiveOperation::CancelCreate { name, revision },
                        Kind::AcknowledgeFailure => {
                            LiveOperation::AcknowledgeFailure { name, revision }
                        }
                        _ => {
                            let lease = || {
                                form.field("Lease seconds")
                                    .parse::<u32>()
                                    .ok()
                                    .filter(|lease| *lease > 0)
                                    .ok_or_else(|| {
                                        "Lease must be a positive number of seconds".to_owned()
                                    })
                            };
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
                        LiveOperation::AcknowledgeFailure { .. } => record.lifecycle_failure_review().map_err(|error| error.to_string())?,
                        LiveOperation::Control { action: LifecycleAction::Delete { destroy_borrowed: false }, .. } if record.ownership == Ownership::Borrowed => "DETACH borrowed instance only. VM and disk are NOT deleted.".into(),
                        LiveOperation::Control { action: LifecycleAction::Detach, .. } => "DETACH local record only. VM and disk are NOT deleted.".into(),
                        LiveOperation::Control { action: LifecycleAction::Delete { .. }, .. } => "PERMANENTLY DELETE this VM AND DISK. Cannot be undone.".into(),
                        LiveOperation::Control { action: LifecycleAction::ApplyPolicy { policy }, .. } => {
                            let doctor = self.snapshot.as_ref().and_then(|snapshot| snapshot.providers.get(&target.provider)).and_then(|provider| provider.doctor.as_ref()).ok_or("Provider TLS metadata unavailable")?;
                            format!("Apply host-only rules (no port/method policy). MITM terminates TLS and can break pinning.\n{}\nDiscovered TLS modes: {:?}; live mode change: {}; reviewed image guest CA: {}; instance guest CA ready: {}", pretty(policy)?, doctor.discovery.tls_modes, doctor.discovery.capabilities.live_tls_mode_change, record.template.manifest.guest_ca, target.live.as_ref().is_some_and(|instance| instance.egress.guest_ca_ready))
                        }
                        LiveOperation::Control { action, .. } => format!("{action:?}\nConditional on the reviewed execution and revision. Saved profiles remain unchanged."),
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
        if event.code == KeyCode::Esc {
            self.open = false;
            return SandboxAction::None;
        }
        if event.code == KeyCode::F(6) {
            self.live_form = None;
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
                    "Saved rule match: {}. {RULE_TEST_NOTICE}",
                    if matched { "ALLOW" } else { "DENY (default)" }
                ),
                Err(error) => error,
            };
            return SandboxAction::None;
        }
        if form.fields.is_empty() {
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
        if event.code == KeyCode::F(3) && !field.choices.is_empty() {
            let next = field
                .choices
                .iter()
                .position(|choice| *choice == field.editor.text())
                .map_or(0, |index| (index + 1) % field.choices.len());
            field.editor.set_text(field.choices[next].into());
            return SandboxAction::None;
        }
        if field.secret
            && matches!(event.code, KeyCode::Char(character) if event.modifiers.is_empty() && !character.is_ascii_graphic())
        {
            return SandboxAction::None;
        }
        if field.secret && event.code == KeyCode::Enter {
            return SandboxAction::None;
        }
        if field.editor.text().len() >= MAX_LIVE_PREVIEW_BYTES
            && matches!(event.code, KeyCode::Char(_) | KeyCode::Enter)
            && event.modifiers.is_empty()
        {
            return SandboxAction::None;
        }
        let result = field.editor.handle_key(event);
        if field.secret {
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
        self.status =
            "Accepted live action. Esc closes, not cancels. Outcome remains durably recoverable."
                .into();
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
        if let Some(picker) = &form.picker {
            picker.view(frame, area);
            return;
        }
        if form.fields.is_empty() {
            frame.render_widget(Paragraph::new("Ctrl+Enter reviews this action. No action runs until the separate confirmation is accepted. Escape closes, F6 discards the action draft.").wrap(Wrap { trim: false }), area);
            return;
        }
        let [list, editor] = Layout::vertical([
            Constraint::Length((form.fields.len() as u16).min(LIVE_FIELD_ROWS)),
            Constraint::Min(1),
        ])
        .areas(area);
        let start = form
            .focus
            .saturating_sub(list.height.saturating_sub(1) as usize);
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
                .style(theme::current().tool_dim),
                area,
            );
            self.hits.push((area, Control::LiveField(index)));
        }
        let count = form.fields.len();
        let field = &mut form.fields[form.focus];
        let block = Block::default().borders(Borders::ALL).title(format!(
            "{} ({}/{})",
            field.label,
            form.focus + 1,
            count
        ));
        self.editor_area = block.inner(editor);
        frame.render_widget(block, editor);
        if field.secret {
            field.editor.view_masked(frame, self.editor_area);
        } else {
            field.editor.view(frame, self.editor_area);
        }
    }
}

fn pretty(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::{Kind, LiveForm, image};
    use caudra_sandbox::{
        dto::{PROTOCOL_VERSION, TRANSFER_PROTOCOL},
        local_admin::AdminOperation,
    };
    use serde_json::json;
    use test_case::test_case;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const RAW: &str = "Raw operation JSON override (optional)";

    fn form(kind: Kind) -> LiveForm {
        let fields = image::fields(&kind, "local".into(), None);
        LiveForm {
            kind,
            fields,
            focus: 0,
            target: None,
            picker: None,
            probe: None,
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
