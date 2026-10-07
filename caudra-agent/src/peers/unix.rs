use std::env;
use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_io::{Async, Timer};
use futures_lite::{AsyncReadExt, AsyncWriteExt, future};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::topics::validate_patterns;
use super::work::valid_memberships;
use super::{
    Delivery, HostInner, MAX_FRAME_BYTES, MAX_LABEL_BYTES, MAX_PATH_BYTES, MAX_SESSIONS,
    OLDER_RECIPIENT, PROTOCOL_VERSION, PeerInfo, PeerSession, Request, Response, Route,
    STATUS_HELD, STATUS_QUEUED, STATUS_RATE_LIMITED, STATUS_REFUSED, STATUS_UNAVAILABLE,
    STATUS_UNKNOWN, SendReceipt, lock, valid_handle, valid_token,
};

const DIRECTORY_MODE: u32 = 0o700;
const FILE_MODE: u32 = 0o600;
const PERMISSION_MASK: u32 = 0o7777;
const STICKY_BIT: u32 = 0o1000;
const MAX_SOCKET_PATH: usize = 103;
const MAX_MANIFEST_BYTES: u64 = 1024;
const MAX_HOSTS: usize = 32;
const MAX_DIRECTORY_ENTRIES: usize = 256;
const MAX_PEERS: usize = 128;
const MAX_CONNECTIONS: usize = 16;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const TIMEOUT: &str = "Local peer transport timed out";
const HUNG_UP: &str = "The recipient hung up without answering";
pub(super) const PARTIAL: &str =
    "Peer discovery is partial: registry, response, or time limit reached";
const UNSAFE_ENTRY: &str = "Peer runtime entry has unsafe ownership, permissions, or type";
const SOCKET_SUFFIX: &str = ".sock";
const MANIFEST_SUFFIX: &str = ".json";
const NAMES_DIRECTORY: &str = "names";
const LOCK_SUFFIX: &str = ".lock";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    incarnation: String,
    socket: String,
}

/// The private directory where every host publishes its manifest and socket.
#[derive(Clone)]
pub(super) struct Directory {
    path: PathBuf,
    identity: Metadata,
}

pub(super) struct Endpoint {
    directory: Directory,
    socket: PathBuf,
    socket_identity: Metadata,
    manifest: PathBuf,
    manifest_identity: Metadata,
}

fn uid() -> u32 {
    // SAFETY: geteuid has no arguments or memory preconditions.
    unsafe { libc::geteuid() }
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn private(metadata: &Metadata, directory: bool) -> Result<(), String> {
    let expected = if directory { DIRECTORY_MODE } else { FILE_MODE };
    if metadata.uid() != uid()
        || metadata.mode() & PERMISSION_MASK != expected
        || (directory && !metadata.is_dir())
    {
        return Err(UNSAFE_ENTRY.into());
    }
    Ok(())
}

fn c_string(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| "Peer runtime path contains a NUL byte".into())
}

fn checked_directory(path: &Path) -> Result<File, String> {
    if !path.is_absolute() {
        return Err("Peer runtime directory must be absolute".into());
    }
    let mut current = File::open("/").map_err(|error| error.to_string())?;
    for component in path.components() {
        match component {
            Component::RootDir => continue,
            Component::Normal(name) => {
                let name = CString::new(name.as_bytes()).map_err(|error| error.to_string())?;
                // SAFETY: the directory fd and terminated component remain live for openat.
                let fd = unsafe {
                    libc::openat(
                        current.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                    )
                };
                if fd < 0 {
                    return Err(io::Error::last_os_error().to_string());
                }
                // SAFETY: successful openat returned a new owned fd.
                current = unsafe { File::from_raw_fd(fd) };
                let metadata = current.metadata().map_err(|error| error.to_string())?;
                let writable = metadata.mode() & 0o022 != 0;
                let root_sticky = metadata.uid() == 0 && metadata.mode() & STICKY_BIT != 0;
                if (metadata.uid() != 0 && metadata.uid() != uid()) || (writable && !root_sticky) {
                    return Err(UNSAFE_ENTRY.into());
                }
            }
            _ => return Err("Peer runtime path cannot contain parent components".into()),
        }
    }
    Ok(current)
}

fn make_private_directory(path: &Path) -> Result<PathBuf, String> {
    let parent = checked_directory(path.parent().ok_or(UNSAFE_ENTRY)?)?;
    let name = c_string(Path::new(path.file_name().ok_or(UNSAFE_ENTRY)?))?;
    // SAFETY: parent fd and the terminated basename are valid for mkdirat.
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), libc::S_IRWXU) };
    if result != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
        return Err(io::Error::last_os_error().to_string());
    }
    let directory = checked_directory(path)?;
    private(
        &directory.metadata().map_err(|error| error.to_string())?,
        true,
    )?;
    Ok(path.to_owned())
}

pub(super) fn runtime_directory() -> Result<PathBuf, String> {
    let namespace =
        caudra_storage::paths::active_app_dir_name().map_err(|error| error.to_string())?;
    let hash = Sha256::digest(namespace.as_bytes());
    let namespace: String = hash[..8].iter().map(|byte| format!("{byte:02x}")).collect();
    let name = format!("caudra-peers-{}-{namespace}", uid());
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        let runtime = PathBuf::from(runtime);
        let path = runtime.join(&name);
        if path.as_os_str().len() + 1 + 32 + SOCKET_SUFFIX.len() <= MAX_SOCKET_PATH
            && checked_directory(&runtime)
                .and_then(|file| {
                    private(&file.metadata().map_err(|error| error.to_string())?, true)
                })
                .is_ok()
        {
            return make_private_directory(&path);
        }
    }
    // Never use TMPDIR: Caudra redirects it to the agent-authorized scratch tree.
    let temporary = Path::new("/tmp")
        .canonicalize()
        .map_err(|error| error.to_string())?;
    make_private_directory(&temporary.join(name))
}

impl Directory {
    pub(super) fn open(path: PathBuf) -> Result<Self, String> {
        let identity = checked_directory(&path)?
            .metadata()
            .map_err(|error| error.to_string())?;
        private(&identity, true)?;
        Ok(Self { path, identity })
    }

    /// Reopens the directory, provided it is still the private one opened.
    fn verify(&self) -> Result<File, String> {
        let file = checked_directory(&self.path)?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        private(&metadata, true)?;
        if !same_file(&self.identity, &metadata) {
            return Err(UNSAFE_ENTRY.into());
        }
        Ok(file)
    }

    fn read_manifest(&self, host: &str) -> Result<Manifest, String> {
        if !valid_token(host) {
            return Err("Invalid host incarnation".into());
        }
        let directory = self.verify()?;
        let name = c_string(Path::new(&format!("{host}{MANIFEST_SUFFIX}")))?;
        // SAFETY: both fd and terminated basename remain valid for openat.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: successful openat returns a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        private(&metadata, false)?;
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            return Err(UNSAFE_ENTRY.into());
        }
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err("Oversized peer manifest".into());
        }
        let manifest: Manifest =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        if manifest.version != PROTOCOL_VERSION
            || manifest.incarnation != host
            || manifest.socket != format!("{host}{SOCKET_SUFFIX}")
        {
            return Err("Unsupported or inconsistent peer manifest".into());
        }
        Ok(manifest)
    }

    /// `None` when another open description holds the lock. Lock files are
    /// never removed: unlinking one that another process is about to lock
    /// would let two sessions hold the same name.
    pub(super) fn lock_handle(&self, handle: &str) -> Result<Option<File>, String> {
        self.verify()?;
        let names = make_private_directory(&self.path.join(NAMES_DIRECTORY))?;
        let directory = checked_directory(&names)?;
        let name = c_string(Path::new(&format!("{handle}{LOCK_SUFFIX}")))?;
        // SAFETY: the directory fd and terminated basename remain valid for openat.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                FILE_MODE as libc::c_uint,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error().to_string());
        }
        // SAFETY: successful openat returns a new owned descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        private(&metadata, false)?;
        if !metadata.is_file() {
            return Err(UNSAFE_ENTRY.into());
        }
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error.to_string()),
        }
    }

    async fn connect(&self, host: &str) -> Result<Async<UnixStream>, String> {
        let manifest = self.read_manifest(host)?;
        let path = self.path.join(manifest.socket);
        let before = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        private(&before, false)?;
        if !before.file_type().is_socket() {
            return Err(UNSAFE_ENTRY.into());
        }
        let stream = Async::<UnixStream>::connect(&path)
            .await
            .map_err(|error| error.to_string())?;
        self.verify()?;
        let after = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
        if !same_file(&before, &after) {
            return Err(UNSAFE_ENTRY.into());
        }
        private(&after, false)?;
        check_peer_uid(stream.get_ref())?;
        Ok(stream)
    }
}

impl Endpoint {
    pub(super) fn bind(
        directory: Directory,
        incarnation: &str,
    ) -> Result<(Self, Async<UnixListener>), String> {
        let dir = directory.verify()?;
        let socket_name = format!("{incarnation}{SOCKET_SUFFIX}");
        let socket = directory.path.join(&socket_name);
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err("Peer Unix socket path exceeds the platform limit".into());
        }
        let listener = UnixListener::bind(&socket)
            .map_err(|error| format!("Cannot bind local peer socket: {error}"))?;
        let socket_identity = fs::symlink_metadata(&socket).map_err(|error| error.to_string())?;
        if !socket_identity.file_type().is_socket() || socket_identity.uid() != uid() {
            return Err(UNSAFE_ENTRY.into());
        }
        let socket_component = c_string(Path::new(&socket_name))?;
        // SAFETY: the directory fd and socket basename are live. NOFOLLOW ensures
        // a replacement symlink cannot cause chmod outside this private directory.
        let changed = unsafe {
            libc::fchmodat(
                dir.as_raw_fd(),
                socket_component.as_ptr(),
                libc::S_IRUSR | libc::S_IWUSR,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if changed != 0 {
            remove_owned(&socket, &socket_identity);
            return Err(io::Error::last_os_error().to_string());
        }
        let current = fs::symlink_metadata(&socket).map_err(|error| error.to_string())?;
        if !same_file(&socket_identity, &current) {
            return Err(UNSAFE_ENTRY.into());
        }
        private(&current, false)?;
        let manifest = directory
            .path
            .join(format!("{incarnation}{MANIFEST_SUFFIX}"));
        let temporary = directory.path.join(format!("{incarnation}.tmp"));
        let published = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(FILE_MODE)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)
                .map_err(|error| error.to_string())?;
            let identity = file.metadata().map_err(|error| error.to_string())?;
            private(&identity, false)?;
            let bytes = serde_json::to_vec(&Manifest {
                version: PROTOCOL_VERSION,
                incarnation: incarnation.into(),
                socket: socket_name,
            })
            .map_err(|error| error.to_string())?;
            let published = file
                .write_all(&bytes)
                .and_then(|()| fs::hard_link(&temporary, &manifest));
            remove_owned(&temporary, &identity);
            published.map_err(|error| error.to_string())?;
            Ok::<_, String>(identity)
        })();
        let manifest_identity = match published {
            Ok(identity) => identity,
            Err(error) => {
                remove_owned(&socket, &socket_identity);
                return Err(error);
            }
        };
        let endpoint = Self {
            directory,
            socket,
            socket_identity,
            manifest,
            manifest_identity,
        };
        let listener = Async::new(listener).map_err(|error| error.to_string())?;
        Ok((endpoint, listener))
    }

    pub(super) fn directory(&self) -> &Directory {
        &self.directory
    }
}

fn remove_owned(path: &Path, identity: &Metadata) {
    if fs::symlink_metadata(path).is_ok_and(|current| {
        same_file(&current, identity) && current.uid() == uid() && !current.file_type().is_symlink()
    }) {
        let _ = fs::remove_file(path);
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        if self.directory.verify().is_ok() {
            remove_owned(&self.manifest, &self.manifest_identity);
            remove_owned(&self.socket, &self.socket_identity);
        }
    }
}

#[cfg(target_os = "linux")]
fn check_peer_uid(stream: &UnixStream) -> Result<(), String> {
    let mut credential = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: both output buffers match the supplied lengths and remain live.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credential as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    if length as usize != size_of::<libc::ucred>() || credential.uid != uid() {
        return Err("Local peer socket belongs to another OS user".into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn check_peer_uid(stream: &UnixStream) -> Result<(), String> {
    let mut peer_uid = 0;
    let mut peer_gid = 0;
    // SAFETY: getpeereid receives valid output pointers and a live socket fd.
    let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut peer_uid, &mut peer_gid) };
    if result != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    if peer_uid != uid() {
        return Err("Local peer socket belongs to another OS user".into());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn check_peer_uid(_stream: &UnixStream) -> Result<(), String> {
    Err("Local peer credentials are unsupported on this Unix platform".into())
}

async fn read_frame<T: DeserializeOwned>(stream: &mut Async<UnixStream>) -> Result<T, String> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .await
        .map_err(|error| error.to_string())?;
    read_body(stream, length).await
}

/// The response to a request, or `None` when the peer hung up without
/// writing a byte, as a peer does with a request it cannot read.
async fn read_response(stream: &mut Async<UnixStream>) -> Result<Option<Response>, String> {
    let mut length = [0_u8; 4];
    let (first, rest) = length.split_at_mut(1);
    if stream
        .read(first)
        .await
        .map_err(|error| error.to_string())?
        == 0
    {
        return Ok(None);
    }
    stream
        .read_exact(rest)
        .await
        .map_err(|error| error.to_string())?;
    read_body(stream, length).await.map(Some)
}

/// The rest of a frame whose length prefix was `length`.
async fn read_body<T: DeserializeOwned>(
    stream: &mut Async<UnixStream>,
    length: [u8; 4],
) -> Result<T, String> {
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err("Peer JSON frame exceeds the 64 KiB limit".into());
    }
    let mut bytes = vec![0; length];
    stream
        .read_exact(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| format!("Invalid peer JSON frame: {error}"))
}

async fn write_frame<T: Serialize>(
    stream: &mut Async<UnixStream>,
    value: &T,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err("Peer JSON frame exceeds the 64 KiB limit".into());
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    stream.flush().await.map_err(|error| error.to_string())
}

async fn timeout<T>(
    duration: Duration,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    future::race(operation, async {
        Timer::after(duration).await;
        Err(TIMEOUT.into())
    })
    .await
}

struct ConnectionPermit(Arc<AtomicUsize>);

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub(super) fn listen(host: Weak<HostInner>, listener: Async<UnixListener>) -> smol::Task<()> {
    smol::spawn(async move {
        let active = Arc::new(AtomicUsize::new(0));
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            if active
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < MAX_CONNECTIONS).then_some(count + 1)
                })
                .is_err()
            {
                continue;
            }
            let permit = ConnectionPermit(active.clone());
            let host = host.clone();
            smol::spawn(async move {
                let _permit = permit;
                let _ = timeout(IO_TIMEOUT, async {
                    check_peer_uid(stream.get_ref())?;
                    let request = read_frame(&mut stream).await?;
                    let response = host.upgrade().ok_or("Peer host closed")?.handle(request);
                    write_frame(&mut stream, &response).await
                })
                .await;
            })
            .detach();
        }
    })
}

pub(super) async fn send(session: &PeerSession, delivery: Delivery, epoch: u64) -> SendReceipt {
    let automation = delivery.sender.automation.is_some();
    deliver(session.0.host.endpoint.directory(), delivery, || {
        let state = lock(&session.0.state);
        state.ensure_open()?;
        if state.epoch != epoch
            || (state.descriptor.blocked && !automation)
            || state.descriptor.mode.is_read_only()
        {
            return Err("Peer send invalidated by a session policy or workspace change".into());
        }
        Ok(())
    })
    .await
}

/// Delivers to the host `delivery.target` names. `before_write` runs once
/// connected, the last moment a sender can still withdraw the message.
pub(super) async fn deliver(
    directory: &Directory,
    delivery: Delivery,
    before_write: impl FnOnce() -> Result<(), String>,
) -> SendReceipt {
    let message_id = delivery.message_id.clone();
    let marked = delivery.sender.automation.is_some();
    let route = match Route::parse(&delivery.target) {
        Ok(route) => route,
        Err(reason) => return SendReceipt::new(STATUS_REFUSED, &message_id, Some(&reason)),
    };
    let request = Request::Send {
        version: PROTOCOL_VERSION,
        delivery: Box::new(delivery),
    };
    if !serde_json::to_vec(&request).is_ok_and(|bytes| bytes.len() <= MAX_FRAME_BYTES) {
        return SendReceipt::new(
            STATUS_REFUSED,
            &message_id,
            Some("Peer JSON frame exceeds the 64 KiB limit"),
        );
    }
    let mut written = false;
    let result = timeout(IO_TIMEOUT, async {
        let mut stream = directory.connect(&route.host).await?;
        before_write()?;
        // Any failure after the first write may have followed receiver admission.
        written = true;
        write_frame(&mut stream, &request).await?;
        match read_response(&mut stream).await? {
            Some(Response::Receipt { receipt })
                if receipt.message_id == message_id
                    && matches!(
                        receipt.status.as_str(),
                        STATUS_QUEUED | STATUS_HELD | STATUS_REFUSED | STATUS_RATE_LIMITED
                    ) =>
            {
                Ok(receipt)
            }
            Some(Response::Error { reason }) => {
                Ok(SendReceipt::new(STATUS_REFUSED, &message_id, Some(&reason)))
            }
            // An older peer cannot read the automation marker, so it hangs up.
            None if marked => Ok(SendReceipt::new(
                STATUS_REFUSED,
                &message_id,
                Some(OLDER_RECIPIENT),
            )),
            None => Err(HUNG_UP.into()),
            _ => Err("Invalid peer send response".into()),
        }
    })
    .await;
    result.unwrap_or_else(|reason| {
        SendReceipt::new(
            if written {
                STATUS_UNKNOWN
            } else {
                STATUS_UNAVAILABLE
            },
            &message_id,
            Some(&reason),
        )
    })
}

pub(super) async fn discover(directory: &Directory) -> Result<Vec<PeerInfo>, String> {
    timeout(DISCOVERY_TIMEOUT, async {
        directory.verify()?;
        let mut hosts = Vec::new();
        for (index, entry) in fs::read_dir(&directory.path)
            .map_err(|error| error.to_string())?
            .enumerate()
        {
            if index >= MAX_DIRECTORY_ENTRIES {
                return Err(PARTIAL.into());
            }
            let entry = entry.map_err(|error| error.to_string())?;
            let name = entry.file_name();
            let Some(host) = name
                .to_str()
                .and_then(|name| name.strip_suffix(MANIFEST_SUFFIX))
                .filter(|host| valid_token(host))
            else {
                continue;
            };
            if hosts.len() == MAX_HOSTS {
                return Err(PARTIAL.into());
            }
            hosts.push(host.to_owned());
        }
        hosts.sort();
        let mut peers = Vec::new();
        for host in hosts {
            let response = timeout(IO_TIMEOUT, async {
                let mut stream = directory.connect(&host).await?;
                write_frame(
                    &mut stream,
                    &Request::List {
                        version: PROTOCOL_VERSION,
                        host: host.clone(),
                    },
                )
                .await?;
                read_frame::<Response>(&mut stream).await
            })
            .await;
            match response {
                Ok(Response::Peers {
                    version,
                    peers: found,
                }) if version == PROTOCOL_VERSION => {
                    if found.len() > MAX_SESSIONS || peers.len() + found.len() > MAX_PEERS {
                        return Err(PARTIAL.into());
                    }
                    for peer in found {
                        let route = Route::parse(&peer.target)?;
                        if route.host != host
                            || route.session != peer.session_id
                            || peer.name.len() > MAX_LABEL_BYTES
                            || peer
                                .handle
                                .as_deref()
                                .is_some_and(|handle| !valid_handle(handle))
                            || peer.cwd.as_os_str().len() > MAX_PATH_BYTES
                            || validate_patterns(&peer.topics).is_err()
                            || !valid_memberships(&peer.groups)
                        {
                            return Err("Invalid discovered peer metadata".into());
                        }
                        peers.push(peer);
                    }
                }
                // Stale manifests are expected after a crash. They are never removed
                // by discovery, which cannot prove ownership of a live replacement.
                Err(reason) if reason != TIMEOUT => {
                    tracing::debug!(reason, "Peer discovery skipped unavailable host")
                }
                _ => return Err(PARTIAL.into()),
            }
        }
        Ok(peers)
    })
    .await
    .map_err(|error| {
        if error == TIMEOUT {
            PARTIAL.into()
        } else {
            error
        }
    })
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::ops::Not;
    use std::os::unix::fs::{PermissionsExt, chown, symlink};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    use async_io::Async;
    use caudra_config::InboundPolicy;
    use caudra_providers::PeerAudience;
    use caudra_storage::id::CaudraId;
    use caudra_storage::sessions::PermissionMode;
    use futures_lite::{AsyncReadExt, AsyncWriteExt, future};
    use serde::{Deserialize, Serialize};
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        DIRECTORY_MODE, Directory, Endpoint, FILE_MODE, MAX_DIRECTORY_ENTRIES, MAX_FRAME_BYTES,
        OLDER_RECIPIENT, PARTIAL, PERMISSION_MASK, Request, Response, STATUS_HELD, STATUS_QUEUED,
        STATUS_RATE_LIMITED, STATUS_REFUSED, STATUS_UNKNOWN, SendReceipt, UNSAFE_ENTRY,
        check_peer_uid, discover, make_private_directory, read_frame, uid, write_frame,
    };
    use crate::AgentMode;
    use crate::peers::tests::{descriptor, directory};
    use crate::peers::{
        MESSAGE_WORDS, PeerDescriptor, PeerHost, PeerSession, Route, SendFailureKind, SendOrigin,
        WireMode, lock, token, valid_name,
    };

    const UNKNOWN: &str = "unknown";
    const REFUSED: &str = "refused";
    const AUTOMATION: &str = "nightly-digest";
    const TEXT: &str = "The nightly build failed";
    const REQUEST_ID: &str = "automation:fire-1:0";

    /// A send request as a peer from before the automation marker reads it.
    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
    enum OlderRequest {
        Send {
            version: u32,
            delivery: OlderDelivery,
        },
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OlderDelivery {
        message_id: String,
        issued_ms: u64,
        target: String,
        sender: OlderSender,
        text: String,
        reply_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_sender: Option<String>,
        #[serde(default, skip_serializing_if = "PeerAudience::is_direct")]
        audience: PeerAudience,
    }

    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct OlderSender {
        route: Route,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        handle: Option<String>,
        canonical_cwd: Option<PathBuf>,
        mode: WireMode,
        permission_mode: PermissionMode,
        #[serde(default, skip_serializing_if = "Not::not")]
        external: bool,
    }

    /// How a recipient answers one send.
    enum Answer {
        Status(&'static str),
        /// A receipt for some other message.
        Mismatched,
        /// What a peer from before the automation marker answers, which
        /// reads ordinary traffic byte for byte.
        Older,
    }

    fn open(directory: &TempDir) -> Directory {
        Directory::open(directory.path().to_owned()).unwrap()
    }

    #[test]
    fn socket_credentials_confirm_same_uid() {
        let (first, second) = UnixStream::pair().unwrap();
        check_peer_uid(&first).unwrap();
        check_peer_uid(&second).unwrap();
    }

    #[test_case(0, b""; "empty_frame")]
    #[test_case(MAX_FRAME_BYTES as u32 + 1, b""; "oversized_frame")]
    #[test_case(1, b"x"; "malformed_json")]
    #[test_case(8, b"{}"; "incomplete_frame")]
    fn invalid_frames_fail_without_allocating_unbounded_buffers(length: u32, payload: &[u8]) {
        smol::block_on(async {
            let (first, second) = UnixStream::pair().unwrap();
            let mut first = Async::new(first).unwrap();
            let mut second = Async::new(second).unwrap();
            first.write_all(&length.to_be_bytes()).await.unwrap();
            first.write_all(payload).await.unwrap();
            drop(first);
            assert!(read_frame::<Request>(&mut second).await.is_err());
        });
    }

    #[test_case(DIRECTORY_MODE, true; "private")]
    #[test_case(0o1700, true; "private_sticky")]
    #[test_case(0o777, false; "shared_without_sticky")]
    #[test_case(0o2777, false; "shared_setgid_is_not_sticky")]
    #[test_case(0o1777, uid() == 0; "shared_sticky_requires_root")]
    fn directory_ancestry_respects_sticky_permissions(mode: u32, allowed: bool) {
        let parent = directory();
        chown(parent.path(), None, Some(unsafe { libc::getegid() })).unwrap();
        fs::set_permissions(parent.path(), Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            fs::metadata(parent.path()).unwrap().permissions().mode() & PERMISSION_MASK,
            mode
        );
        let child = parent.path().join("private");

        let result = make_private_directory(&child);
        if allowed {
            assert_eq!(result.unwrap(), child);
            assert_eq!(
                fs::metadata(&child).unwrap().permissions().mode() & PERMISSION_MASK,
                DIRECTORY_MODE
            );
        } else {
            assert_eq!(result.unwrap_err(), UNSAFE_ENTRY);
            assert!(!child.exists());
        }
        assert_eq!(
            fs::metadata(parent.path()).unwrap().permissions().mode() & PERMISSION_MASK,
            mode
        );
    }

    #[test]
    fn private_directory_modes_are_checked_without_repair() {
        let directory = directory();
        fs::set_permissions(directory.path(), Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            Directory::open(directory.path().to_owned())
                .err()
                .as_deref(),
            Some(UNSAFE_ENTRY)
        );
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(directory.path(), Permissions::from_mode(DIRECTORY_MODE)).unwrap();
        let link = directory.path().join("linked-directory");
        symlink(directory.path(), &link).unwrap();
        assert!(Directory::open(link).is_err());
    }

    #[test]
    fn symlinked_manifest_and_socket_are_rejected_and_not_removed() {
        smol::block_on(async {
            let directory = directory();
            let id = token().unwrap();
            let (endpoint, _listener) = Endpoint::bind(open(&directory), &id).unwrap();
            let socket = endpoint.socket.clone();
            let manifest = endpoint.manifest.clone();
            let saved = directory.path().join("saved-manifest");
            fs::rename(&manifest, &saved).unwrap();
            symlink(&saved, &manifest).unwrap();
            assert!(endpoint.directory.read_manifest(&id).is_err());
            fs::remove_file(&manifest).unwrap();
            fs::rename(&saved, &manifest).unwrap();
            fs::remove_file(&socket).unwrap();
            symlink(&manifest, &socket).unwrap();
            assert!(endpoint.directory.connect(&id).await.is_err());
            drop(endpoint);
            assert!(
                fs::symlink_metadata(&socket)
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        });
    }

    #[test]
    fn manifest_mode_is_checked_before_reading() {
        let directory = directory();
        let id = token().unwrap();
        let (endpoint, _listener) = Endpoint::bind(open(&directory), &id).unwrap();
        fs::set_permissions(&endpoint.manifest, Permissions::from_mode(0o644)).unwrap();
        assert!(endpoint.directory.read_manifest(&id).is_err());
        fs::set_permissions(&endpoint.manifest, Permissions::from_mode(FILE_MODE)).unwrap();
        assert!(endpoint.directory.read_manifest(&id).is_ok());
    }

    #[test]
    fn discovery_reports_registry_limit() {
        let directory = directory();
        let (endpoint, _listener) = Endpoint::bind(open(&directory), &token().unwrap()).unwrap();
        for index in 0..MAX_DIRECTORY_ENTRIES {
            fs::write(directory.path().join(format!("unrelated-{index}")), []).unwrap();
        }
        assert_eq!(
            smol::block_on(discover(&endpoint.directory)).unwrap_err(),
            PARTIAL
        );
    }

    #[test]
    fn direct_handshake_rejects_protocol_and_incarnation_mismatch() {
        smol::block_on(async {
            let directory = directory();
            let host =
                PeerHost::start_in(directory.path().to_owned(), Arc::new(AtomicUsize::new(0)))
                    .unwrap();
            let mut stream = host
                .0
                .endpoint
                .directory
                .connect(&host.0.incarnation)
                .await
                .unwrap();
            write_frame(
                &mut stream,
                &Request::List {
                    version: 0,
                    host: host.0.incarnation.clone(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_frame::<Response>(&mut stream).await.unwrap(),
                Response::Error { .. }
            ));
            let mut stream = host
                .0
                .endpoint
                .directory
                .connect(&host.0.incarnation)
                .await
                .unwrap();
            write_frame(
                &mut stream,
                &Request::List {
                    version: super::PROTOCOL_VERSION,
                    host: token().unwrap(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                read_frame::<Response>(&mut stream).await.unwrap(),
                Response::Error { .. }
            ));
        });
    }

    #[test]
    fn lost_response_is_unknown_and_named_retries_retain_identity() {
        smol::block_on(async {
            let directory = directory();
            let host =
                PeerHost::start_in(directory.path().to_owned(), Arc::new(AtomicUsize::new(0)))
                    .unwrap();
            let sender = host
                .register(PeerDescriptor {
                    session_id: CaudraId::generate(),
                    name: "sender".into(),
                    cwd: directory.path().to_owned(),
                    mode: AgentMode::Build,
                    permission_mode: PermissionMode::Ask,
                    inbound: InboundPolicy::Auto,
                    blocked: false,
                    busy: false,
                })
                .unwrap();
            let id = token().unwrap();
            let (_endpoint, listener) = Endpoint::bind(open(&directory), &id).unwrap();
            let route = Route {
                host: id,
                session: CaudraId::generate(),
                generation: token().unwrap(),
            };
            let target = lock(&sender.0.state).peer_name(&route.target()).unwrap();
            let (receipt, first_id) =
                future::zip(sender.send_named(&target, "text", None, "request"), async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    match read_frame::<Request>(&mut stream).await.unwrap() {
                        Request::Send { delivery, .. } => delivery.message_id,
                        _ => panic!("expected delivery"),
                    }
                })
                .await;
            assert_eq!(receipt.unwrap().status, UNKNOWN);
            assert!(valid_name(&first_id, MESSAGE_WORDS));
            let (receipt, retry_id) =
                future::zip(sender.send_named(&target, "text", None, "request"), async {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    match read_frame::<Request>(&mut stream).await.unwrap() {
                        Request::Send { delivery, .. } => delivery.message_id,
                        _ => panic!("expected retry"),
                    }
                })
                .await;
            assert_eq!(receipt.unwrap().status, UNKNOWN);
            assert_eq!(first_id, retry_id);
            let oversized = "\0".repeat(crate::peers::MAX_BODY_BYTES);
            assert_eq!(
                sender
                    .send_named(&target, &oversized, None, "oversized")
                    .await
                    .unwrap()
                    .status,
                REFUSED
            );
        });
    }

    /// A session, and a recipient it knows by a word target whose host the
    /// test answers for.
    fn fake_recipient(directory: &TempDir) -> (PeerSession, Endpoint, Async<UnixListener>, String) {
        let host =
            PeerHost::start_in(directory.path().to_owned(), Arc::new(AtomicUsize::new(0))).unwrap();
        let sender = host
            .register(descriptor(directory.path(), InboundPolicy::Auto))
            .unwrap();
        let id = token().unwrap();
        let (endpoint, listener) = Endpoint::bind(open(directory), &id).unwrap();
        let route = Route {
            host: id,
            session: CaudraId::generate(),
            generation: token().unwrap(),
        };
        let target = lock(&sender.0.state).peer_name(&route.target()).unwrap();
        (sender, endpoint, listener, target)
    }

    /// Answers the next send on `listener` as `answer` says.
    async fn answer(listener: &Async<UnixListener>, answer: Answer) {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut length = [0_u8; 4];
        stream.read_exact(&mut length).await.unwrap();
        let mut frame = vec![0; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut frame).await.unwrap();
        let Request::Send { delivery, .. } = serde_json::from_slice::<Request>(&frame).unwrap()
        else {
            panic!("expected a delivery");
        };
        let (status, message_id) = match answer {
            Answer::Status(status) => (status, delivery.message_id),
            Answer::Mismatched => (STATUS_QUEUED, token().unwrap()),
            Answer::Older => match serde_json::from_slice::<OlderRequest>(&frame) {
                Ok(older) => {
                    assert_eq!(serde_json::to_vec(&older).unwrap(), frame);
                    (STATUS_QUEUED, delivery.message_id)
                }
                Err(_) => return,
            },
        };
        let receipt = SendReceipt::new(status, &message_id, None);
        write_frame(&mut stream, &Response::Receipt { receipt })
            .await
            .unwrap();
    }

    #[test_case(SendOrigin::Session, Ok(STATUS_QUEUED); "ordinary_traffic")]
    #[test_case(SendOrigin::Automation(AUTOMATION.into()), Err(OLDER_RECIPIENT); "marked_traffic")]
    fn older_peers_read_ordinary_traffic_unchanged_and_refuse_the_marker(
        origin: SendOrigin,
        expected: Result<&str, &str>,
    ) {
        smol::block_on(async {
            let directory = directory();
            let (sender, _endpoint, listener, target) = fake_recipient(&directory);
            let (sent, ()) = future::zip(
                sender.send_from(&origin, &target, TEXT, None, REQUEST_ID),
                answer(&listener, Answer::Older),
            )
            .await;
            assert_eq!(
                sent.map(|receipt| receipt.status)
                    .map_err(|failure| (failure.kind, failure.reason)),
                expected
                    .map(str::to_owned)
                    .map_err(|reason| (SendFailureKind::Refused, reason.to_owned()))
            );
        });
    }

    #[test_case(Answer::Status(STATUS_QUEUED), Ok(STATUS_QUEUED); "queued")]
    #[test_case(Answer::Status(STATUS_HELD), Ok(STATUS_HELD); "held")]
    #[test_case(Answer::Mismatched, Ok(STATUS_UNKNOWN); "unknown")]
    #[test_case(Answer::Status(STATUS_REFUSED), Err(SendFailureKind::Refused); "refused")]
    #[test_case(Answer::Status(STATUS_RATE_LIMITED), Err(SendFailureKind::RateLimited); "rate_limited")]
    fn automation_sends_report_what_the_recipient_answered(
        given: Answer,
        expected: Result<&str, SendFailureKind>,
    ) {
        smol::block_on(async {
            let directory = directory();
            let (sender, _endpoint, listener, target) = fake_recipient(&directory);
            let origin = SendOrigin::Automation(AUTOMATION.into());
            let (sent, ()) = future::zip(
                sender.send_from(&origin, &target, TEXT, None, REQUEST_ID),
                answer(&listener, given),
            )
            .await;
            assert_eq!(
                sent.map(|receipt| receipt.status)
                    .map_err(|failure| failure.kind),
                expected.map(str::to_owned)
            );
        });
    }
}
