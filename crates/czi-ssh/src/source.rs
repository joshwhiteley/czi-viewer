use std::io;

use czi_core::{RandomAccessSource, SourceError, SourceInfo};

use crate::{
    OpenSshConfig, SftpAttributes, SftpError, SftpLocation, SftpProtocolError, SftpSession,
    SharedSftpSession, SshProfile,
};

/// A read-only random-access source served by a shared, serialized SFTP v3 session.
///
/// The source captures its canonical path, length, and modification time at open. It never
/// reconnects after a failed protocol operation.
pub struct SftpSource {
    canonical_path: SftpLocation,
    attributes: SftpAttributes,
    handle: Option<Vec<u8>>,
    info: SourceInfo,
    session: SharedSftpSession,
}

impl SftpSource {
    /// Prefer an available interactive bridge, otherwise connect through direct `/usr/bin/ssh`,
    /// then resolve `location` and open the canonical path read-only.
    ///
    /// The SFTP server must return v3 FSTAT attributes containing both size and modification time.
    ///
    /// # Errors
    ///
    /// Returns an error if OpenSSH, SFTP negotiation, path resolution, or read-only open fails.
    pub fn open(
        profile: &SshProfile,
        location: &SftpLocation,
        config: &OpenSshConfig,
    ) -> Result<Self, SftpError> {
        Self::open_with_session(SftpSession::connect_preferred(profile, config)?, location)
    }

    /// Open a read-only source from an already authenticated strict SFTP session.
    ///
    /// This wraps the session in a shared owner for source-only callers.
    ///
    /// # Errors
    ///
    /// Returns an error if path resolution, read-only open, or FSTAT fails.
    pub fn open_with_session(
        session: SftpSession,
        location: &SftpLocation,
    ) -> Result<Self, SftpError> {
        Self::open_with_shared_session(SharedSftpSession::new(session), location)
    }

    /// Open a read-only source using an existing shared authenticated SFTP session.
    ///
    /// # Errors
    ///
    /// Returns an error if path resolution, read-only open, or FSTAT fails.
    pub fn open_with_shared_session(
        session: SharedSftpSession,
        location: &SftpLocation,
    ) -> Result<Self, SftpError> {
        let (canonical_path, handle, attributes, size, mtime) =
            session.with_session(|session| {
                let canonical_path = session.realpath(location)?;
                let handle = session.open_read(&canonical_path)?;
                let attributes = match session.fstat(&handle) {
                    Ok(attributes) => attributes,
                    Err(error) => {
                        let _ = session.close(&handle);
                        return Err(error);
                    }
                };
                let Some(size) = attributes.size else {
                    let _ = session.close(&handle);
                    return Err(
                        SftpProtocolError::MissingRequiredAttribute { attribute: "size" }.into(),
                    );
                };
                let Some((_, mtime)) = attributes.access_modify_time else {
                    let _ = session.close(&handle);
                    return Err(SftpProtocolError::MissingRequiredAttribute {
                        attribute: "modification time",
                    }
                    .into());
                };
                Ok((canonical_path, handle, attributes, size, mtime))
            })?;
        let info = SourceInfo {
            length: size,
            version: source_version(&canonical_path, size, mtime),
        };
        Ok(Self {
            canonical_path,
            attributes,
            handle: Some(handle),
            info,
            session,
        })
    }

    /// Return the canonical path received from REALPATH.
    #[must_use]
    pub fn canonical_path(&self) -> &SftpLocation {
        &self.canonical_path
    }

    /// Return the v3 FSTAT attributes captured when the source was opened.
    #[must_use]
    pub fn attributes(&self) -> &SftpAttributes {
        &self.attributes
    }

    /// Close the remote SFTP file handle before dropping this source.
    ///
    /// This is the graceful close path. Drop queues a CLOSE without waiting for a concurrent
    /// browser/read operation; the shared session drains it before/after its next operation or at
    /// final shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error if the SFTP server does not acknowledge `CLOSE`.
    pub fn close(mut self) -> Result<(), SftpError> {
        let handle = self.handle.take().ok_or_else(|| {
            SftpError::io(
                "close SFTP source",
                io::Error::other("SFTP source file handle is already closed"),
            )
        })?;
        self.session.with_session(|session| session.close(&handle))
    }
}

impl RandomAccessSource for SftpSource {
    fn info(&self) -> SourceInfo {
        self.info
    }

    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError> {
        check_range(offset, dst.len(), self.info.length)?;
        if dst.is_empty() {
            return Ok(());
        }
        let handle = self
            .handle
            .as_deref()
            .ok_or_else(|| SourceError::Io(io::Error::other("SFTP source is closed")))?;
        self.session
            .with_session(|session| session.read_exact_at(handle, offset, dst))
            .map_err(|error| SourceError::Io(error.into_source_io()))
    }
}

impl Drop for SftpSource {
    fn drop(&mut self) {
        let Some(handle) = self.handle.take() else {
            return;
        };
        self.session.defer_close(handle);
    }
}

impl std::fmt::Debug for SftpSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SftpSource")
            .field("canonical_path", &self.canonical_path)
            .field("attributes", &self.attributes)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

fn source_version(path: &SftpLocation, size: u64, mtime: u32) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in path
        .as_str()
        .as_bytes()
        .iter()
        .chain([0_u8].iter())
        .chain(size.to_be_bytes().iter())
        .chain(mtime.to_be_bytes().iter())
    {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn check_range(offset: u64, size: usize, length: u64) -> Result<(), SourceError> {
    let size =
        u64::try_from(size).map_err(|_| SourceError::LengthConversion { length: u64::MAX })?;
    let end = offset
        .checked_add(size)
        .ok_or(SourceError::RangeOverflow { offset, size })?;
    if end > length {
        return Err(SourceError::OutOfBounds {
            offset,
            end,
            length,
        });
    }
    Ok(())
}

#[cfg(test)]
impl SftpSource {
    pub(crate) fn open_with_test_session(
        session: SftpSession,
        location: &SftpLocation,
    ) -> Result<Self, SftpError> {
        Self::open_with_shared_session(SharedSftpSession::new(session), location)
    }

    pub(crate) fn test_source_version(path: &SftpLocation, size: u64, mtime: u32) -> u64 {
        source_version(path, size, mtime)
    }
}
