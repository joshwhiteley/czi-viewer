use std::error::Error;
use std::fmt;
use std::io;

/// Why an SSH destination was rejected before a process was launched.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SshProfileError {
    /// The destination was empty.
    Empty,
    /// The destination exceeds the OpenSSH profile limit.
    TooLong {
        /// Number of UTF-8 bytes supplied.
        length: usize,
    },
    /// The destination contained a NUL byte.
    ContainsNul,
    /// The destination could be interpreted by OpenSSH as an option.
    LeadingDash,
}

impl fmt::Display for SshProfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("SSH profile must not be empty"),
            Self::TooLong { length } => {
                write!(
                    formatter,
                    "SSH profile has {length} bytes; the limit is 255"
                )
            }
            Self::ContainsNul => formatter.write_str("SSH profile must not contain NUL"),
            Self::LeadingDash => formatter.write_str("SSH profile must not begin with '-'"),
        }
    }
}

impl Error for SshProfileError {}

/// Why a remote SFTP path was rejected before it was sent in an SFTP packet.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SftpLocationError {
    /// The path was empty.
    Empty,
    /// The path exceeds the SFTP path limit.
    TooLong {
        /// Number of UTF-8 bytes supplied.
        length: usize,
    },
    /// The path contained a NUL byte.
    ContainsNul,
}

impl fmt::Display for SftpLocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("SFTP path must not be empty"),
            Self::TooLong { length } => {
                write!(formatter, "SFTP path has {length} bytes; the limit is 4096")
            }
            Self::ContainsNul => formatter.write_str("SFTP path must not contain NUL"),
        }
    }
}

impl Error for SftpLocationError {}

/// Why an OpenSSH control socket configuration was rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OpenSshConfigError {
    /// A master command needs a control socket.
    MissingControlPath,
    /// A control socket path cannot be copied safely into a terminal command.
    NonUtf8ControlPath,
    /// The socket path would exceed the portable Unix-domain socket limit.
    SocketPathTooLong {
        /// Number of bytes in the socket path.
        length: usize,
    },
    /// A path expected to be a directory was not one.
    ControlDirectoryNotDirectory,
    /// A unique private control directory could not be created.
    ControlDirectoryUnavailable,
}

impl fmt::Display for OpenSshConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingControlPath => {
                formatter.write_str("a control path is required for a master")
            }
            Self::NonUtf8ControlPath => {
                formatter.write_str("control path must be UTF-8 for a copyable terminal command")
            }
            Self::SocketPathTooLong { length } => write!(
                formatter,
                "control socket path has {length} bytes; the portable limit is 100"
            ),
            Self::ControlDirectoryNotDirectory => {
                formatter.write_str("control path parent is not a directory")
            }
            Self::ControlDirectoryUnavailable => {
                formatter.write_str("could not create a unique private control directory")
            }
        }
    }
}

impl Error for OpenSshConfigError {}

/// A strict SFTP v3 framing or message violation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SftpProtocolError {
    /// A packet length was zero or exceeded the 1 MiB cap.
    InvalidPacketLength {
        /// Length from the packet header, excluding the length field itself.
        length: u32,
    },
    /// A packet did not contain a required field.
    Truncated {
        /// Field being decoded.
        field: &'static str,
    },
    /// An SFTP string was not UTF-8 where UTF-8 is required by this crate.
    InvalidUtf8 {
        /// Field being decoded.
        field: &'static str,
    },
    /// A packet had bytes after its required fields.
    TrailingData {
        /// Packet or field being decoded.
        context: &'static str,
    },
    /// A response was not one of the permitted types.
    UnexpectedPacket {
        /// Received SFTP packet type.
        actual: u8,
        /// Operation waiting for the response.
        operation: &'static str,
    },
    /// A response ID was not the one issued for an operation.
    MismatchedRequestId {
        /// Request ID expected by the client.
        expected: u32,
        /// Request ID received from the server.
        actual: u32,
    },
    /// A server selected an SFTP version other than v3.
    UnsupportedVersion {
        /// Server SFTP version.
        version: u32,
    },
    /// An ATTRS payload advertised unrecognized v3 attribute flags.
    UnknownAttributeFlags {
        /// Full flags word from the packet.
        flags: u32,
    },
    /// FSTAT omitted a value required to create a stable random-access source.
    MissingRequiredAttribute {
        /// Required v3 attribute name.
        attribute: &'static str,
    },
    /// A NAME response did not contain the required number of entries.
    UnexpectedNameCount {
        /// Number of entries expected for this operation.
        expected: u32,
        /// Number of entries received.
        actual: u32,
    },
    /// A DATA response was larger than the matching READ request.
    DataTooLong {
        /// Number of requested bytes.
        requested: usize,
        /// Number of bytes received.
        actual: usize,
    },
    /// A DATA response to a nonempty read was empty.
    EmptyData,
    /// A nonempty read ended before the source's captured length.
    UnexpectedEof,
    /// A request counter would reuse an SFTP request ID.
    RequestIdExhausted,
    /// A READDIR response contained no entries instead of `SSH_FX_EOF`.
    EmptyNameResponse,
    /// A fallible packet allocation failed.
    Allocation {
        /// Requested allocation size.
        size: usize,
    },
}

impl fmt::Display for SftpProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPacketLength { length } => {
                write!(formatter, "invalid SFTP packet length {length}")
            }
            Self::Truncated { field } => write!(formatter, "truncated SFTP {field}"),
            Self::InvalidUtf8 { field } => write!(formatter, "SFTP {field} is not UTF-8"),
            Self::TrailingData { context } => write!(formatter, "trailing data in SFTP {context}"),
            Self::UnexpectedPacket { actual, operation } => {
                write!(
                    formatter,
                    "unexpected SFTP packet type {actual} while waiting for {operation}"
                )
            }
            Self::MismatchedRequestId { expected, actual } => write!(
                formatter,
                "SFTP response request ID {actual} did not match expected ID {expected}"
            ),
            Self::UnsupportedVersion { version } => {
                write!(formatter, "SFTP server version {version} is not v3")
            }
            Self::UnknownAttributeFlags { flags } => {
                write!(
                    formatter,
                    "SFTP v3 attributes have unknown flags {flags:#010x}"
                )
            }
            Self::MissingRequiredAttribute { attribute } => {
                write!(
                    formatter,
                    "SFTP FSTAT omitted required {attribute} attribute"
                )
            }
            Self::UnexpectedNameCount { expected, actual } => write!(
                formatter,
                "SFTP NAME had {actual} entries; expected {expected}"
            ),
            Self::DataTooLong { requested, actual } => write!(
                formatter,
                "SFTP DATA had {actual} bytes for a {requested}-byte READ"
            ),
            Self::EmptyData => formatter.write_str("SFTP DATA response was empty"),
            Self::UnexpectedEof => {
                formatter.write_str("SFTP source ended before its captured length")
            }
            Self::RequestIdExhausted => formatter.write_str("SFTP request ID space is exhausted"),
            Self::EmptyNameResponse => {
                formatter.write_str("SFTP NAME response contained no directory entries")
            }
            Self::Allocation { size } => write!(formatter, "cannot allocate {size} bytes for SFTP"),
        }
    }
}

impl Error for SftpProtocolError {}

/// Errors from the OpenSSH process and strict SFTP v3 client.
#[derive(Debug)]
pub enum SftpError {
    /// The supplied destination was invalid.
    InvalidProfile(SshProfileError),
    /// The supplied SFTP path was invalid.
    InvalidLocation(SftpLocationError),
    /// The supplied OpenSSH configuration was invalid.
    InvalidConfig(OpenSshConfigError),
    /// A local operation failed while setting up or communicating with OpenSSH.
    Io {
        /// Operation that failed.
        context: &'static str,
        /// Underlying local I/O error.
        source: io::Error,
    },
    /// `/usr/bin/ssh` could not be started.
    Spawn {
        /// Underlying process-launch error.
        source: io::Error,
    },
    /// The OpenSSH child stopped before completing a protocol operation.
    ChildExited {
        /// Protocol operation in progress.
        operation: &'static str,
        /// Captured process status, if it could be collected.
        status: Option<std::process::ExitStatus>,
        /// Bounded stderr captured from OpenSSH.
        stderr: String,
    },
    /// The SFTP server returned a non-success status.
    RemoteStatus {
        /// SFTP operation that received the status.
        operation: &'static str,
        /// SFTP v3 status code.
        code: u32,
        /// Server-provided status message.
        message: String,
    },
    /// The SFTP stream violated v3 framing or message rules.
    Protocol(SftpProtocolError),
    /// An internal session mutex was poisoned.
    SessionPoisoned,
}

impl SftpError {
    pub(crate) fn io(context: &'static str, source: io::Error) -> Self {
        Self::Io { context, source }
    }

    pub(crate) fn into_source_io(self) -> io::Error {
        io::Error::other(self)
    }
}

impl fmt::Display for SftpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidProfile(error) => write!(formatter, "invalid SSH profile: {error}"),
            Self::InvalidLocation(error) => write!(formatter, "invalid SFTP location: {error}"),
            Self::InvalidConfig(error) => {
                write!(formatter, "invalid OpenSSH configuration: {error}")
            }
            Self::Io { context, source } => write!(formatter, "{context}: {source}"),
            Self::Spawn { source } => write!(formatter, "could not start /usr/bin/ssh: {source}"),
            Self::ChildExited {
                operation,
                status,
                stderr,
            } => write!(
                formatter,
                "OpenSSH exited during {operation} (status: {status:?}, stderr: {stderr:?})"
            ),
            Self::RemoteStatus {
                operation,
                code,
                message,
            } => write!(
                formatter,
                "SFTP {operation} failed with status {code}: {message}"
            ),
            Self::Protocol(error) => write!(formatter, "invalid SFTP protocol: {error}"),
            Self::SessionPoisoned => formatter.write_str("SFTP session lock was poisoned"),
        }
    }
}

impl Error for SftpError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidProfile(error) => Some(error),
            Self::InvalidLocation(error) => Some(error),
            Self::InvalidConfig(error) => Some(error),
            Self::Io { source, .. } | Self::Spawn { source } => Some(source),
            Self::Protocol(error) => Some(error),
            Self::ChildExited { .. } | Self::RemoteStatus { .. } | Self::SessionPoisoned => None,
        }
    }
}

impl From<SshProfileError> for SftpError {
    fn from(error: SshProfileError) -> Self {
        Self::InvalidProfile(error)
    }
}

impl From<SftpLocationError> for SftpError {
    fn from(error: SftpLocationError) -> Self {
        Self::InvalidLocation(error)
    }
}

impl From<OpenSshConfigError> for SftpError {
    fn from(error: OpenSshConfigError) -> Self {
        Self::InvalidConfig(error)
    }
}

impl From<SftpProtocolError> for SftpError {
    fn from(error: SftpProtocolError) -> Self {
        Self::Protocol(error)
    }
}
