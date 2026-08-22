//! Pure-Rust building blocks for reading and examining CZI image documents.

mod metadata;
mod parser;
mod query;
mod source;

pub use metadata::{
    ChannelMetadata, MetadataAttribute, MetadataDiagnostic, MetadataDocument, MetadataNode,
    MetadataParseLimits, MetadataParseOptions, MetadataSummary, PhysicalPixelSize,
    summarize_metadata,
};
pub use parser::{
    AttachmentEntry, AttachmentIndex, CompressionMode, CziDataset, CziError, DatasetIndex,
    DecodedPixels, DecodedTile, DimensionCode, DimensionEntry, DirectoryEntry, FileHeader,
    MetadataIndex, ParseOptions, PixelType, PyramidType, SegmentHeader, SegmentKind, TileIndex,
    TilePayload,
};
pub use query::{
    PhysicalSize, PlaneInfo, PlaneKey, PlaneSelector, PyramidScale, SceneId, SparseAxisChoices,
    SpatialRect, TileHit, TileId, TileQueryError, TileQueryIndex, ViewQuery, ViewQueryResult,
};
pub use source::{
    BlockCache, BlockCacheConfig, BlockCacheError, CacheStats, DEFAULT_BLOCK_SIZE,
    DEFAULT_MAX_BYTES, LocalFileSource, MemorySource, RandomAccessSource, SourceError, SourceInfo,
};

/// The library version exposed in diagnostic reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
