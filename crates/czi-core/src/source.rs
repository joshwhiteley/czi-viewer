//! Thread-safe, bounded random-access sources.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use thiserror::Error;

/// Stable information captured for a source at open time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceInfo {
    /// Number of bytes available in the source.
    pub length: u64,
    /// Opaque source revision. Local files derive this from modification time and length.
    pub version: u64,
}

/// Errors returned by a random-access source.
#[derive(Debug, Error)]
pub enum SourceError {
    /// The requested range is outside the source.
    #[error("read range [{offset}, {end}) is outside source length {length}")]
    OutOfBounds {
        /// First requested byte.
        offset: u64,
        /// Exclusive end of the requested range.
        end: u64,
        /// Source length.
        length: u64,
    },
    /// Computing the end of a requested range overflowed.
    #[error("read range at offset {offset} with size {size} overflows u64")]
    RangeOverflow {
        /// First requested byte.
        offset: u64,
        /// Requested size.
        size: u64,
    },
    /// The local source could not be opened or read.
    #[error("source I/O failed: {0}")]
    Io(#[from] io::Error),
    /// A source length could not be represented by the platform.
    #[error("source length {length} cannot be represented on this platform")]
    LengthConversion {
        /// Unrepresentable length.
        length: u64,
    },
}

/// A source that supports bounded, thread-safe random reads.
///
/// Implementations must not return bytes outside `info().length`. Callers can share an
/// implementation between dataset readers and worker threads.
pub trait RandomAccessSource: Send + Sync {
    /// Return the source length and revision captured by this source.
    fn info(&self) -> SourceInfo;

    /// Read exactly `dst.len()` bytes beginning at `offset`.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::OutOfBounds`] when the range is outside the source, or an I/O
    /// error when the backing source cannot complete the read.
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError>;

    /// Read an owned byte range without exposing an unchecked allocation conversion.
    ///
    /// # Errors
    ///
    /// Returns a bounds, range-overflow, or platform-conversion error before allocating, or
    /// propagates the implementation's read error.
    fn read_vec_at(&self, offset: u64, size: u64) -> Result<Vec<u8>, SourceError> {
        let length = self.info().length;
        let end = checked_end(offset, size)?;
        if end > length {
            return Err(SourceError::OutOfBounds {
                offset,
                end,
                length,
            });
        }
        let size =
            usize::try_from(size).map_err(|_| SourceError::LengthConversion { length: size })?;
        let mut bytes = vec![0; size];
        self.read_at(offset, &mut bytes)?;
        Ok(bytes)
    }
}

/// A portable local-file random-access source.
///
/// Reads use a mutex-protected `File` plus `Seek`, so this implementation does not require
/// platform-specific positional-read APIs.
pub struct LocalFileSource {
    path: PathBuf,
    file: Mutex<File>,
    info: SourceInfo,
}

impl LocalFileSource {
    /// Open a file read-only and capture its length and revision.
    ///
    /// # Errors
    ///
    /// Returns [`SourceError::Io`] when the file cannot be opened or inspected.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SourceError> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().read(true).open(&path)?;
        let metadata = file.metadata()?;
        let info = source_info(&metadata);
        Ok(Self {
            path,
            file: Mutex::new(file),
            info,
        })
    }

    /// Return the path used to open this source.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl RandomAccessSource for LocalFileSource {
    fn info(&self) -> SourceInfo {
        self.info
    }

    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError> {
        check_range(offset, dst.len(), self.info.length)?;
        if dst.is_empty() {
            return Ok(());
        }
        let mut file = self.file.lock().map_err(|_| {
            SourceError::Io(io::Error::other("local file source lock was poisoned"))
        })?;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(dst)?;
        Ok(())
    }
}

impl std::fmt::Debug for LocalFileSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalFileSource")
            .field("path", &self.path)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// An owned in-memory source, useful for callers and parser tests.
#[derive(Clone, Debug)]
pub struct MemorySource {
    bytes: Arc<[u8]>,
    info: SourceInfo,
}

impl MemorySource {
    /// Create a source with version zero.
    pub fn new(bytes: impl Into<Arc<[u8]>>) -> Self {
        let bytes = bytes.into();
        Self {
            info: SourceInfo {
                length: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                version: 0,
            },
            bytes,
        }
    }

    /// Create a source with an explicit opaque version.
    pub fn with_version(bytes: impl Into<Arc<[u8]>>, version: u64) -> Self {
        let mut source = Self::new(bytes);
        source.info.version = version;
        source
    }
}

impl RandomAccessSource for MemorySource {
    fn info(&self) -> SourceInfo {
        self.info
    }

    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError> {
        check_range(offset, dst.len(), self.info.length)?;
        if dst.is_empty() {
            return Ok(());
        }
        let start = usize::try_from(offset)
            .map_err(|_| SourceError::LengthConversion { length: offset })?;
        let end = start
            .checked_add(dst.len())
            .ok_or(SourceError::RangeOverflow {
                offset,
                size: u64::try_from(dst.len()).unwrap_or(u64::MAX),
            })?;
        dst.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }
}

fn source_info(metadata: &Metadata) -> SourceInfo {
    let length = metadata.len();
    let modified_nanos = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |duration| duration.as_nanos());
    let modified = u64::try_from(modified_nanos).unwrap_or(u64::MAX);
    SourceInfo {
        length,
        version: modified.rotate_left(17) ^ length,
    }
}

fn checked_end(offset: u64, size: u64) -> Result<u64, SourceError> {
    offset
        .checked_add(size)
        .ok_or(SourceError::RangeOverflow { offset, size })
}

fn check_range(offset: u64, size: usize, length: u64) -> Result<(), SourceError> {
    let size =
        u64::try_from(size).map_err(|_| SourceError::LengthConversion { length: u64::MAX })?;
    let end = checked_end(offset, size)?;
    if end > length {
        return Err(SourceError::OutOfBounds {
            offset,
            end,
            length,
        });
    }
    Ok(())
}
