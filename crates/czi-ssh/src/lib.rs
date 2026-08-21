//! Read-only SFTP v3 sources backed by the system OpenSSH client.
//!
//! Production connections invoke only `/usr/bin/ssh` with an argument vector. This crate does
//! not use a shell or an SSH protocol crate.

mod bridge;
mod command;
mod error;
mod location;
mod protocol;
mod source;

pub use bridge::BridgeCancellation;
#[cfg(unix)]
pub use bridge::{
    BridgeListener, authenticate_bridge_client, authenticate_bridge_server, connect_bridge_socket,
};
pub use command::{ControlPath, OPENSSH_PATH, OpenSshConfig};
pub use error::{
    OpenSshConfigError, SftpError, SftpLocationError, SftpProtocolError, SshProfileError,
};
pub use location::{
    RemoteDirEntry, SftpAttributes, SftpExtendedAttribute, SftpLocation, SshProfile,
};
pub use protocol::SftpSession;
pub use source::SftpSource;
