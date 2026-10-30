//! `/env` from the app's side: which shell target the modal describes, and the reads and writes it
//! hands to a background thread so the interface never waits on a file or a guest.

use std::mem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use caudra_agent::secrets::scope::{delete, forget_to_session, remember, replace};
use caudra_agent::secrets::{SecretError, SecretScope, SecretVaults, SessionSecrets};
use caudra_config::sandbox::SandboxName;
use caudra_config::sandbox::persistence::{LoadedSandboxes, SandboxStore};
use caudra_config::shell_env::{GlobalShellEnvFile, ShellEnvEdit};
use caudra_workcell::{GuestShellEnvironment, ShellEnvironmentTarget};
use flume::{Receiver, TryRecvError};
use workcell_tool_contract::shell_env::ShellEnvironmentSelection;

use super::App;
use crate::components::env_modal::{
    EnvAction, EnvReply, EnvRequest, EnvTab, EnvTarget, GuestListing, LocalEnvironment,
    SandboxTarget, SecretEdit,
};
use crate::components::{Action, Overlay};
use crate::repaint::Dirty;

const NO_CONFIG_DIRECTORY: &str =
    "Caudra cannot resolve its configuration directory, so there is no global caudra.toml to edit.";
const WORKER_STOPPED: &str = "The background read stopped before it answered; reopen /env.";
const PROFILES_UNREADABLE: &str = "Cannot read the saved sandbox profiles";

/// What `/env` edits for this session, and the work it has in flight.
pub(crate) struct ShellEnvLink {
    pub(crate) target: ShellEnvironmentTarget,
    /// The global `caudra.toml` the Local tab reads and writes.
    pub(crate) config_path: Option<PathBuf>,
    pub(super) reply: Option<Receiver<Completion>>,
    /// The saved sandboxes a profile write commits against.
    baseline: Option<Arc<LoadedSandboxes>>,
    /// A remembered secret's file changed, which no config watcher sees, so the settings that
    /// resolve its `{file:…}` reference are read again.
    settings_stale: bool,
}

impl Default for ShellEnvLink {
    fn default() -> Self {
        Self {
            target: ShellEnvironmentTarget::Local,
            config_path: caudra_config::global_config_path(),
            reply: None,
            baseline: None,
            settings_stale: false,
        }
    }
}

pub(super) struct Completion {
    reply: EnvReply,
    baseline: Option<Arc<LoadedSandboxes>>,
    settings_stale: bool,
}

enum Effect {
    LoadLocal {
        path: Option<PathBuf>,
        vault: SessionSecrets,
    },
    EditLocal {
        file: Box<GlobalShellEnvFile>,
        edit: ShellEnvEdit,
        vault: SessionSecrets,
    },
    EditSecret {
        path: Option<PathBuf>,
        vault: SessionSecrets,
        edit: SecretEdit,
    },
    LoadSandbox(GuestShellEnvironment),
    EditSandbox {
        guest: GuestShellEnvironment,
        selection: ShellEnvironmentSelection,
        baseline: Option<Arc<LoadedSandboxes>>,
    },
}

impl App {
    pub(super) fn open_env(&mut self) -> Vec<Action> {
        let action = self.env_modal.open(self.env_target());
        self.handle_env_action(action)
    }

    pub(super) fn open_secrets(&mut self) -> Vec<Action> {
        let action = self.env_modal.open_secrets(self.env_target());
        self.handle_env_action(action)
    }

    /// Whether a remembered secret changed since the settings were last read, which clears it.
    pub(crate) fn take_settings_stale(&mut self) -> bool {
        mem::take(&mut self.shell_env.settings_stale)
    }

    /// The vault the agent of the current session uses, keyed by its root session, so it
    /// follows every switch, resume and new session.
    fn session_secrets(&self) -> SessionSecrets {
        SecretVaults::for_session(&self.storage, self.state.session.id)
    }

    fn env_target(&self) -> EnvTarget {
        match &self.shell_env.target {
            ShellEnvironmentTarget::Local => EnvTarget::Local,
            ShellEnvironmentTarget::Remote { label } => EnvTarget::Remote {
                label: label.clone(),
            },
            ShellEnvironmentTarget::Sandbox(guest) => EnvTarget::Sandbox(SandboxTarget {
                name: guest.sandbox().to_string(),
                profile: guest.profile().map(ToString::to_string),
                supported: guest.capability().is_some(),
            }),
        }
    }

    pub(super) fn handle_env_action(&mut self, action: EnvAction) -> Vec<Action> {
        match action {
            EnvAction::Consumed => {}
            EnvAction::Close => {
                self.env_modal.close();
                self.shell_env.reply = None;
            }
            EnvAction::Copy(text) => self.copy_to_clipboard(&text),
            EnvAction::Request(request) => self.start_env_request(request),
        }
        Vec::new()
    }

    fn start_env_request(&mut self, request: EnvRequest) {
        let link = &self.shell_env;
        let effect = match (request, &link.target) {
            (EnvRequest::LoadLocal, _) => Effect::LoadLocal {
                path: link.config_path.clone(),
                vault: self.session_secrets(),
            },
            (EnvRequest::EditLocal { file, edit }, _) => Effect::EditLocal {
                file,
                edit,
                vault: self.session_secrets(),
            },
            (EnvRequest::Secret(edit), ShellEnvironmentTarget::Local) => Effect::EditSecret {
                path: link.config_path.clone(),
                vault: self.session_secrets(),
                edit,
            },
            (EnvRequest::LoadSandbox, ShellEnvironmentTarget::Sandbox(guest)) => {
                Effect::LoadSandbox(guest.clone())
            }
            (EnvRequest::EditSandbox(selection), ShellEnvironmentTarget::Sandbox(guest)) => {
                Effect::EditSandbox {
                    guest: guest.clone(),
                    selection,
                    baseline: link.baseline.clone(),
                }
            }
            (
                EnvRequest::LoadSandbox | EnvRequest::EditSandbox(_),
                ShellEnvironmentTarget::Local | ShellEnvironmentTarget::Remote { .. },
            )
            | (
                EnvRequest::Secret(_),
                ShellEnvironmentTarget::Remote { .. } | ShellEnvironmentTarget::Sandbox(_),
            ) => return,
        };
        let (sender, receiver) = flume::bounded(1);
        smol::spawn(async move {
            let _ = sender.send(run(effect).await);
        })
        .detach();
        self.shell_env.reply = Some(receiver);
    }

    pub(super) fn poll_env(&mut self) -> Dirty {
        let Some(receiver) = &self.shell_env.reply else {
            return Dirty::NO;
        };
        let completion = match receiver.try_recv() {
            Ok(completion) => completion,
            Err(TryRecvError::Empty) => return Dirty::NO,
            Err(TryRecvError::Disconnected) => stopped(self.env_modal.tab()),
        };
        self.shell_env.reply = None;
        self.receive_env(completion);
        Dirty::YES
    }

    fn receive_env(&mut self, completion: Completion) {
        if let Some(baseline) = completion.baseline {
            self.shell_env.baseline = Some(baseline);
        }
        self.shell_env.settings_stale |= completion.settings_stale;
        self.env_modal.receive(completion.reply);
    }

    /// Waits for the work `/env` handed off, for tests that read what it wrote.
    #[cfg(test)]
    pub(crate) fn finish_env_work(&mut self) {
        if let Some(receiver) = self.shell_env.reply.take() {
            let completion = receiver
                .recv()
                .unwrap_or_else(|_| stopped(self.env_modal.tab()));
            self.receive_env(completion);
        }
    }
}

fn stopped(tab: EnvTab) -> Completion {
    let reply = match tab {
        EnvTab::Local => EnvReply::Local {
            environment: Err(WORKER_STOPPED.to_owned()),
            error: None,
        },
        EnvTab::Sandbox => EnvReply::Sandbox {
            listing: Err(WORKER_STOPPED.to_owned()),
            error: None,
        },
    };
    local(reply)
}

async fn run(effect: Effect) -> Completion {
    match effect {
        Effect::LoadLocal { path, vault } => {
            local(smol::unblock(move || load_local(path, &vault)).await)
        }
        Effect::EditLocal { file, edit, vault } => {
            local(smol::unblock(move || edit_local(&file, edit, &vault)).await)
        }
        Effect::EditSecret { path, vault, edit } => {
            smol::unblock(move || edit_secret(path, &vault, edit)).await
        }
        Effect::LoadSandbox(guest) => {
            let (baseline, error) = match guest.profile() {
                Some(_) => match smol::unblock(load_profiles).await {
                    Ok(baseline) => (Some(baseline), None),
                    Err(error) => (None, Some(error)),
                },
                None => (None, None),
            };
            sandbox(&guest, baseline, error).await
        }
        Effect::EditSandbox {
            guest,
            selection,
            baseline,
        } => {
            guest.replace(selection.clone());
            let (baseline, error) = match guest.profile().cloned() {
                Some(profile) => {
                    smol::unblock(move || persist(&profile, baseline, selection)).await
                }
                None => (None, None),
            };
            sandbox(&guest, baseline, error).await
        }
    }
}

fn local(reply: EnvReply) -> Completion {
    Completion {
        reply,
        baseline: None,
        settings_stale: false,
    }
}

async fn sandbox(
    guest: &GuestShellEnvironment,
    baseline: Option<Arc<LoadedSandboxes>>,
    error: Option<String>,
) -> Completion {
    let listing = guest.list().await.map(|response| GuestListing {
        selection: guest.selection(),
        entries: response.entries,
        truncated: response.truncated,
    });
    Completion {
        reply: EnvReply::Sandbox { listing, error },
        baseline,
        settings_stale: false,
    }
}

fn load_local(path: Option<PathBuf>, vault: &SessionSecrets) -> EnvReply {
    EnvReply::Local {
        environment: path
            .ok_or_else(|| NO_CONFIG_DIRECTORY.to_owned())
            .and_then(|path| capture(&path, vault)),
        error: None,
    }
}

fn capture(path: &Path, vault: &SessionSecrets) -> Result<Box<LocalEnvironment>, String> {
    GlobalShellEnvFile::load(path)
        .map(|file| Box::new(LocalEnvironment::capture(file, vault)))
        .map_err(|error| error.to_string())
}

/// A refused write reloads the file, so a conflict shows what is there now.
fn edit_local(file: &GlobalShellEnvFile, edit: ShellEnvEdit, vault: &SessionSecrets) -> EnvReply {
    match file.apply(edit) {
        Ok(saved) => EnvReply::Local {
            environment: Ok(Box::new(LocalEnvironment::capture(saved, vault))),
            error: None,
        },
        Err(error) => EnvReply::Local {
            environment: capture(file.path(), vault),
            error: Some(error.to_string()),
        },
    }
}

/// Runs one secret write, then lists the environment as it reads now, whatever happened.
fn edit_secret(path: Option<PathBuf>, vault: &SessionSecrets, edit: SecretEdit) -> Completion {
    let Some(path) = path else {
        return local(EnvReply::Local {
            environment: Err(NO_CONFIG_DIRECTORY.to_owned()),
            error: None,
        });
    };
    let remembered_file = matches!(
        edit,
        SecretEdit::Save {
            scope: SecretScope::Remembered,
            ..
        }
    );
    let written = match edit {
        SecretEdit::Save { name, scope, value } => replace(vault, &path, &name, scope, value),
        SecretEdit::Delete { name, scope } => delete(vault, &path, &name, scope),
        SecretEdit::Move {
            name,
            from: SecretScope::Session,
            foreign,
        } => remember(vault, &path, &name, foreign),
        SecretEdit::Move {
            name,
            from: SecretScope::Remembered,
            ..
        } => forget_to_session(vault, &path, &name),
        SecretEdit::Extend { name } => vault.extend(&name),
    };
    let settings_stale = remembered_file && written.is_ok();
    let environment = capture(&path, vault);
    let reply = match written {
        Ok(()) => EnvReply::Local {
            environment,
            error: None,
        },
        Err(SecretError::ForeignEntry(name)) => EnvReply::ForeignEntry { environment, name },
        Err(error) => EnvReply::Local {
            environment,
            error: Some(error.to_string()),
        },
    };
    Completion {
        reply,
        baseline: None,
        settings_stale,
    }
}

fn load_profiles() -> Result<Arc<LoadedSandboxes>, String> {
    SandboxStore::user_global()
        .and_then(|store| store.load())
        .map(Arc::new)
        .map_err(|error| format!("{PROFILES_UNREADABLE}: {error}"))
}

/// Commits against `baseline`; a conflict reloads it for the next attempt and says so.
fn persist(
    profile: &SandboxName,
    baseline: Option<Arc<LoadedSandboxes>>,
    selection: ShellEnvironmentSelection,
) -> (Option<Arc<LoadedSandboxes>>, Option<String>) {
    let saved = SandboxStore::user_global().and_then(|store| {
        let baseline = match baseline {
            Some(baseline) => baseline,
            None => Arc::new(store.load()?),
        };
        store.save_profile_shell_env(&baseline, profile, Some(selection))
    });
    match saved {
        Ok(saved) => (Some(Arc::new(saved)), None),
        Err(error) => (
            load_profiles().ok(),
            Some(format!(
                "Applied to this session, but profile {profile} was not saved: {error}"
            )),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use caudra_agent::secrets::scope::ForeignEntry;
    use caudra_agent::secrets::{SESSION_SECRET_TTL, SecretValue};
    use caudra_config::config_file::{CONFIG_FILE, CONFIG_VERSION};
    use caudra_storage::StateDir;
    use caudra_storage::id::CaudraId;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::*;

    const NAME: &str = "CLOUDFLARE_API_TOKEN";
    const VALUE: &str = "secret-canary-4711";
    const FOREIGN: &str = "configured-elsewhere";

    #[test_case(ForeignEntry::Refuse ; "asked_first")]
    #[test_case(ForeignEntry::Replace ; "replaced_once_confirmed")]
    fn remembering_over_a_set_entry_the_user_wrote_needs_a_confirm(foreign: ForeignEntry) {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(
            &path,
            format!("version = {CONFIG_VERSION}\n\n[agent.shell_env]\nset = {{ {NAME} = \"{FOREIGN}\" }}\n"),
        )
        .unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let vault = SecretVaults::for_session(&state, CaudraId::generate());
        vault.set(NAME, SecretValue::new(VALUE.to_owned())).unwrap();

        let completion = edit_secret(
            Some(path),
            &vault,
            SecretEdit::Move {
                name: NAME.to_owned(),
                from: SecretScope::Session,
                foreign,
            },
        );

        let secrets = match (foreign, completion.reply) {
            (ForeignEntry::Refuse, EnvReply::ForeignEntry { environment, name }) => {
                assert_eq!(name, NAME);
                assert!(vault.contains(NAME));
                environment.unwrap().secrets
            }
            (
                ForeignEntry::Replace,
                EnvReply::Local {
                    environment,
                    error: None,
                },
            ) => {
                assert!(!vault.contains(NAME));
                environment.unwrap().secrets
            }
            _ => panic!("unexpected reply"),
        };
        let [listed] = secrets.try_into().unwrap();
        let scope = match foreign {
            ForeignEntry::Refuse => SecretScope::Session,
            ForeignEntry::Replace => SecretScope::Remembered,
        };
        assert_eq!((listed.name.as_str(), listed.scope), (NAME, scope));
        assert!(!completion.settings_stale);
    }

    #[test]
    fn extending_lists_the_later_expiry() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join(CONFIG_FILE);
        fs::write(&path, format!("version = {CONFIG_VERSION}\n")).unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let vault = SecretVaults::for_session(&state, CaudraId::generate());
        vault.set(NAME, SecretValue::new(VALUE.to_owned())).unwrap();
        let [before] = vault.listing().try_into().unwrap();

        let completion = edit_secret(
            Some(path),
            &vault,
            SecretEdit::Extend {
                name: NAME.to_owned(),
            },
        );

        let EnvReply::Local {
            environment,
            error: None,
        } = completion.reply
        else {
            panic!("unexpected reply");
        };
        let [listed] = environment.unwrap().secrets.try_into().unwrap();
        assert_eq!(
            listed.expires_at,
            before
                .expires_at
                .map(|expires_at| expires_at + SESSION_SECRET_TTL)
        );
    }
}
