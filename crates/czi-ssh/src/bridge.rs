//! Validated private Unix sockets for interactive SFTP bridges.

#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::{Arc, Mutex, MutexGuard};
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use crate::command::{PRIVATE_CONTROL_BASE, PRIVATE_SOCKET_NAME, SOCKET_PATH_LIMIT};
#[cfg(unix)]
use crate::{ControlPath, OpenSshConfigError, SftpError};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

#[cfg(unix)]
const CLIENT_MAGIC: [u8; 4] = *b"CZB1";
#[cfg(unix)]
const SERVER_MAGIC: [u8; 4] = *b"CZB2";
#[cfg(unix)]
const NONCE_LENGTH: usize = 16;
#[cfg(unix)]
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[cfg(unix)]
struct CancellationState {
    cancelled: bool,
    next_token: u64,
    streams: std::collections::BTreeMap<u64, UnixStream>,
}

#[cfg(unix)]
impl Default for CancellationState {
    fn default() -> Self {
        Self {
            cancelled: false,
            next_token: 1,
            streams: std::collections::BTreeMap::new(),
        }
    }
}

/// An out-of-band closer for the active interactive bridge stream.
///
/// The GUI uses this before joining its worker so a Terminal authentication prompt or blocked
/// SFTP read cannot keep the application alive indefinitely.
#[derive(Clone, Default)]
pub struct BridgeCancellation {
    #[cfg(unix)]
    state: Arc<Mutex<CancellationState>>,
}

/// A token-scoped active bridge stream registration.
///
/// The registration is removed on drop. It is intentionally held by the transport rather than
/// its caller so old sessions cannot unregister newer streams.
#[cfg(unix)]
#[must_use]
pub(crate) struct BridgeRegistration {
    cancellation: BridgeCancellation,
    token: u64,
}

impl BridgeCancellation {
    /// Latch cancellation and close every registered bridge stream.
    pub fn cancel(&self) {
        #[cfg(unix)]
        {
            let streams = {
                let mut state = self.lock_state();
                state.cancelled = true;
                std::mem::take(&mut state.streams)
            };
            for stream in streams.into_values() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
    }

    #[cfg(unix)]
    pub(crate) fn register(&self, stream: &UnixStream) -> Result<BridgeRegistration, SftpError> {
        let stream = stream
            .try_clone()
            .map_err(|source| SftpError::io("clone bridge cancellation stream", source))?;
        let mut state = self.lock_state();
        if state.cancelled {
            drop(state);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return Err(SftpError::io(
                "register bridge cancellation stream",
                io::Error::new(
                    io::ErrorKind::Interrupted,
                    "interactive bridge cancellation is already active",
                ),
            ));
        }
        let token = state.next_token;
        state.next_token = state.next_token.checked_add(1).ok_or_else(|| {
            SftpError::io(
                "register bridge cancellation stream",
                io::Error::other("bridge registration token exhausted"),
            )
        })?;
        let previous = state.streams.insert(token, stream);
        debug_assert!(previous.is_none());
        Ok(BridgeRegistration {
            cancellation: self.clone(),
            token,
        })
    }

    #[cfg(unix)]
    fn lock_state(&self) -> MutexGuard<'_, CancellationState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(unix)]
impl Drop for BridgeRegistration {
    fn drop(&mut self) {
        let _ = self.cancellation.lock_state().streams.remove(&self.token);
    }
}

#[cfg(unix)]
#[derive(Clone, Copy)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl SocketIdentity {
    fn from_metadata(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// A one-client listener for an application-private interactive SFTP bridge.
#[cfg(unix)]
pub struct BridgeListener {
    listener: UnixListener,
    socket_path: std::path::PathBuf,
    socket_identity: SocketIdentity,
}

#[cfg(unix)]
impl BridgeListener {
    /// Bind the validated private bridge socket without overwriting an existing path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path is not an expected private socket path, has unsafe
    /// permissions, already exists, or cannot be bound.
    pub fn bind(socket_path: impl AsRef<Path>) -> Result<Self, SftpError> {
        let socket_path = socket_path.as_ref();
        validate_bridge_socket_parent(socket_path)?;
        match fs::symlink_metadata(socket_path) {
            Ok(_) => return Err(OpenSshConfigError::BridgeSocketAlreadyExists.into()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(SftpError::io("inspect bridge socket", source)),
        }
        let listener = UnixListener::bind(socket_path)
            .map_err(|source| SftpError::io("bind bridge socket", source))?;
        if let Err(source) = fs::set_permissions(socket_path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(socket_path);
            return Err(SftpError::io("set bridge socket permissions", source));
        }
        let metadata = fs::symlink_metadata(socket_path)
            .map_err(|source| SftpError::io("inspect bound bridge socket", source))?;
        let socket_identity = SocketIdentity::from_metadata(&metadata);
        listener
            .set_nonblocking(true)
            .map_err(|source| SftpError::io("configure bridge socket", source))?;
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            socket_identity,
        })
    }

    /// Accept the one viewer connection and remove the listener path immediately.
    ///
    /// The accepted stream remains usable after the path is removed, while a second bridge cannot
    /// accidentally connect to this helper. If the viewer removes or replaces its private socket
    /// path before connecting, returns `None` so the idle helper exits.
    ///
    /// # Errors
    ///
    /// Returns an error if accepting or inspecting the bridge socket fails.
    pub fn accept(self) -> Result<Option<UnixStream>, SftpError> {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .map_err(|source| SftpError::io("configure bridge stream", source))?;
                    return Ok(Some(stream));
                }
                Err(source) if source.kind() == io::ErrorKind::Interrupted => {}
                Err(source) if source.kind() == io::ErrorKind::WouldBlock => {
                    if !self.socket_path_is_current()? {
                        return Ok(None);
                    }
                    thread::sleep(ACCEPT_POLL_INTERVAL);
                }
                Err(source) => return Err(SftpError::io("accept bridge connection", source)),
            }
        }
    }

    fn socket_path_is_current(&self) -> Result<bool, SftpError> {
        match fs::symlink_metadata(&self.socket_path) {
            Ok(metadata) => {
                let identity = SocketIdentity::from_metadata(&metadata);
                Ok(metadata.file_type().is_socket()
                    && identity.device == self.socket_identity.device
                    && identity.inode == self.socket_identity.inode)
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(SftpError::io("inspect bridge socket", source)),
        }
    }
}

#[cfg(unix)]
impl Drop for BridgeListener {
    fn drop(&mut self) {
        if self.socket_path_is_current().unwrap_or(false) {
            let _ = fs::remove_file(&self.socket_path);
        }
    }
}

/// Connect to a validated bridge socket, returning `None` when no helper is waiting.
///
/// # Errors
///
/// Returns an error for malformed, unsafe, or inaccessible sockets. A missing socket means no
/// interactive bridge is available and callers may use direct batch SFTP instead.
#[cfg(unix)]
pub fn connect_bridge_socket(control_path: &ControlPath) -> Result<Option<UnixStream>, SftpError> {
    let socket_path = control_path.socket_path();
    validate_bridge_socket_parent(socket_path)?;
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(SftpError::io("inspect bridge socket", source)),
    };
    if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(OpenSshConfigError::BridgeSocketUnsafe.into());
    }
    UnixStream::connect(socket_path)
        .map(Some)
        .map_err(|source| SftpError::io("connect interactive SFTP bridge", source))
}

/// Authenticate a viewer bridge connection before binary SFTP begins.
///
/// The nonce makes the response specific to this socket connection, while the profile prevents a
/// bridge authenticated for one OpenSSH profile from being used for another.
///
/// # Errors
///
/// Returns an error when the bridge response is malformed, does not echo the nonce, or rejects
/// the requested profile.
#[cfg(unix)]
pub fn authenticate_bridge_client(
    stream: &mut UnixStream,
    profile: &crate::SshProfile,
) -> Result<(), SftpError> {
    let nonce = bridge_nonce().map_err(|source| SftpError::io("create bridge nonce", source))?;
    let profile = profile.as_str().as_bytes();
    let length = u16::try_from(profile.len()).map_err(|_| {
        SftpError::io(
            "authenticate interactive SFTP bridge",
            io::Error::new(io::ErrorKind::InvalidInput, "bridge profile is too long"),
        )
    })?;
    stream
        .write_all(&CLIENT_MAGIC)
        .and_then(|()| stream.write_all(&nonce))
        .and_then(|()| stream.write_all(&length.to_be_bytes()))
        .and_then(|()| stream.write_all(profile))
        .and_then(|()| stream.flush())
        .map_err(|source| SftpError::io("authenticate interactive SFTP bridge", source))?;
    let mut response = [0_u8; 4 + NONCE_LENGTH + 1];
    stream
        .read_exact(&mut response)
        .map_err(|source| SftpError::io("authenticate interactive SFTP bridge", source))?;
    if response[..4] != SERVER_MAGIC || response[4..4 + NONCE_LENGTH] != nonce {
        return Err(SftpError::io(
            "authenticate interactive SFTP bridge",
            io::Error::new(io::ErrorKind::InvalidData, "invalid bridge response"),
        ));
    }
    if response[4 + NONCE_LENGTH] != 0 {
        return Err(SftpError::io(
            "authenticate interactive SFTP bridge",
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bridge profile did not match",
            ),
        ));
    }
    Ok(())
}

/// Authenticate a Terminal bridge connection before proxying SFTP bytes.
///
/// # Errors
///
/// Returns an error for malformed framing or I/O. A profile mismatch is acknowledged to the
/// client and then returned as a permission error without starting OpenSSH.
#[cfg(unix)]
pub fn authenticate_bridge_server(
    stream: &mut UnixStream,
    profile: &crate::SshProfile,
) -> Result<(), SftpError> {
    let mut header = [0_u8; 4 + NONCE_LENGTH + 2];
    stream
        .read_exact(&mut header)
        .map_err(|source| SftpError::io("read interactive SFTP bridge handshake", source))?;
    if header[..4] != CLIENT_MAGIC {
        return Err(SftpError::io(
            "read interactive SFTP bridge handshake",
            io::Error::new(io::ErrorKind::InvalidData, "invalid bridge request"),
        ));
    }
    let length = usize::from(u16::from_be_bytes([
        header[4 + NONCE_LENGTH],
        header[4 + NONCE_LENGTH + 1],
    ]));
    let mut requested = vec![0_u8; length];
    stream
        .read_exact(&mut requested)
        .map_err(|source| SftpError::io("read interactive SFTP bridge profile", source))?;
    let matches = requested == profile.as_str().as_bytes();
    let mut response = [0_u8; 4 + NONCE_LENGTH + 1];
    response[..4].copy_from_slice(&SERVER_MAGIC);
    response[4..4 + NONCE_LENGTH].copy_from_slice(&header[4..4 + NONCE_LENGTH]);
    response[4 + NONCE_LENGTH] = u8::from(!matches);
    stream
        .write_all(&response)
        .and_then(|()| stream.flush())
        .map_err(|source| SftpError::io("write interactive SFTP bridge handshake", source))?;
    if matches {
        Ok(())
    } else {
        Err(SftpError::io(
            "authenticate interactive SFTP bridge",
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "bridge profile did not match",
            ),
        ))
    }
}

#[cfg(unix)]
fn validate_bridge_socket_parent(socket_path: &Path) -> Result<(), SftpError> {
    if !socket_path.is_absolute()
        || socket_path.as_os_str().len() > SOCKET_PATH_LIMIT
        || socket_path.file_name() != Some(OsStr::new(PRIVATE_SOCKET_NAME))
    {
        return Err(OpenSshConfigError::BridgeSocketPathInvalid.into());
    }
    let Some(directory) = socket_path.parent() else {
        return Err(OpenSshConfigError::BridgeSocketPathInvalid.into());
    };
    if directory.parent() != Some(Path::new(PRIVATE_CONTROL_BASE))
        || !directory
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| name.starts_with("cz-"))
    {
        return Err(OpenSshConfigError::BridgeSocketPathInvalid.into());
    }
    let metadata = fs::symlink_metadata(directory)
        .map_err(|source| SftpError::io("inspect bridge socket directory", source))?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || metadata.permissions().mode() & 0o777 != 0o700
    {
        return Err(OpenSshConfigError::BridgeSocketUnsafe.into());
    }
    Ok(())
}

#[cfg(unix)]
fn bridge_nonce() -> io::Result<[u8; NONCE_LENGTH]> {
    let mut nonce = [0_u8; NONCE_LENGTH];
    fs::File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    Ok(nonce)
}
