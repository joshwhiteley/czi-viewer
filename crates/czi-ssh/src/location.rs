use crate::{SftpLocationError, SshProfileError};

/// A validated OpenSSH destination such as `alice@example.org`.
///
/// The value is passed as one argument to OpenSSH, never interpolated into a shell command.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SshProfile(String);

impl SshProfile {
    /// Validate an OpenSSH destination.
    ///
    /// A profile must be nonempty, at most 255 UTF-8 bytes, contain no NUL byte, and not begin
    /// with `-` so it cannot be interpreted as an OpenSSH option.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` violates one of these constraints.
    pub fn new(value: impl Into<String>) -> Result<Self, SshProfileError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SshProfileError::Empty);
        }
        if value.len() > 255 {
            return Err(SshProfileError::TooLong {
                length: value.len(),
            });
        }
        if value.contains('\0') {
            return Err(SshProfileError::ContainsNul);
        }
        if value.starts_with('-') {
            return Err(SshProfileError::LeadingDash);
        }
        Ok(Self(value))
    }

    /// Return the validated destination text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SshProfile {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A validated UTF-8 remote path carried inside an SFTP packet.
///
/// Remote paths are deliberately never added to an OpenSSH argument vector.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SftpLocation(String);

impl SftpLocation {
    /// Validate a UTF-8 remote SFTP path.
    ///
    /// The path must be nonempty, at most 4096 UTF-8 bytes, and contain no NUL byte.
    ///
    /// # Errors
    ///
    /// Returns an error if `value` violates one of these constraints.
    pub fn new(value: impl Into<String>) -> Result<Self, SftpLocationError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SftpLocationError::Empty);
        }
        if value.len() > 4096 {
            return Err(SftpLocationError::TooLong {
                length: value.len(),
            });
        }
        if value.contains('\0') {
            return Err(SftpLocationError::ContainsNul);
        }
        Ok(Self(value))
    }

    /// Return the validated UTF-8 path.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for SftpLocation {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// SFTP v3 extended attribute data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SftpExtendedAttribute {
    /// Extension type name bytes.
    pub name: Vec<u8>,
    /// Extension value bytes.
    pub value: Vec<u8>,
}

/// Attributes decoded from a strict SFTP v3 ATTRS field.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SftpAttributes {
    /// File size, when supplied by the server.
    pub size: Option<u64>,
    /// Unix user and group IDs, when supplied by the server.
    pub uid_gid: Option<(u32, u32)>,
    /// POSIX permissions and file-type bits, when supplied by the server.
    pub permissions: Option<u32>,
    /// POSIX access time and modification time in seconds, when supplied by the server.
    pub access_modify_time: Option<(u32, u32)>,
    /// Server extension attributes, when supplied by the server.
    pub extended: Vec<SftpExtendedAttribute>,
}

/// One entry returned from an SFTP v3 READDIR response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteDirEntry {
    /// UTF-8 filename returned by the server.
    pub path: SftpLocation,
    /// Server-provided long display name.
    pub long_name: String,
    /// Attributes returned with the directory entry.
    pub attributes: SftpAttributes,
}
