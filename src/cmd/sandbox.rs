use caudra_agent::tools::ToolRegistry;
use caudra_config::sandbox::TransferPolicy;
use caudra_config::sandbox::{SandboxName, SandboxProvider, persistence::SandboxStore};
use caudra_sandbox::{
    Controller, LifecycleAction, Ownership, Store, generate_api_key, local_admin,
};
use caudra_sandbox::{dto::Policy, local_admin::AdminRequest};
use caudra_storage::remote_operation_journal::RemoteOperationJournal;
use caudra_storage::{
    StateClass, StateDir,
    id::CaudraId,
    sandbox_auth::{
        MAX_SANDBOX_API_KEY_BYTES, SandboxApiKey, SandboxCredentialRef, delete_sandbox_api_key,
        list_sandbox_credentials, save_sandbox_api_key,
    },
    state::{self, StateKey},
    workspace_binding::StoredWorkspaceBinding,
};
use caudra_ui::sandbox::transfer::{TransferConnection, TransferConnector};
use caudra_ui::sandbox::{SandboxAttachment, SandboxConnector};
use caudra_workcell::TransferSession;
use caudra_workspace::WorkspacePath;
use color_eyre::{
    Result,
    eyre::{Context, bail},
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fs::File;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use super::workcell_runtime::WorkcellRuntime;
use crate::cli::{Cli, SandboxAction, SandboxAuthAction, WorkcellSelectorArgs};

const MIN_API_KEY_BYTES: usize = 32;
const LAST_SANDBOX: StateKey = StateKey {
    name: "sandbox.last_source",
    class: StateClass::Persistent,
};

pub(super) struct PreparedSandbox {
    pub runtime: WorkcellRuntime,
    pub registry: ToolRegistry,
}

pub(super) fn connector(storage: StateDir, cwd: PathBuf) -> SandboxConnector {
    Arc::new(move |name, revision| {
        let prepare = || -> Result<SandboxAttachment> {
            let registry = ToolRegistry::default();
            let runtime = WorkcellRuntime::initialize_sandbox_reviewed(
                &name,
                false,
                &cwd,
                &storage,
                &registry,
                Some(&revision),
            )?;
            let binding = runtime.stored_binding().cloned().ok_or_else(|| {
                color_eyre::eyre::eyre!("Sandbox did not supply a workspace binding")
            })?;
            if !RemoteOperationJournal::open(&storage)?
                .list_pending(&binding)?
                .is_empty()
            {
                bail!("Reconcile pending Workcell mutations before attaching this sandbox");
            }
            Ok(SandboxAttachment {
                name: name.clone(),
                binding: Box::new(binding),
                runtime: Box::new(PreparedSandbox { runtime, registry }),
            })
        };
        prepare().map_err(|error| error.to_string())
    })
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub(super) fn transfer_connector(state: StateDir) -> TransferConnector {
    Arc::new(move |link, mut host| {
        let open = || -> Result<TransferConnection> {
            let loaded = SandboxStore::user_global()?.load()?;
            if loaded.saved().revision() != &link.configuration_revision {
                bail!("Saved profile changed; reload and review transfer roots again");
            }
            let controller = Controller::new(&state)?;
            let record = controller.store().get(&link.name)?;
            if record.revision()? != link.instance_revision {
                bail!("Instance changed; refresh before transferring");
            }
            let policy = record
                .launch
                .as_ref()
                .map(|launch| launch.configuration().transfer.value().clone())
                .unwrap_or_else(TransferPolicy::default);
            let (client, remote, lease) = super::workcell_runtime::connect_transfer(
                &link.name,
                &link.instance_revision,
                &link.remote_root,
                &state,
            )?;
            let expected = controller.store().get(&link.name)?.revision()?;
            let validation_state = state.clone();
            let host_validity = host.validity.clone();
            let validate = Arc::new(move || {
                let check = || -> Result<()> {
                    host_validity().map_err(|error| color_eyre::eyre::eyre!(error))?;
                    if SandboxStore::user_global()?.load()?.saved().revision()
                        != &link.configuration_revision
                        || Controller::new(&validation_state)?
                            .store()
                            .get(&link.name)?
                            .revision()?
                            != expected
                    {
                        bail!(
                            "Instance or saved profile changed; stale review refused. Reopen Transfer."
                        );
                    }
                    Ok(())
                };
                check().map_err(|error| error.to_string())
            });
            host.validity = validate.clone();
            let session = smol::block_on(TransferSession::open(
                link.local_root,
                client,
                remote,
                &policy,
                &state,
                host,
            ))?;
            Ok(TransferConnection {
                session,
                lifetime: Box::new(lease),
                validate,
            })
        };
        open().map_err(|error| error.to_string())
    })
}

fn provider(name: &str) -> Result<SandboxProvider> {
    let loaded = SandboxStore::user_global()?.load()?;
    loaded
        .saved()
        .configuration()
        .providers
        .get(&SandboxName::parse(name)?)
        .cloned()
        .ok_or_else(|| color_eyre::eyre::eyre!("saved sandbox provider is missing"))
}

pub fn run(action: SandboxAction, state: &StateDir) -> Result<()> {
    let controller = Controller::new(state)?;
    match action {
        SandboxAction::Transfer(args) => return super::sandbox_transfer::run(args, state),
        SandboxAction::AcknowledgeFailure { name, yes } => {
            let name = SandboxName::parse(&name)?;
            let record = controller.store().get(&name)?;
            println!("{}", record.lifecycle_failure_review()?);
            confirm(
                yes,
                "Acknowledge this failure without claiming the requested intent succeeded?",
            )?;
            print_json(&controller.acknowledge_lifecycle_failure(&name, &record.revision()?)?)?;
        }
        SandboxAction::Detach { name } => print_json(&smol::block_on(
            controller.action(&SandboxName::parse(&name)?, LifecycleAction::Detach),
        )?)?,
        SandboxAction::Cancel { name, yes } => {
            let name = SandboxName::parse(&name)?;
            let record = controller.store().get(&name)?;
            print_json(&record)?;
            confirm(
                yes,
                "Cancel this in-progress create? This is not a VM delete.",
            )?;
            print_json(&smol::block_on(
                controller.cancel_create(&name, &record.revision()?),
            )?)?;
        }
        SandboxAction::Network {
            name,
            policy,
            test,
            apply,
            yes,
        } => {
            let name = SandboxName::parse(&name)?;
            let policy: Policy = read_json_file(&policy)?;
            policy.validate()?;
            let record = smol::block_on(controller.inspect(&name))?;
            print_json(
                &serde_json::json!({"reviewed_instance":record.instance,"proposed_policy":policy}),
            )?;
            if let Some(destination) = test {
                println!(
                    "Rule match: {}. Rule evaluation only: no DNS lookup or real network probe. Operator blocks and TLS/destination availability still apply.",
                    policy.test_destination(&destination)?
                );
            }
            if apply {
                confirm(
                    yes,
                    "Apply this network policy conditionally to the reviewed execution/revision?",
                )?;
                print_json(&smol::block_on(controller.action_at(
                    &name,
                    &record.revision()?,
                    LifecycleAction::ApplyPolicy { policy },
                ))?)?;
            }
        }
        SandboxAction::Images {
            provider: name,
            request,
            yes,
        } => {
            let request: AdminRequest = read_json_file(&request)?;
            let command = request.command(&provider(&name)?)?;
            println!("{}", command.preview()?);
            if yes {
                print_json(&command.execute_approved()?)?;
            } else {
                println!(
                    "Preview only. Re-run with --yes to approve this local helper. The provider daemon must be offline; Caudra never stops it."
                );
            }
        }
        SandboxAction::Doctor {
            provider: name,
            local,
        } => {
            let provider = provider(&name)?;
            if local {
                print_json(&local_admin::inspect_local(&provider)?)?;
            }
            print_json(&smol::block_on(controller.doctor(&provider))?)?;
        }
        SandboxAction::Create { name, profile } => {
            let name = SandboxName::parse(&name)?;
            let profile = SandboxName::parse(&profile)?;
            let loaded = SandboxStore::user_global()?.load()?;
            smol::block_on(controller.create(loaded.saved(), &profile, name.clone()))
                .wrap_err("creation did not complete; any reserved name remains recoverable with sandbox inspect")?;
            smol::block_on(controller.wait_ready(&name)).wrap_err(
                "sandbox is not ready; its create operation and any retained disk remain saved",
            )?;
            verify(&name, state).wrap_err("daemon creation completed but Workcell attachment failed; sandbox record was retained")?;
            print_json(&controller.store().get(&name)?)?;
        }
        SandboxAction::List {
            provider: Some(name),
        } => print_json(&smol::block_on(
            controller.provider_instances(&provider(&name)?),
        )?)?,
        SandboxAction::List { provider: None } => print_json(&controller.snapshots()?)?,
        SandboxAction::Inspect { name } => print_json(&smol::block_on(
            controller.inspect(&SandboxName::parse(&name)?),
        )?)?,
        SandboxAction::Attach {
            name,
            provider: provider_name,
            instance,
            cwd,
        } => {
            let name = SandboxName::parse(&name)?;
            if let (Some(provider_name), Some(instance)) = (provider_name, instance) {
                smol::block_on(controller.borrow(
                    name.clone(),
                    SandboxName::parse(&provider_name)?,
                    provider(&provider_name)?,
                    &instance,
                    WorkspacePath::new(cwd)?,
                ))?;
            }
            verify(&name, state)?;
            print_json(&controller.store().get(&name)?)?;
        }
        SandboxAction::Resume {
            name,
            lease_seconds,
            yes,
        } => {
            confirm(yes, "Cold-boot resume this saved sandbox?")?;
            let name = SandboxName::parse(&name)?;
            smol::block_on(controller.action(&name, LifecycleAction::Resume { lease_seconds }))?;
            verify(&name, state)?;
            print_json(&controller.store().get(&name)?)?;
        }
        SandboxAction::Pause { name } => print_json(&smol::block_on(
            controller.action(&SandboxName::parse(&name)?, LifecycleAction::Pause),
        )?)?,
        SandboxAction::Extend {
            name,
            lease_seconds,
        } => print_json(&smol::block_on(controller.action(
            &SandboxName::parse(&name)?,
            LifecycleAction::Extend { lease_seconds },
        ))?)?,
        SandboxAction::Delete {
            name,
            yes,
            destroy_borrowed,
        } => {
            let name = SandboxName::parse(&name)?;
            if controller.store().get(&name)?.ownership == Ownership::Owned || destroy_borrowed {
                confirm(yes, "Permanently delete this sandbox and its disk?")?;
            }
            print_json(&smol::block_on(
                controller.action(&name, LifecycleAction::Delete { destroy_borrowed }),
            )?)?;
        }
    }
    Ok(())
}

fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(local_admin::MAX_ADMIN_PREVIEW_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > local_admin::MAX_ADMIN_PREVIEW_BYTES {
        bail!("sandbox input exceeds the review bound")
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| color_eyre::eyre::eyre!("invalid strict sandbox request JSON"))
}

fn verify(name: &SandboxName, state: &StateDir) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let registry = ToolRegistry::default();
    let _runtime = WorkcellRuntime::initialize_sandbox(name, false, &cwd, state, &registry)?;
    Ok(())
}

fn confirm(yes: bool, message: &str) -> Result<()> {
    if yes {
        return Ok(());
    }
    if !io::stdin().is_terminal() {
        bail!("explicit confirmation required; pass --yes");
    }
    eprint!("{message} [y/N] ");
    io::stderr().flush()?;
    let mut line = String::new();
    io::stdin().lock().take(16).read_line(&mut line)?;
    if !matches!(line.trim(), "y" | "Y" | "yes") {
        bail!("sandbox action cancelled");
    }
    Ok(())
}

pub fn auth(action: SandboxAuthAction, state: &StateDir) -> Result<()> {
    match action {
        SandboxAuthAction::List => {
            for reference in list_sandbox_credentials(state)? {
                println!("{reference}");
            }
        }
        SandboxAuthAction::Delete { name } => {
            let reference = SandboxCredentialRef::new(name.as_str())?;
            delete_sandbox_api_key(state, &reference)?;
            println!("Deleted {reference}.");
        }
        SandboxAuthAction::Generate { name } => {
            let reference = SandboxCredentialRef::new(name.as_str())?;
            save_sandbox_api_key(state, &reference, &generate_api_key()?)?;
            println!(
                "Saved {reference} in owner-only lifecycle auth state. Daemon setup remains an external operator action."
            );
        }
        SandboxAuthAction::Set { name, stdin } => {
            let value = if stdin {
                read_key(io::stdin().lock())?
            } else {
                rpassword::prompt_password("Sandbox lifecycle API key: ")?
            };
            if value.len() < MIN_API_KEY_BYTES {
                bail!(
                    "sandbox lifecycle keys require at least 32 bytes; prefer auth sandbox generate"
                );
            }
            let reference = SandboxCredentialRef::new(name.as_str())?;
            save_sandbox_api_key(state, &reference, &SandboxApiKey::new(value)?)?;
            println!("Saved {reference}.");
        }
    }
    Ok(())
}

fn read_key(reader: impl Read) -> Result<String> {
    let mut value = String::new();
    reader
        .take((MAX_SANDBOX_API_KEY_BYTES + 3) as u64)
        .read_to_string(&mut value)
        .map_err(|_| color_eyre::eyre::eyre!("failed to read sandbox API key"))?;
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    if value.len() > MAX_SANDBOX_API_KEY_BYTES {
        bail!("sandbox API key exceeds its size bound");
    }
    Ok(value)
}

pub fn recover_session_source(
    cli: &mut Cli,
    state: &StateDir,
    cwd: &Path,
) -> Result<Option<StoredWorkspaceBinding>> {
    if cli.is_sdk_mode() && cli.fork_session {
        return Ok(None);
    }
    let Some(session_id) = cli.session.as_deref() else {
        if cli.continue_session
            && !cli.workcell.is_set()
            && let Some(record_id) =
                state::get::<CaudraId>(state, &state::project_scope(cwd), LAST_SANDBOX)?
        {
            let record = Store::open(state)?.by_id(record_id)?;
            select_saved_source(&mut cli.workcell, &record.name)?;
            return record.workcell_binding.map(Some).ok_or_else(|| {
                color_eyre::eyre::eyre!(
                    "saved sandbox attachment is incomplete; inspect it before resuming"
                )
            });
        }
        return Ok(None);
    };
    let session = crate::setup::load_session(session_id.parse()?, state)?;
    let Some(binding) = session.workspace_binding() else {
        return Ok(None);
    };
    if binding.sandbox_record().is_none() {
        return Ok(None);
    }
    recover_binding_source(&mut cli.workcell, state, binding)?;
    Ok(Some(binding.clone()))
}

pub(super) fn recover_binding_source(
    args: &mut WorkcellSelectorArgs,
    state: &StateDir,
    binding: &StoredWorkspaceBinding,
) -> Result<()> {
    let Some(record_id) = binding.sandbox_record() else {
        return Ok(());
    };
    let record = Store::open(state)?.by_id(record_id)?;
    select_saved_source(args, &record.name)?;
    if record
        .workcell_binding
        .as_ref()
        .is_none_or(|saved| !saved.same_workspace_identity(binding))
    {
        bail!("saved session sandbox authority does not match its lifecycle record");
    }
    Ok(())
}

pub fn remember_session_source(
    state: &StateDir,
    cwd: &Path,
    binding: Option<&StoredWorkspaceBinding>,
) -> Result<()> {
    if state.is_ephemeral() {
        return Ok(());
    }
    let scope = state::project_scope(cwd);
    if let Some(record) = binding.and_then(StoredWorkspaceBinding::sandbox_record) {
        state::set(state, &scope, LAST_SANDBOX, &record)?;
    } else {
        state::delete(state, &scope, LAST_SANDBOX)?;
    }
    Ok(())
}

fn select_saved_source(args: &mut WorkcellSelectorArgs, name: &SandboxName) -> Result<()> {
    if args.is_set() && args.sandbox.as_deref() != Some(name.as_str()) {
        bail!(
            "session belongs to a different sandbox source; refusing to rebind or fall back locally"
        );
    }
    args.sandbox = Some(name.to_string());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LAST_SANDBOX, auth, read_key, recover_session_source, select_saved_source};
    use crate::cli::{Cli, SandboxAuthAction, WorkcellSelectorArgs};
    use caudra_config::sandbox::SandboxName;
    use caudra_storage::{
        StateDir,
        auth::WorkcellCredentialName,
        id::CaudraId,
        sandbox_auth::{
            MAX_SANDBOX_API_KEY_BYTES, SandboxCredentialRef, list_sandbox_credentials,
            load_sandbox_api_key,
        },
        state,
        workspace_binding::StoredWorkspaceBinding,
    };
    use clap::Parser;
    use std::{fs::Permissions, os::unix::fs::PermissionsExt};
    use test_case::test_case;

    const NAME: &str = "original";
    const SECRET: &str = "sandbox-secret-0123456789-abcdefghijk";

    #[test_case(false; "default_target_never_looks_up_source")]
    #[test_case(true; "explicit_target_never_conflicts_with_source")]
    fn sdk_history_fork_skips_all_source_provenance_recovery(explicit: bool) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let source = CaudraId::generate().to_string();
        let mut cli = Cli::try_parse_from([
            "caudra",
            "--print",
            "--input-format",
            "stream-json",
            "--session",
            &source,
            "--fork-session",
        ])
        .unwrap();
        if explicit {
            cli.workcell.sandbox = Some(NAME.into());
        }
        assert!(
            recover_session_source(&mut cli, &storage, temp.path())
                .unwrap()
                .is_none()
        );
        assert_eq!(cli.workcell.sandbox.as_deref(), explicit.then_some(NAME));
    }

    #[test_case(false)]
    #[test_case(true)]
    fn saved_source_selection_requires_exact_name_and_never_falls_back(conflict: bool) {
        let mut args = WorkcellSelectorArgs {
            sandbox: conflict.then(|| "other".into()),
            ..Default::default()
        };
        let result = select_saved_source(&mut args, &SandboxName::parse(NAME).unwrap());
        assert_eq!(result.is_err(), conflict);
        if !conflict {
            assert_eq!(args.sandbox.as_deref(), Some(NAME));
        }
        assert!(args.endpoint.is_none());
        assert!(args.credential_ref.is_none());
    }

    #[test]
    fn auth_crud_uses_purpose_refs_and_bounded_hidden_input() {
        let temp = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let state = StateDir::from_path(temp.path().join("state"));
        let name = WorkcellCredentialName::new(NAME).unwrap();
        auth(SandboxAuthAction::Generate { name: name.clone() }, &state).unwrap();
        let reference = SandboxCredentialRef::new(NAME).unwrap();
        assert_eq!(
            list_sandbox_credentials(&state).unwrap(),
            vec![reference.clone()]
        );
        assert_eq!(
            load_sandbox_api_key(&state, &reference)
                .unwrap()
                .unwrap()
                .expose_secret()
                .len(),
            64
        );
        assert_eq!(
            read_key(format!("{SECRET}\r\n").as_bytes()).unwrap(),
            SECRET
        );
        assert!(read_key(vec![b'x'; MAX_SANDBOX_API_KEY_BYTES + 3].as_slice()).is_err());
        auth(SandboxAuthAction::Delete { name }, &state).unwrap();
        assert!(load_sandbox_api_key(&state, &reference).unwrap().is_none());
    }

    #[test_case(false; "session_id")]
    #[test_case(true; "continue_source")]
    fn missing_saved_provenance_never_resumes_locally(continue_source: bool) {
        let temp = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        let record = CaudraId::generate();
        let local = StoredWorkspaceBinding::local_from_cwd(".");
        let value = serde_json::to_string(&local)
            .unwrap()
            .replace(local.trust_anchor().as_str(), "https://sandbox.test");
        let remote = serde_json::from_str::<StoredWorkspaceBinding>(&value).unwrap();
        let binding = remote.with_sandbox_record(record).unwrap();
        let mut session =
            caudra_agent::StoredSession::new_with_workspace("test/model", ".", binding);
        session.save(&storage).unwrap();
        let mut cli = if continue_source {
            state::set(
                &storage,
                &state::project_scope(temp.path()),
                LAST_SANDBOX,
                &record,
            )
            .unwrap();
            Cli::try_parse_from(["caudra", "--continue"]).unwrap()
        } else {
            Cli::try_parse_from(["caudra", "--session", &session.id.to_string()]).unwrap()
        };
        assert!(recover_session_source(&mut cli, &storage, temp.path()).is_err());
        assert!(!cli.workcell.is_set());
    }
}
