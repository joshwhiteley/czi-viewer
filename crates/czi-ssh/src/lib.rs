//! Read-only SFTP v3 sources backed by the system OpenSSH client.
//!
//! Production connections invoke only `/usr/bin/ssh` with an argument vector. This crate does
//! not use a shell or an SSH protocol crate.

mod command;
mod error;
mod location;

pub use command::{ControlPath, OPENSSH_PATH, OpenSshConfig};
pub use error::{
    OpenSshConfigError, SftpError, SftpLocationError, SftpProtocolError, SshProfileError,
};
pub use location::{
    RemoteDirEntry, SftpAttributes, SftpExtendedAttribute, SftpLocation, SshProfile,
};
