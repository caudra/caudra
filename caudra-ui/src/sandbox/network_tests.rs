use super::{
    NetworkGate, NetworkReconcileRequest, NetworkReconcileStatus, StoreEffect, StoreResult,
    execute_store_effect, start_network_reconcile_with_store,
};
use caudra_config::sandbox::{DomainRule, SandboxDraft, SandboxName, persistence::SandboxStore};
use caudra_sandbox::{Controller, dto::Policy};
use caudra_storage::{
    StateDir,
    sandbox_auth::{SandboxApiKey, SandboxCredentialRef, save_sandbox_api_key},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::Permissions,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};
use test_case::test_case;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NOW: &str = "2026-09-21T12:00:00Z";
const LATER: &str = "2026-09-21T12:05:00Z";
const OWNER: &str = "test-owner";
const NETWORK: &str = "net";
const PROFILE: &str = "dev";
const DOMAIN: &str = "example.test";
const DENY_REVISION: &str = "fb44c0f5ba3bfc047e139e764fc03a852d08600a8777ea16ef62c2892c9503d9";
const ALLOW_REVISION: &str = "9702568256299b79c8a8a32db948ac6bba9a0e476cdadbbc7168f6c84394a017";
const REQUEST_DIGEST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const KEY: &str = "test-only-lifecycle-key-01234567890123456789";
const TIMEOUT: Duration = Duration::from_secs(10);
const FIRST: &str = "first";
const SECOND: &str = "second";

struct FakeProvider {
    address: SocketAddr,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    policies: Arc<Mutex<Vec<String>>>,
    instances: Arc<Mutex<BTreeMap<String, Value>>>,
}

impl FakeProvider {
    fn start(lost: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let policies = Arc::new(Mutex::new(Vec::new()));
        let sent = policies.clone();
        let instances = Arc::new(Mutex::new(BTreeMap::<String, Value>::new()));
        let remote = instances.clone();
        let worker = thread::spawn(move || {
            let mut operations = BTreeMap::<String, String>::new();
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stop.load(Ordering::Acquire) {
                    break;
                }
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.is_empty() || line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                }
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length: ")
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let body: Value = if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                };
                let mut request = headers.lines().next().unwrap().split_whitespace();
                let method = request.next().unwrap();
                let path = request.next().unwrap();
                let value = if path.ends_with("/discover") {
                    discovery()
                } else if path.contains("/templates/") {
                    template()
                } else if method == "POST" {
                    let mut instances = remote.lock().unwrap();
                    let id = if instances.is_empty() { FIRST } else { SECOND };
                    let value = instance(id);
                    instances.insert(id.into(), value.clone());
                    let key = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("idempotency-key: ")
                                .map(str::to_owned)
                        })
                        .unwrap();
                    operations.insert(key.clone(), id.into());
                    operation(&key, id, value)
                } else if path.contains("/operations/") {
                    let key = path.rsplit('/').next().unwrap();
                    let id = &operations[key];
                    operation(key, id, remote.lock().unwrap()[id].clone())
                } else {
                    let id = if path.contains(FIRST) { FIRST } else { SECOND };
                    let mut instances = remote.lock().unwrap();
                    let current = instances.get_mut(id).unwrap();
                    if path.ends_with("/policy") {
                        assert_eq!(method, "PUT");
                        assert_eq!(body["expectedRevision"], current["revision"]);
                        sent.lock().unwrap().push(id.into());
                        if lost && id == FIRST {
                            continue;
                        }
                        let policy: Policy =
                            serde_json::from_value(body["egress"].clone()).unwrap();
                        let revision = ALLOW_REVISION;
                        current["egress"] = json!({"enforced":true,"revision":revision,"effectiveRevision":revision,"policy":policy});
                        current["revision"] = json!(current["revision"].as_u64().unwrap() + 1);
                    } else {
                        assert_eq!(method, "GET");
                    }
                    current.clone()
                };
                let bytes = serde_json::to_vec(&value).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
                let _ = stream.write_all(&bytes);
            }
        });
        Self {
            address,
            stopped,
            worker: Some(worker),
            policies,
            instances,
        }
    }
}

impl Drop for FakeProvider {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

fn operation(key: &str, id: &str, instance: Value) -> Value {
    json!({"operationID":key,"ownerID":OWNER,"sandboxID":id,"executionID":id,"requestDigest":REQUEST_DIGEST,"status":"succeeded","cancelRequested":false,"createdAt":NOW,"updatedAt":NOW,"historyDeadline":null,"instance":instance})
}

fn template() -> Value {
    json!({"schemaVersion":1,"id":"base","architecture":"x86_64","machine":"q35",
        "minimum":{"cpuCount":1,"memoryMB":512,"diskSizeMB":1024},"defaults":{"cpuCount":2,"memoryMB":1024,"diskSizeMB":1024},
        "networkTopology":"slirp-enforced","workcell":{"version":"test","sha256":"","protocolVersion":"2026-07-28","transferProtocol":"workcell-reviewed-v1","remoteWorkspace":true,"workspaceSnapshots":true,"reviewedTransfer":true},
        "build":{"recipe":"import","recipeSHA256":"","sourceRevision":""},"revision":DIGEST,"imageSHA256":DIGEST,"warmStart":false,
        "image":{"format":"qcow2","fileSizeBytes":1024,"virtualSizeBytes":1073741824_u64,"clusterSize":65536,"backingPolicy":"standalone"}})
}

fn discovery() -> Value {
    json!({"apiVersion":"1","ownerID":OWNER,"serverTime":NOW,"authentication":"api_key_namespace","templateID":"base","networkTopology":"slirp-enforced","tlsModes":["sni-only"],
        "capabilities":{"idempotentCreate":true,"operationLookup":true,"conditionalMutations":true,"explicitCredentials":true,"persistentDisk":true,"memoryPause":false,"egressPolicy":true,"cancelCreate":true,"templateCatalog":true,"conditionalTemplateCreate":true,"warmStart":false,"localTemplateAdmin":true,"httpTemplateAdmin":false},
        "limits":{"maxLeaseSeconds":3600,"runtimeAdmission":4,"operationJournalEntries":4096,"listPageSize":100,"resources":{"cpuCount":4,"memoryMB":4096,"diskSizeMB":8192},"newKeyMaxAgeSeconds":300,"newKeyFutureSkewSeconds":30},
        "retention":{"pausedDiskMaxAgeSeconds":0,"operationHistorySeconds":86400,"historyStartsAfter":"instance_removed"},"idempotencyKey":"uuidv7","recovery":"query_operation_never_replay_unknown","credentialScope":"sandbox_lifetime","proxyOrigin":"client_configured"})
}

fn instance(id: &str) -> Value {
    let policy = Policy {
        mode: "sni-only".into(),
        domains: Vec::new(),
        cidrs: Vec::new(),
    };
    let revision = DENY_REVISION;
    json!({"ownerID":OWNER,"sandboxID":id,"executionID":id,"revision":1,"state":"running","workspaceGeneration":"generation",
        "expectedWorkcell":{"serverID":id,"workspaceID":id,"workspaceGeneration":"generation","projectID":id,"principalID":OWNER},
        "template":{"id":"base","revision":DIGEST,"imageIdentity":DIGEST},"resources":{"cpuCount":2,"memoryMB":1024,"diskSizeMB":1024},"networkTopology":"slirp-enforced","persistent":true,"pauseUnclean":false,"leaseDeadline":LATER,
        "retention":{"pausedDiskMaxAgeSeconds":0,"deadline":null},"egress":{"enforced":true,"revision":revision,"effectiveRevision":revision,"policy":policy}})
}

#[test_case(false, false, false; "applied_then_noop")]
#[test_case(true, false, false; "partial_unknown_never_replayed")]
#[test_case(false, true, false; "paused_deferred_without_resume")]
#[test_case(false, false, true; "stale_committed_revision_sends_nothing")]
fn saved_network_worker_commits_then_reconciles_fake_provider(
    lost: bool,
    paused: bool,
    stale: bool,
) {
    let server = FakeProvider::start(lost);
    let directory = tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let storage = StateDir::from_path(directory.path().join("state"));
    let config_path = directory.path().join("config");
    let store = SandboxStore::from_config_dir(&config_path).unwrap();
    let credential = SandboxCredentialRef::new("lifecycle").unwrap();
    save_sandbox_api_key(
        &storage,
        &credential,
        &SandboxApiKey::new(KEY.into()).unwrap(),
    )
    .unwrap();
    let endpoint = format!("http://{}", server.address);
    let draft: SandboxDraft = serde_json::from_value(json!({"providers":{"daemon":{"kind":"e2b-libvirt","api_endpoint":endpoint,"proxy_endpoint":endpoint,"credential_ref":"sandbox-api:lifecycle"}},"networks":{"net":{"enforcement":"required"}},"transfers":{"transfer":{}},
        "profiles":{"dev":{"provider":"daemon","template":"base","template_revision":DIGEST,"cpus":2,"memory_mib":1024,"disk_gib":1,"cwd":".","network":"net","transfer":"transfer","persistent":true,"running_ttl_seconds":300,"on_exit":"detach"}}})).unwrap();
    let baseline = Arc::new(store.save(&store.load().unwrap(), &draft).unwrap());
    let controller = Controller::new(&storage).unwrap();
    let profile = SandboxName::parse(PROFILE).unwrap();
    let first = smol::block_on(controller.create(
        baseline.saved(),
        &profile,
        SandboxName::parse(FIRST).unwrap(),
    ))
    .unwrap();
    let second = smol::block_on(controller.create(
        baseline.saved(),
        &profile,
        SandboxName::parse(SECOND).unwrap(),
    ))
    .unwrap();
    if paused {
        let mut instances = server.instances.lock().unwrap();
        instances.get_mut(FIRST).unwrap()["state"] = json!("paused");
        instances.get_mut(FIRST).unwrap()["leaseDeadline"] = Value::Null;
    }
    let mut draft = baseline.draft();
    draft
        .networks
        .get_mut(&SandboxName::parse(NETWORK).unwrap())
        .unwrap()
        .domains
        .push(DomainRule::parse(DOMAIN).unwrap());
    draft
        .networks
        .insert(SandboxName::parse("other").unwrap(), Default::default());
    draft.profiles.get_mut(&profile).unwrap().network = SandboxName::parse("other").unwrap();
    let StoreResult::Saved(committed) = execute_store_effect(
        Ok(store),
        StoreEffect::Save {
            baseline: baseline.clone(),
            draft,
        },
    ) else {
        panic!("save failed");
    };
    assert!(server.policies.lock().unwrap().is_empty());
    let mut request = NetworkReconcileRequest::committed(&baseline, committed.clone()).unwrap();
    if stale {
        let store = SandboxStore::from_config_dir(&config_path).unwrap();
        let mut draft = committed.draft();
        draft
            .networks
            .get_mut(&SandboxName::parse(NETWORK).unwrap())
            .unwrap()
            .domains
            .clear();
        store.save(&committed, &draft).unwrap();
    }
    let worker_path = config_path.clone();
    let report = start_network_reconcile_with_store(request.clone(), storage.clone(), move || {
        SandboxStore::from_config_dir(&worker_path)
    })
    .recv_timeout(TIMEOUT)
    .unwrap();
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!(report.instances.len(), 2);
    assert_eq!(report.revision, *committed.saved().revision());
    if stale {
        assert!(
            report
                .instances
                .iter()
                .all(|result| result.status == NetworkReconcileStatus::Failed)
        );
        assert!(server.policies.lock().unwrap().is_empty());
        return;
    }
    assert_eq!(
        report.instances[0].status,
        if lost {
            NetworkReconcileStatus::Unknown
        } else if paused {
            NetworkReconcileStatus::Deferred
        } else {
            NetworkReconcileStatus::Applied
        }
    );
    assert_eq!(report.instances[1].status, NetworkReconcileStatus::Applied);
    assert!(
        report.instances[0]
            .detail
            .contains("current profile network: other")
    );
    let mut gate = NetworkGate {
        pending: 1,
        running: true,
        ..Default::default()
    };
    gate.finish(&report);
    assert_eq!(gate.blocker(first.id).is_some(), lost || paused);
    assert!(gate.blocker(second.id).is_none());
    let sends = server.policies.lock().unwrap().clone();
    request.recovery = true;
    let retry = start_network_reconcile_with_store(request, storage.clone(), move || {
        SandboxStore::from_config_dir(&config_path)
    })
    .recv_timeout(TIMEOUT)
    .unwrap();
    assert!(retry.error.is_none());
    assert_eq!(*server.policies.lock().unwrap(), sends);
    assert_eq!(retry.instances[1].status, NetworkReconcileStatus::Noop);
    if lost {
        assert_eq!(retry.instances[0].status, NetworkReconcileStatus::Unknown);
    }
    assert_eq!(
        controller.store().get(&first.name).unwrap().launch,
        first.launch
    );
    assert_eq!(
        controller.store().get(&second.name).unwrap().provider,
        second.provider
    );
}
