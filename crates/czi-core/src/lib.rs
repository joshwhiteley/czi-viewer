//! Pure-Rust building blocks for reading and examining CZI image documents.

mod source;

pub use source::{LocalFileSource, MemorySource, RandomAccessSource, SourceError, SourceInfo};

/// The library version exposed in diagnostic reports.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
