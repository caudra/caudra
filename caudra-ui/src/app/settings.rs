//! `/settings` from the app's side: the global `caudra.toml` it edits, the local Workcell whose
//! web search it reports on, and the reads and writes it hands to a background thread.

use std::mem;
use std::path::{Path, PathBuf};

use caudra_agent::pstack::roles::{self, Kind};
use caudra_config::config_file::{load_global_config, project_config_path};
use caudra_config::settings_file::{GlobalSettingsFile, ProjectSettings};
use caudra_config::websearch::{BACKENDS_KEY, GlobalWebsearchFile, MODE_KEY, WebsearchConfig};
use caudra_workcell::WebsearchControl;
use flume::{Receiver, TryRecvError};

use super::App;
use crate::components::settings_modal::{
    PstackAction, PstackRoles, RoleEdit, SettingsAction, SettingsData, SettingsEdit, SettingsFiles,
    SettingsModal, SettingsReply, SettingsRequest, apply_role_edit, environment_backends,
    environment_mode,
};
use crate::components::{Action, Overlay};
use crate::repaint::Dirty;

const NO_CONFIG_DIRECTORY: &str =
    "Caudra cannot resolve its configuration directory, so there is no global caudra.toml to edit.";
const WORKER_STOPPED: &str = "The background read stopped before it answered; reopen /settings.";
const LIST_SEPARATOR: &str = "; ";
const ROLES_NOT_SAVED: &str = "pstack's roles were not saved: ";

/// What `/settings` edits for this session, and the work it has in flight.
pub(crate) struct SettingsLink {
    /// The local Workcell's web search, `None` when web tools run on a remote one.
    pub(crate) websearch: Option<WebsearchControl>,
    pub(crate) config_path: Option<PathBuf>,
    /// Set by the event loop: `agent.task_model_choice`, which pstack's roles resolve against.
    pub(crate) task_model_choice: bool,
    /// The modal set aside while the model picker chooses a pstack seat, back when it closes.
    pub(crate) parked: Option<Box<SettingsModal>>,
    reply: Option<Receiver<SettingsReply>>,
}

impl Default for SettingsLink {
    fn default() -> Self {
        Self {
            websearch: None,
            config_path: caudra_config::global_config_path(),
            task_model_choice: true,
            parked: None,
            reply: None,
        }
    }
}

impl App {
    pub(super) fn open_settings(&mut self) -> Vec<Action> {
        let action = self.settings_modal.open(self.settings.websearch.is_some());
        self.settings_modal.set_pstack(self.pstack_roles());
        self.handle_settings_action(action)
    }

    pub(super) fn handle_settings_action(&mut self, action: SettingsAction) -> Vec<Action> {
        match action {
            SettingsAction::Consumed => {}
            SettingsAction::Close => {
                self.settings_modal.close();
                self.settings.reply = None;
            }
            SettingsAction::Request(request) => self.start_settings_request(request),
            SettingsAction::OpenDocs(topic) => self.open_docs(topic),
            SettingsAction::Pstack(PstackAction::Edit(edit)) => self.edit_pstack_roles(edit),
            SettingsAction::Pstack(PstackAction::Pick { kind, seat, model }) => {
                return self.pick_pstack_model(kind, seat, model);
            }
        }
        Vec::new()
    }

    /// pstack's roles as stored now, `None` while pstack is off.
    pub(super) fn pstack_roles(&self) -> Option<PstackRoles> {
        self.skill_packs.pstack.then(|| {
            PstackRoles::new(
                roles::table(&self.model_policy, self.settings.task_model_choice),
                roles::load(),
                roles::effort_levels,
            )
        })
    }

    /// Reads the stored roles fresh, applies `edit` against the table they resolve to, writes
    /// them back, and shows what pstack serves now.
    fn edit_pstack_roles(&mut self, edit: RoleEdit) {
        let mut stored = roles::load();
        let table = roles::table(&self.model_policy, self.settings.task_model_choice);
        apply_role_edit(&mut stored, &table, edit, roles::effort_levels);
        let error = roles::store(&stored)
            .err()
            .map(|error| format!("{ROLES_NOT_SAVED}{error}"));
        let roles = self.pstack_roles();
        self.settings_modal.pstack_saved(roles, error);
    }

    /// Sets the modal aside and opens the model picker on the seat; [`Self::unpark_settings`]
    /// brings the modal back as it was.
    fn pick_pstack_model(
        &mut self,
        kind: Kind,
        seat: Option<usize>,
        model: Option<String>,
    ) -> Vec<Action> {
        self.settings.parked = Some(Box::new(mem::take(&mut self.settings_modal)));
        self.model_picker.open_pstack(
            &self.state.model,
            &self.model_policy,
            self.active_decision_endpoint(),
            kind,
            seat,
            model,
        );
        vec![Action::RefreshModels]
    }

    /// The modal set aside for the model picker, back once the picker has closed.
    pub(super) fn unpark_settings(&mut self) {
        if self.model_picker.is_open() {
            return;
        }
        if let Some(modal) = self.settings.parked.take() {
            self.settings_modal = *modal;
        }
    }

    /// What the model picker chose for a pstack seat, saved once the modal is back.
    pub(super) fn save_pstack_pick(&mut self, edit: RoleEdit) {
        self.unpark_settings();
        self.edit_pstack_roles(edit);
    }

    fn start_settings_request(&mut self, request: SettingsRequest) {
        let path = self.settings.config_path.clone();
        let project = self
            .workspace_session
            .is_none()
            .then(|| project_config_path(Path::new(&self.state.session.cwd)));
        let control = self.settings.websearch.clone();
        let (sender, receiver) = flume::bounded(1);
        smol::spawn(async move {
            let reply =
                smol::unblock(move || run(request, path, project.as_deref(), control.as_ref()))
                    .await;
            let _ = sender.send(reply);
        })
        .detach();
        self.settings.reply = Some(receiver);
    }

    pub(super) fn poll_settings(&mut self) -> Dirty {
        let Some(receiver) = &self.settings.reply else {
            return Dirty::NO;
        };
        let reply = match receiver.try_recv() {
            Ok(reply) => reply,
            Err(TryRecvError::Empty) => return Dirty::NO,
            Err(TryRecvError::Disconnected) => stopped(),
        };
        self.settings.reply = None;
        match self.settings.parked.as_mut() {
            Some(parked) => parked.receive(reply),
            None => self.settings_modal.receive(reply),
        }
        Dirty::YES
    }
}

fn stopped() -> SettingsReply {
    SettingsReply {
        data: Err(WORKER_STOPPED.to_owned()),
        error: None,
        note: None,
    }
}

/// A write is read back from the file whatever happened, so a refusal shows what is there now.
fn run(
    request: SettingsRequest,
    path: Option<PathBuf>,
    project: Option<&Path>,
    control: Option<&WebsearchControl>,
) -> SettingsReply {
    let Some(path) = path else {
        return SettingsReply {
            data: Err(NO_CONFIG_DIRECTORY.to_owned()),
            error: None,
            note: None,
        };
    };
    let (error, websearch) = match request {
        SettingsRequest::Load => (None, false),
        SettingsRequest::Edit { files, edit } => {
            let websearch = matches!(*edit, SettingsEdit::Websearch(_));
            (apply(&files, *edit).err(), websearch)
        }
    };
    let data = load(&path, project, control);
    let note = match (&data, control) {
        (Ok(_), Some(control)) if websearch && error.is_none() => {
            expanded_websearch(&path).ok().map(|config| {
                let report = control.apply(&config);
                let mut parts = report.warnings();
                parts.push(report.summary());
                parts.join(LIST_SEPARATOR)
            })
        }
        _ => None,
    };
    SettingsReply {
        data: data.map(Box::new),
        error,
        note,
    }
}

fn apply(files: &SettingsFiles, edit: SettingsEdit) -> Result<(), String> {
    match edit {
        SettingsEdit::Websearch(edits) => edits
            .into_iter()
            .try_fold(files.websearch.clone(), |file, edit| file.apply(edit))
            .map(drop),
        SettingsEdit::Key { table, key, value } => match value {
            Some(value) => files.settings.set(table, key, value).map(drop),
            None => files.settings.remove(table, key).map(drop),
        },
    }
    .map_err(|error| error.to_string())
}

/// A project file that does not load marks nothing, so the global settings stay editable.
fn load(
    path: &Path,
    project: Option<&Path>,
    control: Option<&WebsearchControl>,
) -> Result<SettingsData, String> {
    let settings = GlobalSettingsFile::load(path).map_err(|error| error.to_string())?;
    let websearch = GlobalWebsearchFile::load(path).map_err(|error| error.to_string())?;
    let project = project
        .map(ProjectSettings::load)
        .transpose()
        .inspect_err(|error| tracing::warn!(%error, "project config unreadable for /settings"))
        .unwrap_or_default()
        .unwrap_or_default();
    let statuses = match control {
        Some(control) => control.status(&expanded_websearch(path)?),
        None => Vec::new(),
    };
    Ok(SettingsData {
        files: SettingsFiles {
            settings,
            websearch,
        },
        project,
        statuses,
        default_backends: environment_backends(environment(BACKENDS_KEY).as_deref()),
        default_mode: environment_mode(environment(MODE_KEY).as_deref()),
    })
}

/// `[websearch]` with its `{env:NAME}` and `{file:PATH}` references read, as the tool sees it.
fn expanded_websearch(path: &Path) -> Result<WebsearchConfig, String> {
    load_global_config(path)
        .map_err(|error| error.to_string())?
        .settings
        .into_config(false)
        .map(|config| config.websearch)
        .map_err(|error| error.to_string())
}

/// The variable `[websearch]`'s `key` gives way to, as Workcell reads it.
fn environment(key: &str) -> Option<String> {
    WebsearchConfig::FIELDS
        .iter()
        .find(|field| field.name == key)
        .and_then(|field| field.env)
        .and_then(|name| caudra_config::current_global_env_value(name).ok().flatten())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_config::config_file::CONFIG_VERSION;
    use caudra_config::websearch::{WebsearchBackendName, WebsearchEdit};
    use tempfile::TempDir;
    use toml_edit::Value;

    use super::*;
    use crate::app::tests::{cmd, test_app};

    const CONFIG_NAME: &str = "caudra.toml";
    const COMMENT: &str = "# kept by every edit";
    const UI_TABLE: &[&str] = &["ui"];
    const UPDATE_CHECK: &str = "update_check";
    const STORAGE_TABLE: &[&str] = &["storage"];
    const MAX_EAGER_LOAD: &str = "max_eager_load_mb";
    const NOT_A_SIZE: &str = "large";
    const SEARXNG_LIST: &str = "backends = [\"searxng\"]";

    fn config() -> (TempDir, PathBuf) {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join(CONFIG_NAME);
        fs::write(&path, format!("{COMMENT}\nversion = {CONFIG_VERSION}\n")).expect("write");
        (dir, path)
    }

    fn edited(path: &Path, edit: SettingsEdit) -> SettingsReply {
        let files = load(path, None, None).expect("load").files;
        let request = SettingsRequest::Edit {
            files: Box::new(files),
            edit: Box::new(edit),
        };
        run(request, Some(path.to_path_buf()), None, None)
    }

    #[test]
    fn a_key_edit_keeps_the_comments() {
        let (_dir, path) = config();
        let reply = edited(
            &path,
            SettingsEdit::Key {
                table: UI_TABLE,
                key: UPDATE_CHECK,
                value: Some(Value::from(false)),
            },
        );
        assert!(reply.error.is_none());
        let data = reply.data.expect("reloaded");
        let written = data.files.settings.get(UI_TABLE, UPDATE_CHECK);
        assert_eq!(written.and_then(|item| item.as_bool()), Some(false));
        assert!(fs::read_to_string(&path).expect("read").contains(COMMENT));
    }

    #[test]
    fn a_value_of_the_wrong_type_is_refused() {
        let (_dir, path) = config();
        let before = fs::read_to_string(&path).expect("read");
        let reply = edited(
            &path,
            SettingsEdit::Key {
                table: STORAGE_TABLE,
                key: MAX_EAGER_LOAD,
                value: Some(Value::from(NOT_A_SIZE)),
            },
        );
        assert!(reply.error.is_some());
        assert!(reply.data.is_ok());
        assert_eq!(fs::read_to_string(&path).expect("read"), before);
    }

    #[test]
    fn the_command_opens_the_modal_and_reads_off_the_ui_thread() {
        let mut app = test_app();
        app.settings.config_path = None;
        app.execute_command(cmd("/settings"), 0);
        assert!(app.settings_modal.is_open());
        let reply = app
            .settings
            .reply
            .take()
            .expect("a read in flight")
            .recv()
            .expect("a reply");
        assert_eq!(reply.data.err().as_deref(), Some(NO_CONFIG_DIRECTORY));
    }

    #[test]
    fn a_backend_edit_writes_the_list() {
        let (_dir, path) = config();
        let reply = edited(
            &path,
            SettingsEdit::Websearch(vec![WebsearchEdit::Backends(Some(vec![
                WebsearchBackendName::Searxng,
            ]))]),
        );
        assert!(reply.error.is_none());
        let text = fs::read_to_string(&path).expect("read");
        assert!(text.contains(SEARXNG_LIST));
        assert!(text.contains(COMMENT));
    }
}
