//! `/env` from the app's side: which shell target the modal describes, and the reads and writes it
//! hands to a background thread so the interface never waits on a file or a guest.

use std::path::PathBuf;
use std::sync::Arc;

use caudra_config::sandbox::SandboxName;
use caudra_config::sandbox::persistence::{LoadedSandboxes, SandboxStore};
use caudra_config::shell_env::{GlobalShellEnvFile, ShellEnvEdit};
use caudra_workcell::{GuestShellEnvironment, ShellEnvironmentTarget};
use flume::{Receiver, TryRecvError};
use workcell_tool_contract::shell_env::ShellEnvironmentSelection;

use super::App;
use crate::components::env_modal::{
    EnvAction, EnvReply, EnvRequest, EnvTab, EnvTarget, GuestListing, LocalEnvironment,
    SandboxTarget,
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
}

impl Default for ShellEnvLink {
    fn default() -> Self {
        Self {
            target: ShellEnvironmentTarget::Local,
            config_path: caudra_config::global_config_path(),
            reply: None,
            baseline: None,
        }
    }
}

pub(super) struct Completion {
    reply: EnvReply,
    baseline: Option<Arc<LoadedSandboxes>>,
}

enum Effect {
    LoadLocal(Option<PathBuf>),
    EditLocal {
        file: Box<GlobalShellEnvFile>,
        edit: ShellEnvEdit,
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
        let target = match &self.shell_env.target {
            ShellEnvironmentTarget::Local => EnvTarget::Local,
            ShellEnvironmentTarget::Remote { label } => EnvTarget::Remote {
                label: label.clone(),
            },
            ShellEnvironmentTarget::Sandbox(guest) => EnvTarget::Sandbox(SandboxTarget {
                name: guest.sandbox().to_string(),
                profile: guest.profile().map(ToString::to_string),
                supported: guest.capability().is_some(),
            }),
        };
        let action = self.env_modal.open(target);
        self.handle_env_action(action)
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
            (EnvRequest::LoadLocal, _) => Effect::LoadLocal(link.config_path.clone()),
            (EnvRequest::EditLocal { file, edit }, _) => Effect::EditLocal { file, edit },
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
    Completion {
        reply,
        baseline: None,
    }
}

async fn run(effect: Effect) -> Completion {
    match effect {
        Effect::LoadLocal(path) => local(smol::unblock(move || load_local(path)).await),
        Effect::EditLocal { file, edit } => {
            local(smol::unblock(move || edit_local(&file, edit)).await)
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
    }
}

fn load_local(path: Option<PathBuf>) -> EnvReply {
    let environment = path
        .ok_or_else(|| NO_CONFIG_DIRECTORY.to_owned())
        .and_then(|path| GlobalShellEnvFile::load(&path).map_err(|error| error.to_string()))
        .map(|file| Box::new(LocalEnvironment::capture(file)));
    EnvReply::Local {
        environment,
        error: None,
    }
}

/// A refused write reloads the file, so a conflict shows what is there now.
fn edit_local(file: &GlobalShellEnvFile, edit: ShellEnvEdit) -> EnvReply {
    match file.apply(edit) {
        Ok(saved) => EnvReply::Local {
            environment: Ok(Box::new(LocalEnvironment::capture(saved))),
            error: None,
        },
        Err(error) => EnvReply::Local {
            environment: GlobalShellEnvFile::load(file.path())
                .map(|file| Box::new(LocalEnvironment::capture(file)))
                .map_err(|error| error.to_string()),
            error: Some(error.to_string()),
        },
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
