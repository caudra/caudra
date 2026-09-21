use super::{
    TextEditor,
    live::{Kind, LiveField, LiveForm},
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
use crossterm::event::KeyCode;
use ratatui::{
    Frame,
    layout::Rect,
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

pub(super) struct HostPicker {
    directory: PathBuf,
    entries: Vec<(PathBuf, bool)>,
    selected: usize,
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
        Ok(())
    }

    pub fn key(&mut self, key: KeyCode) -> Result<Option<PathBuf>, String> {
        match key {
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
        Ok(None)
    }

    pub fn view(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("HOST qcow2 picker | Enter select/open | Backspace parent | Esc cancel");
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let start = self
            .selected
            .saturating_sub(inner.height.saturating_sub(3) as usize);
        let rows = self
            .entries
            .iter()
            .enumerate()
            .skip(start)
            .take(inner.height.saturating_sub(1) as usize)
            .map(|(index, (path, directory))| {
                format!(
                    "{} {}{}",
                    if index == self.selected { ">" } else { " " },
                    path.file_name().unwrap_or_default().to_string_lossy(),
                    if *directory { "/" } else { "" }
                )
            });
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n{}",
                self.directory.display(),
                rows.collect::<Vec<_>>().join("\n")
            )),
            inner,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::HostPicker;
    use crossterm::event::KeyCode;
    use std::fs;
    use test_case::test_case;

    const SOURCE: &str = "source with spaces.qcow2";

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
