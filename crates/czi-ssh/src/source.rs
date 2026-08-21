use std::io;
use std::sync::Mutex;

use czi_core::{RandomAccessSource, SourceError, SourceInfo};

use crate::{
    OpenSshConfig, SftpAttributes, SftpError, SftpLocation, SftpProtocolError, SftpSession,
    SshProfile,
};

/// A read-only, mutex-serialized random-access source served by SFTP v3.
///
/// The source captures its canonical path, length, and modification time at open. It never
/// reconnects after a failed protocol operation.
pub struct SftpSource {
    canonical_path: SftpLocation,
    attributes: SftpAttributes,
    handle: Vec<u8>,
    info: SourceInfo,
    session: Mutex<SftpSession>,
}

impl SftpSource {
    /// Connect through `/usr/bin/ssh`, resolve `location`, and open the canonical path read-only.
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
        Self::open_session(SftpSession::connect(profile, config)?, location)
    }

    fn open_session(mut session: SftpSession, location: &SftpLocation) -> Result<Self, SftpError> {
        let canonical_path = session.realpath(location)?;
        let handle = session.open_read(&canonical_path)?;
        let attributes = match session.fstat(&handle) {
            Ok(attributes) => attributes,
            Err(error) => {
                let _ = session.close(&handle);
                return Err(error);
            }
        };
        let size = attributes
            .size
            .ok_or(SftpProtocolError::MissingRequiredAttribute { attribute: "size" })?;
        let mtime = attributes
            .access_modify_time
            .ok_or(SftpProtocolError::MissingRequiredAttribute {
                attribute: "modification time",
            })?
            .1;
        let info = SourceInfo {
            length: size,
            version: source_version(&canonical_path, size, mtime),
        };
        Ok(Self {
            canonical_path,
            attributes,
            handle,
            info,
            session: Mutex::new(session),
        })
    }

    /// Return the canonical path received from REALPATH.
    pub fn canonical_path(&self) -> &SftpLocation {
        &self.canonical_path
    }

    /// Return the v3 FSTAT attributes captured when the source was opened.
    pub fn attributes(&self) -> &SftpAttributes {
        &self.attributes
    }

    /// Close the remote SFTP file handle before dropping this source.
    ///
    /// This is the graceful close path. Dropping a source instead terminates and reaps the
    /// OpenSSH child without blocking on remote protocol I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if the SFTP server does not acknowledge `CLOSE`.
    pub fn close(mut self) -> Result<(), SftpError> {
        self.session
            .get_mut()
            .map_err(|_| SftpError::SessionPoisoned)?
            .close(&self.handle)
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
        let mut session = self.session.lock().map_err(|_| {
            SourceError::Io(io::Error::other("SFTP source session lock was poisoned"))
        })?;
        session
            .read_exact_at(&self.handle, offset, dst)
            .map_err(|error| SourceError::Io(error.into_source_io()))
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
        Self::open_session(session, location)
    }

    pub(crate) fn test_source_version(path: &SftpLocation, size: u64, mtime: u32) -> u64 {
        source_version(path, size, mtime)
    }
}
