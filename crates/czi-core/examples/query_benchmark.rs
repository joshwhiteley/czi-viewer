//! Measure sparse viewport queries on a synthetic 100k-tile geometry index.
//!
//! The large case is built directly as a `DatasetIndex`, so the example measures parser-independent
//! geometry indexing and querying without writing a 40 MiB temporary CZI. It also reports opening
//! and decoding the small synthetic CZI used by the other czi-core examples.
//!
//! Usage: `cargo run -p czi-core --example query_benchmark -- [tile-count] [query-count]`

#[path = "../tests/support/synthetic_czi.rs"]
mod synthetic_czi;

use std::time::Instant;

use czi_core::{
    CompressionMode, CziDataset, DatasetIndex, DimensionCode, DimensionEntry, DirectoryEntry,
    FileHeader, MemorySource, PixelType, PlaneSelector, PyramidType, SourceInfo, SpatialRect,
    TileIndex, TileQueryIndex, ViewQuery,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tile_count = argument(1, 100_000);
    let query_count = argument(2, 2_000);
    if tile_count == 0 || query_count == 0 {
        return Err("tile-count and query-count must be positive".into());
    }

    let index = synthetic_index(tile_count);
    let started = Instant::now();
    let query_index = TileQueryIndex::new(&index)?;
    let index_elapsed = started.elapsed();

    let selector = PlaneSelector::default();
    let started = Instant::now();
    let mut hit_count = 0_usize;
    for query_number in 0..query_count {
        let query_number = i64::try_from(query_number).expect("query count fits i64");
        let viewport = SpatialRect::from_start_size(
            (query_number * 97) % 130_000 - 2_048,
            (query_number * 53) % 900 - 2_048,
            256,
            192,
        )?;
        let query = ViewQuery::new(selector, viewport, 1.0)?;
        hit_count += query_index.query(&query)?.len();
    }
    let query_elapsed = started.elapsed();

    let demo_bytes = synthetic_czi::demo_czi();
    let started = Instant::now();
    let dataset = CziDataset::open(MemorySource::new(demo_bytes))?;
    let open_elapsed = started.elapsed();
    let started = Instant::now();
    let decoded = dataset.decoded_tile(0)?;
    let decode_elapsed = started.elapsed();

    println!(
        "geometry: {tile_count} tiles, {query_count} queries, {hit_count} hits; index={index_elapsed:?}, queries={query_elapsed:?}"
    );
    println!(
        "parser: {} synthetic tiles opened in {open_elapsed:?}; decoded {}x{} in {decode_elapsed:?}",
        dataset.index().tile_count(),
        decoded.width,
        decoded.height
    );
    Ok(())
}

fn argument(index: usize, default: usize) -> usize {
    std::env::args()
        .nth(index)
        .map_or(default, |value| value.parse().expect("numeric argument"))
}

fn synthetic_index(tile_count: usize) -> DatasetIndex {
    let mut tiles = Vec::with_capacity(tile_count);
    for tile_id in 0..tile_count {
        let tile_id_i32 = i32::try_from(tile_id).expect("synthetic tile count fits i32");
        let x = i32::try_from(tile_id % 4_000).expect("x fits i32") * 32 - 2_048;
        let y = i32::try_from(tile_id / 4_000).expect("y fits i32") * 32 - 2_048;
        tiles.push(TileIndex {
            entry: DirectoryEntry {
                schema_type: *b"DV",
                pixel_type: PixelType::Gray8,
                file_position: u64::try_from(tile_id).expect("tile id fits u64"),
                file_part: 0,
                compression: CompressionMode::Uncompressed,
                pyramid_type: PyramidType::None,
                dimensions: vec![
                    dimension(DimensionCode::X, x, 32),
                    dimension(DimensionCode::Y, y, 32),
                    dimension(DimensionCode::M, tile_id_i32, 1),
                ],
            },
        });
    }
    DatasetIndex {
        source: SourceInfo {
            length: 0,
            version: 0,
        },
        file_header: FileHeader {
            segment: czi_core::SegmentHeader {
                offset: 0,
                id: [0; 16],
                allocated_size: 0,
                used_size: 0,
            },
            major_version: 1,
            minor_version: 0,
            primary_file_guid: [0; 16],
            file_guid: [0; 16],
            file_part: 0,
            directory_position: 0,
            metadata_position: 0,
            update_pending: false,
            attachment_directory_position: 0,
        },
        tiles,
        metadata: None,
        metadata_diagnostics: Vec::new(),
        attachments: Vec::new(),
    }
}

fn dimension(code: DimensionCode, start: i32, size: u32) -> DimensionEntry {
    DimensionEntry {
        code,
        start,
        logical_size: size,
        start_coordinate: 0.0,
        stored_size: size,
        stored_size_raw: i32::try_from(size).expect("dimension fits i32"),
    }
}
