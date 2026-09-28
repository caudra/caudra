use std::{
    io::{self, BufRead, IsTerminal, Write},
    sync::Arc,
    thread,
};

use async_trait::async_trait;
use caudra_agent::{
    AgentEvent, CancelToken, EventSender,
    permissions::PermissionAnswer,
    workspace_transfer::{
        CleanBufferLease, LocalRootIdentity, PullBufferGuard, Side, TransferAction, TransferError,
        TransferEvent, TransferEvents,
    },
};
use caudra_config::sandbox::{SandboxName, persistence::SandboxStore};
use caudra_sandbox::Controller;
use caudra_storage::StateDir;
use caudra_ui::sandbox::transfer::{TransferLink, transfer_permissions};
use caudra_workcell::TransferSessionHost;
use caudra_workspace::WorkspacePath;
use color_eyre::{
    Result,
    eyre::{bail, eyre},
};
use flume::Sender;
use serde_json::{Value, json};

use crate::cli::{SandboxTransferArgs, SandboxTransferMode};

const RESPONSE_BYTES: u64 = 16 * 1024;
const CONFIRMATION_REQUIRED: &str =
    "Explicit matching plan confirmation required; no transfer executed";
const HEADLESS_REQUIRED: &str =
    "Headless transfer requires --json-input and explicit permission/plan replies; EOF denies";

enum Message {
    Output(Value),
    Confirm(Value, Sender<bool>),
    Done(Result<(), String>),
}
struct Progress(Sender<Message>);
impl TransferEvents for Progress {
    fn emit(&self, event: TransferEvent) {
        let _ = self
            .0
            .send(Message::Output(json!({"event":"progress", "detail":event})));
    }
}

struct HeadlessBuffers;
struct NoEditorLease;
impl CleanBufferLease for NoEditorLease {}
#[async_trait]
impl PullBufferGuard for HeadlessBuffers {
    async fn lock_clean(
        &self,
        _: &LocalRootIdentity,
        _: &WorkspacePath,
    ) -> Result<Box<dyn CleanBufferLease>, TransferError> {
        Ok(Box::new(NoEditorLease))
    }
}

fn output(value: &Value) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

fn response(input: impl BufRead, json_input: bool) -> Result<Value> {
    let mut line = String::new();
    let count = input.take(RESPONSE_BYTES + 1).read_line(&mut line)?;
    if count == 0 || count as u64 > RESPONSE_BYTES {
        bail!(CONFIRMATION_REQUIRED);
    }
    if json_input {
        Ok(serde_json::from_str(&line)?)
    } else {
        Ok(Value::String(line.trim().into()))
    }
}

fn confirmed(reply: &Value, plan_id: &str) -> bool {
    reply.as_str() == Some(plan_id)
        || reply.get("confirm_plan").and_then(Value::as_str) == Some(plan_id)
}

fn answer(reply: &Value, id: &str, json_input: bool) -> PermissionAnswer {
    let value = if json_input {
        if reply.get("request_id").and_then(Value::as_str) != Some(id) {
            return PermissionAnswer::Deny;
        }
        reply.get("answer").and_then(Value::as_str)
    } else {
        reply.as_str()
    };
    value
        .and_then(PermissionAnswer::decode)
        .unwrap_or(PermissionAnswer::Deny)
}

pub(super) fn run(args: SandboxTransferArgs, state: &StateDir) -> Result<()> {
    if !io::stdin().is_terminal() && !args.json_input {
        bail!(HEADLESS_REQUIRED);
    }
    if !args.local_root.is_absolute() {
        bail!("--local-root must be absolute");
    }
    if matches!(
        args.mode,
        SandboxTransferMode::Seed | SandboxTransferMode::Push | SandboxTransferMode::Pull
    ) && args.selected.is_empty()
    {
        bail!("Select exact files with --select before reviewing a transfer");
    }
    let name = SandboxName::parse(&args.name)?;
    let saved = SandboxStore::user_global()?.load()?;
    let record = Controller::new(state)?.store().get(&name)?;
    let link = TransferLink {
        name,
        instance_revision: record.revision()?,
        configuration_revision: saved.saved().revision().clone(),
        local_root: args.local_root.clone(),
        remote_root: WorkspacePath::new(&args.remote_root)?,
        attached_binding: None,
        include_ignored: false,
        skip_dotfiles: false,
    };
    let paths = args
        .selected
        .iter()
        .map(WorkspacePath::new)
        .collect::<Result<Vec<_>, _>>()?;
    let permissions = Arc::new(transfer_permissions(args.local_root, state));
    let (event_tx, events) = flume::unbounded();
    let (messages, received) = flume::unbounded();
    let (cancel, token) = CancelToken::new();
    let validity_token = token.clone();
    let host = TransferSessionHost {
        permissions: permissions.clone(),
        permission_events: EventSender::new(event_tx, 0),
        buffers: Arc::new(HeadlessBuffers),
        progress: Arc::new(Progress(messages.clone())),
        cancel: token.clone(),
        validity: Arc::new(move || {
            if validity_token.is_cancelled() {
                Err("Transfer cancelled".into())
            } else {
                Ok(())
            }
        }),
    };
    let connector = super::sandbox::transfer_connector(state.clone());
    let worker = thread::Builder::new().name("transfer-cli".into()).spawn(move || {
        let work = || -> Result<()> {
            let mut connection = connector(link, host).map_err(|error| eyre!(error))?;
            smol::block_on(async {
                if args.mode == SandboxTransferMode::Reconcile {
                    let report = connection.session.reconcile(&token).await?;
                    messages.send(Message::Output(json!({"event":"result", "result":report})))?;
                    return Ok(());
                }
                let comparison = connection.session.compare(&token).await?;
                let scan = json!({"local":comparison.scan(&Side::Local), "remote":comparison.scan(&Side::Remote)});
                messages.send(Message::Output(json!({"event":"comparison", "context":comparison.context(), "rows":comparison.rows(), "complete":comparison.complete(), "scan":scan, "recovery":connection.session.recovery()?})))?;
                let action = match args.mode { SandboxTransferMode::Seed => TransferAction::Seed, SandboxTransferMode::Push => TransferAction::Push, SandboxTransferMode::Pull => TransferAction::Pull, _ => return Ok(()) };
                let plan = connection.session.review(action, &paths, &token).await?;
                let preview = json!({"event":"plan", "plan_id":plan.digest(), "review":plan.review(), "recovery_coverage":"None", "dry_run":args.dry_run});
                if args.dry_run { messages.send(Message::Output(preview))?; return Ok(()); }
                let (approval, approved) = flume::bounded(1);
                messages.send(Message::Confirm(preview, approval))?;
                if !matches!(token.race(approved.recv_async()).await, Ok(Ok(true))) { bail!(CONFIRMATION_REQUIRED); }
                (connection.validate)().map_err(|error| eyre!(error))?;
                let report = connection.session.execute(plan.digest(), &token).await?;
                let failed = report.stopped.is_some();
                messages.send(Message::Output(json!({"event":"result", "result":report})))?;
                if failed { bail!("Transfer stopped; inspect result/recovery IDs, reconcile rather than replay"); }
                Ok(())
            })
        };
        let _ = messages.send(Message::Done(work().map_err(|error| error.to_string())));
    })?;
    let result = (|| -> Result<()> {
        loop {
            enum Incoming {
                Permission(Box<caudra_agent::Envelope>),
                Message(Message),
            }
            let incoming = smol::block_on(futures_lite::future::race(
                async {
                    match events.recv_async().await {
                        Ok(event) => Incoming::Permission(Box::new(event)),
                        Err(_) => futures_lite::future::pending().await,
                    }
                },
                async {
                    Incoming::Message(received.recv_async().await.unwrap_or(Message::Done(Err(
                        "Worker disconnected; outcome unknown, reconcile".into(),
                    ))))
                },
            ));
            match incoming {
                Incoming::Permission(event) => {
                    if let AgentEvent::PermissionRequest(request) = event.event {
                        output(&json!({"event":"permission", "request":request}))?;
                        if !args.json_input {
                            eprintln!(
                                "Type allow, deny, or a displayed allow_option response for this exact permission:"
                            );
                        }
                        let reply =
                            response(io::stdin().lock(), args.json_input).unwrap_or(Value::Null);
                        if !permissions
                            .answer(&request.id, answer(&reply, &request.id, args.json_input))
                        {
                            permissions.answer(&request.id, PermissionAnswer::Deny);
                            bail!("Stale or invalid permission response; transfer cancelled");
                        }
                    }
                }
                Incoming::Message(Message::Output(value)) => output(&value)?,
                Incoming::Message(Message::Confirm(preview, reply)) => {
                    output(&preview)?;
                    if !args.json_input {
                        eprintln!(
                            "Review the plan and all new directories/overwrites above. Type the exact plan_id to continue to both-end authorization (anything else denies):"
                        );
                    }
                    let approved =
                        response(io::stdin().lock(), args.json_input).is_ok_and(|reply| {
                            confirmed(&reply, preview["plan_id"].as_str().unwrap_or_default())
                        });
                    let _ = reply.send(approved);
                }
                Incoming::Message(Message::Done(result)) => {
                    return result.map_err(|error| eyre!(error));
                }
            }
        }
    })();
    cancel.cancel();
    worker
        .join()
        .map_err(|_| eyre!("Worker panicked; outcome unknown, reconcile"))?;
    result
}

#[cfg(test)]
mod tests {
    use super::{
        CONFIRMATION_REQUIRED, RESPONSE_BYTES, answer, confirmed, response, transfer_permissions,
    };
    use crate::cli::{Cli, Command, SandboxAction, SandboxTransferMode};
    use caudra_agent::{
        AgentEvent, CancelToken, EventSender, permissions::PermissionAnswer,
        tools::PermissionScopes,
    };
    use caudra_config::ToolKey;
    use caudra_storage::StateDir;
    use clap::Parser;
    use futures_lite::future;
    use serde_json::{Value, json};
    use smol::lock::Mutex as AsyncMutex;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use test_case::test_case;

    const PLAN: &str = "sha256:reviewed";
    const REQUEST: &str = "permission-a";
    const TOOL: &str = "workspace_transfer";
    const PERMISSIONS_PATH: &str = ".caudra/permissions.toml";
    const DENY_TRANSFER: &str = "[workspace_transfer]\ndeny = true\n";
    const DENY_DEFAULT: &str = "default = 'deny'\n";

    #[cfg(unix)]
    #[test_case(DENY_TRANSFER; "explicit_transfer_denial")]
    #[test_case(DENY_DEFAULT; "project_default_denial")]
    fn cli_honors_chosen_local_roots_configured_denials(configuration: &str) {
        let root = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join(PERMISSIONS_PATH);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, configuration).unwrap();
        let permissions = transfer_permissions(
            root.path().into(),
            &StateDir::from_path(state.path().into()),
        );
        let (sender, events) = flume::unbounded();
        let sender = EventSender::new(sender, 0);
        let (_legacy_sender, legacy_receiver) = flume::bounded(1);
        let legacy_receiver = AsyncMutex::new(legacy_receiver);
        let denied_without_prompt = smol::block_on(future::race(
            async {
                permissions
                    .enforce(
                        &ToolKey::native(TOOL),
                        &PermissionScopes::single(REQUEST.into()),
                        &Value::Null,
                        &sender,
                        Some(&legacy_receiver),
                        REQUEST,
                        &CancelToken::none(),
                        None,
                    )
                    .await
                    .is_err()
            },
            async {
                while let Ok(event) = events.recv_async().await {
                    if matches!(event.event, AgentEvent::PermissionRequest(_)) {
                        return false;
                    }
                }
                false
            },
        ));
        assert!(denied_without_prompt);
    }

    #[test_case(false; "terminal")]
    #[test_case(true; "json_lines")]
    fn eof_and_oversized_replies_are_rejected_before_consent(json_input: bool) {
        for input in [String::new(), "x".repeat(RESPONSE_BYTES as usize + 1)] {
            assert_eq!(
                response(input.as_bytes(), json_input)
                    .unwrap_err()
                    .to_string(),
                CONFIRMATION_REQUIRED,
            );
        }
    }

    #[test_case(false; "terminal")]
    #[test_case(true; "json_lines")]
    fn reading_one_reply_preserves_the_next_independent_decision(json_input: bool) {
        let input = if json_input {
            format!(
                "{}\n{}\n",
                json!({"request_id": REQUEST, "answer":"allow"}),
                json!({"confirm_plan":PLAN})
            )
        } else {
            format!("allow\n{PLAN}\n")
        };
        let mut reader = input.as_bytes();
        assert_eq!(
            answer(
                &response(&mut reader, json_input).unwrap(),
                REQUEST,
                json_input
            ),
            PermissionAnswer::AllowOnce
        );
        assert!(confirmed(&response(&mut reader, json_input).unwrap(), PLAN));
        assert!(response(&mut reader, json_input).is_err());
    }

    #[test_case(json!({"confirm_plan":PLAN}), true; "exact_plan")]
    #[test_case(json!({"confirm_plan":"old"}), false; "stale_plan")]
    #[test_case(json!("yes"), false; "generic_yes_is_not_consent")]
    #[test_case(Value::Null, false; "eof_denies")]
    fn confirmation_is_bound(reply: Value, expected: bool) {
        assert_eq!(confirmed(&reply, PLAN), expected);
    }
    #[test_case(json!({"request_id":REQUEST,"answer":"allow"}), PermissionAnswer::AllowOnce; "exact_request")]
    #[test_case(json!({"request_id":"old","answer":"allow"}), PermissionAnswer::Deny; "stale_request")]
    #[test_case(json!(true), PermissionAnswer::Deny; "no_allow_all_boolean")]
    fn permissions_are_independently_answered(reply: Value, expected: PermissionAnswer) {
        assert_eq!(answer(&reply, REQUEST, true), expected);
    }

    #[test_case("compare", SandboxTransferMode::Compare; "compare")]
    #[test_case("seed", SandboxTransferMode::Seed; "seed")]
    #[test_case("push", SandboxTransferMode::Push; "push")]
    #[test_case("pull", SandboxTransferMode::Pull; "pull")]
    fn cli_has_no_implicit_roots_or_generic_yes_bypass(mode: &str, expected: SandboxTransferMode) {
        let args = [
            "caudra",
            "sandbox",
            "transfer",
            mode,
            "dev",
            "--local-root",
            "/chosen/local",
            "--remote-root",
            "chosen/remote",
            "--select",
            "file",
            "--dry-run",
            "--json-input",
        ];
        let cli = Cli::try_parse_from(args).unwrap();
        let Some(Command::Sandbox {
            action: SandboxAction::Transfer(args),
        }) = cli.command
        else {
            panic!("transfer CLI");
        };
        assert_eq!(args.mode, expected);
        assert!(args.dry_run && args.json_input);
        assert!(Cli::try_parse_from(["caudra", "sandbox", "transfer", mode, "dev"]).is_err());
        assert!(
            Cli::try_parse_from([
                "caudra",
                "sandbox",
                "transfer",
                mode,
                "dev",
                "--local-root",
                "/chosen/local",
                "--remote-root",
                ".",
                "--yes"
            ])
            .is_err()
        );
    }
}
