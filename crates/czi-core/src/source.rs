//! Thread-safe, bounded random-access sources.

use std::fs::{File, Metadata, OpenOptions};
use std::io;
#[cfg(not(any(unix, windows)))]
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(not(any(unix, windows)))]
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use thiserror::Error;

mod block_cache;

pub use block_cache::{
    BlockCache, BlockCacheConfig, BlockCacheError, CacheStats, DEFAULT_BLOCK_SIZE,
    DEFAULT_MAX_BYTES,
};

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
    /// The requested owned range could not be allocated.
    #[error("cannot allocate {size} bytes for source read")]
    Allocation {
        /// Requested allocation size.
        size: usize,
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
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| SourceError::Allocation { size })?;
        bytes.resize(size, 0);
        self.read_at(offset, &mut bytes)?;
        Ok(bytes)
    }
}

/// A portable local-file random-access source.
///
/// Unix and Windows use safe positioned-read APIs, so independent reads can run concurrently
/// without sharing a file cursor. Other targets use a mutex-protected seek/read fallback.
pub struct LocalFileSource {
    path: PathBuf,
    #[cfg(any(unix, windows))]
    file: File,
    #[cfg(not(any(unix, windows)))]
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
            #[cfg(any(unix, windows))]
            file,
            #[cfg(not(any(unix, windows)))]
            file: Mutex::new(file),
            info,
        })
    }

    /// Return the path used to open this source.
    #[must_use]
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
        #[cfg(any(unix, windows))]
        {
            read_exact_positioned(&self.file, offset, dst)?;
        }
        #[cfg(not(any(unix, windows)))]
        {
            let mut file = self.file.lock().map_err(|_| {
                SourceError::Io(io::Error::other("local file source lock was poisoned"))
            })?;
            file.seek(SeekFrom::Start(offset))?;
            file.read_exact(dst)?;
        }
        Ok(())
    }
}

#[cfg(any(unix, windows))]
fn read_exact_positioned(file: &File, mut offset: u64, mut dst: &mut [u8]) -> io::Result<()> {
    while !dst.is_empty() {
        match positioned_read(file, dst, offset) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ));
            }
            Ok(read) => {
                offset = offset
                    .checked_add(u64::try_from(read).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "positioned read is too large")
                    })?)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "positioned read offset overflow",
                        )
                    })?;
                dst = &mut dst[read..];
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn positioned_read(file: &File, dst: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, dst, offset)
}

#[cfg(windows)]
fn positioned_read(file: &File, dst: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, dst, offset)
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
