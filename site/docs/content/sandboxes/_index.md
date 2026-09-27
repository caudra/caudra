+++
title = "Managed Sandboxes"
weight = 35
[extra]
group = "Guides"
+++

# Managed sandboxes

Use a managed sandbox when Caudra should create or attach to an e2b-libvirt VM and use its Workcell workspace. Sandboxes are optional. A fresh run without a workspace selector uses embedded, local Workcell. Saving a profile or opening the manager does not allocate a VM or export files.

The terminal UI, model connections, credentials and conversation state stay on the client. Workspace tools run in the selected sandbox. Client-local extensions remain client-local. The same [remote identity and project-trust rules](/docs/remote-workspaces/#project-context-and-trust) apply, with no fallback to local execution if attachment fails.

## Compatibility and release status

Caudra pins its Workcell dependency in `Cargo.toml`. Reviewed transfers require compatible remote contracts, and empty-directory publication requires its additional negotiated capability. Matching version labels alone do not establish compatibility. There is no raw-transfer fallback.

Use compatible Caudra, e2b-libvirt and in-guest Workcell builds. Image manifests require `protocolVersion = "2026-07-28"`, `transferProtocol = "workcell-reviewed-v1"`, `remoteWorkspace = true` and `reviewedTransfer = true`. Caudra also validates workspace identity and the [required live capabilities](/docs/remote-workspaces/#prepare-the-server), including durable snapshots, reviewed publication and the complete operation lifecycle, before attachment. Doctor reads provider metadata but does not boot-test an image.

Current persisted shapes are sandbox configuration and instance-store version `1`, remote workspace binding version `2`, remote operation journal version `5`, and transfer journal/archive version `3`. File-only version `2` transfer records remain readable and are upgraded on a journal write. Existing version `2` archive pages remain readable. Other incompatible journal versions are refused. Lifecycle intents require explicit postcondition fields for their action: the requested policy and revision, or the requested lease with a minimum deadline when that lease is finite. Directory recovery metadata is required. Incompatible configuration, remote bindings and incomplete intents fail without rewriting their state. Inspect unresolved effects with the originating build before adopting a current workspace. Existing local embedded sessions still load, including sessions without a stored workspace binding.

## Configure and connect

1. Have the operator prepare the daemon and a compatible template. Caudra does not install libvirt, start the daemon, or change host permissions. See [operator constraints](#operator-constraints).
2. Save a lifecycle key with `caudra auth sandbox set local`, using its hidden prompt. For a new owner namespace, `caudra auth sandbox generate local` generates and saves a key without printing it. `set local --stdin` accepts a secret source through stdin. `caudra auth sandbox list` lists names, and `caudra auth sandbox delete local` removes the credential, not any VMs.
3. Open `/sandbox providers`, create the provider, and reference `sandbox-api:local`. Save it, then use Doctor. Select a compatible image and create a profile in `/sandbox profiles`. Network and Transfer policies are reusable records reached from Profiles. The [configuration example](#configuration-schema) shows the exact fields.

For a saved provider `local`, profile `rust`, and new instance name `dev`:

```bash
caudra sandbox doctor --provider local --local
caudra sandbox create dev --profile rust
caudra sandbox attach dev
caudra --sandbox dev
```

Doctor performs authenticated discovery and catalog reads. `--local` also checks whether `/dev/kvm` exists and is readable and writable. It does not test libvirt launch authorization or run a guest probe. Create explicitly allocates, waits for readiness, and verifies Workcell. A failed verification retains the record for inspection.

The CLI `attach` command verifies a saved instance and prints its record. It does not open the TUI. `--sandbox dev` starts a session on that exact instance and never creates a replacement. It works for the TUI, `--print`, SDK stream mode and ACP, and cannot be combined with `--workcell-*` selectors.

To save an existing provider instance as borrowed:

```bash
caudra sandbox list --provider local
caudra sandbox attach borrowed --provider local --instance INSTANCE_ID --cwd .
caudra --sandbox borrowed
```

Borrowing verifies identity without taking ownership of the disk. No files are copied by Create or Attach. An optional initial-seed root in the TUI Create form is carried to the workbench Transfer view after you explicitly attach to the created sandbox. Upload still requires comparison, review and approval.

### Resume a conversation or VM

```bash
caudra --session SESSION_ID
caudra --continue
caudra --sandbox dev --sandbox-resume --session SESSION_ID
```

`--session` restores the saved sandbox source before resolving Workcell. `--continue` can recover the last saved sandbox source for the client directory. It is a resume preference, not an automatic sandbox default for fresh runs. A paused VM requires explicit approval through `--sandbox-resume` or a separate `sandbox resume` command. Conversation resume and cold-boot VM resume are different operations.

Caudra validates the original authority, principal, workspace generation and cursor. Missing records, changed identity or an unavailable provider stop resume. No new VM is substituted under old history. SDK `--fork-session` is a history-only fork and does not implicitly recover the source sandbox. See [Sessions](/docs/sessions/) for transcript and snapshot behavior.

## Configuration schema

Managed configuration lives in user-global `sandboxes.toml`, beside `init.lua` and `workcell.toml`. It is separate from model-provider `providers.toml` and direct Workcell endpoint profiles. The default Linux path is `~/.config/caudra/sandboxes.toml`. [Debug builds and namespaces](/docs/configuration/#directory-layout) can select a different directory.

The file uses `version = 1` and `[sandbox.providers.NAME]`, `[sandbox.networks.NAME]`, `[sandbox.transfers.NAME]`, and `[sandbox.profiles.NAME]`. Unknown fields, unsupported versions and dangling references are rejected. The file must be a user-owned regular file with no group/other access or symlink components. Saves check the previous file revision rather than overwriting concurrent edits.

Replace the endpoints, resource values and template ID with your provider's values. A profile names a template ID rather than a revision. Each create launches the revision that the provider catalog currently lists for that ID.

```toml
version = 1

[sandbox.providers.local]
kind = "e2b-libvirt"
api_endpoint = "http://127.0.0.1:3000"
proxy_endpoint = "http://127.0.0.1:49983"
credential_ref = "sandbox-api:local"

[sandbox.networks.rust]
enforcement = "required"
tls_mode = "sni-only"
domains = ["github.com", "*.githubusercontent.com", "index.crates.io", "static.crates.io"]
cidrs = []

[sandbox.transfers.source]
respect_gitignore = true
initial_seed = "ask"
delete_extraneous = false

[sandbox.profiles.rust]
provider = "local"
template = "caudra-rust"
cpus = 4
memory_mib = 4096
disk_gib = 20
cwd = "."
network = "rust"
transfer = "source"
persistent = true
running_ttl_seconds = 3600
on_exit = "detach"
```

The implemented schema is in `caudra-config/src/sandbox.rs`: `SandboxProvider`, `NetworkPolicy`, `TransferPolicy`, `SandboxProfile`, and the versioned `SandboxDocument`. Persistence rules are in `caudra-config/src/sandbox/persistence.rs`.

| Record | Rules and defaults |
|--------|--------------------|
| Provider | `kind` is `e2b-libvirt`. Both endpoints are origins, without paths, credentials, query or fragment. HTTPS is accepted, or HTTP on a numeric loopback address. `http://localhost` is rejected. `credential_ref` is a lifecycle `sandbox-api:NAME` reference, not a Workcell `credential:NAME` reference. |
| Profile | Provider, template ID, resources, cwd, network, transfer and running TTL are required. A running TTL of `0` has [no expiry](#leases-with-no-expiry). `persistent` defaults to `true`. `on_exit` defaults to `detach`, the only supported value. `cwd` is Workcell-root-relative, with `.` selecting the root. |
| Network | `enforcement` is required and is `required` or `off`. TLS defaults to `sni-only`, and lists default to empty. Required enforcement with empty lists is deny-all. `off` must have empty lists and cannot select MITM. |
| Transfer | Defaults are `respect_gitignore = true`, `initial_seed = "ask"`, and `delete_extraneous = false`. `initial_seed = "none"` disables the initial-seed offer, not later explicit transfers. `delete_extraneous = true` is rejected. |

Omitting `exclude` keeps the built-in profile defaults: `**/.git/**`, `**/.env*`, `**/target/**`, `**/node_modules/**`, `**/.venv/**`, `**/.ssh/**`, `**/.aws/**`, `**/.caudra/**`, `**/*.pem`, and `**/*.key`. Supplying an array replaces those defaults. Exclusions are relative globs without traversal, negation or absolute paths. Independent protected-path checks remain in force even with `exclude = []` or gitignore handling disabled.

CPU count, memory MiB, disk GiB and TTL must fit discovered provider limits and template minimums. A TTL of `0` fits only a provider without a lease cap. Unsupported architecture, topology, TLS or persistence is refused before creation. Disk size is virtual capacity, not a reservation of host space or a promise to resize the guest filesystem. Save can validate configuration offline, but launch compatibility remains unverified until discovery succeeds.

### Saved defaults versus effective state

The manager separates field drafts, saved configuration and live instance state. Applying a field changes the draft. Saving changes future launch defaults. It does not change the running VM, apply network rules, transfer files or rebind the conversation.

Instances retain a resolved launch snapshot, including provider, profile, network and transfer revisions and the template revision they launched from. Instance details show saved-default drift separately from the live descriptor. A live lease or network change requires its own reviewed conditional action. Editing a shared policy affects future launches, and deleting a profile does not delete its instances. Existing owned instances use their launch-time transfer policy. Borrowed instances use the default transfer policy.

Configuration Import replaces the draft only after validation. Export contains configuration and credential references, without secret values or live instance IDs, and saves to a new private file rather than overwriting one. Absolute client transfer roots are not reusable profile fields.

## TUI manager

`/sandbox` and `/sandbox status` open Instances. `/sandbox profiles`, `/sandbox images`, and `/sandbox providers` select the other views. `/sandbox doctor` opens Providers, where Doctor is an explicit action. These are manager views, not slash-command forms of every CLI action.

Use `1` through `4` to switch views outside text fields, `/` to search, and Enter to inspect or edit. Tab moves focus without inserting a tab. F2 selects references. Ctrl+Enter applies a configuration form to the draft or opens a live-action review. Ctrl+S validates and saves configuration. Dirty navigation offers Save, Discard or Keep editing, with Keep editing selected. A file conflict preserves the draft and offers Compare, Reload or Save as a new file.

From the list or detail action context:

| View | Actions |
|------|---------|
| Profiles | `n` New, `d` Duplicate, Delete stages profile deletion, `g` Network policies, `t` Transfer policies, `v` Create VM |
| Instances | `a` Attach, `u` Resume, `p` Pause, `e` Extend, `g` Network, `r` Reconcile, `z` Cancel create, `f` Acknowledge failure, `d` Detach, Delete reviews disk deletion |
| Providers | `h` Doctor, `k` lifecycle credential editor |
| Images | `i` Import, `b` Build, `g` GC, `l` offline Inspect |

VM Running and Workcell Ready are separate states. Ready requires this runtime's authenticated Workcell connection. Instance details also show lease and disk-retention deadlines, ownership, blockers and last-known versus live state.

Live reviews bind to the displayed identity and revision. Enter alone does not accept their default Keep choice. Changing sandbox authority waits for agents, Workbench operations, workflows and permission requests to settle, and refuses unsaved editor or composer drafts. Attach saves and closes the old runtime before opening a fresh session on the verified target. It does not carry old history or grants across authority boundaries.

A control action on the current sandbox first saves, quiesces and detaches that runtime. Pause, Delete, Detach and failure acknowledgement leave it detached. A failed control also leaves it detached and recoverable, without switching tools to the host. Other runtime holders can still block the action.

Closing the manager does not cancel an accepted lifecycle or image operation. Cancel create is a separate action and can race completion. File transfers live in the [workbench Transfer view](/docs/workbench/#transfer). Leaving that view requests cancellation and waits for worker cleanup. Transfer-policy editing remains in Profiles.

## Images and template catalog

Images are daemon-owned immutable catalog revisions. Details expose architecture, resource minimums/defaults, network topology, image digest, Workcell metadata, guest roots when disclosed, and visible instance references. Importing a newer revision of a template ID changes what the next create launches, with no profile edit. Existing disks keep the revision they launched from. Create checks the current revision, then sends the template ID with that revision as the expected revision. If the catalog moves in between, the daemon refuses the create rather than launching an image Caudra did not check. Create never sends a client host path.

Import and Build use typed forms. Import takes a host qcow2, expected digest, template metadata and an expected current catalog revision. Leave that revision empty only for a new template ID. F2 opens a host qcow2 picker, separate from the remote project picker. Selection executes nothing. F4 prepares a separately approved `qemu-img` probe using the selected source and trusted executable. The probe hashes the image and checks bounded format, backing-chain and virtual-size metadata. It is not a boot or guest-integrity test. Changing the source, digest or executable requires another probe before TUI Import.

Build offers the fixed `base`, `egress` and `caudra` recipes. Its fields name the scripts directory, source template revision where needed, binary paths and expected recipe/binary digests. F3 cycles typed choices. The forms also expose Workcell protocol/features, resources and guest CA assertions. A raw JSON override is available for advanced import/build input, but validation and approval still apply.

These are trusted local administration operations, restricted to a numeric-loopback provider. Supply absolute helper, `qemu-img`, database and catalog paths. Ctrl+Enter previews the exact helper arguments, stdin, working directory and fixed environment. Approving trusts those host executables and build inputs. Build can use network and disk through the selected recipe. No arbitrary shell fragment or inherited project environment is accepted.

The provider daemon must be offline for the helper's exclusive database/catalog locks. Stop your own daemon separately with the operator's procedure. Caudra never stops a third-party daemon or silently invokes sudo. Import copies the image into managed storage so overlays do not depend on a mutable source file. GC is explicit and refuses referenced revisions. Public HTTP catalog APIs do not accept host paths.

For an already prepared strict admin request file:

```bash
caudra sandbox images --provider local --request image-request.json
caudra sandbox images --provider local --request image-request.json --yes
```

The first command previews only. The second executes the approved offline helper. The request schema is `AdminRequest` in `caudra-sandbox/src/local_admin.rs`: `helper` has `executable`, `qemu_img`, `database`, and `catalog_dir`, and `operation` has `action` (`import`, `build`, `inspect`, or `gc`) plus `input`. Import/Build inputs use the camelCase fields of `ImportRequest`/`BuildRequest`, while the template `Manifest` is in `caudra-sandbox/src/dto.rs`. These are different schemas from `sandboxes.toml`. The TUI forms avoid constructing them by hand.

## Network policy

The Workcell proxy carries workspace traffic between client and guest. Guest egress policy controls the guest's outbound traffic. Neither changes the route of Caudra's client-side model requests.

`required` enforcement must match both the provider and image topology. There is no fallback to unrestricted networking. `off` is an explicit unrestricted choice supported only on compatible providers. The sample allowlist is a starting point, not a complete package-registry/CDN policy.

Domain rules are hostnames or leading wildcard subdomains, not URLs. CIDRs describe address ranges. Policy has no port, HTTP path or method fields. Wildcards widen the permitted subdomains, and client rules cannot relax operator blocks on private or metadata destinations.

With `sni-only`, TLS passes through and the proxy matches visible hostnames, preserving end-to-end payload encryption and certificate pinning. `mitm` terminates TLS and can break pinning. MITM requires discovered provider support and an explicitly reviewed image `guestCA` capability. Egress topology alone does not establish guest CA readiness. The fixed build recipes do not provision that CA and refuse a claim that they do. Prepare a compatible image separately and import it.

Current e2b-libvirt reports `liveTlsModeChange = false`. Same-mode allowlist changes are supported, but changing an existing instance between SNI-only and MITM is refused. Changing the saved profile cannot bypass this restriction.

Instances show the saved policy revision and a separately confirmed effective revision. Unavailable or unconfirmed effective state is not proof that proposed rules are active. To test or apply a live policy, use the instance Network form, or a strict JSON file containing `{"mode":"sni-only","domains":["github.com"],"cidrs":[]}`:

```bash
caudra sandbox network dev --policy policy.json --test github.com
caudra sandbox network dev --policy policy.json --apply --yes
```

This JSON uses `Policy` in `caudra-sandbox/src/dto.rs`, with `mode`, not the TOML `tls_mode`/`enforcement` fields. Test evaluates rule matching only. It does not resolve DNS or probe connectivity. Apply checks the reviewed execution and revision and leaves saved profiles unchanged. F4 in the Network form performs the same rule-only test.

## Reviewed file transfers

Connecting is not mirroring. Choose an existing absolute client directory and an existing Workcell-root-relative remote directory for each link. The remote root is independent of the agent's current cwd. `.` means the exposed Workcell root, not the guest filesystem root. Remote paths use forward slashes and cannot be absolute or contain `..`. Do not select a home directory when you intend to export one project. A local root containing Caudra's persistent state directory is refused, keeping recovery records outside the transfer tree.

| Action | Effect |
|--------|--------|
| Compare | Read bounded inventories and file identities on both ends. No workspace writes. |
| Seed | Copy selected local-only regular files to absent remote destinations. Existing files are not replaced. |
| Push | Copy selected local files, creating or conditionally replacing remote files. |
| Pull | Copy selected remote files, independently authorizing and conditionally publishing to the client root. |

Both roots, the authenticated remote authority, file revisions, content digests and filters are bound into the review. Read/export permissions come from the selected local root's policy, not the active conversation's unrelated local or remote project. Root identity is rechecked, including the canonical local directory's device and inode. A protected canonical ancestor is rejected, so selecting a subdirectory of a credential store does not bypass exclusions.

Transfers handle regular binary and text files with content and executable-bit metadata only. They do not follow symlinks, cross mounts or nested repositories, or copy devices, sockets, FIFOs, ownership, setuid bits, ACLs or xattrs. Protected names include `.env*`, repository metadata, credential stores and `.caudra` anywhere in the path. Gitignore and configured excludes add filtering, not authorization. Hidden files are considered by the inventory. Renaming secrets to an ordinary source filename is not content-based secret detection, so review every export.

Copying empty directories requires negotiated directory-publication support on both ends. Unsupported directory effects are reported rather than replaced with shell commands or placeholder files. New directories use safe publisher defaults and do not copy source ownership or extended attributes.

Selecting a remote workspace does not authorize arbitrary host writes. Compare and preview can ask for native read permissions. Execute consents to the displayed plan, then checks native permissions on both ends, including actual destination and new-parent effects. Normal deny rules still apply. Pulled files do not gain configuration or workflow trust merely because the transfer was approved.

### TUI transfer review

Attach to the sandbox, open `/workbench`, and select Transfer or press `Ctrl+X 4`. Choose both roots and Compare. The local and sandbox panes support linked or independent folder navigation, file and folder selection, change badges and read-only inspection. Upload maps to Push and Download maps to Pull. An initial-seed handoff uses create-only Seed semantics. The sandbox manager no longer contains a file-transfer mode.

The review lists new files, overwrites and directory effects, including selected empty directories. Text diffs use bounded prefixes and mark truncation. Binary previews report size and digest. A truncated overall review cannot be executed until you select fewer entries. Changed roots, filters, source/destination revisions or instance state require a fresh review. Unsaved Caudra editor buffers block admission, and ordinary editing remains blocked until the transfer worker closes and drains. The CLI cannot inspect buffers in other editors, so save or close them separately.

### Exact CLI commands and prompts

Replace `/home/alice/code/app` with your existing client project. The examples select files relative to the two roots, not globs:

```bash
caudra sandbox transfer compare dev --local-root /home/alice/code/app --remote-root .
caudra sandbox transfer seed dev --local-root /home/alice/code/app --remote-root . --select Cargo.toml --select src/main.rs --dry-run
caudra sandbox transfer seed dev --local-root /home/alice/code/app --remote-root . --select Cargo.toml --select src/main.rs
caudra sandbox transfer push dev --local-root /home/alice/code/app --remote-root . --select src/main.rs
caudra sandbox transfer pull dev --local-root /home/alice/code/app --remote-root . --select output/result.bin
caudra sandbox transfer reconcile dev --local-root /home/alice/code/app --remote-root .
```

Repeat `--select` for each exact file. No file is implicitly selected, and transfer has no `--yes` flag. `--dry-run` builds a review without publishing, but read permissions still apply. Differing files are conflicts to review, not candidates for an automatic newest-mtime winner. Missing parents are explicit plan effects, not hidden directory creation.

The command emits JSON lines for `permission`, `comparison`, `plan`, `progress`, and `result` events. In a terminal, answer each permission with `allow`, `deny`, or a displayed `allow_option` response. At the plan prompt, type the exact emitted `plan_id`. That is plan consent, not a blanket permission grant.

Non-terminal stdin requires `--json-input`, even for Compare or Reconcile. A controller must read the emitted requests and send one JSON reply per line, using their actual IDs:

```json
{"request_id":"ID_FROM_PERMISSION_REQUEST","answer":"allow"}
{"confirm_plan":"ID_FROM_PLAN_EVENT"}
```

Permission prompts can occur before and after plan consent. EOF, invalid or mismatched replies deny rather than execute unattended. A prior dry-run's plan ID is not approval for a new invocation. The CLI schema and response parsing are in `src/cli.rs` (`SandboxTransferArgs`) and `src/cmd/sandbox_transfer.rs`.

### Partial outcomes and recovery

Publication is journaled before effects and settled per operation. Inspect result IDs, operation IDs, stopped reasons and deferred cleanup. A later conflict, cancellation or connection loss does not undo earlier confirmed files or directories. Uploaded staging bytes alone are not a published file.

Use Reconcile in Transfer or `sandbox transfer reconcile` with the original roots to query recorded publication status. It does not resend uploads or publications. Records from other roots or authorities remain visible for recovery but are not reconciled against the wrong workspace. Unknown outcomes stay blocked. Matching current bytes alone do not prove whether an uncertain operation ran. Once recovery settles, Compare again and review any remaining work as a new plan.

Keep the persistent client transfer journal and its archive pages. They retain operation receipts and last-confirmed transfer bases across restarts. Do not delete them to bypass recovery blocks. There is no transfer-specific force-acknowledgement command.

Transfers never delete destination-only files. No bidirectional mirroring, transfer-on-exit, rollback or whole-project undo is provided. `RollbackCoverage` is `None`. Publication is not atomic across multiple files, and replacement is not an atomic compare-and-swap against arbitrary external writers. Keep other writers out of the destination while transferring. Workspace snapshots are a separate feature, not transfer backup coverage.

## Lifecycle controls and failure recovery

```bash
caudra sandbox list
caudra sandbox inspect dev
caudra sandbox extend dev --lease-seconds 3600
caudra sandbox pause dev
caudra sandbox resume dev --lease-seconds 3600
caudra sandbox detach dev
```

`list` shows saved records. `list --provider local` reads owner-scoped live instances. `inspect` has the alias `reconcile` and queries recorded operations rather than replaying them. `--lease-seconds 0` asks for a lease with no expiry. Extend cannot shorten a lease. Pause requires an owned persistent instance. Pause and Resume preserve its filesystem, but Resume cold-boots and does not restore processes or RAM. Exiting Caudra detaches without pausing or deleting the VM. The explicit `detach` command marks the local record detached and leaves the VM and disk alone.

Destructive and recovery actions have separate commands:

```bash
caudra sandbox cancel dev
caudra sandbox acknowledge-failure dev
caudra sandbox delete dev --yes
caudra sandbox delete borrowed --destroy-borrowed --yes
```

Cancel applies only to an in-progress create, and can delete that create's VM and disk. A successful create requires Delete instead. Deleting a borrowed record without `--destroy-borrowed` only detaches it. Resume, Cancel, Acknowledge failure and disk deletion prompt unless their supported `--yes` flag is supplied. That flag does not bypass identity, lease or recovery checks.

After a lost create reply, preserve the reserved name and run Inspect. Unknown or expired operation history is not evidence that no VM exists. Do not issue a replacement Create or discard the saved record. Persistent disks can remain after failed creation or readiness checks.

Pending lifecycle intents block attachment until their postconditions are confirmed or an applicable failure is explicitly acknowledged. `acknowledge-failure` displays the requested intent and last observed state, then records failure acknowledgement. It does not send a retry, cancel the request, claim the intended change succeeded or prove that the VM is safe. Use it only after inspecting the actual provider state. It is not a way to forget an unknown create.

Lifecycle Inspect, transfer Reconcile, and `/remote reconcile` have different journals. Use [remote recovery](/docs/remote-workspaces/#recovery-commands) for uncertain model-tool mutations, and transfer recovery for publication outcomes. Do not substitute `/remote acknowledge` for a transfer result.

### Leases and disk retention

e2b-libvirt distinguishes the running lease from paused-disk retention. Persistent disks use their create-time retention policy, seven days by default. An operator-configured zero max-age means indefinite paused retention, while a null deadline means no current retention deadline. Inspect the live descriptor and instance details instead of treating `persistent = true` as unlimited storage. Nonpersistent instances do not retain their disks on expiry.

Expiry stops a persistent VM even if the guest cannot flush, records an unclean pause and retains the disk subject to retention. Unflushed data can be lost. Explicit pause can refuse a failed flush. Workcell proxy access ends with the running lease, independently of a client conversation remaining open.

A running command does not extend the lease. A shell call can run for up to six hours, so extend the lease before a long job starts. Expiry stops the VM and the command with it.

The daemon retains create-operation history while the instance record exists and for 24 hours after removal or a failure before insertion. Its 4096-entry operation journal refuses new work rather than evicting history early. Unknown/pruned history returns `history_unavailable` with an unknown outcome. Database loss, rollback or clock problems require operator recovery, not blind retries.

### Leases with no expiry

A lease of `0` has no expiry. The VM keeps running, and keeps using host CPU, memory and disk, until it is paused or deleted. That includes the time after Caudra exits. Instance details show its lease deadline as `none (runs until paused or deleted)`.

The operator opts in by starting the daemon with `E2B_LOCAL_MAX_TIMEOUT=0`, which removes the lease cap. Discovery then reports `maxLeaseSeconds` as `0`. A daemon with a cap refuses a lease of `0`, and Caudra checks the cap before it sends anything. Persistent and non-persistent instances follow the same rule.

Extend never shortens a lease. Extending to `0` removes the expiry of a running instance. Caudra refuses a finite Extend on an instance that has no expiry, before any request. To give a persistent instance a finite lease again, Pause and then Resume with the lease you want, or use Restart from the manager. A non-persistent instance cannot pause, so it runs until you delete it.

The E2B-compatible API stays finite. Its timeouts must be positive, and it lists an instance with no expiry with `endAt` set to `9999-12-31T23:59:59Z`. An E2B set-timeout call replaces that lease with the finite timeout it gives.

## Operator constraints

- The current provider is e2b-libvirt on a Linux KVM/libvirt host. `/dev/kvm` access and successful domain enumeration do not establish permission to launch. Use one daemon per libvirt namespace and its existing lock. Do not start a second daemon per profile.
- Lifecycle API keys are owner namespaces, not a registered-key admission system. Keep the listener inaccessible to untrusted clients and guests. HTTPS alone does not fix admission. The separately configured Workcell proxy origin is a trust boundary, and its sandbox bearer is held in memory by Caudra. The daemon's bearer lasts across sandbox resumes and is not an execution-scoped, individually revocable attach credential.
- Catalog instances cold-boot (`warmStart: false`). Resource requests are bounded by template minimums and operator maxima. Resume reuses the recorded image/resources and can refuse if current operator limits no longer permit them. Immutable backing images must remain available for dependent disks.
- Enforced slirp egress needs the matching image and host relay setup. The current e2b-libvirt relay requires the operator's session libvirt configuration to disable QEMU's seccomp sandbox, reducing defense in depth. Caudra does not make that change or restart libvirt. Review this deployment tradeoff before using enforced mode.
- Built-in image recipes do not install a MITM guest CA or support arbitrary custom guest layouts. Imported images can declare reviewed capabilities, but those assertions and metadata probes do not replace live Workcell validation.
- Reviewed transfers currently require the Unix local-root/publication implementation. They are not a Windows host-transfer guarantee. End-to-end Workcell tests do not establish real KVM lifecycle or offline-image-build acceptance. Verify those separately for the operator's deployment.
