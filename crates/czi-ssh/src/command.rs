use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{OpenSshConfigError, SftpError, SshProfile};

/// The only OpenSSH executable used by production connections.
pub const OPENSSH_PATH: &str = "/usr/bin/ssh";

const SOCKET_PATH_LIMIT: usize = 100;

/// An application-private directory and socket for OpenSSH connection multiplexing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlPath {
    directory: PathBuf,
    socket_path: PathBuf,
}

impl ControlPath {
    /// Create a unique application-private socket directory below the system temporary directory.
    ///
    /// The directory permissions are set to `0700` on Unix platforms. The returned socket is
    /// named `master.sock` and is constrained to a portable Unix-domain socket path length.
    ///
    /// # Errors
    ///
    /// Returns an error when the directory or its permissions cannot be created.
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
            let directory = base.join(format!("czi-ssh-{}-{nanos}-{counter}", process::id()));
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

    /// Use `directory/master.sock` as a control socket after setting the directory to `0700`.
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
        let socket_path = directory.join("master.sock");
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

    /// Return the exact socket path passed to OpenSSH.
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

/// OpenSSH settings used by SFTP and optional connection-master commands.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OpenSshConfig {
    control_path: Option<ControlPath>,
}

impl OpenSshConfig {
    /// Create a configuration with OpenSSH's normal host-key handling and no control socket.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach an application-private control socket to this configuration.
    #[must_use]
    pub fn with_control_path(mut self, control_path: ControlPath) -> Self {
        self.control_path = Some(control_path);
        self
    }

    /// Return the configured private control socket, if any.
    #[must_use]
    pub fn control_path(&self) -> Option<&ControlPath> {
        self.control_path.as_ref()
    }

    /// Build the production `/usr/bin/ssh` argument vector for an SFTP subsystem child.
    ///
    /// This always uses `BatchMode=yes`, disables agent and forwarding features, requests no TTY,
    /// and passes only the destination and `sftp` subsystem after the SSH options. Remote paths
    /// belong in SFTP packets and cannot enter this vector.
    #[must_use]
    pub fn sftp_argv(&self, profile: &SshProfile) -> Vec<OsString> {
        let mut argv = common_argv(true);
        self.push_client_control_options(&mut argv);
        argv.push(OsString::from("-T"));
        argv.push(OsString::from("-s"));
        argv.push(OsString::from(profile.as_str()));
        argv.push(OsString::from("sftp"));
        argv
    }

    /// Build a noninteractive foreground OpenSSH master command argument vector.
    ///
    /// The command uses `BatchMode=yes`; callers can launch it directly and retain its child
    /// process. A configured private control path is required.
    ///
    /// # Errors
    ///
    /// Returns an error when no private control path is configured.
    pub fn noninteractive_master_argv(
        &self,
        profile: &SshProfile,
    ) -> Result<Vec<OsString>, OpenSshConfigError> {
        self.master_argv(profile, true)
    }

    /// Build a safely shell-quoted command a user can paste into Terminal to bootstrap a master.
    ///
    /// This is intentionally the only shell-form command exposed by the crate. It uses
    /// `BatchMode=no` so OpenSSH can prompt in the visible terminal. Production SFTP sessions
    /// continue to use an argument vector and `BatchMode=yes`.
    ///
    /// # Errors
    ///
    /// Returns an error when no private control path is configured or it is not UTF-8.
    pub fn terminal_bootstrap_command(
        &self,
        profile: &SshProfile,
    ) -> Result<String, OpenSshConfigError> {
        let argv = self.master_argv(profile, false)?;
        argv.iter()
            .map(|argument| quote_for_posix_shell(argument.as_os_str()))
            .collect::<Result<Vec<_>, _>>()
            .map(|arguments| arguments.join(" "))
    }

    fn master_argv(
        &self,
        profile: &SshProfile,
        batch_mode: bool,
    ) -> Result<Vec<OsString>, OpenSshConfigError> {
        let control_path = self
            .control_path
            .as_ref()
            .ok_or(OpenSshConfigError::MissingControlPath)?;
        let mut argv = common_argv(batch_mode);
        push_option(&mut argv, "ControlMaster=yes");
        push_option(&mut argv, "ControlPersist=no");
        push_path_option(&mut argv, "ControlPath", control_path.socket_path());
        argv.push(OsString::from("-M"));
        argv.push(OsString::from("-N"));
        argv.push(OsString::from(profile.as_str()));
        Ok(argv)
    }

    fn push_client_control_options(&self, argv: &mut Vec<OsString>) {
        if let Some(control_path) = &self.control_path {
            push_option(argv, "ControlMaster=auto");
            push_option(argv, "ControlPersist=no");
            push_path_option(argv, "ControlPath", control_path.socket_path());
        }
    }
}

fn common_argv(batch_mode: bool) -> Vec<OsString> {
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
    push_option(&mut argv, "StrictHostKeyChecking=ask");
    argv
}

fn push_option(argv: &mut Vec<OsString>, value: &str) {
    argv.push(OsString::from("-o"));
    argv.push(OsString::from(value));
}

fn push_path_option(argv: &mut Vec<OsString>, name: &str, path: &Path) {
    let mut value = OsString::from(name);
    value.push("=");
    value.push(path.as_os_str());
    argv.push(OsString::from("-o"));
    argv.push(value);
}

fn quote_for_posix_shell(value: &OsStr) -> Result<String, OpenSshConfigError> {
    let value = value
        .to_str()
        .ok_or(OpenSshConfigError::NonUtf8ControlPath)?;
    Ok(format!("'{}'", value.replace('\'', "'\"'\"'")))
}
