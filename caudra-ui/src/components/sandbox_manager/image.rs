use super::{
    EditorMouse, TextEditor,
    live::{Kind, LiveField, LiveForm},
    view::hover_style,
};
use crate::{
    components::scrollbar::{Scrollbar, ScrollbarMouse},
    theme,
};
use caudra_config::sandbox::{Revision, SandboxName};
use caudra_sandbox::{
    dto::{
        BuildManifest, Manifest, PROTOCOL_VERSION, Resources, TRANSFER_PROTOCOL, Template,
        WorkcellManifest,
    },
    local_admin::{
        AdminOperation, BuildRecipe, BuildRequest, ImportRequest, LocalHelper, validate_path,
    },
};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position, Rect},
    widgets::{Block, Borders, Paragraph},
};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub(super) const PROVIDER: &str = "Provider";
pub(super) const HELPER: &str = "Local helper executable (absolute)";
pub(super) const QEMU: &str = "Trusted qemu-img executable (absolute)";
pub(super) const DATABASE: &str = "Local database (absolute)";
pub(super) const CATALOG: &str = "Local catalog directory (absolute)";
pub(super) const SOURCE: &str = "Source qcow2 (absolute; F2 host picker)";
pub(super) const SHA256: &str = "Image SHA-256 (F4 reviewed probe)";
const EXPECTED: &str = "Expected current revision (empty for NEW template)";
const ID: &str = "Template ID";
const REVISION: &str = "Immutable template revision";
const RAW: &str = "Raw operation JSON override (optional)";
const RECIPE: &str = "Recipe variant";
const SOURCE_ID: &str = "Source template ID (derived builds)";
const SOURCE_REVISION: &str = "Source template revision (derived builds)";
const BOOLEAN: &[&str] = &["false", "true"];
const MAX_PICKER_ENTRIES: usize = 2048;
pub(super) const ADMIN_NOTICE: &str = "OFFLINE: daemon must be stopped separately. Caudra cannot stop third-party processes. Ctrl+Enter reviews exact commands + stdin; F2 browses host qcow2; F3 cycles choices; F4 reviews a pinned qemu-img probe. No secrets/environment injection.";

pub(super) fn fields(kind: &Kind, provider: String, template: Option<&Template>) -> Vec<LiveField> {
    let mut fields = Vec::new();
    let mut add = |label, value: String, choices: &'static [&'static str]| {
        let mut editor = TextEditor::new();
        editor.set_text(value);
        fields.push(LiveField {
            label,
            editor,
            secret: false,
            choices,
        });
    };
    add(PROVIDER, provider, &[]);
    for label in [HELPER, QEMU, DATABASE, CATALOG] {
        add(label, String::new(), &[]);
    }
    if matches!(kind, Kind::InspectImage | Kind::Gc) {
        add(
            ID,
            template
                .map(|template| template.manifest.id.to_string())
                .unwrap_or_default(),
            &[],
        );
        add(
            REVISION,
            template
                .map(|template| template.revision.as_str().to_owned())
                .unwrap_or_default(),
            &[],
        );
    } else {
        if *kind == Kind::ImportImage {
            add(SOURCE, String::new(), &[]);
            add(SHA256, String::new(), &[]);
        } else {
            add(RECIPE, "caudra".into(), &["base", "egress", "caudra"]);
            add("Recipe scripts directory (absolute)", String::new(), &[]);
            add(SOURCE_ID, String::new(), &[]);
            add(SOURCE_REVISION, String::new(), &[]);
            add(
                "Workcell binary (base/caudra; absolute)",
                String::new(),
                &[],
            );
            add(
                "Container proxy binary (base only; absolute)",
                String::new(),
                &[],
            );
            add("Container proxy SHA-256 (base only)", String::new(), &[]);
        }
        add(ID, String::new(), &[]);
        add(EXPECTED, String::new(), &[]);
        add("Manifest schema version", "1".into(), &["1"]);
        add("Architecture", "x86_64".into(), &["x86_64"]);
        add("Machine", "q35".into(), &["q35"]);
        for (label, value) in [
            ("Minimum CPUs", "1"),
            ("Default CPUs", "2"),
            ("Minimum memory MiB", "512"),
            ("Default memory MiB", "1024"),
            ("Minimum disk MiB", ""),
            ("Default disk MiB", ""),
        ] {
            add(label, value.into(), &[]);
        }
        add(
            "Network topology",
            "slirp-unrestricted".into(),
            &[
                "slirp-unrestricted",
                "slirp-enforced",
                "passt-unrestricted",
                "managed-unrestricted",
            ],
        );
        add(
            "Guest CA compatible (operator reviewed; NOT automatic)",
            "false".into(),
            BOOLEAN,
        );
        add("Workcell version", String::new(), &[]);
        add("Workcell binary SHA-256", String::new(), &[]);
        add(
            "Workcell protocol",
            PROTOCOL_VERSION.into(),
            &[PROTOCOL_VERSION],
        );
        add(
            "Workcell transfer contract",
            TRANSFER_PROTOCOL.into(),
            &[TRANSFER_PROTOCOL],
        );
        for label in [
            "Remote workspace feature",
            "Workspace snapshots feature",
            "Reviewed transfer feature",
        ] {
            add(label, "false".into(), BOOLEAN);
        }
        add("Guest workspace root", String::new(), &[]);
        add("Guest snapshot root", String::new(), &[]);
        add("Guest transfer root", String::new(), &[]);
        if *kind == Kind::ImportImage {
            add(
                "Manifest build recipe",
                "import".into(),
                &["import", "base", "egress", "caudra"],
            );
            add("Manifest source revision (optional)", String::new(), &[]);
        }
        add("Recipe SHA-256 (required for build)", String::new(), &[]);
    }
    add(RAW, String::new(), &[]);
    fields
}

pub(super) fn helper(form: &LiveForm) -> LocalHelper {
    LocalHelper {
        executable: form.field(HELPER).into(),
        qemu_img: form.field(QEMU).into(),
        database: form.field(DATABASE).into(),
        catalog_dir: form.field(CATALOG).into(),
    }
}

pub(super) fn operation(form: &LiveForm) -> Result<AdminOperation, String> {
    let text = |label| form.field(label);
    let revision =
        |label| Revision::parse(&text(label)).map_err(|error| format!("{label}: {error}"));
    if !text(RAW).is_empty() {
        return match form.kind {
            Kind::ImportImage => serde_json::from_str(&text(RAW)).map(AdminOperation::Import),
            Kind::Build => serde_json::from_str(&text(RAW)).map(AdminOperation::Build),
            _ => return Err("Raw override is only available for import/build".into()),
        }
        .map_err(|error| format!("{RAW}: {error}"));
    }
    let id = SandboxName::parse(&text(ID)).map_err(|error| format!("{ID}: {error}"))?;
    if matches!(form.kind, Kind::Gc | Kind::InspectImage) {
        let id = id.to_string();
        let revision = revision(REVISION)?.as_str().to_owned();
        return Ok(if form.kind == Kind::Gc {
            AdminOperation::Gc { id, revision }
        } else {
            AdminOperation::Inspect { id, revision }
        });
    }
    let number = |label| {
        text(label)
            .parse::<u32>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("{label}: enter a positive integer"))
    };
    let boolean = |label| {
        text(label)
            .parse::<bool>()
            .map_err(|_| format!("{label}: choose true or false with F3"))
    };
    let build = form.kind == Kind::Build;
    let manifest = Manifest {
        schema_version: number("Manifest schema version")?,
        id,
        architecture: text("Architecture"),
        machine: text("Machine"),
        minimum: Resources {
            cpu_count: number("Minimum CPUs")?,
            memory_mb: number("Minimum memory MiB")?,
            disk_size_mb: number("Minimum disk MiB")?,
        },
        defaults: Resources {
            cpu_count: number("Default CPUs")?,
            memory_mb: number("Default memory MiB")?,
            disk_size_mb: number("Default disk MiB")?,
        },
        network_topology: text("Network topology"),
        guest_ca: boolean("Guest CA compatible (operator reviewed; NOT automatic)")?,
        workcell: WorkcellManifest {
            version: text("Workcell version"),
            sha256: text("Workcell binary SHA-256"),
            protocol_version: text("Workcell protocol"),
            transfer_protocol: text("Workcell transfer contract"),
            remote_workspace: boolean("Remote workspace feature")?,
            workspace_snapshots: boolean("Workspace snapshots feature")?,
            reviewed_transfer: boolean("Reviewed transfer feature")?,
            workspace_root: text("Guest workspace root"),
            snapshot_root: text("Guest snapshot root"),
            transfer_root: text("Guest transfer root"),
        },
        build: BuildManifest {
            recipe: text(if build {
                RECIPE
            } else {
                "Manifest build recipe"
            }),
            recipe_sha256: text("Recipe SHA-256 (required for build)"),
            source_revision: text(if build {
                SOURCE_REVISION
            } else {
                "Manifest source revision (optional)"
            }),
        },
    };
    manifest.validate().map_err(|error| error.to_string())?;
    if !text(EXPECTED).is_empty() {
        revision(EXPECTED)?;
    }
    if build {
        let recipe = match text(RECIPE).as_str() {
            "base" => BuildRecipe::Base,
            "egress" => BuildRecipe::Egress,
            "caudra" => BuildRecipe::Caudra,
            _ => return Err("Choose base, egress or caudra".into()),
        };
        let request = BuildRequest {
            recipe,
            scripts_dir: text("Recipe scripts directory (absolute)").into(),
            expected_revision: text(EXPECTED),
            manifest,
            source_template_id: text(SOURCE_ID),
            source_revision: text(SOURCE_REVISION),
            workcell_binary: text("Workcell binary (base/caudra; absolute)").into(),
            container_proxy_binary: text("Container proxy binary (base only; absolute)").into(),
            container_proxy_sha256: text("Container proxy SHA-256 (base only)"),
        };
        request.validate().map_err(|error| error.to_string())?;
        Ok(AdminOperation::Build(request))
    } else {
        let source_path = PathBuf::from(text(SOURCE));
        validate_path(&source_path).map_err(|error| error.to_string())?;
        Ok(AdminOperation::Import(ImportRequest {
            source_path,
            expected_sha256: revision(SHA256)?,
            expected_revision: text(EXPECTED),
            manifest,
        }))
    }
}

#[derive(Clone, PartialEq, Eq)]
enum PickerControl {
    Entry(usize),
    Parent,
    Select,
    Cancel,
}

pub(super) struct HostPicker {
    directory: PathBuf,
    entries: Vec<(PathBuf, bool)>,
    selected: usize,
    top: usize,
    list_area: Rect,
    scrollbar: Scrollbar,
    path: TextEditor,
    path_area: Rect,
    path_dragging: bool,
    pub copy: Option<String>,
    hits: Vec<(Rect, PickerControl)>,
    hovered: Option<PickerControl>,
    pressed: Option<PickerControl>,
}

impl HostPicker {
    pub fn open(path: &str) -> Result<Self, String> {
        let path = PathBuf::from(if path.is_empty() { "/" } else { path });
        if path != Path::new("/") {
            validate_path(&path).map_err(|error| error.to_string())?;
        }
        let directory = if path.is_dir() {
            path
        } else {
            path.parent()
                .ok_or("Choose an absolute host directory")?
                .to_owned()
        };
        let mut picker = Self {
            directory,
            entries: Vec::new(),
            selected: 0,
            top: 0,
            list_area: Rect::ZERO,
            scrollbar: Scrollbar::default(),
            path: TextEditor::new(),
            path_area: Rect::ZERO,
            path_dragging: false,
            copy: None,
            hits: Vec::new(),
            hovered: None,
            pressed: None,
        };
        picker.reload()?;
        Ok(picker)
    }

    fn reload(&mut self) -> Result<(), String> {
        if fs::canonicalize(&self.directory).map_err(|_| "Cannot resolve host directory")?
            != self.directory
        {
            return Err("Use the canonical host directory; symlink paths are not accepted".into());
        }
        let mut entries = Vec::new();
        for (index, entry) in fs::read_dir(&self.directory)
            .map_err(|_| "Cannot read host directory")?
            .take(MAX_PICKER_ENTRIES + 1)
            .enumerate()
        {
            let entry = entry.map_err(|_| "Cannot read host directory entry")?;
            if index == MAX_PICKER_ENTRIES {
                return Err(
                    "Directory too large; enter a narrower absolute directory in the source field"
                        .into(),
                );
            }
            let kind = entry.file_type().map_err(|_| "Cannot inspect host entry")?;
            if !kind.is_symlink()
                && (kind.is_dir() || kind.is_file())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| !name.chars().any(char::is_control))
            {
                entries.push((entry.path(), kind.is_dir()));
            }
        }
        entries.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        self.entries = entries;
        self.selected = 0;
        self.top = 0;
        Ok(())
    }

    pub fn key(&mut self, key: KeyCode) -> Result<Option<PathBuf>, String> {
        self.reset_mouse();
        match key {
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.entries.len().saturating_sub(1),
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.entries.len().saturating_sub(1))
            }
            KeyCode::Backspace => {
                if let Some(parent) = self.directory.parent() {
                    self.directory = parent.to_owned();
                    self.reload()?;
                }
            }
            KeyCode::Enter => {
                if let Some((path, directory)) = self.entries.get(self.selected) {
                    if *directory {
                        self.directory = path.clone();
                        self.reload()?;
                    } else {
                        validate_path(path).map_err(|error| error.to_string())?;
                        if path.extension().and_then(|extension| extension.to_str())
                            != Some("qcow2")
                        {
                            return Err("Select a .qcow2 file; format/backing validation requires the separately approved F4 probe".into());
                        }
                        return Ok(Some(path.clone()));
                    }
                }
            }
            _ => {}
        }
        self.reveal_selected();
        Ok(None)
    }

    fn reveal_selected(&mut self) {
        let height = usize::from(self.list_area.height.max(1));
        self.top = self
            .top
            .min(self.selected)
            .max(self.selected.saturating_sub(height - 1));
    }

    pub fn reset_mouse(&mut self) {
        let release = MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        if self.path_dragging {
            self.path.cancel_selection();
        }
        self.scrollbar.handle(&release);
        self.path_dragging = false;
        self.copy = None;
        self.clear_controls();
    }

    fn clear_controls(&mut self) {
        self.hits.clear();
        self.hovered = None;
        self.pressed = None;
    }

    pub fn scroll(&mut self, delta: i32) {
        self.reset_mouse();
        self.top = self.top.saturating_add_signed(-(delta as isize)).min(
            self.entries
                .len()
                .saturating_sub(usize::from(self.list_area.height)),
        );
    }

    pub fn scroll_at(&mut self, at: Position, delta: i32) {
        if self.path_area.contains(at) {
            self.path.scroll(delta);
        } else if self.list_area.contains(at) {
            self.scroll(delta);
        }
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Option<KeyCode> {
        let at = Position::new(event.column, event.row);
        if !self.path_dragging {
            match self.scrollbar.handle(&event) {
                ScrollbarMouse::Ignored => {}
                ScrollbarMouse::Consumed => {
                    self.clear_controls();
                    return None;
                }
                ScrollbarMouse::ScrollTo(top) => {
                    self.top = top as usize;
                    self.clear_controls();
                    return None;
                }
            }
        }
        if self.path_area.contains(at) || self.path_dragging {
            if event.kind == MouseEventKind::Down(MouseButton::Left) {
                self.path_dragging = true;
            }
            if let EditorMouse::Copy(text) = self.path.handle_mouse(&event) {
                self.copy = Some(text);
            }
            if event.kind == MouseEventKind::Up(MouseButton::Left) {
                self.path_dragging = false;
            }
            self.clear_controls();
            return None;
        }
        let hit = self
            .hits
            .iter()
            .find(|(area, _)| area.contains(at))
            .map(|(_, control)| control.clone());
        self.hovered = hit.clone();
        if hit.is_none() {
            self.pressed = None;
        }
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.pressed = hit,
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.pressed.take();
                if hit != pressed {
                    return None;
                }
                let key = match hit? {
                    PickerControl::Entry(index) => {
                        self.selected = index;
                        KeyCode::Enter
                    }
                    PickerControl::Parent => KeyCode::Backspace,
                    PickerControl::Select => KeyCode::Enter,
                    PickerControl::Cancel => KeyCode::Esc,
                };
                self.reset_mouse();
                return Some(key);
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.list_area.contains(at) =>
            {
                self.scroll_at(
                    at,
                    if event.kind == MouseEventKind::ScrollUp {
                        1
                    } else {
                        -1
                    },
                );
            }
            MouseEventKind::Drag(_) => self.pressed = None,
            MouseEventKind::Moved if hit != self.pressed => self.pressed = None,
            MouseEventKind::Down(_) => self.pressed = None,
            _ => {}
        }
        None
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("HOST qcow2 | Enter open/select | Home/End | Esc cancel");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let [directory, list, path, buttons] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(2),
            Constraint::Length(1),
        ])
        .areas(inner);
        if self.list_area != list || self.path_area != path {
            self.reset_mouse();
        }
        frame.render_widget(Paragraph::new(self.directory.to_string_lossy()), directory);
        self.list_area = list;
        self.top = self
            .top
            .min(self.entries.len().saturating_sub(usize::from(list.height)));
        self.path_area = path;
        let selected_path = self
            .entries
            .get(self.selected)
            .map_or(&self.directory, |(path, _)| path);
        let text = selected_path.to_string_lossy();
        if self.path.text() != text {
            let text = text.into_owned();
            self.reset_mouse();
            self.path.set_text(text);
        }
        self.path.view(frame, path);
        let start = self.top;
        let mut hits = Vec::new();
        for (row, (index, _)) in self
            .entries
            .iter()
            .enumerate()
            .skip(start)
            .take(list.height as usize)
            .enumerate()
        {
            hits.push((
                Rect {
                    y: list.y + row as u16,
                    height: 1,
                    ..list
                },
                PickerControl::Entry(index),
            ));
        }
        let areas = Layout::horizontal([Constraint::Fill(1); 3]).split(buttons);
        for (area, control, enabled) in [
            (
                areas[0],
                PickerControl::Parent,
                self.directory.parent().is_some(),
            ),
            (areas[1], PickerControl::Select, !self.entries.is_empty()),
            (areas[2], PickerControl::Cancel, true),
        ] {
            if enabled && !area.is_empty() {
                hits.push((area, control));
            }
        }
        if hits != self.hits {
            self.clear_controls();
        }
        self.hits = hits;
        for (area, control) in &self.hits {
            let label = match control {
                PickerControl::Entry(index) => {
                    let (path, directory) = &self.entries[*index];
                    format!(
                        "{} {}{}",
                        if *index == self.selected { ">" } else { " " },
                        path.file_name().unwrap_or_default().to_string_lossy(),
                        if *directory { "/" } else { "" }
                    )
                }
                PickerControl::Parent => "[Backspace Parent]".into(),
                PickerControl::Select => "[Enter Open/select]".into(),
                PickerControl::Cancel => "[Esc Cancel]".into(),
            };
            frame.render_widget(
                Paragraph::new(label).style(hover_style(
                    theme::current().tool_dim,
                    self.hovered.as_ref() == Some(control),
                )),
                *area,
            );
        }
        self.scrollbar
            .draw(frame, list, self.entries.len() as u32, self.top as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::{HostPicker, Kind, PickerControl};
    use crate::components::scrollbar;
    use caudra_workbench::scroll::{SCROLLBAR_THUMB, SCROLLBAR_THUMB_GRABBED};
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend, style::Modifier};
    use std::{env, fs, process::Command};
    use test_case::test_case;

    const SOURCE: &str = "source with spaces.qcow2";
    const DISABLED_TEST_PROCESS: &str = "CAUDRA_PICKER_DISABLED_TEST_PROCESS";

    #[test]
    fn wrapped_unicode_path_drag_copies_offscreen_content() {
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        let path = temp.path().join(format!("{}.qcow2", "文件é".repeat(60)));
        picker.entries = vec![(path.clone(), false)];
        let mut terminal = Terminal::new(TestBackend::new(20, 12)).unwrap();
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        assert_eq!(
            terminal.backend().buffer()[(picker.path_area.right() - 1, picker.path_area.y)]
                .symbol(),
            SCROLLBAR_THUMB
        );
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.path_area.x,
            row: picker.path_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(picker.mouse(at), None);
        let drag = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: picker.path_area.right() - 2,
            row: picker.path_area.bottom(),
            ..at
        };
        for _ in 0..20 {
            assert_eq!(picker.mouse(drag), None);
            terminal
                .draw(|frame| picker.view(frame, frame.area()))
                .unwrap();
        }
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..drag
            }),
            None
        );
        assert_eq!(picker.copy.as_deref(), path.to_str());
        assert_eq!(picker.path.text(), path.to_string_lossy());
    }

    #[test_case(0; "resize")]
    #[test_case(1; "keyboard")]
    #[test_case(2; "close_reset")]
    #[test_case(3; "path_replaced")]
    fn interrupted_path_drag_cannot_copy_on_late_release(interrupt: u8) {
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        render(&mut picker, 80);
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.path_area.x,
            row: picker.path_area.y,
            modifiers: KeyModifiers::NONE,
        };
        picker.mouse(at);
        picker.mouse(MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: at.column + 4,
            ..at
        });
        match interrupt {
            0 => render(&mut picker, 40),
            1 => {
                picker.key(KeyCode::Home).unwrap();
            }
            2 => picker.reset_mouse(),
            _ => {
                picker.entries = vec![(temp.path().join(SOURCE), false)];
                render(&mut picker, 80);
            }
        }
        assert!(!picker.path_dragging);
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            None
        );
        assert!(picker.copy.is_none());
    }

    #[test]
    fn path_scrollbar_click_does_not_copy_or_open() {
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        picker.entries = vec![(
            temp.path().join(format!("{}.qcow2", "文件".repeat(60))),
            false,
        )];
        render(&mut picker, 20);
        let source = picker.path.text();
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.path_area.right() - 1,
            row: picker.path_area.bottom() - 1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(picker.mouse(at), None);
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            None
        );
        assert!(picker.copy.is_none());
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.path.text(), source);
    }

    #[test_case(0, 0; "zero")]
    #[test_case(1, 1; "single_cell")]
    #[test_case(3, 8; "single_inner_column")]
    #[test_case(8, 3; "no_path_height")]
    fn picker_tiny_viewports_preserve_source(width: u16, height: u16) {
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        picker.scroll(i32::MIN);
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        assert_eq!(picker.path.text(), temp.path().to_string_lossy());
        assert_eq!(picker.top, 0);
        assert!(picker.copy.is_none());
    }

    #[test]
    fn disabled_scrollbar_has_no_thumb_or_pointer_capture() {
        if env::var_os(DISABLED_TEST_PROCESS).is_none() {
            let status = Command::new(env::current_exe().unwrap())
                .args(["--exact", "components::sandbox_manager::image::tests::disabled_scrollbar_has_no_thumb_or_pointer_capture"])
                .env(DISABLED_TEST_PROCESS, "1")
                .status().unwrap();
            assert!(status.success());
            return;
        }
        scrollbar::set_enabled(false);
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        picker.entries = (0..40)
            .map(|index| (temp.path().join(format!("{index}.qcow2")), false))
            .collect();
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.list_area.right() - 1,
            row: picker.list_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_ne!(
            terminal.backend().buffer()[(at.column, at.row)].symbol(),
            SCROLLBAR_THUMB
        );
        assert_eq!(picker.mouse(at), None);
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            Some(KeyCode::Enter)
        );
    }

    #[test]
    fn picker_scrollbar_drag_wheel_and_keys_preserve_selection() {
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        picker.entries = (0..40)
            .map(|index| (temp.path().join(format!("{index}.qcow2")), false))
            .collect();
        render(&mut picker, 80);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        let bar = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.list_area.right() - 1,
            row: picker.list_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(
            terminal.backend().buffer()[(bar.column, bar.row)].symbol(),
            SCROLLBAR_THUMB
        );
        assert_eq!(picker.mouse(bar), None);
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        assert_eq!(
            terminal.backend().buffer()[(bar.column, bar.row)].symbol(),
            SCROLLBAR_THUMB_GRABBED
        );
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                row: picker.list_area.bottom() + 5,
                ..bar
            }),
            None
        );
        assert_eq!(picker.selected, 0);
        assert_eq!(
            picker.top,
            picker.entries.len() - usize::from(picker.list_area.height)
        );
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..bar
            }),
            None
        );
        picker.scroll(i32::MAX);
        assert_eq!(picker.top, 0);
        picker.scroll(i32::MIN);
        assert_eq!(picker.selected, 0);
        picker.key(KeyCode::Home).unwrap();
        assert_eq!(picker.top, 0);
        picker.key(KeyCode::End).unwrap();
        render(&mut picker, 80);
        assert_eq!(picker.selected, 39);
        assert!(
            picker
                .hits
                .iter()
                .any(|(_, control)| *control == PickerControl::Entry(39))
        );
    }

    #[test]
    fn picker_path_drag_copies_without_opening() {
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        render(&mut picker, 80);
        let original = picker.path.text();
        let at = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: picker.path_area.x,
            row: picker.path_area.y,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(picker.mouse(at), None);
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: at.column + 4,
                ..at
            }),
            None
        );
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column: at.column + 4,
                ..at
            }),
            None
        );
        assert_eq!(picker.copy.as_deref(), Some(&original[..4]));
        assert_eq!(picker.path.text(), original);
        assert_eq!(picker.directory, temp.path());
    }

    #[test_case(PickerControl::Entry(0); "selected_entry")]
    #[test_case(PickerControl::Parent; "parent")]
    #[test_case(PickerControl::Select; "select")]
    #[test_case(PickerControl::Cancel; "cancel")]
    fn hovered_picker_control_has_distinct_rendered_style(control: PickerControl) {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(SOURCE), []).unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        let moved = event(&picker, control, MouseEventKind::Moved);
        let position = (moved.column, moved.row);
        let before = terminal.backend().buffer()[position].clone();
        assert_eq!(picker.mouse(moved), None);
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
        let after = &terminal.backend().buffer()[position];
        assert_eq!(before.symbol(), after.symbol());
        assert_ne!(before.style(), after.style());
        assert!(after.modifier.contains(Modifier::UNDERLINED));
        assert_eq!(picker.selected, 0);
    }

    fn render(picker: &mut HostPicker, width: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, 12)).unwrap();
        terminal
            .draw(|frame| picker.view(frame, frame.area()))
            .unwrap();
    }

    fn event(picker: &HostPicker, control: PickerControl, kind: MouseEventKind) -> MouseEvent {
        let (area, _) = picker.hits.iter().find(|(_, hit)| *hit == control).unwrap();
        MouseEvent {
            kind,
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test_case(KeyCode::Home, 0; "first")]
    #[test_case(KeyCode::End, 2; "last")]
    fn picker_boundary_keys(code: KeyCode, expected: usize) {
        let temp = tempfile::tempdir().unwrap();
        for name in ["a.qcow2", "b.qcow2", "c.qcow2"] {
            fs::write(temp.path().join(name), []).unwrap();
        }
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        picker.selected = 1;
        picker.key(code).unwrap();
        assert_eq!(picker.selected, expected);
    }

    #[test_case(KeyCode::Home; "home")]
    #[test_case(KeyCode::End; "end")]
    fn empty_picker_boundaries_leave_select_disabled(code: KeyCode) {
        let temp = tempfile::tempdir().unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        picker.key(code).unwrap();
        render(&mut picker, 80);
        assert_eq!(picker.selected, 0);
        assert!(
            !picker
                .hits
                .iter()
                .any(|(_, control)| *control == PickerControl::Select)
        );
        assert_eq!(picker.key(KeyCode::Enter).unwrap(), None);
    }

    #[test_case(PickerControl::Entry(0), KeyCode::Enter; "entry")]
    #[test_case(PickerControl::Select, KeyCode::Enter; "select")]
    #[test_case(PickerControl::Parent, KeyCode::Backspace; "parent")]
    #[test_case(PickerControl::Cancel, KeyCode::Esc; "cancel")]
    fn picker_mouse_activates_only_matching_release(control: PickerControl, expected: KeyCode) {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(SOURCE), []).unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        render(&mut picker, 80);
        let at = event(&picker, control.clone(), MouseEventKind::Moved);
        assert_eq!(picker.mouse(at), None);
        assert!(picker.hovered == Some(control));
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            None
        );
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                ..at
            }),
            None
        );
        render(&mut picker, 80);
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            Some(expected)
        );
        assert!(picker.hovered.is_none());
        assert!(picker.pressed.is_none());
    }

    #[test_case(0; "keyboard")]
    #[test_case(1; "resize")]
    #[test_case(2; "pointer_leaves")]
    fn picker_invalidates_stale_press(change: u8) {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(SOURCE), []).unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        render(&mut picker, 80);
        let at = event(
            &picker,
            PickerControl::Entry(0),
            MouseEventKind::Down(MouseButton::Left),
        );
        picker.mouse(at);
        match change {
            0 => {
                picker.key(KeyCode::Home).unwrap();
            }
            1 => render(&mut picker, 60),
            _ => {
                picker.mouse(MouseEvent {
                    kind: MouseEventKind::Moved,
                    column: 0,
                    row: 0,
                    ..at
                });
            }
        }
        assert!(picker.hovered.is_none());
        assert!(picker.pressed.is_none());
        assert_eq!(
            picker.mouse(MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                ..at
            }),
            None
        );
    }

    #[test_case(false; "select_file")]
    #[test_case(true; "cancel")]
    fn picker_manager_mouse_forwarding_never_submits(cancel: bool) {
        let (temp, _store, mut manager) = fixture();
        let source = temp.path().join(SOURCE);
        fs::write(&source, []).unwrap();
        let state = manager.state.as_mut().unwrap();
        state.open_live(Kind::ImportImage);
        let form = state.live_form.as_mut().unwrap();
        form.picker = Some(HostPicker::open(temp.path().to_str().unwrap()).unwrap());
        let picker = form.picker.as_mut().unwrap();
        let index = picker
            .entries
            .iter()
            .position(|(path, _)| path == &source)
            .unwrap();
        render(picker, 80);
        let control = if cancel {
            PickerControl::Cancel
        } else {
            PickerControl::Entry(index)
        };
        let at = event(picker, control, MouseEventKind::Down(MouseButton::Left));
        state.mouse(at);
        state.mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            ..at
        });
        let form = state.live_form.as_ref().unwrap();
        assert!(form.picker.is_none());
        assert_eq!(
            form.field(super::SOURCE),
            if cancel {
                String::new()
            } else {
                source.to_string_lossy().into_owned()
            }
        );
        assert!(state.confirmation.is_none());
        assert!(state.live_pending.is_none());
    }

    #[test]
    fn host_picker_navigates_and_selects_exact_local_path() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("images");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join(SOURCE), []).unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        assert_eq!(picker.key(KeyCode::Enter).unwrap(), None);
        assert_eq!(
            picker.key(KeyCode::Enter).unwrap(),
            Some(directory.join(SOURCE))
        );
        picker.key(KeyCode::Backspace).unwrap();
        assert_eq!(picker.directory, temp.path());
    }

    #[test_case("image.raw"; "raw_extension")]
    #[test_case("image.qcow"; "unsupported_extension")]
    fn picker_does_not_select_unsupported_files(file: &str) {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join(file), []).unwrap();
        let mut picker = HostPicker::open(temp.path().to_str().unwrap()).unwrap();
        assert!(picker.key(KeyCode::Enter).is_err());
        assert!(HostPicker::open("../images").is_err());
    }
}
