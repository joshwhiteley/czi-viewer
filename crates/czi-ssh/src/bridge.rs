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
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use crate::command::{PRIVATE_CONTROL_BASE, PRIVATE_SOCKET_NAME, SOCKET_PATH_LIMIT};
#[cfg(unix)]
use crate::{ControlPath, OpenSshConfigError, SftpError};

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

/// An out-of-band closer for the active interactive bridge stream.
///
/// The GUI uses this before joining its worker so a Terminal authentication prompt or blocked
/// SFTP read cannot keep the application alive indefinitely.
#[derive(Clone, Default)]
pub struct BridgeCancellation {
    #[cfg(unix)]
    stream: Arc<Mutex<Option<UnixStream>>>,
}

impl BridgeCancellation {
    /// Close the active bridge stream, if any.
    pub fn cancel(&self) {
        #[cfg(unix)]
        if let Ok(mut stream) = self.stream.lock()
            && let Some(stream) = stream.take()
        {
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    #[cfg(unix)]
    pub(crate) fn register(&self, stream: &UnixStream) -> Result<(), SftpError> {
        let stream = stream
            .try_clone()
            .map_err(|source| SftpError::io("clone bridge cancellation stream", source))?;
        let previous = self
            .stream
            .lock()
            .map_err(|_| {
                SftpError::io(
                    "lock bridge cancellation stream",
                    io::Error::other("poisoned lock"),
                )
            })?
            .replace(stream);
        if let Some(previous) = previous {
            let _ = previous.shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    }

    pub(crate) fn clear(&self) {
        #[cfg(unix)]
        if let Ok(mut stream) = self.stream.lock() {
            let _ = stream.take();
        }
    }
}

/// A one-client listener for an application-private interactive SFTP bridge.
#[cfg(unix)]
pub struct BridgeListener {
    listener: UnixListener,
    socket_path: std::path::PathBuf,
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
        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
        })
    }

    /// Accept the one viewer connection and remove the listener path immediately.
    ///
    /// The accepted stream remains usable after the path is removed, while a second bridge cannot
    /// accidentally connect to this helper.
    ///
    /// # Errors
    ///
    /// Returns an error if accepting the viewer stream fails.
    pub fn accept(self) -> Result<UnixStream, SftpError> {
        self.listener
            .accept()
            .map(|(stream, _)| stream)
            .map_err(|source| SftpError::io("accept bridge connection", source))
    }
}

#[cfg(unix)]
impl Drop for BridgeListener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
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
