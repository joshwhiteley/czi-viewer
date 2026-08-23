//! Read-only SFTP v3 sources backed by the system OpenSSH client.
//!
//! Production connections invoke only `/usr/bin/ssh` with an argument vector. This crate does
//! not use a shell or an SSH protocol crate.

mod bridge;
mod command;
mod error;
mod location;
mod protocol;
mod shared;
mod source;

pub use bridge::BridgeCancellation;
#[cfg(unix)]
pub use bridge::{
    BridgeListener, authenticate_bridge_client, authenticate_bridge_server, connect_bridge_socket,
};
pub use command::{ControlPath, HostKeyAlias, LoopbackEndpoint, OPENSSH_PATH, OpenSshConfig};
pub use error::{
    OpenSshConfigError, SftpError, SftpLocationError, SftpProtocolError, SshProfileError,
};
pub use location::{
    RemoteDirEntry, SftpAttributes, SftpExtendedAttribute, SftpLocation, SshProfile,
};
pub use protocol::{
    EMBEDDED_PTY_EXEC_MODE, EmbeddedSshCancellation, PendingEmbeddedSftpSession,
    SSH_CONSOLE_OUTPUT_LIMIT, SftpSession, SshConsole, run_embedded_pty_executor_if_requested,
};
pub use shared::SharedSftpSession;
pub use source::SftpSource;
