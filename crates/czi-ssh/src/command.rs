use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{OpenSshConfigError, SftpError, SshProfile};

/// The only OpenSSH executable used by production connections.
pub const OPENSSH_PATH: &str = "/usr/bin/ssh";

/// macOS has a 104-byte `sockaddr_un::sun_path`. OpenSSH creates its listener at a temporary
/// name by appending a suffix observed to be 17 bytes. Keep the configured name at 80 bytes,
/// leaving room for that suffix, a NUL terminator, and additional implementation variance.
pub(crate) const SOCKET_PATH_LIMIT: usize = 80;

#[cfg(unix)]
pub(crate) const PRIVATE_CONTROL_BASE: &str = "/tmp";
pub(crate) const PRIVATE_SOCKET_NAME: &str = "s";

/// An application-private directory and socket for interactive SFTP bridging.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPath {
    directory: PathBuf,
    socket_path: PathBuf,
}

impl ControlPath {
    /// Create a unique application-private socket directory below `/tmp` on Unix.
    ///
    /// A deliberately short base avoids macOS `sun_path` limits and ignores `TMPDIR`. The private
    /// child directory permissions are set to `0700` on Unix platforms. The returned socket has
    /// a conservative 80-byte maximum that reserves space for OpenSSH's temporary socket suffix.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or its permissions cannot be created.
    #[cfg(unix)]
    pub fn create_private() -> Result<Self, SftpError> {
        Self::create_private_in(PRIVATE_CONTROL_BASE)
    }

    /// Create a unique application-private socket directory below the system temporary directory.
    ///
    /// This fallback keeps the portable behavior for non-Unix platforms.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or its permissions cannot be created.
    #[cfg(not(unix))]
    pub fn create_private() -> Result<Self, SftpError> {
        Self::create_private_in(std::env::temp_dir())
    }

    /// Create a unique application-private socket directory below `base`.
    ///
    /// # Errors
    ///
    /// Returns an error when `base` cannot contain a private directory or no unique name is
    /// available.
    pub fn create_private_in(base: impl AsRef<Path>) -> Result<Self, SftpError> {
        let base = base.as_ref();
        fs::create_dir_all(base)
            .map_err(|source| SftpError::io("create control socket base", source))?;
        if !base.is_dir() {
            return Err(OpenSshConfigError::ControlDirectoryNotDirectory.into());
        }

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        for counter in 0..256_u16 {
            let directory = base.join(format!("cz-{:x}-{nanos:x}-{counter:x}", process::id()));
            match create_private_directory(&directory) {
                Ok(()) => {
                    set_private_permissions(&directory)?;
                    return Self::from_private_directory(directory);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(source) => {
                    return Err(SftpError::io("create private control directory", source));
                }
            }
        }
        Err(OpenSshConfigError::ControlDirectoryUnavailable.into())
    }

    /// Use `directory/s` as a control socket after setting the directory to `0700`.
    ///
    /// # Errors
    ///
    /// Returns an error when `directory` is not a directory, its permissions cannot be changed,
    /// or the socket path is too long.
    pub fn from_private_directory(directory: impl Into<PathBuf>) -> Result<Self, SftpError> {
        let directory = directory.into();
        if !directory.is_dir() {
            return Err(OpenSshConfigError::ControlDirectoryNotDirectory.into());
        }
        set_private_permissions(&directory)?;
        let socket_path = directory.join(PRIVATE_SOCKET_NAME);
        let length = socket_path.as_os_str().len();
        if length > SOCKET_PATH_LIMIT {
            return Err(OpenSshConfigError::SocketPathTooLong { length }.into());
        }
        Ok(Self {
            directory,
            socket_path,
        })
    }

    /// Return the private socket directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Return the exact private Unix-socket path used by the interactive SFTP bridge.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

#[cfg(unix)]
fn create_private_directory(directory: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(directory)
}

#[cfg(not(unix))]
fn create_private_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir(directory)
}

#[cfg(unix)]
fn set_private_permissions(directory: &Path) -> Result<(), SftpError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|source| SftpError::io("set private control directory permissions", source))
}

#[cfg(not(unix))]
fn set_private_permissions(_directory: &Path) -> Result<(), SftpError> {
    Ok(())
}

/// A bounded ASCII DNS name used only for OpenSSH host-key lookup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostKeyAlias(String);

impl HostKeyAlias {
    /// Validate an ASCII DNS name for `HostKeyAlias`.
    ///
    /// Names are limited to 253 bytes. Labels must be 1 to 63 bytes, contain only ASCII letters,
    /// digits, or hyphens, and begin and end with a letter or digit.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty, oversized, malformed, or non-ASCII DNS name.
    pub fn new(value: impl Into<String>) -> Result<Self, OpenSshConfigError> {
        let value = value.into();
        if value.is_empty() {
            return Err(OpenSshConfigError::HostKeyAliasEmpty);
        }
        if value.len() > 253 {
            return Err(OpenSshConfigError::HostKeyAliasTooLong {
                length: value.len(),
            });
        }
        if value.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }) {
            return Err(OpenSshConfigError::HostKeyAliasInvalidDnsName);
        }
        Ok(Self(value))
    }

    /// Return the validated DNS name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated loopback TCP endpoint for an SSH server reached through a local tunnel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopbackEndpoint {
    port: u16,
    host_key_alias: HostKeyAlias,
}

impl LoopbackEndpoint {
    /// Create a loopback endpoint while retaining host-key identity for the real SSH server.
    ///
    /// # Errors
    ///
    /// Returns an error when `port` is zero. `host_key_alias` has already passed the same
    /// DNS-name validation.
    pub fn new(port: u16, host_key_alias: HostKeyAlias) -> Result<Self, OpenSshConfigError> {
        if port == 0 {
            return Err(OpenSshConfigError::LoopbackPortZero);
        }
        Ok(Self {
            port,
            host_key_alias,
        })
    }

    /// Return the local TCP port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Return the host identity used for known-host verification.
    #[must_use]
    pub fn host_key_alias(&self) -> &HostKeyAlias {
        &self.host_key_alias
    }
}

/// OpenSSH settings used by SFTP and interactive bridge commands.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenSshConfig {
    control_path: Option<ControlPath>,
    loopback_endpoint: Option<LoopbackEndpoint>,
}

impl OpenSshConfig {
    /// Create a configuration with OpenSSH's normal host-key handling and no control socket.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach an application-private bridge socket to this configuration.
    #[must_use]
    pub fn with_control_path(mut self, control_path: ControlPath) -> Self {
        self.control_path = Some(control_path);
        self
    }

    /// Route SSH through a validated local TCP endpoint.
    #[must_use]
    pub fn with_loopback_endpoint(mut self, endpoint: LoopbackEndpoint) -> Self {
        self.loopback_endpoint = Some(endpoint);
        self
    }

    /// Return the configured private bridge socket, if any.
    #[must_use]
    pub fn control_path(&self) -> Option<&ControlPath> {
        self.control_path.as_ref()
    }

    /// Return the configured loopback endpoint, if any.
    #[must_use]
    pub fn loopback_endpoint(&self) -> Option<&LoopbackEndpoint> {
        self.loopback_endpoint.as_ref()
    }

    /// Build the production `/usr/bin/ssh` argument vector for an SFTP subsystem child.
    ///
    /// This always uses `BatchMode=yes`, strict known-host verification, bounded connection
    /// settings, disables agent and forwarding features, requests no TTY, and passes only the
    /// destination and `sftp` subsystem after the SSH options. It deliberately does not use
    /// `ControlMaster`; remote paths belong in SFTP packets and cannot enter this vector.
    #[must_use]
    pub fn sftp_argv(&self, profile: &SshProfile) -> Vec<OsString> {
        let mut argv = common_argv(true, self.loopback_endpoint.as_ref());
        argv.push(OsString::from("-T"));
        argv.push(OsString::from("-s"));
        argv.push(OsString::from(profile.as_str()));
        argv.push(OsString::from("sftp"));
        argv
    }

    /// Build the interactive SFTP-subsystem arguments for the embedded PTY transport.
    ///
    /// SSH stdin and stdout remain reserved for binary SFTP packets. Host-key, password, and 2FA
    /// interaction uses the child process's local controlling terminal through stderr and
    /// `/dev/tty`. The `-T` option disables only a *remote* terminal; it does not affect that local
    /// PTY. This never configures `ControlMaster` or a control path. A configured loopback
    /// endpoint overrides only network routing while preserving the destination profile's user
    /// and other normal OpenSSH settings.
    #[must_use]
    pub fn embedded_sftp_argv(&self, profile: &SshProfile) -> Vec<OsString> {
        let mut argv = common_argv(false, self.loopback_endpoint.as_ref());
        argv.push(OsString::from("-T"));
        argv.push(OsString::from("-s"));
        argv.push(OsString::from(profile.as_str()));
        argv.push(OsString::from("sftp"));
        argv
    }

    /// Build an interactive direct OpenSSH SFTP-subsystem argument vector for a Terminal bridge.
    ///
    /// This remains the visible-Terminal fallback and uses the same security arguments as the
    /// embedded PTY transport.
    #[must_use]
    pub fn interactive_sftp_argv(&self, profile: &SshProfile) -> Vec<OsString> {
        self.embedded_sftp_argv(profile)
    }

    /// Build a safely shell-quoted command that starts the same executable's interactive SFTP
    /// bridge in a visible Terminal.
    ///
    /// The command passes only a validated profile and this configuration's private socket path;
    /// remote paths cannot enter it.
    ///
    /// # Errors
    ///
    /// Returns an error when no private socket is configured or an argument is not UTF-8.
    pub fn terminal_bridge_command(
        &self,
        executable: &Path,
        bridge_mode: &str,
        profile: &SshProfile,
    ) -> Result<String, OpenSshConfigError> {
        let control_path = self
            .control_path
            .as_ref()
            .ok_or(OpenSshConfigError::MissingControlPath)?;
        [
            executable.as_os_str(),
            OsStr::new(bridge_mode),
            OsStr::new(profile.as_str()),
            control_path.socket_path().as_os_str(),
        ]
        .iter()
        .map(|argument| quote_for_posix_shell(argument))
        .collect::<Result<Vec<_>, _>>()
        .map(|arguments| arguments.join(" "))
    }
}

fn common_argv(batch_mode: bool, loopback_endpoint: Option<&LoopbackEndpoint>) -> Vec<OsString> {
    let mut argv = vec![OsString::from(OPENSSH_PATH)];
    push_option(
        &mut argv,
        if batch_mode {
            "BatchMode=yes"
        } else {
            "BatchMode=no"
        },
    );
    push_option(&mut argv, "ForwardAgent=no");
    push_option(&mut argv, "ForwardX11=no");
    push_option(&mut argv, "ClearAllForwardings=yes");
    push_option(&mut argv, "PermitLocalCommand=no");
    push_option(&mut argv, "ControlMaster=no");
    push_option(&mut argv, "ControlPath=none");
    if let Some(endpoint) = loopback_endpoint {
        push_option(&mut argv, "HostName=127.0.0.1");
        push_option(&mut argv, &format!("Port={}", endpoint.port()));
        push_option(
            &mut argv,
            &format!("HostKeyAlias={}", endpoint.host_key_alias().as_str()),
        );
        push_option(&mut argv, "ProxyCommand=none");
        push_option(&mut argv, "ProxyJump=none");
    }
    push_option(
        &mut argv,
        if batch_mode {
            "StrictHostKeyChecking=yes"
        } else {
            "StrictHostKeyChecking=ask"
        },
    );
    if batch_mode {
        push_option(&mut argv, "ConnectTimeout=15");
        push_option(&mut argv, "ServerAliveInterval=30");
        push_option(&mut argv, "ServerAliveCountMax=3");
        push_option(&mut argv, "NumberOfPasswordPrompts=0");
    }
    argv
}

fn push_option(argv: &mut Vec<OsString>, value: &str) {
    argv.push(OsString::from("-o"));
    argv.push(OsString::from(value));
}

fn quote_for_posix_shell(value: &OsStr) -> Result<String, OpenSshConfigError> {
    let value = value
        .to_str()
        .ok_or(OpenSshConfigError::NonUtf8ControlPath)?;
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}
