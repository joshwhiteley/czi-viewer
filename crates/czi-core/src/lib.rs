//! Pure-Rust building blocks for reading and examining CZI image documents.

mod parser;
mod source;

pub use parser::{
    AttachmentEntry, AttachmentIndex, CompressionMode, CziDataset, CziError, DatasetIndex,
    DimensionCode, DimensionEntry, DirectoryEntry, FileHeader, MetadataIndex, ParseOptions,
    PixelType, PyramidType, SegmentHeader, SegmentKind, TileIndex,
};
pub use source::{LocalFileSource, MemorySource, RandomAccessSource, SourceError, SourceInfo};

/// The library version exposed in diagnostic reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
