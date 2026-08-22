//! Safe CZI container indexing. Pixel codecs are intentionally out of scope for Phase 1.

use std::fmt;
use std::fmt::Write as _;
use std::sync::Arc;

use thiserror::Error;

use crate::source::{RandomAccessSource, SourceError, SourceInfo};

const SEGMENT_HEADER_SIZE: u64 = 32;
const FILE_HEADER_DATA_SIZE: u64 = 512;
const METADATA_FIXED_SIZE: u64 = 256;
const DIRECTORY_FIXED_SIZE: u64 = 128;
const ATTACHMENT_DIRECTORY_FIXED_SIZE: u64 = 256;
const ATTACHMENT_ENTRY_SIZE: u64 = 128;
const SUBBLOCK_FIXED_SIZE: u64 = 16;
const DV_FIXED_SIZE: u64 = 32;
const DIMENSION_ENTRY_SIZE: u64 = 20;
const SUBBLOCK_MIN_DATA_SIZE: u64 = 256;
const DEFAULT_MAX_METADATA_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_MAX_DIRECTORY_ENTRIES: u64 = 1_000_000;
const DEFAULT_MAX_DIMENSIONS: u64 = 64;
const DEFAULT_MAX_TOTAL_DIMENSIONS: u64 = 1_000_000;
const DEFAULT_MAX_INDEX_BYTES: u64 = 256 * 1024 * 1024;

const FILE_ID: &[u8; 16] = b"ZISRAWFILE\0\0\0\0\0\0";
const DIRECTORY_ID: &[u8; 16] = b"ZISRAWDIRECTORY\0";
const SUBBLOCK_ID: &[u8; 16] = b"ZISRAWSUBBLOCK\0\0";
const METADATA_ID: &[u8; 16] = b"ZISRAWMETADATA\0\0";
const ATTACHMENT_ID: &[u8; 16] = b"ZISRAWATTACH\0\0\0\0";
const ATTACHMENT_DIRECTORY_ID: &[u8; 16] = b"ZISRAWATTDIR\0\0\0\0";

/// Errors raised while opening or indexing a CZI document.
#[derive(Debug, Error)]
pub enum CziError {
    /// The backing source rejected a bounded read.
    #[error(transparent)]
    Source(#[from] SourceError),
    /// A segment has an invalid length or the source ends before it.
    #[error("invalid segment at offset {offset}: {reason}")]
    InvalidSegment {
        /// Segment offset.
        offset: u64,
        /// Validation failure.
        reason: String,
    },
    /// A segment identifier is not the one required by the parser.
    #[error("unexpected segment at offset {offset}: expected {expected}, found {found}")]
    UnexpectedSegment {
        /// Segment offset.
        offset: u64,
        /// Required identifier.
        expected: String,
        /// Actual identifier.
        found: String,
    },
    /// A segment schema is not supported by this indexer.
    #[error("unsupported {context} schema {schema}")]
    UnsupportedSchema {
        /// Structure containing the schema.
        context: &'static str,
        /// Two-byte schema identifier.
        schema: String,
    },
    /// The CZI metadata XML exceeds the configured bound.
    #[error("metadata XML size {size} exceeds configured maximum {maximum}")]
    MetadataTooLarge {
        /// Declared XML size.
        size: u64,
        /// Configured maximum.
        maximum: u64,
    },
    /// A signed size or count is invalid.
    #[error("invalid {kind} {value} at offset {offset}")]
    InvalidNumber {
        /// Number kind.
        kind: &'static str,
        /// Invalid value.
        value: i64,
        /// Containing structure offset.
        offset: u64,
    },
    /// A checked integer computation overflowed.
    #[error("integer overflow while calculating {context}")]
    Overflow {
        /// Calculation description.
        context: &'static str,
    },
    /// A fixed-width UTF-8 field is malformed.
    #[error("invalid UTF-8 in {context} at offset {offset}")]
    InvalidUtf8 {
        /// Field name.
        context: &'static str,
        /// Field offset.
        offset: u64,
    },
    /// A referenced location does not contain the expected structure.
    #[error("missing {what} at offset {offset}")]
    Missing {
        /// Structure name.
        what: &'static str,
        /// Expected offset.
        offset: u64,
    },
    /// The file was marked as being updated and may have inconsistent indexes.
    #[error("CZI file at offset {offset} is marked update-pending")]
    UpdatePending {
        /// File header offset.
        offset: u64,
    },
    /// A reference points to a different file part than this dataset.
    #[error("{context} references file part {actual}, expected part {expected}")]
    CrossFilePartReference {
        /// Reference kind.
        context: &'static str,
        /// Dataset file part.
        expected: i32,
        /// Referenced file part.
        actual: i32,
    },
    /// A summary descriptor and its inline descriptor disagree.
    #[error("{context} descriptor mismatch in {field} at offset {offset}")]
    DescriptorMismatch {
        /// Descriptor kind.
        context: &'static str,
        /// Mismatching field.
        field: &'static str,
        /// Descriptor offset.
        offset: u64,
    },
    /// A dimension code occurs more than once in one DV descriptor.
    #[error("duplicate dimension code {code} at offset {offset}")]
    DuplicateDimension {
        /// Dimension code.
        code: String,
        /// Descriptor offset.
        offset: u64,
    },
    /// A configured resource limit was exceeded.
    #[error("{kind} {value} exceeds configured maximum {maximum}")]
    LimitExceeded {
        /// Limited resource.
        kind: &'static str,
        /// Observed value.
        value: u64,
        /// Configured maximum.
        maximum: u64,
    },
    /// An allocation failed before any unbounded memory growth occurred.
    #[error("cannot allocate {size} bytes for {context}")]
    Allocation {
        /// Allocation context.
        context: &'static str,
        /// Requested bytes.
        size: usize,
    },
    /// An uncompressed tile payload does not match its declared stored dimensions.
    #[error("uncompressed tile payload is {actual} bytes, expected {expected} at offset {offset}")]
    PayloadSizeMismatch {
        /// Payload offset.
        offset: u64,
        /// Declared data size.
        actual: u64,
        /// Computed stored byte size.
        expected: u64,
    },
    /// A pixel type has no decoded-tile representation in this build.
    #[error("pixel type {pixel_type:?} is unsupported for decoded tiles")]
    UnsupportedPixel {
        /// Pixel type declared by the tile.
        pixel_type: PixelType,
    },
    /// A compression mode has no decoder in this build.
    #[error("compression {compression:?} is unsupported for decoded tiles")]
    UnsupportedCompression {
        /// Compression mode declared by the tile.
        compression: CompressionMode,
    },
    /// A tile cannot be represented as one two-dimensional image.
    #[error("stored dimension {code} has size {stored_size}, expected 1 for a decoded tile")]
    UnsupportedTileDimensions {
        /// Non-spatial dimension code.
        code: DimensionCode,
        /// Stored number of values in that dimension.
        stored_size: u32,
    },
}

/// Limits used while indexing a CZI source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseOptions {
    /// Maximum metadata XML bytes to allocate.
    pub max_metadata_bytes: u64,
    /// Maximum number of entries in a summary directory.
    pub max_directory_entries: u64,
    /// Maximum dimensions in one DV entry.
    pub max_dimensions_per_entry: u64,
    /// Maximum dimensions across the complete summary directory.
    pub max_total_dimensions: u64,
    /// Maximum bytes used by the complete summary directory payload.
    pub max_index_bytes: u64,
}

impl Default for ParseOptions {
    fn default() -> Self {
        Self {
            max_metadata_bytes: DEFAULT_MAX_METADATA_BYTES,
            max_directory_entries: DEFAULT_MAX_DIRECTORY_ENTRIES,
            max_dimensions_per_entry: DEFAULT_MAX_DIMENSIONS,
            max_total_dimensions: DEFAULT_MAX_TOTAL_DIMENSIONS,
            max_index_bytes: DEFAULT_MAX_INDEX_BYTES,
        }
    }
}

impl ParseOptions {
    /// Set the maximum metadata XML allocation.
    #[must_use]
    pub const fn with_max_metadata_bytes(mut self, maximum: u64) -> Self {
        self.max_metadata_bytes = maximum;
        self
    }

    /// Set the maximum directory entry count.
    #[must_use]
    pub const fn with_max_directory_entries(mut self, maximum: u64) -> Self {
        self.max_directory_entries = maximum;
        self
    }

    /// Set the maximum dimensions in a DV entry.
    #[must_use]
    pub const fn with_max_dimensions_per_entry(mut self, maximum: u64) -> Self {
        self.max_dimensions_per_entry = maximum;
        self
    }

    /// Set the aggregate dimension budget.
    #[must_use]
    pub const fn with_max_total_dimensions(mut self, maximum: u64) -> Self {
        self.max_total_dimensions = maximum;
        self
    }

    /// Set the summary-directory allocation budget.
    #[must_use]
    pub const fn with_max_index_bytes(mut self, maximum: u64) -> Self {
        self.max_index_bytes = maximum;
        self
    }
}

/// The known CZI segment identifiers plus unknown identifiers retained as raw bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentKind {
    /// File header.
    File,
    /// Summary directory.
    Directory,
    /// Image subblock.
    Subblock,
    /// Global metadata.
    Metadata,
    /// Attachment payload.
    Attachment,
    /// Attachment summary directory.
    AttachmentDirectory,
    /// Deleted segment.
    Deleted,
    /// An identifier not defined by this crate.
    Unknown,
}

/// A validated segment header and its bounded data extent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentHeader {
    /// Segment offset in the source.
    pub offset: u64,
    /// Raw sixteen-byte identifier.
    pub id: [u8; 16],
    /// Allocated data bytes following this 32-byte header.
    pub allocated_size: u64,
    /// Used data bytes. A zero on disk is represented as `allocated_size`.
    pub used_size: u64,
}

impl SegmentHeader {
    /// Classify this segment identifier.
    #[must_use]
    pub fn kind(&self) -> SegmentKind {
        match &self.id {
            id if id == FILE_ID => SegmentKind::File,
            id if id == DIRECTORY_ID => SegmentKind::Directory,
            id if id == SUBBLOCK_ID => SegmentKind::Subblock,
            id if id == METADATA_ID => SegmentKind::Metadata,
            id if id == ATTACHMENT_ID => SegmentKind::Attachment,
            id if id == ATTACHMENT_DIRECTORY_ID => SegmentKind::AttachmentDirectory,
            id if id.starts_with(b"DELETED") => SegmentKind::Deleted,
            _ => SegmentKind::Unknown,
        }
    }

    /// Return the identifier without NUL padding.
    #[must_use]
    pub fn id_string(&self) -> String {
        fixed_string(&self.id).unwrap_or_else(|_| String::from("<invalid id>"))
    }

    /// Return the first byte after the used segment data.
    ///
    /// # Errors
    ///
    /// Returns [`CziError::Overflow`] if the segment extent cannot be represented.
    pub fn used_end(&self) -> Result<u64, CziError> {
        checked_add(
            checked_add(self.offset, SEGMENT_HEADER_SIZE, "segment header end")?,
            self.used_size,
            "segment used end",
        )
    }

    /// Return the first byte after the allocated segment data.
    ///
    /// # Errors
    ///
    /// Returns [`CziError::Overflow`] if the segment extent cannot be represented.
    pub fn allocated_end(&self) -> Result<u64, CziError> {
        checked_add(
            checked_add(self.offset, SEGMENT_HEADER_SIZE, "segment header end")?,
            self.allocated_size,
            "segment allocated end",
        )
    }
}

/// File header fields from the first segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHeader {
    /// Header segment.
    pub segment: SegmentHeader,
    /// CZI major version.
    pub major_version: u32,
    /// CZI minor version.
    pub minor_version: u32,
    /// Primary file GUID bytes.
    pub primary_file_guid: [u8; 16],
    /// Current file GUID bytes.
    pub file_guid: [u8; 16],
    /// Multi-file part number.
    pub file_part: i32,
    /// Summary directory segment location.
    pub directory_position: u64,
    /// Global metadata segment location, or zero when absent.
    pub metadata_position: u64,
    /// Whether the writer marked the file as being updated.
    pub update_pending: bool,
    /// Attachment directory segment location, or zero when absent.
    pub attachment_directory_position: u64,
}

/// A standard CZI dimension code or an unknown four-byte code preserved verbatim.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DimensionCode {
    /// X pixel coordinate.
    X,
    /// Y pixel coordinate.
    Y,
    /// Z slice.
    Z,
    /// Channel.
    C,
    /// Time point.
    T,
    /// Rotation.
    R,
    /// Scene.
    S,
    /// Illumination.
    I,
    /// Acquisition block.
    B,
    /// Mosaic index.
    M,
    /// Phase.
    H,
    /// View.
    V,
    /// Unknown code retained from the file.
    Unknown([u8; 4]),
}

impl DimensionCode {
    fn from_raw(raw: [u8; 4]) -> Self {
        if raw[1..].iter().any(|byte| *byte != 0) {
            return Self::Unknown(raw);
        }
        match raw[0] {
            b'X' => Self::X,
            b'Y' => Self::Y,
            b'Z' => Self::Z,
            b'C' => Self::C,
            b'T' => Self::T,
            b'R' => Self::R,
            b'S' => Self::S,
            b'I' => Self::I,
            b'B' => Self::B,
            b'M' => Self::M,
            b'H' => Self::H,
            b'V' => Self::V,
            _ => Self::Unknown(raw),
        }
    }

    /// Return the original four-byte code.
    #[must_use]
    pub const fn raw(self) -> [u8; 4] {
        match self {
            Self::X => [b'X', 0, 0, 0],
            Self::Y => [b'Y', 0, 0, 0],
            Self::Z => [b'Z', 0, 0, 0],
            Self::C => [b'C', 0, 0, 0],
            Self::T => [b'T', 0, 0, 0],
            Self::R => [b'R', 0, 0, 0],
            Self::S => [b'S', 0, 0, 0],
            Self::I => [b'I', 0, 0, 0],
            Self::B => [b'B', 0, 0, 0],
            Self::M => [b'M', 0, 0, 0],
            Self::H => [b'H', 0, 0, 0],
            Self::V => [b'V', 0, 0, 0],
            Self::Unknown(raw) => raw,
        }
    }

    /// Return a compact display form, retaining non-UTF-8 data as hex.
    #[must_use]
    pub fn as_string(self) -> String {
        let raw = self.raw();
        if let Some(end) = raw.iter().position(|byte| *byte == 0) {
            String::from_utf8_lossy(&raw[..end]).into_owned()
        } else {
            let mut output = String::with_capacity(raw.len() * 2);
            for byte in raw {
                let _ = write!(output, "{byte:02x}");
            }
            output
        }
    }

    fn is_mosaic(self) -> bool {
        matches!(self, Self::M)
    }
}

impl fmt::Display for DimensionCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str((*self).as_string().as_str())
    }
}

/// One variable-length DV dimension entry.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DimensionEntry {
    /// Dimension code, including unknown codes.
    pub code: DimensionCode,
    /// Logical start index. X/Y may be negative for mosaics.
    pub start: i32,
    /// Logical number of items.
    pub logical_size: u32,
    /// Physical start coordinate.
    pub start_coordinate: f32,
    /// Stored number of items. A zero on disk is normalized to logical size.
    pub stored_size: u32,
    /// Original signed stored-size field, where zero means logical size.
    pub stored_size_raw: i32,
}

/// Pixel storage type from a DV directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelType {
    /// Eight-bit grayscale.
    Gray8,
    /// Sixteen-bit grayscale.
    Gray16,
    /// Thirty-two-bit floating point grayscale.
    Gray32Float,
    /// Eight-bit BGR.
    Bgr24,
    /// Sixteen-bit BGR.
    Bgr48,
    /// Ninety-six-bit floating point BGR.
    Bgr96Float,
    /// Eight-bit BGRA.
    Bgra32,
    /// Complex float grayscale.
    Gray64ComplexFloat,
    /// Complex float BGR.
    Bgr192ComplexFloat,
    /// Thirty-two-bit integer grayscale.
    Gray32,
    /// Sixty-four-bit floating point grayscale.
    Gray64,
    /// Pixel type not known to this parser.
    Unknown(i32),
}

impl PixelType {
    fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Gray8,
            1 => Self::Gray16,
            2 => Self::Gray32Float,
            3 => Self::Bgr24,
            4 => Self::Bgr48,
            8 => Self::Bgr96Float,
            9 => Self::Bgra32,
            10 => Self::Gray64ComplexFloat,
            11 => Self::Bgr192ComplexFloat,
            12 => Self::Gray32,
            13 => Self::Gray64,
            value => Self::Unknown(value),
        }
    }

    /// Return the raw CZI pixel type value.
    #[must_use]
    pub const fn raw(self) -> i32 {
        match self {
            Self::Gray8 => 0,
            Self::Gray16 => 1,
            Self::Gray32Float => 2,
            Self::Bgr24 => 3,
            Self::Bgr48 => 4,
            Self::Bgr96Float => 8,
            Self::Bgra32 => 9,
            Self::Gray64ComplexFloat => 10,
            Self::Bgr192ComplexFloat => 11,
            Self::Gray32 => 12,
            Self::Gray64 => 13,
            Self::Unknown(value) => value,
        }
    }

    /// Return bytes per stored item when known.
    #[must_use]
    pub const fn bytes_per_item(self) -> Option<u64> {
        match self {
            Self::Gray8 => Some(1),
            Self::Gray16 => Some(2),
            Self::Gray32Float | Self::Gray32 | Self::Bgra32 => Some(4),
            Self::Bgr24 => Some(3),
            Self::Bgr48 => Some(6),
            Self::Bgr96Float => Some(12),
            Self::Gray64ComplexFloat | Self::Gray64 => Some(8),
            Self::Bgr192ComplexFloat => Some(24),
            Self::Unknown(_) => None,
        }
    }
}

/// Pixel compression mode. No codec is linked by this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompressionMode {
    /// No compression.
    Uncompressed,
    /// JPEG.
    Jpeg,
    /// LZW.
    Lzw,
    /// Undocumented JPEG lossless mode.
    JpegLossless,
    /// JPEG XR.
    JpegXr,
    /// Zstandard.
    Zstd,
    /// Zstandard with the CZI header/shuffle mode.
    Zstd1,
    /// Experimental chunked compression.
    Chunked,
    /// Camera-specific raw compression (100..999).
    CameraRaw(i32),
    /// System-specific raw compression (1000 and above).
    SystemRaw(i32),
    /// Compression mode not known to this parser.
    Unknown(i32),
}

impl CompressionMode {
    fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Uncompressed,
            1 => Self::Jpeg,
            2 => Self::Lzw,
            3 => Self::JpegLossless,
            4 => Self::JpegXr,
            5 => Self::Zstd,
            6 => Self::Zstd1,
            7 => Self::Chunked,
            100..=999 => Self::CameraRaw(value),
            1000.. => Self::SystemRaw(value),
            value => Self::Unknown(value),
        }
    }

    /// Return the raw CZI compression value.
    #[must_use]
    pub const fn raw(self) -> i32 {
        match self {
            Self::Uncompressed => 0,
            Self::Jpeg => 1,
            Self::Lzw => 2,
            Self::JpegLossless => 3,
            Self::JpegXr => 4,
            Self::Zstd => 5,
            Self::Zstd1 => 6,
            Self::Chunked => 7,
            Self::CameraRaw(value) | Self::SystemRaw(value) | Self::Unknown(value) => value,
        }
    }
}

/// Pyramid classification stored in a DV entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PyramidType {
    /// No pyramid.
    None,
    /// One subblock per pyramid level.
    SingleSubblock,
    /// Multiple subblocks per pyramid level.
    MultiSubblock,
    /// Unknown pyramid value.
    Unknown(u8),
}

impl PyramidType {
    fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::SingleSubblock,
            2 => Self::MultiSubblock,
            value => Self::Unknown(value),
        }
    }

    /// Return the raw CZI pyramid value.
    #[must_use]
    pub const fn raw(self) -> u8 {
        match self {
            Self::None => 0,
            Self::SingleSubblock => 1,
            Self::MultiSubblock => 2,
            Self::Unknown(value) => value,
        }
    }
}

/// A parsed DV directory entry. It describes one tile or plane, never a dense frame.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectoryEntry {
    /// Original two-byte schema.
    pub schema_type: [u8; 2],
    /// Pixel storage type.
    pub pixel_type: PixelType,
    /// Subblock segment location.
    pub file_position: u64,
    /// Multi-file part number.
    pub file_part: i32,
    /// Pixel compression.
    pub compression: CompressionMode,
    /// Pyramid classification.
    pub pyramid_type: PyramidType,
    /// All logical dimensions in file order, including M and unknown codes.
    pub dimensions: Vec<DimensionEntry>,
}

impl DirectoryEntry {
    /// Return the M index when the entry contains one.
    #[must_use]
    pub fn m_index(&self) -> Option<i32> {
        self.dimensions
            .iter()
            .find(|dimension| dimension.code.is_mosaic())
            .map(|dimension| dimension.start)
    }

    /// Return the encoded DV entry size.
    ///
    /// # Errors
    ///
    /// Returns [`CziError::Overflow`] if the dimension count or encoded size cannot be
    /// represented.
    pub fn encoded_size(&self) -> Result<u64, CziError> {
        let count = u64::try_from(self.dimensions.len()).map_err(|_| CziError::Overflow {
            context: "dimension count conversion",
        })?;
        checked_add(
            DV_FIXED_SIZE,
            checked_mul(count, DIMENSION_ENTRY_SIZE, "DV dimension bytes")?,
            "DV entry bytes",
        )
    }

    /// Compute the stored byte count when the pixel type is known.
    #[must_use]
    pub fn stored_byte_size(&self) -> Option<u64> {
        let items = self.dimensions.iter().try_fold(1_u64, |total, dimension| {
            total.checked_mul(u64::from(dimension.stored_size))
        })?;
        self.pixel_type.bytes_per_item()?.checked_mul(items)
    }
}

/// Summary-directory information for one image tile.
#[derive(Clone, Debug, PartialEq)]
pub struct TileIndex {
    /// The summary directory entry.
    pub entry: DirectoryEntry,
}

/// Lazily resolved location and sizes for one image subblock.
#[derive(Clone, Debug, PartialEq)]
pub struct TilePayload {
    /// Validated subblock segment header.
    pub segment: SegmentHeader,
    /// Inline subblock metadata location.
    pub metadata_offset: u64,
    /// Inline subblock metadata bytes.
    pub metadata_size: u64,
    /// Pixel payload location.
    pub data_offset: u64,
    /// Pixel payload bytes, compressed or raw according to `entry.compression`.
    pub data_size: u64,
    /// Inline subblock attachment location.
    pub attachment_offset: u64,
    /// Inline subblock attachment bytes.
    pub attachment_size: u64,
}

/// Pixels decoded from one CZI tile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodedPixels {
    /// Eight-bit grayscale values.
    Gray8(Vec<u8>),
    /// Sixteen-bit grayscale values decoded from CZI little-endian storage.
    Gray16(Vec<u16>),
}

/// One decoded two-dimensional CZI tile.
///
/// This type deliberately represents one stored tile only. It does not assemble mosaics or
/// request a dense plane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedTile {
    /// Stored X dimension in pixels.
    pub width: u32,
    /// Stored Y dimension in pixels.
    pub height: u32,
    /// Decoded grayscale pixels in row-major order.
    pub pixels: DecodedPixels,
}

/// Global metadata XML and its segment location.
#[derive(Clone, Debug, PartialEq)]
pub struct MetadataIndex {
    /// Metadata segment header.
    pub segment: SegmentHeader,
    /// XML location in the source.
    pub xml_offset: u64,
    /// XML byte length.
    pub xml_size: u64,
    /// Metadata attachment byte length.
    pub attachment_size: u64,
    /// UTF-8 XML content, bounded by `ParseOptions`.
    pub xml: String,
}

/// One attachment directory entry and its payload location.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentIndex {
    /// Original A1 directory entry.
    pub entry: AttachmentEntry,
    /// Validated attachment segment header.
    pub segment: SegmentHeader,
    /// Attachment payload location.
    pub data_offset: u64,
    /// Attachment payload size.
    pub data_size: u64,
}

/// Attachment directory entry with unknown fields retained where useful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentEntry {
    /// Original two-byte schema.
    pub schema_type: [u8; 2],
    /// Attachment segment location.
    pub file_position: u64,
    /// Multi-file part number.
    pub file_part: i32,
    /// Content GUID bytes.
    pub content_guid: [u8; 16],
    /// Eight-byte content type, without NUL padding.
    pub content_file_type: String,
    /// UTF-8 attachment name, without NUL padding.
    pub name: String,
}

/// The complete tile-first index of one CZI source.
#[derive(Clone, Debug, PartialEq)]
pub struct DatasetIndex {
    /// Source length and revision at indexing time.
    pub source: SourceInfo,
    /// CZI file header.
    pub file_header: FileHeader,
    /// Indexed image tiles.
    pub tiles: Vec<TileIndex>,
    /// Global metadata, if the header points to one.
    pub metadata: Option<MetadataIndex>,
    /// Non-fatal global metadata read, size, or encoding diagnostics.
    pub metadata_diagnostics: Vec<String>,
    /// Attachment directory entries.
    pub attachments: Vec<AttachmentIndex>,
}

impl DatasetIndex {
    /// Return the number of indexed tiles.
    #[must_use]
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Return the tile at an index without allocating image pixels.
    #[must_use]
    pub fn tile(&self, index: usize) -> Option<&TileIndex> {
        self.tiles.get(index)
    }
}

/// A read-only CZI dataset backed by a shared random-access source.
///
/// The public API exposes tile locations and caller-provided tile reads. It deliberately has no
/// full-frame or dense-mosaic operation.
pub struct CziDataset {
    source: Arc<dyn RandomAccessSource>,
    index: DatasetIndex,
    options: ParseOptions,
}

impl fmt::Debug for CziDataset {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CziDataset")
            .field("source", &self.source.info())
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl CziDataset {
    /// Open and index a source using default limits.
    ///
    /// # Errors
    ///
    /// Returns a structured parse or source error when the CZI is malformed, unsupported, or
    /// cannot be read within the configured safety limits.
    pub fn open<S>(source: S) -> Result<Self, CziError>
    where
        S: RandomAccessSource + 'static,
    {
        Self::open_with_options(source, ParseOptions::default())
    }

    /// Open and index a source using explicit safety limits.
    ///
    /// # Errors
    ///
    /// Returns a structured parse or source error when the CZI is malformed, unsupported, or
    /// cannot be read within the configured safety limits.
    pub fn open_with_options<S>(source: S, options: ParseOptions) -> Result<Self, CziError>
    where
        S: RandomAccessSource + 'static,
    {
        Self::open_shared_with_options(Arc::new(source), options)
    }

    /// Open and index an already shared source.
    ///
    /// # Errors
    ///
    /// Returns a structured parse or source error when the CZI is malformed or cannot be read.
    pub fn open_shared(source: Arc<dyn RandomAccessSource>) -> Result<Self, CziError> {
        Self::open_shared_with_options(source, ParseOptions::default())
    }

    /// Open and index an already shared source using explicit limits.
    ///
    /// # Errors
    ///
    /// Returns a structured parse or source error when the CZI is malformed, unsupported, or
    /// cannot be read within the configured safety limits.
    pub fn open_shared_with_options(
        source: Arc<dyn RandomAccessSource>,
        options: ParseOptions,
    ) -> Result<Self, CziError> {
        let index = parse_index(&source, options)?;
        Ok(Self {
            source,
            index,
            options,
        })
    }

    /// Return the immutable tile-first index.
    #[must_use]
    pub const fn index(&self) -> &DatasetIndex {
        &self.index
    }

    /// Return the shared source information.
    #[must_use]
    pub fn source_info(&self) -> SourceInfo {
        self.source.info()
    }

    /// Resolve one tile's subblock header and inline DV descriptor on demand.
    ///
    /// Opening a dataset does not read subblock headers. This method performs the bounded reads
    /// needed for one tile and validates its inline descriptor against the summary entry.
    ///
    /// # Errors
    ///
    /// Returns a structured parse or source error when the tile header, descriptor, or payload
    /// extent is malformed.
    pub fn tile_payload(&self, index: usize) -> Result<TilePayload, CziError> {
        let tile = self.index.tiles.get(index).ok_or(CziError::Missing {
            what: "tile",
            offset: u64::try_from(index).map_err(|_| CziError::Overflow {
                context: "tile index conversion",
            })?,
        })?;
        parse_tile(
            &self.source,
            &tile.entry,
            self.index.file_header.file_part,
            self.options,
        )
    }

    /// Read one tile's pixel payload into a caller-provided buffer.
    ///
    /// The buffer must be at least the compressed/raw payload size. No codec or dense image
    /// allocation is performed.
    ///
    /// # Errors
    ///
    /// Returns an error when `index` is not a tile, the buffer is too small, or the source read
    /// fails.
    pub fn read_tile_data(&self, index: usize, buffer: &mut [u8]) -> Result<usize, CziError> {
        let tile = self.tile_payload(index)?;
        let size = usize::try_from(tile.data_size).map_err(|_| CziError::Overflow {
            context: "tile payload size conversion",
        })?;
        if buffer.len() < size {
            return Err(CziError::InvalidSegment {
                offset: tile.data_offset,
                reason: format!("tile buffer has {} bytes, needs {size}", buffer.len()),
            });
        }
        self.source.read_at(tile.data_offset, &mut buffer[..size])?;
        Ok(size)
    }

    /// Decode one uncompressed Gray8 or Gray16 tile.
    ///
    /// The subblock descriptor and payload are resolved only for the requested tile. X and Y
    /// use their stored sizes; every other stored dimension must be one, so this method never
    /// creates a dense multi-dimensional plane or mosaic.
    ///
    /// # Errors
    ///
    /// Returns [`CziError::UnsupportedPixel`] for pixel types other than Gray8 and Gray16,
    /// [`CziError::UnsupportedCompression`] for every compressed payload, or a structured
    /// source/format/allocation error when the tile cannot safely be decoded.
    pub fn decoded_tile(&self, index: usize) -> Result<DecodedTile, CziError> {
        let tile = self.index.tiles.get(index).ok_or(CziError::Missing {
            what: "tile",
            offset: u64::try_from(index).map_err(|_| CziError::Overflow {
                context: "tile index conversion",
            })?,
        })?;
        let payload = self.tile_payload(index)?;
        if !matches!(tile.entry.compression, CompressionMode::Uncompressed) {
            return Err(CziError::UnsupportedCompression {
                compression: tile.entry.compression,
            });
        }
        let (width, height) = stored_xy_dimensions(&tile.entry)?;
        let pixels = checked_mul(
            u64::from(width),
            u64::from(height),
            "decoded tile pixel count",
        )?;

        match tile.entry.pixel_type {
            PixelType::Gray8 => {
                let bytes = read_vec(&self.source, payload.data_offset, payload.data_size)?;
                Ok(DecodedTile {
                    width,
                    height,
                    pixels: DecodedPixels::Gray8(bytes),
                })
            }
            PixelType::Gray16 => {
                let bytes = read_vec(&self.source, payload.data_offset, payload.data_size)?;
                let pixel_count = usize::try_from(pixels).map_err(|_| CziError::Overflow {
                    context: "decoded Gray16 pixel count conversion",
                })?;
                let mut values = Vec::new();
                try_reserve_exact(
                    &mut values,
                    pixel_count,
                    "decoded Gray16 pixels",
                    checked_mul(pixels, 2, "decoded Gray16 byte count")?,
                )?;
                for value in bytes.chunks_exact(2) {
                    values.push(u16::from_le_bytes([value[0], value[1]]));
                }
                Ok(DecodedTile {
                    width,
                    height,
                    pixels: DecodedPixels::Gray16(values),
                })
            }
            pixel_type => Err(CziError::UnsupportedPixel { pixel_type }),
        }
    }
}

fn stored_xy_dimensions(entry: &DirectoryEntry) -> Result<(u32, u32), CziError> {
    let mut width = None;
    let mut height = None;
    for dimension in &entry.dimensions {
        match dimension.code {
            DimensionCode::X => width = Some(dimension.stored_size),
            DimensionCode::Y => height = Some(dimension.stored_size),
            code if dimension.stored_size != 1 => {
                return Err(CziError::UnsupportedTileDimensions {
                    code,
                    stored_size: dimension.stored_size,
                });
            }
            _ => {}
        }
    }
    let width = width.ok_or(CziError::Missing {
        what: "stored X dimension",
        offset: entry.file_position,
    })?;
    let height = height.ok_or(CziError::Missing {
        what: "stored Y dimension",
        offset: entry.file_position,
    })?;
    Ok((width, height))
}

fn parse_index(
    source: &Arc<dyn RandomAccessSource>,
    options: ParseOptions,
) -> Result<DatasetIndex, CziError> {
    let source_info = source.info();
    let (file_header, header_metadata_diagnostic) = parse_file_header(source)?;
    if file_header.update_pending {
        return Err(CziError::UpdatePending {
            offset: file_header.segment.offset,
        });
    }
    if file_header.directory_position == 0 {
        return Err(CziError::Missing {
            what: "subblock directory",
            offset: 0,
        });
    }
    let directory = parse_segment(source, file_header.directory_position)?;
    expect_kind(&directory, SegmentKind::Directory, "ZISRAWDIRECTORY")?;
    let entries = parse_directory(source, directory, options)?;
    for entry in &entries {
        ensure_file_part("tile", file_header.file_part, entry.file_part)?;
    }
    let mut tiles = Vec::new();
    try_reserve_exact(
        &mut tiles,
        entries.len(),
        "tile index",
        u64::try_from(entries.len()).map_err(|_| CziError::Overflow {
            context: "tile index count conversion",
        })?,
    )?;
    tiles.extend(entries.into_iter().map(|entry| TileIndex { entry }));
    let (metadata, mut metadata_diagnostics) = if file_header.metadata_position == 0 {
        (None, Vec::new())
    } else {
        match parse_metadata(
            source,
            file_header.metadata_position,
            options.max_metadata_bytes,
        ) {
            Ok(metadata) => (Some(metadata), Vec::new()),
            Err(error) => (None, vec![format!("Global metadata was skipped: {error}")]),
        }
    };
    if let Some(diagnostic) = header_metadata_diagnostic {
        metadata_diagnostics.push(diagnostic);
    }
    let attachments = if file_header.attachment_directory_position == 0 {
        Vec::new()
    } else {
        parse_attachment_directory(
            source,
            file_header.attachment_directory_position,
            file_header.file_part,
            options,
        )?
    };
    Ok(DatasetIndex {
        source: source_info,
        file_header,
        tiles,
        metadata,
        metadata_diagnostics,
        attachments,
    })
}

fn parse_file_header(
    source: &Arc<dyn RandomAccessSource>,
) -> Result<(FileHeader, Option<String>), CziError> {
    let segment = parse_segment(source, 0)?;
    expect_kind(&segment, SegmentKind::File, "ZISRAWFILE")?;
    require_data_size(&segment, FILE_HEADER_DATA_SIZE, "file header")?;
    let data = read_data(source, &segment, 0, FILE_HEADER_DATA_SIZE)?;
    let mut primary_file_guid = [0; 16];
    primary_file_guid.copy_from_slice(&data[16..32]);
    let mut file_guid = [0; 16];
    file_guid.copy_from_slice(&data[32..48]);
    let directory_position = nonnegative_i64(le_i64(&data, 52), "directory position", 52)?;
    let raw_metadata_position = le_i64(&data, 60);
    let (metadata_position, metadata_diagnostic) = if raw_metadata_position < 0 {
        (
            0,
            Some(String::from(
                "Global metadata was skipped: metadata position at offset 60 is negative.",
            )),
        )
    } else {
        (
            u64::try_from(raw_metadata_position)
                .expect("nonnegative metadata position converts to u64"),
            None,
        )
    };
    let attachment_directory_position =
        nonnegative_i64(le_i64(&data, 72), "attachment directory position", 72)?;
    Ok((
        FileHeader {
            segment,
            major_version: le_u32(&data, 0),
            minor_version: le_u32(&data, 4),
            primary_file_guid,
            file_guid,
            file_part: le_i32(&data, 48),
            directory_position,
            metadata_position,
            update_pending: le_i32(&data, 68) != 0,
            attachment_directory_position,
        },
        metadata_diagnostic,
    ))
}

#[allow(clippy::too_many_lines)]
fn parse_directory(
    source: &Arc<dyn RandomAccessSource>,
    segment: SegmentHeader,
    options: ParseOptions,
) -> Result<Vec<DirectoryEntry>, CziError> {
    require_data_size(&segment, DIRECTORY_FIXED_SIZE, "subblock directory")?;
    if segment.used_size > options.max_index_bytes {
        return Err(CziError::LimitExceeded {
            kind: "summary directory bytes",
            value: segment.used_size,
            maximum: options.max_index_bytes,
        });
    }
    let directory_data = read_data(source, &segment, 0, segment.used_size)?;
    let count = le_i32(&directory_data, 0);
    if count < 0 {
        return Err(CziError::InvalidNumber {
            kind: "directory entry count",
            value: i64::from(count),
            offset: segment.offset,
        });
    }
    let count = u64::try_from(count).map_err(|_| CziError::Overflow {
        context: "directory entry count conversion",
    })?;
    if count > options.max_directory_entries {
        return Err(CziError::InvalidNumber {
            kind: "directory entry count",
            value: i64::try_from(count).unwrap_or(i64::MAX),
            offset: segment.offset,
        });
    }
    let payload_size = u64::try_from(directory_data.len())
        .map_err(|_| CziError::Overflow {
            context: "directory payload length conversion",
        })?
        .checked_sub(DIRECTORY_FIXED_SIZE)
        .ok_or(CziError::InvalidSegment {
            offset: segment.offset,
            reason: String::from("used data is smaller than directory fixed data"),
        })?;
    let minimum_bytes = checked_mul(count, DV_FIXED_SIZE, "directory minimum entry bytes")?;
    if minimum_bytes > payload_size {
        return Err(CziError::InvalidNumber {
            kind: "directory entry count",
            value: i64::try_from(count).unwrap_or(i64::MAX),
            offset: segment.offset,
        });
    }
    let mut entries = Vec::new();
    try_reserve_exact(
        &mut entries,
        usize::try_from(count).map_err(|_| CziError::Overflow {
            context: "directory allocation size",
        })?,
        "directory entries",
        count,
    )?;
    let mut cursor = usize::try_from(DIRECTORY_FIXED_SIZE).map_err(|_| CziError::Overflow {
        context: "directory fixed size conversion",
    })?;
    let dv_fixed_size = usize::try_from(DV_FIXED_SIZE).map_err(|_| CziError::Overflow {
        context: "DV fixed size conversion",
    })?;
    let mut total_dimensions = 0_u64;
    for _ in 0..count {
        let fixed_end = cursor
            .checked_add(dv_fixed_size)
            .ok_or(CziError::Overflow {
                context: "directory fixed prefix end",
            })?;
        if fixed_end > directory_data.len() {
            return Err(CziError::InvalidSegment {
                offset: segment.offset,
                reason: String::from("directory payload ended inside DV fixed prefix"),
            });
        }
        let entry_offset = checked_add(
            segment.offset,
            checked_add(
                SEGMENT_HEADER_SIZE,
                u64::try_from(cursor).map_err(|_| CziError::Overflow {
                    context: "directory cursor conversion",
                })?,
                "directory entry offset",
            )?,
            "directory entry offset",
        )?;
        let schema = [directory_data[cursor], directory_data[cursor + 1]];
        if &schema == b"DE" {
            return Err(CziError::UnsupportedSchema {
                context: "subblock directory",
                schema: String::from("DE"),
            });
        }
        if &schema != b"DV" {
            return Err(CziError::UnsupportedSchema {
                context: "subblock directory",
                schema: display_schema(schema),
            });
        }
        let dimensions_count = le_i32(&directory_data, cursor + 28);
        let dimensions_count = checked_count(
            dimensions_count,
            options.max_dimensions_per_entry,
            "dimension count",
            entry_offset,
        )?;
        if dimensions_count == 0 {
            return Err(CziError::InvalidNumber {
                kind: "dimension count",
                value: 0,
                offset: entry_offset,
            });
        }
        total_dimensions =
            total_dimensions
                .checked_add(dimensions_count)
                .ok_or(CziError::Overflow {
                    context: "aggregate dimension count",
                })?;
        if total_dimensions > options.max_total_dimensions {
            return Err(CziError::LimitExceeded {
                kind: "aggregate dimension count",
                value: total_dimensions,
                maximum: options.max_total_dimensions,
            });
        }
        let dimensions_bytes = checked_mul(
            dimensions_count,
            DIMENSION_ENTRY_SIZE,
            "directory dimension bytes",
        )?;
        let entry_size = checked_add(DV_FIXED_SIZE, dimensions_bytes, "directory entry size")?;
        let end = cursor
            .checked_add(usize::try_from(entry_size).map_err(|_| CziError::Overflow {
                context: "directory entry size conversion",
            })?)
            .ok_or(CziError::Overflow {
                context: "directory payload cursor",
            })?;
        if end > directory_data.len() {
            return Err(CziError::InvalidSegment {
                offset: entry_offset,
                reason: String::from("DV entry extends beyond directory used data"),
            });
        }
        let entry = parse_dv_entry(&directory_data[cursor..end], entry_offset, dimensions_count)?;
        entries.push(entry);
        cursor = end;
    }
    Ok(entries)
}

fn parse_dv_entry(
    bytes: &[u8],
    offset: u64,
    dimensions_count: u64,
) -> Result<DirectoryEntry, CziError> {
    let mut schema_type = [0; 2];
    schema_type.copy_from_slice(&bytes[..2]);
    let mut dimensions = Vec::new();
    try_reserve_exact(
        &mut dimensions,
        usize::try_from(dimensions_count).map_err(|_| CziError::Overflow {
            context: "dimension allocation size",
        })?,
        "dimension entries",
        dimensions_count,
    )?;
    for index in 0..dimensions_count {
        let relative = checked_add(
            DV_FIXED_SIZE,
            checked_mul(index, DIMENSION_ENTRY_SIZE, "dimension entry offset")?,
            "dimension entry offset",
        )?;
        let start = usize::try_from(relative).map_err(|_| CziError::Overflow {
            context: "dimension entry slice offset",
        })?;
        let dimension_size =
            usize::try_from(DIMENSION_ENTRY_SIZE).map_err(|_| CziError::Overflow {
                context: "dimension entry size conversion",
            })?;
        let end = start
            .checked_add(dimension_size)
            .ok_or(CziError::Overflow {
                context: "dimension entry slice end",
            })?;
        let dimension = &bytes[start..end];
        let mut raw_code = [0; 4];
        raw_code.copy_from_slice(&dimension[..4]);
        let logical = le_i32(dimension, 8);
        if logical <= 0 {
            return Err(CziError::InvalidNumber {
                kind: "logical dimension size",
                value: i64::from(logical),
                offset,
            });
        }
        let stored_raw = le_i32(dimension, 16);
        if stored_raw < 0 {
            return Err(CziError::InvalidNumber {
                kind: "stored dimension size",
                value: i64::from(stored_raw),
                offset,
            });
        }
        let logical_size = u32::try_from(logical).map_err(|_| CziError::Overflow {
            context: "logical dimension size conversion",
        })?;
        let stored_size = if stored_raw == 0 {
            logical_size
        } else {
            u32::try_from(stored_raw).map_err(|_| CziError::Overflow {
                context: "stored dimension size conversion",
            })?
        };
        let code = DimensionCode::from_raw(raw_code);
        if dimensions
            .iter()
            .any(|dimension: &DimensionEntry| dimension.code == code)
        {
            return Err(CziError::DuplicateDimension {
                code: code.as_string(),
                offset,
            });
        }
        dimensions.push(DimensionEntry {
            code,
            start: le_i32(dimension, 4),
            logical_size,
            start_coordinate: le_f32(dimension, 12),
            stored_size,
            stored_size_raw: stored_raw,
        });
    }
    Ok(DirectoryEntry {
        schema_type,
        pixel_type: PixelType::from_raw(le_i32(bytes, 2)),
        file_position: nonnegative_i64(
            le_i64(bytes, 6),
            "subblock file position",
            checked_add(offset, 6, "subblock file position offset")?,
        )?,
        file_part: le_i32(bytes, 14),
        compression: CompressionMode::from_raw(le_i32(bytes, 18)),
        pyramid_type: PyramidType::from_raw(bytes[22]),
        dimensions,
    })
}

fn parse_tile(
    source: &Arc<dyn RandomAccessSource>,
    entry: &DirectoryEntry,
    file_part: i32,
    options: ParseOptions,
) -> Result<TilePayload, CziError> {
    ensure_file_part("tile", file_part, entry.file_part)?;
    let segment = parse_segment(source, entry.file_position)?;
    expect_kind(&segment, SegmentKind::Subblock, "ZISRAWSUBBLOCK")?;
    require_data_size(&segment, SUBBLOCK_FIXED_SIZE, "subblock")?;
    let fixed = read_data(source, &segment, 0, SUBBLOCK_FIXED_SIZE)?;
    let metadata_size = nonnegative_i32(
        le_i32(&fixed, 0),
        "subblock metadata size",
        data_offset(&segment, 0)?,
    )?;
    let attachment_size = nonnegative_i32(
        le_i32(&fixed, 4),
        "subblock attachment size",
        data_offset(&segment, 4)?,
    )?;
    let data_size = nonnegative_i64(
        le_i64(&fixed, 8),
        "subblock data size",
        data_offset(&segment, 8)?,
    )?;
    let inline_offset = data_offset(&segment, SUBBLOCK_FIXED_SIZE)?;
    let inline_fixed = read_data(source, &segment, SUBBLOCK_FIXED_SIZE, DV_FIXED_SIZE)?;
    let inline_schema = [inline_fixed[0], inline_fixed[1]];
    if &inline_schema == b"DE" {
        return Err(CziError::UnsupportedSchema {
            context: "subblock",
            schema: String::from("DE"),
        });
    }
    if &inline_schema != b"DV" {
        return Err(CziError::UnsupportedSchema {
            context: "subblock",
            schema: display_schema(inline_schema),
        });
    }
    let inline_count = le_i32(&inline_fixed, 28);
    let inline_count = checked_count(
        inline_count,
        options.max_dimensions_per_entry,
        "inline dimension count",
        inline_offset,
    )?;
    if inline_count == 0 {
        return Err(CziError::InvalidNumber {
            kind: "inline dimension count",
            value: 0,
            offset: inline_offset,
        });
    }
    let inline_size = checked_add(
        DV_FIXED_SIZE,
        checked_mul(inline_count, DIMENSION_ENTRY_SIZE, "inline dimension bytes")?,
        "inline DV entry size",
    )?;
    let inline_bytes = read_data(source, &segment, SUBBLOCK_FIXED_SIZE, inline_size)?;
    let inline_entry = parse_dv_entry(&inline_bytes, inline_offset, inline_count)?;
    reconcile_dv_entries(entry, &inline_entry, inline_offset)?;
    let header_data_size = subblock_header_data_size(inline_size)?;
    require_data_size(&segment, header_data_size, "subblock")?;
    let payload_end = checked_add(
        checked_add(header_data_size, metadata_size, "tile metadata end")?,
        checked_add(data_size, attachment_size, "tile payload sizes")?,
        "tile payload end",
    )?;
    if payload_end > segment.used_size {
        return Err(CziError::InvalidSegment {
            offset: entry.file_position,
            reason: String::from("subblock payload exceeds used segment data"),
        });
    }
    if matches!(entry.compression, CompressionMode::Uncompressed) {
        if let Some(expected) = entry.stored_byte_size() {
            if data_size != expected {
                return Err(CziError::PayloadSizeMismatch {
                    offset: entry.file_position,
                    actual: data_size,
                    expected,
                });
            }
        }
    }
    let metadata_offset = data_offset(&segment, header_data_size)?;
    let data_offset = checked_add(metadata_offset, metadata_size, "tile data offset")?;
    let attachment_offset = checked_add(data_offset, data_size, "tile attachment offset")?;
    Ok(TilePayload {
        segment,
        metadata_offset,
        metadata_size,
        data_offset,
        data_size,
        attachment_offset,
        attachment_size,
    })
}

fn subblock_header_data_size(inline_dv_size: u64) -> Result<u64, CziError> {
    checked_add(
        SUBBLOCK_FIXED_SIZE,
        inline_dv_size,
        "subblock header extent",
    )
    .map(|size| size.max(SUBBLOCK_MIN_DATA_SIZE))
}

fn reconcile_dv_entries(
    summary: &DirectoryEntry,
    inline: &DirectoryEntry,
    offset: u64,
) -> Result<(), CziError> {
    if summary.schema_type != inline.schema_type {
        return Err(descriptor_mismatch("DV", "schema", offset));
    }
    if summary.pixel_type != inline.pixel_type {
        return Err(descriptor_mismatch("DV", "pixel type", offset));
    }
    if summary.file_position != inline.file_position {
        return Err(descriptor_mismatch("DV", "file position", offset));
    }
    if summary.file_part != inline.file_part {
        return Err(descriptor_mismatch("DV", "file part", offset));
    }
    if summary.compression != inline.compression {
        return Err(descriptor_mismatch("DV", "compression", offset));
    }
    if summary.pyramid_type != inline.pyramid_type {
        return Err(descriptor_mismatch("DV", "pyramid type", offset));
    }
    if summary.dimensions.len() != inline.dimensions.len() {
        return Err(descriptor_mismatch("DV", "dimension count", offset));
    }
    for (summary_dimension, inline_dimension) in summary.dimensions.iter().zip(&inline.dimensions) {
        if summary_dimension.code != inline_dimension.code {
            return Err(descriptor_mismatch("DV", "dimension code", offset));
        }
        if summary_dimension.start != inline_dimension.start {
            return Err(descriptor_mismatch("DV", "dimension start", offset));
        }
        if summary_dimension.logical_size != inline_dimension.logical_size {
            return Err(descriptor_mismatch("DV", "logical dimension size", offset));
        }
        if summary_dimension.start_coordinate.to_bits()
            != inline_dimension.start_coordinate.to_bits()
        {
            return Err(descriptor_mismatch("DV", "dimension coordinate", offset));
        }
        if summary_dimension.stored_size_raw != inline_dimension.stored_size_raw {
            return Err(descriptor_mismatch("DV", "stored dimension size", offset));
        }
    }
    Ok(())
}

fn reconcile_attachment_entries(
    summary: &AttachmentEntry,
    inline: &AttachmentEntry,
    offset: u64,
) -> Result<(), CziError> {
    if summary.schema_type != inline.schema_type {
        return Err(descriptor_mismatch("A1", "schema", offset));
    }
    if summary.file_position != inline.file_position {
        return Err(descriptor_mismatch("A1", "file position", offset));
    }
    if summary.file_part != inline.file_part {
        return Err(descriptor_mismatch("A1", "file part", offset));
    }
    if summary.content_guid != inline.content_guid {
        return Err(descriptor_mismatch("A1", "content GUID", offset));
    }
    if summary.content_file_type != inline.content_file_type {
        return Err(descriptor_mismatch("A1", "content file type", offset));
    }
    if summary.name != inline.name {
        return Err(descriptor_mismatch("A1", "name", offset));
    }
    Ok(())
}

fn descriptor_mismatch(context: &'static str, field: &'static str, offset: u64) -> CziError {
    CziError::DescriptorMismatch {
        context,
        field,
        offset,
    }
}

fn ensure_file_part(context: &'static str, expected: i32, actual: i32) -> Result<(), CziError> {
    if expected != actual {
        return Err(CziError::CrossFilePartReference {
            context,
            expected,
            actual,
        });
    }
    Ok(())
}

fn parse_metadata(
    source: &Arc<dyn RandomAccessSource>,
    offset: u64,
    maximum: u64,
) -> Result<MetadataIndex, CziError> {
    let segment = parse_segment(source, offset)?;
    expect_kind(&segment, SegmentKind::Metadata, "ZISRAWMETADATA")?;
    require_data_size(&segment, METADATA_FIXED_SIZE, "metadata")?;
    let fixed = read_data(source, &segment, 0, 8)?;
    let xml_size = nonnegative_i32(
        le_i32(&fixed, 0),
        "metadata XML size",
        data_offset(&segment, 0)?,
    )?;
    let attachment_size = nonnegative_i32(
        le_i32(&fixed, 4),
        "metadata attachment size",
        data_offset(&segment, 4)?,
    )?;
    if xml_size > maximum {
        return Err(CziError::MetadataTooLarge {
            size: xml_size,
            maximum,
        });
    }
    let end = checked_add(
        checked_add(METADATA_FIXED_SIZE, xml_size, "metadata XML end")?,
        attachment_size,
        "metadata attachment end",
    )?;
    if end > segment.used_size {
        return Err(CziError::InvalidSegment {
            offset,
            reason: String::from("metadata content exceeds used segment data"),
        });
    }
    let xml_offset = data_offset(&segment, METADATA_FIXED_SIZE)?;
    let xml = String::from_utf8(read_vec(source, xml_offset, xml_size)?).map_err(|_| {
        CziError::InvalidUtf8 {
            context: "metadata XML",
            offset: xml_offset,
        }
    })?;
    Ok(MetadataIndex {
        segment,
        xml_offset,
        xml_size,
        attachment_size,
        xml,
    })
}

#[allow(clippy::too_many_lines)]
fn parse_attachment_directory(
    source: &Arc<dyn RandomAccessSource>,
    offset: u64,
    file_part: i32,
    options: ParseOptions,
) -> Result<Vec<AttachmentIndex>, CziError> {
    let segment = parse_segment(source, offset)?;
    expect_kind(&segment, SegmentKind::AttachmentDirectory, "ZISRAWATTDIR")?;
    require_data_size(
        &segment,
        ATTACHMENT_DIRECTORY_FIXED_SIZE,
        "attachment directory",
    )?;
    if segment.used_size > options.max_index_bytes {
        return Err(CziError::LimitExceeded {
            kind: "attachment directory bytes",
            value: segment.used_size,
            maximum: options.max_index_bytes,
        });
    }
    let directory_data = read_data(source, &segment, 0, segment.used_size)?;
    let count = le_i32(&directory_data, 0);
    let count = checked_count(
        count,
        options.max_directory_entries,
        "attachment entry count",
        offset,
    )?;
    let payload_size = u64::try_from(directory_data.len())
        .map_err(|_| CziError::Overflow {
            context: "attachment directory length conversion",
        })?
        .checked_sub(ATTACHMENT_DIRECTORY_FIXED_SIZE)
        .ok_or(CziError::InvalidSegment {
            offset,
            reason: String::from("used data is smaller than attachment directory fixed data"),
        })?;
    let required = checked_mul(count, ATTACHMENT_ENTRY_SIZE, "attachment directory bytes")?;
    if required > payload_size {
        return Err(CziError::InvalidNumber {
            kind: "attachment entry count",
            value: i64::try_from(count).unwrap_or(i64::MAX),
            offset,
        });
    }
    let mut attachments = Vec::new();
    try_reserve_exact(
        &mut attachments,
        usize::try_from(count).map_err(|_| CziError::Overflow {
            context: "attachment directory allocation size",
        })?,
        "attachment entries",
        count,
    )?;
    let mut cursor =
        usize::try_from(ATTACHMENT_DIRECTORY_FIXED_SIZE).map_err(|_| CziError::Overflow {
            context: "attachment directory fixed size conversion",
        })?;
    let attachment_entry_size =
        usize::try_from(ATTACHMENT_ENTRY_SIZE).map_err(|_| CziError::Overflow {
            context: "attachment entry size conversion",
        })?;
    for _ in 0..count {
        let entry_end = cursor
            .checked_add(attachment_entry_size)
            .ok_or(CziError::Overflow {
                context: "attachment directory entry end",
            })?;
        if entry_end > directory_data.len() {
            return Err(CziError::InvalidSegment {
                offset,
                reason: String::from("attachment directory ended inside A1 entry"),
            });
        }
        let entry_offset = checked_add(
            segment.offset,
            checked_add(
                SEGMENT_HEADER_SIZE,
                u64::try_from(cursor).map_err(|_| CziError::Overflow {
                    context: "attachment cursor conversion",
                })?,
                "attachment entry offset",
            )?,
            "attachment entry offset",
        )?;
        let entry = parse_attachment_entry(&directory_data[cursor..entry_end], entry_offset)?;
        ensure_file_part("attachment", file_part, entry.file_part)?;
        let attachment_segment = parse_segment(source, entry.file_position)?;
        expect_kind(&attachment_segment, SegmentKind::Attachment, "ZISRAWATTACH")?;
        require_data_size(
            &attachment_segment,
            ATTACHMENT_DIRECTORY_FIXED_SIZE,
            "attachment",
        )?;
        let attachment_fixed = read_data(source, &attachment_segment, 0, 256)?;
        let data_size = nonnegative_i32(
            le_i32(&attachment_fixed, 0),
            "attachment data size",
            data_offset(&attachment_segment, 0)?,
        )?;
        let inline_entry_offset = data_offset(&attachment_segment, 16)?;
        let inline_entry = parse_attachment_entry(&attachment_fixed[16..144], inline_entry_offset)?;
        reconcile_attachment_entries(&entry, &inline_entry, inline_entry_offset)?;
        let data_end = checked_add(
            ATTACHMENT_DIRECTORY_FIXED_SIZE,
            data_size,
            "attachment data end",
        )?;
        if data_end > attachment_segment.used_size {
            return Err(CziError::InvalidSegment {
                offset: entry.file_position,
                reason: String::from("attachment data exceeds used segment data"),
            });
        }
        let data_offset = data_offset(&attachment_segment, ATTACHMENT_DIRECTORY_FIXED_SIZE)?;
        attachments.push(AttachmentIndex {
            entry,
            segment: attachment_segment,
            data_offset,
            data_size,
        });
        cursor = entry_end;
    }
    Ok(attachments)
}

fn parse_attachment_entry(bytes: &[u8], offset: u64) -> Result<AttachmentEntry, CziError> {
    let mut schema_type = [0; 2];
    schema_type.copy_from_slice(&bytes[..2]);
    if &schema_type != b"A1" {
        return Err(CziError::UnsupportedSchema {
            context: "attachment directory",
            schema: display_schema(schema_type),
        });
    }
    let mut content_guid = [0; 16];
    content_guid.copy_from_slice(&bytes[24..40]);
    let content_type_offset = checked_add(offset, 40, "attachment content type offset")?;
    let name_offset = checked_add(offset, 48, "attachment name offset")?;
    let content_file_type = fixed_string(&bytes[40..48]).map_err(|_| CziError::InvalidUtf8 {
        context: "attachment content type",
        offset: content_type_offset,
    })?;
    let name = fixed_string(&bytes[48..128]).map_err(|_| CziError::InvalidUtf8 {
        context: "attachment name",
        offset: name_offset,
    })?;
    let file_position = nonnegative_i64(
        le_i64(bytes, 12),
        "attachment file position",
        checked_add(offset, 12, "attachment file position offset")?,
    )?;
    Ok(AttachmentEntry {
        schema_type,
        file_position,
        file_part: le_i32(bytes, 20),
        content_guid,
        content_file_type,
        name,
    })
}

fn parse_segment(
    source: &Arc<dyn RandomAccessSource>,
    offset: u64,
) -> Result<SegmentHeader, CziError> {
    let bytes = read_vec(source, offset, SEGMENT_HEADER_SIZE)?;
    let mut id = [0; 16];
    id.copy_from_slice(&bytes[..16]);
    let allocated_size = nonnegative_i64(
        le_i64(&bytes, 16),
        "segment allocated size",
        checked_add(offset, 16, "segment allocated size offset")?,
    )?;
    let on_disk_used_size = nonnegative_i64(
        le_i64(&bytes, 24),
        "segment used size",
        checked_add(offset, 24, "segment used size offset")?,
    )?;
    if allocated_size == 0 {
        return Err(CziError::InvalidSegment {
            offset,
            reason: String::from("allocated data size is zero"),
        });
    }
    if on_disk_used_size > allocated_size {
        return Err(CziError::InvalidSegment {
            offset,
            reason: String::from("used data size exceeds allocated data size"),
        });
    }
    let allocated_end = checked_add(
        checked_add(offset, SEGMENT_HEADER_SIZE, "segment header end")?,
        allocated_size,
        "segment allocated end",
    )?;
    if allocated_end > source.info().length {
        return Err(CziError::InvalidSegment {
            offset,
            reason: format!(
                "segment ends at {allocated_end}, source length is {}",
                source.info().length
            ),
        });
    }
    Ok(SegmentHeader {
        offset,
        id,
        allocated_size,
        used_size: if on_disk_used_size == 0 {
            allocated_size
        } else {
            on_disk_used_size
        },
    })
}

fn expect_kind(
    segment: &SegmentHeader,
    expected: SegmentKind,
    expected_name: &str,
) -> Result<(), CziError> {
    if segment.kind() != expected {
        return Err(CziError::UnexpectedSegment {
            offset: segment.offset,
            expected: expected_name.to_owned(),
            found: segment.id_string(),
        });
    }
    Ok(())
}

fn require_data_size(
    segment: &SegmentHeader,
    minimum: u64,
    what: &'static str,
) -> Result<(), CziError> {
    if segment.used_size < minimum {
        return Err(CziError::InvalidSegment {
            offset: segment.offset,
            reason: format!(
                "{what} data is {} bytes, needs {minimum}",
                segment.used_size
            ),
        });
    }
    Ok(())
}

fn data_offset(segment: &SegmentHeader, relative: u64) -> Result<u64, CziError> {
    checked_add(
        checked_add(segment.offset, SEGMENT_HEADER_SIZE, "segment data offset")?,
        relative,
        "segment data offset",
    )
}

fn read_data(
    source: &Arc<dyn RandomAccessSource>,
    segment: &SegmentHeader,
    relative: u64,
    size: u64,
) -> Result<Vec<u8>, CziError> {
    let end = checked_add(relative, size, "segment data range")?;
    if end > segment.used_size {
        return Err(CziError::InvalidSegment {
            offset: segment.offset,
            reason: String::from("requested data exceeds used segment data"),
        });
    }
    read_vec(source, data_offset(segment, relative)?, size)
}

fn read_vec(
    source: &Arc<dyn RandomAccessSource>,
    offset: u64,
    size: u64,
) -> Result<Vec<u8>, CziError> {
    source.read_vec_at(offset, size).map_err(CziError::from)
}

fn try_reserve_exact<T>(
    values: &mut Vec<T>,
    additional: usize,
    context: &'static str,
    size: u64,
) -> Result<(), CziError> {
    let size = usize::try_from(size).map_err(|_| CziError::Overflow {
        context: "allocation size conversion",
    })?;
    values
        .try_reserve_exact(additional)
        .map_err(|_| CziError::Allocation { context, size })
}

fn checked_count(
    value: i32,
    maximum: u64,
    kind: &'static str,
    offset: u64,
) -> Result<u64, CziError> {
    if value < 0 {
        return Err(CziError::InvalidNumber {
            kind,
            value: i64::from(value),
            offset,
        });
    }
    let value = u64::try_from(value).map_err(|_| CziError::Overflow {
        context: "signed count conversion",
    })?;
    if value > maximum {
        return Err(CziError::InvalidNumber {
            kind,
            value: i64::try_from(value).unwrap_or(i64::MAX),
            offset,
        });
    }
    Ok(value)
}

fn checked_add(left: u64, right: u64, context: &'static str) -> Result<u64, CziError> {
    left.checked_add(right)
        .ok_or(CziError::Overflow { context })
}

fn checked_mul(left: u64, right: u64, context: &'static str) -> Result<u64, CziError> {
    left.checked_mul(right)
        .ok_or(CziError::Overflow { context })
}

fn nonnegative_i32(value: i32, kind: &'static str, offset: u64) -> Result<u64, CziError> {
    if value < 0 {
        return Err(CziError::InvalidNumber {
            kind,
            value: i64::from(value),
            offset,
        });
    }
    u64::try_from(value).map_err(|_| CziError::Overflow {
        context: "signed i32 conversion",
    })
}

fn nonnegative_i64(value: i64, kind: &'static str, offset: u64) -> Result<u64, CziError> {
    if value < 0 {
        return Err(CziError::InvalidNumber {
            kind,
            value,
            offset,
        });
    }
    u64::try_from(value).map_err(|_| CziError::Overflow {
        context: "signed i64 conversion",
    })
}

fn fixed_string(bytes: &[u8]) -> Result<String, std::str::Utf8Error> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end]).map(str::to_owned)
}

fn display_schema(schema: [u8; 2]) -> String {
    String::from_utf8_lossy(&schema).into_owned()
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

fn le_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(le_u32(bytes, offset).to_le_bytes())
}

fn le_i64(bytes: &[u8], offset: usize) -> i64 {
    let mut value = [0; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    i64::from_le_bytes(value)
}

fn le_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(le_u32(bytes, offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::MemorySource;

    #[test]
    fn unknown_dimension_codes_are_preserved() {
        let mut bytes = vec![0; 52];
        bytes[..2].copy_from_slice(b"DV");
        bytes[28..32].copy_from_slice(&1_i32.to_le_bytes());
        bytes[32..36].copy_from_slice(b"Q\0\0\0");
        bytes[40..44].copy_from_slice(&2_i32.to_le_bytes());
        let entry = parse_dv_entry(&bytes, 0, 1).expect("entry parses");
        assert_eq!(
            entry.dimensions[0].code,
            DimensionCode::Unknown(*b"Q\0\0\0")
        );
        assert_eq!(entry.dimensions[0].logical_size, 2);
    }

    #[test]
    fn source_read_bounds_are_checked() {
        let source = MemorySource::new(Arc::<[u8]>::from([1, 2, 3]));
        let error = source.read_vec_at(2, 2).expect_err("read must fail");
        assert!(matches!(error, SourceError::OutOfBounds { .. }));
    }

    #[test]
    fn subblock_header_extent_has_dimension_boundaries() {
        assert_eq!(subblock_header_data_size(32 + 10 * 20).unwrap(), 256);
        assert_eq!(subblock_header_data_size(32 + 11 * 20).unwrap(), 268);
        assert_eq!(subblock_header_data_size(32 + 12 * 20).unwrap(), 288);
    }
}
