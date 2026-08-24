use czi_core::{
    AttachmentIndex, BlockCache, CompressionMode, CziDataset, CziError, DecodedPixels,
    DimensionCode, LocalFileSource, MemorySource, ParseOptions, PixelType, PyramidType,
    RandomAccessSource, SourceError,
};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[path = "support/synthetic_czi.rs"]
mod synthetic_czi;

use synthetic_czi::{append_segment, dimension};

const SEGMENT_HEADER_SIZE: usize = 32;
const FILE_HEADER_DATA_SIZE: usize = 512;
const DIRECTORY_DATA_SIZE: usize = 128;
const DV_FIXED_SIZE: usize = 32;
const DIMENSION_SIZE: usize = 20;

#[derive(Debug)]
struct SyntheticFile {
    bytes: Vec<u8>,
    directory_offset: u64,
    subblock_offset: u64,
    attachment_offset: Option<u64>,
}

#[test]
fn parses_tile_first_index_and_metadata_without_dense_pixels() {
    let file = synthetic_file(true);
    let dataset = open(file.bytes.clone());
    let index = dataset.index();

    assert_eq!(index.file_header.major_version, 1);
    assert_eq!(index.file_header.minor_version, 0);
    assert_eq!(index.tile_count(), 1);
    assert_eq!(
        index.metadata.as_ref().expect("metadata").xml,
        "<ImageDocument/>"
    );
    assert_eq!(index.attachments.len(), 1);

    let tile = index.tile(0).expect("tile");
    assert_eq!(tile.entry.pixel_type, PixelType::Gray16);
    assert_eq!(tile.entry.compression, CompressionMode::Uncompressed);
    assert_eq!(tile.entry.pyramid_type, PyramidType::MultiSubblock);
    assert_eq!(tile.entry.m_index(), Some(7));
    assert_eq!(tile.entry.dimensions[1].stored_size, 2);
    assert_eq!(
        tile.entry.dimensions[3].code,
        DimensionCode::Unknown(*b"Q\0\0\0")
    );
    assert_eq!(tile.entry.stored_byte_size(), Some(64));

    let mut payload = [0; 64];
    assert_eq!(
        dataset.read_tile_data(0, &mut payload).expect("payload"),
        64
    );
    assert_eq!(&payload[..4], [1, 2, 3, 4]);
}

#[test]
fn block_cache_reuses_czi_parser_ranges_from_a_fake_source() {
    let file = synthetic_file(false);
    let reads = Arc::new(AtomicUsize::new(0));
    let source = CountingSource {
        inner: Arc::new(MemorySource::new(file.bytes)),
        reads: Arc::clone(&reads),
    };
    let cache = BlockCache::with_defaults(source).expect("default block cache");
    let dataset = CziDataset::open(cache).expect("cached synthetic CZI");
    let reads_after_open = reads.load(Ordering::Relaxed);
    assert!(reads_after_open > 0);

    dataset.tile_payload(0).expect("first tile parse");
    let reads_after_first_parse = reads.load(Ordering::Relaxed);
    dataset.tile_payload(0).expect("repeated tile parse");
    assert_eq!(reads.load(Ordering::Relaxed), reads_after_first_parse);
    assert_eq!(reads_after_first_parse, reads_after_open);
}

#[test]
fn decodes_uncompressed_gray_tiles_with_stored_xy_dimensions() {
    let mut gray16 = synthetic_file(false);
    make_single_plane(&mut gray16, 1, 32);
    let data = tile_data_range(&gray16);
    gray16.bytes[data..data + 8].copy_from_slice(&[0x34, 0x12, 0xcd, 0xab, 0, 0, 0xff, 0xff]);
    let tile = open(gray16.bytes).decoded_tile(0).expect("Gray16 tile");
    assert_eq!((tile.width, tile.height), (8, 2));
    assert_eq!(
        tile.pixels,
        DecodedPixels::Gray16(vec![
            0x1234, 0xabcd, 0, 0xffff, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0
        ])
    );

    let mut gray8 = synthetic_file(false);
    make_single_plane(&mut gray8, 0, 16);
    let data = tile_data_range(&gray8);
    gray8.bytes[data..data + 16]
        .copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 255]);
    let tile = open(gray8.bytes).decoded_tile(0).expect("Gray8 tile");
    assert_eq!((tile.width, tile.height), (8, 2));
    assert_eq!(
        tile.pixels,
        DecodedPixels::Gray8(vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 255])
    );
}

#[test]
fn decode_reports_unsupported_pixel_compression_and_dimensions() {
    let dataset = open(synthetic_file(false).bytes);
    let error = dataset
        .decoded_tile(0)
        .expect_err("non-spatial stored dimension");
    assert!(matches!(
        error,
        CziError::UnsupportedTileDimensions {
            code,
            stored_size: 2,
        } if code == DimensionCode::Unknown(*b"Q\0\0\0")
    ));

    let mut compressed = synthetic_file(false);
    set_tile_compression(&mut compressed, 1);
    let error = open(compressed.bytes)
        .decoded_tile(0)
        .expect_err("compressed tile");
    assert!(matches!(
        error,
        CziError::UnsupportedCompression {
            compression: CompressionMode::Jpeg,
        }
    ));

    let mut unsupported = synthetic_file(false);
    make_single_plane(&mut unsupported, 2, 64);
    let error = open(unsupported.bytes)
        .decoded_tile(0)
        .expect_err("unsupported pixel type");
    assert!(matches!(
        error,
        CziError::UnsupportedPixel {
            pixel_type: PixelType::Gray32Float,
        }
    ));
}

#[test]
fn parses_attachment_directory_entries() {
    let file = synthetic_file(true);
    let dataset = open(file.bytes);
    let attachment: &AttachmentIndex = &dataset.index().attachments[0];
    assert_eq!(attachment.entry.name, "thumbnail");
    assert_eq!(attachment.entry.content_file_type, "JPG");
    assert_eq!(attachment.data_size, 3);
}

#[test]
fn rejects_bad_magic() {
    let mut file = synthetic_file(false).bytes;
    file[..10].copy_from_slice(b"NOTCZI!!!!");
    let error = CziDataset::open(MemorySource::new(file)).expect_err("bad magic");
    assert!(matches!(error, CziError::UnexpectedSegment { .. }));
}

#[test]
fn rejects_truncated_input() {
    let file = synthetic_file(false).bytes;
    let truncated = file[..file.len() - 1].to_vec();
    let dataset = CziDataset::open(MemorySource::new(truncated)).expect("lazy open");
    let error = dataset.tile_payload(0).expect_err("truncated tile");
    assert!(matches!(error, CziError::InvalidSegment { .. }));
}

#[test]
fn rejects_invalid_counts_and_sizes() {
    let mut count_file = synthetic_file(false);
    let count_offset =
        usize::try_from(count_file.directory_offset).expect("offset") + SEGMENT_HEADER_SIZE;
    count_file.bytes[count_offset..count_offset + 4].copy_from_slice(&(-1_i32).to_le_bytes());
    let error = CziDataset::open(MemorySource::new(count_file.bytes)).expect_err("negative count");
    assert!(matches!(error, CziError::InvalidNumber { .. }));

    let mut size_file = synthetic_file(false);
    let dimension_size_offset = usize::try_from(size_file.directory_offset).expect("offset")
        + SEGMENT_HEADER_SIZE
        + DIRECTORY_DATA_SIZE
        + DV_FIXED_SIZE
        + 8;
    size_file.bytes[dimension_size_offset..dimension_size_offset + 4]
        .copy_from_slice(&0_i32.to_le_bytes());
    let error = CziDataset::open(MemorySource::new(size_file.bytes)).expect_err("zero size");
    assert!(matches!(error, CziError::InvalidNumber { .. }));

    let mut short_header = synthetic_file(false);
    short_header.bytes[24..32].copy_from_slice(&80_i64.to_le_bytes());
    let error = CziDataset::open(MemorySource::new(short_header.bytes)).expect_err("short header");
    assert!(matches!(error, CziError::InvalidSegment { .. }));
}

#[test]
fn rejects_de_schema_explicitly() {
    let mut file = synthetic_file(false);
    let schema_offset = usize::try_from(file.directory_offset).expect("offset")
        + SEGMENT_HEADER_SIZE
        + DIRECTORY_DATA_SIZE;
    file.bytes[schema_offset..schema_offset + 2].copy_from_slice(b"DE");
    let error = CziDataset::open(MemorySource::new(file.bytes)).expect_err("DE schema");
    assert!(matches!(
        error,
        CziError::UnsupportedSchema {
            context: "subblock directory",
            ..
        }
    ));
}

#[test]
fn metadata_over_limit_is_non_fatal_and_diagnostic() {
    let file = synthetic_file(false);
    let dataset = CziDataset::open_with_options(
        MemorySource::new(file.bytes),
        ParseOptions::default().with_max_metadata_bytes(2),
    )
    .expect("metadata limit must not prevent image opening");
    assert!(dataset.index().metadata.is_none());
    assert!(dataset.index().metadata_diagnostics[0].contains("metadata XML"));

    let mut file = synthetic_file(false);
    let metadata_position = SEGMENT_HEADER_SIZE + 60;
    file.bytes[metadata_position..metadata_position + 8].copy_from_slice(&(-1_i64).to_le_bytes());
    let dataset = CziDataset::open(MemorySource::new(file.bytes))
        .expect("negative metadata pointer must not prevent image opening");
    assert!(dataset.index().metadata.is_none());
    assert!(dataset.index().metadata_diagnostics[0].contains("metadata position"));

    let file = synthetic_file(false);
    let error = CziDataset::open_with_options(
        MemorySource::new(file.bytes),
        ParseOptions::default().with_max_total_dimensions(3),
    )
    .expect_err("aggregate dimension limit");
    assert!(matches!(error, CziError::LimitExceeded { .. }));

    let file = synthetic_file(false);
    let error = CziDataset::open_with_options(
        MemorySource::new(file.bytes),
        ParseOptions::default().with_max_index_bytes(1),
    )
    .expect_err("index byte limit");
    assert!(matches!(error, CziError::LimitExceeded { .. }));
}

#[test]
fn defers_subblock_validation_and_reconciles_inline_dv() {
    let mut file = synthetic_file(false);
    let inline_pixel_type =
        usize::try_from(file.subblock_offset).expect("offset") + SEGMENT_HEADER_SIZE + 16 + 2;
    file.bytes[inline_pixel_type..inline_pixel_type + 4].copy_from_slice(&0_i32.to_le_bytes());
    let dataset = open(file.bytes);
    let error = dataset.tile_payload(0).expect_err("inline mismatch");
    assert!(matches!(
        error,
        CziError::DescriptorMismatch { context: "DV", .. }
    ));

    let mut file = synthetic_file(false);
    let inline_coordinate = usize::try_from(file.subblock_offset).expect("offset")
        + SEGMENT_HEADER_SIZE
        + 16
        + DV_FIXED_SIZE
        + 12;
    file.bytes[inline_coordinate..inline_coordinate + 4]
        .copy_from_slice(&1.0_f32.to_bits().to_le_bytes());
    let dataset = open(file.bytes);
    let error = dataset.tile_payload(0).expect_err("coordinate mismatch");
    assert!(matches!(
        error,
        CziError::DescriptorMismatch {
            field: "dimension coordinate",
            ..
        }
    ));
}

#[test]
fn validates_uncompressed_payload_size_lazily() {
    let mut file = synthetic_file(false);
    let fixed_data_size =
        usize::try_from(file.subblock_offset).expect("offset") + SEGMENT_HEADER_SIZE + 8;
    file.bytes[fixed_data_size..fixed_data_size + 8].copy_from_slice(&63_i64.to_le_bytes());
    let dataset = open(file.bytes);
    let error = dataset.tile_payload(0).expect_err("payload mismatch");
    assert!(matches!(
        error,
        CziError::PayloadSizeMismatch {
            expected: 64,
            actual: 63,
            ..
        }
    ));

    let mut file = synthetic_file(false);
    let fixed_data_size =
        usize::try_from(file.subblock_offset).expect("offset") + SEGMENT_HEADER_SIZE + 8;
    file.bytes[fixed_data_size..fixed_data_size + 8].copy_from_slice(&(-1_i64).to_le_bytes());
    let dataset = open(file.bytes);
    let error = dataset.tile_payload(0).expect_err("negative payload size");
    assert!(matches!(
        error,
        CziError::InvalidNumber {
            kind: "subblock data size",
            ..
        }
    ));
}

#[test]
fn rejects_update_pending_cross_part_duplicates_and_negative_signed_fields() {
    let mut update = synthetic_file(false);
    update.bytes[SEGMENT_HEADER_SIZE + 68..SEGMENT_HEADER_SIZE + 72]
        .copy_from_slice(&1_i32.to_le_bytes());
    let error = CziDataset::open(MemorySource::new(update.bytes)).expect_err("update pending");
    assert!(matches!(error, CziError::UpdatePending { .. }));

    let mut cross_part = synthetic_file(false);
    let file_part = usize::try_from(cross_part.directory_offset).expect("offset")
        + SEGMENT_HEADER_SIZE
        + DIRECTORY_DATA_SIZE
        + 14;
    cross_part.bytes[file_part..file_part + 4].copy_from_slice(&1_i32.to_le_bytes());
    let error = CziDataset::open(MemorySource::new(cross_part.bytes)).expect_err("cross part");
    assert!(matches!(
        error,
        CziError::CrossFilePartReference {
            context: "tile",
            ..
        }
    ));

    let mut duplicate = synthetic_file(false);
    let duplicate_code = usize::try_from(duplicate.directory_offset).expect("offset")
        + SEGMENT_HEADER_SIZE
        + DIRECTORY_DATA_SIZE
        + DV_FIXED_SIZE
        + 3 * DIMENSION_SIZE;
    duplicate.bytes[duplicate_code..duplicate_code + 4].copy_from_slice(b"X\0\0\0");
    let error = CziDataset::open(MemorySource::new(duplicate.bytes)).expect_err("duplicate");
    assert!(matches!(error, CziError::DuplicateDimension { .. }));

    let mut negative_segment = synthetic_file(false);
    negative_segment.bytes[16..24].copy_from_slice(&(-1_i64).to_le_bytes());
    let error =
        CziDataset::open(MemorySource::new(negative_segment.bytes)).expect_err("negative segment");
    assert!(matches!(
        error,
        CziError::InvalidNumber {
            kind: "segment allocated size",
            ..
        }
    ));
}

#[test]
fn reconciles_inline_a1_and_rejects_signed_metadata_and_attachment_sizes() {
    let mut file = synthetic_file(true);
    let attachment_offset = file.attachment_offset.expect("attachment");
    let name = usize::try_from(attachment_offset).expect("offset") + SEGMENT_HEADER_SIZE + 16 + 48;
    file.bytes[name..name + 9].copy_from_slice(b"different");
    let error = CziDataset::open(MemorySource::new(file.bytes)).expect_err("A1 mismatch");
    assert!(matches!(
        error,
        CziError::DescriptorMismatch { context: "A1", .. }
    ));

    let mut metadata = synthetic_file(false);
    let metadata_offset = metadata.bytes[SEGMENT_HEADER_SIZE + 60..SEGMENT_HEADER_SIZE + 68]
        .try_into()
        .map(i64::from_le_bytes)
        .expect("metadata pointer");
    let metadata_size = usize::try_from(u64::try_from(metadata_offset).expect("pointer"))
        .expect("offset")
        + SEGMENT_HEADER_SIZE;
    metadata.bytes[metadata_size..metadata_size + 4].copy_from_slice(&(-1_i32).to_le_bytes());
    let dataset = CziDataset::open(MemorySource::new(metadata.bytes))
        .expect("bad metadata must not prevent image opening");
    assert!(dataset.index().metadata.is_none());
    assert!(dataset.index().metadata_diagnostics[0].contains("metadata XML size"));

    let mut attachment = synthetic_file(true);
    let attachment_offset = attachment.attachment_offset.expect("attachment");
    let data_size = usize::try_from(attachment_offset).expect("offset") + SEGMENT_HEADER_SIZE;
    attachment.bytes[data_size..data_size + 4].copy_from_slice(&(-1_i32).to_le_bytes());
    let error =
        CziDataset::open(MemorySource::new(attachment.bytes)).expect_err("negative attachment");
    assert!(matches!(
        error,
        CziError::InvalidNumber {
            kind: "attachment data size",
            ..
        }
    ));
}

#[test]
fn checks_overflow_and_random_access_bounds() {
    let mut file = synthetic_file(false);
    let header_offset = 0;
    file.bytes[header_offset + SEGMENT_HEADER_SIZE + 52..header_offset + SEGMENT_HEADER_SIZE + 60]
        .copy_from_slice(&(u64::MAX - 10).to_le_bytes());
    let error = CziDataset::open(MemorySource::new(file.bytes)).expect_err("overflow");
    assert!(matches!(error, CziError::InvalidNumber { .. }));

    let source = MemorySource::new(vec![0; 4]);
    let mut target = [0; 2];
    let error = source.read_at(3, &mut target).expect_err("out of bounds");
    assert!(matches!(error, SourceError::OutOfBounds { .. }));
}

#[test]
#[ignore = "requires local non-redistributable fixtures and CZI_RUN_FIXTURES=1"]
fn index_local_fixtures_without_pixel_allocation() {
    if std::env::var_os("CZI_RUN_FIXTURES").is_none() {
        return;
    }
    let fixtures = [
        (
            "CZI_HADA_FIXTURE",
            12_731_678_336_u64,
            2_700_usize,
            12_usize,
            3_usize,
        ),
        (
            "CZI_PLATE_FIXTURE",
            32_498_112_u64,
            3_usize,
            1_usize,
            3_usize,
        ),
    ];
    for (environment, expected_length, expected_tiles, expected_scenes, expected_channels) in
        fixtures
    {
        let Some(path) = std::env::var_os(environment).map(PathBuf::from) else {
            eprintln!("skipping fixture because {environment} is not set");
            continue;
        };
        if !path.exists() {
            eprintln!("skipping missing fixture {}", path.display());
            continue;
        }
        let source = LocalFileSource::open(path).expect("fixture source");
        assert_eq!(source.info().length, expected_length);
        let dataset = CziDataset::open(source).expect("fixture index");
        assert_eq!(dataset.index().tile_count(), expected_tiles);
        assert!(dataset.index().metadata.is_some());
        assert!(
            dataset
                .index()
                .tiles
                .iter()
                .all(|tile| tile.entry.pixel_type == PixelType::Gray16)
        );
        assert!(
            dataset
                .index()
                .tiles
                .iter()
                .all(|tile| tile.entry.compression == CompressionMode::Uncompressed)
        );
        assert_eq!(
            distinct_dimension_starts(&dataset, DimensionCode::S).len(),
            expected_scenes
        );
        assert_eq!(
            distinct_dimension_starts(&dataset, DimensionCode::C).len(),
            expected_channels
        );
    }
}

#[test]
#[ignore = "requires the local plate fixture and CZI_RUN_FIXTURES=1"]
fn decodes_one_tile_from_local_plate_fixture() {
    if std::env::var_os("CZI_RUN_FIXTURES").is_none() {
        return;
    }
    let Some(path) = std::env::var_os("CZI_PLATE_FIXTURE").map(PathBuf::from) else {
        eprintln!("skipping fixture because CZI_PLATE_FIXTURE is not set");
        return;
    };
    if !path.exists() {
        eprintln!("skipping missing fixture {}", path.display());
        return;
    }
    let dataset = CziDataset::open(LocalFileSource::open(path).expect("fixture source"))
        .expect("fixture index");
    let tile = dataset.decoded_tile(0).expect("fixture tile");
    assert!(tile.width > 0 && tile.height > 0);
    let pixel_count = usize::try_from(u64::from(tile.width) * u64::from(tile.height))
        .expect("fixture pixel count");
    match tile.pixels {
        DecodedPixels::Gray16(values) => assert_eq!(values.len(), pixel_count),
        DecodedPixels::Gray8(_) => panic!("plate fixture should be Gray16"),
    }
}

#[test]
#[ignore = "requires the 2,700-tile HADA fixture and CZI_RUN_FIXTURES=1"]
fn opening_hada_uses_bounded_source_reads() {
    if std::env::var_os("CZI_RUN_FIXTURES").is_none() {
        return;
    }
    let Some(path) = std::env::var_os("CZI_HADA_FIXTURE").map(PathBuf::from) else {
        eprintln!("skipping fixture because CZI_HADA_FIXTURE is not set");
        return;
    };
    if !path.exists() {
        eprintln!("skipping missing fixture {}", path.display());
        return;
    }
    let reads = Arc::new(AtomicUsize::new(0));
    let source = CountingSource {
        inner: Arc::new(LocalFileSource::open(path).expect("fixture source")),
        reads: Arc::clone(&reads),
    };
    let dataset = CziDataset::open(source).expect("fixture index");
    assert_eq!(dataset.index().tile_count(), 2_700);
    assert!(reads.load(Ordering::Relaxed) <= 16);
}

#[test]
#[ignore = "requires downloaded public fixture cache and CZI_RUN_FIXTURES=1"]
fn index_public_fixture_cache_without_pixel_allocation() {
    if std::env::var_os("CZI_RUN_FIXTURES").is_none() {
        return;
    }
    let Some(cache) = public_fixture_dir() else {
        eprintln!(
            "skipping public fixtures: set CZI_PUBLIC_FIXTURE_DIR or provide the repository-relative test-data/cache"
        );
        return;
    };
    for name in [
        "T=3_Z=5_CH=2.czi",
        "Zeiss-5-JXR.czi",
        "Zeiss-5-SlidePreview-Zstd1-HiLo.czi",
    ] {
        let path = cache.join(name);
        if !path.exists() {
            eprintln!("skipping missing public fixture {}", path.display());
            continue;
        }
        let dataset = CziDataset::open(LocalFileSource::open(path).expect("fixture source"))
            .expect("public fixture index");
        assert!(!dataset.index().tiles.is_empty());
    }
}

fn public_fixture_dir() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CZI_PUBLIC_FIXTURE_DIR") {
        let path = PathBuf::from(path);
        return path.is_dir().then_some(path);
    }
    let repository_cache = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test-data/cache");
    repository_cache
        .canonicalize()
        .ok()
        .filter(|path| path.is_dir())
}

fn open(bytes: Vec<u8>) -> CziDataset {
    CziDataset::open(MemorySource::new(bytes)).expect("synthetic CZI")
}

#[derive(Clone)]
struct CountingSource {
    inner: Arc<dyn RandomAccessSource>,
    reads: Arc<AtomicUsize>,
}

impl RandomAccessSource for CountingSource {
    fn info(&self) -> czi_core::SourceInfo {
        self.inner.info()
    }

    fn read_at(&self, offset: u64, destination: &mut [u8]) -> Result<(), SourceError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at(offset, destination)
    }
}

fn synthetic_file(with_attachments: bool) -> SyntheticFile {
    let mut file = Vec::new();
    let header_offset = append_segment(
        &mut file,
        b"ZISRAWFILE",
        vec![0; FILE_HEADER_DATA_SIZE],
        None,
    );

    let directory_offset = append_segment(
        &mut file,
        b"ZISRAWDIRECTORY",
        directory_data(0),
        Some(DIRECTORY_DATA_SIZE + DV_FIXED_SIZE + 4 * DIMENSION_SIZE),
    );
    let metadata_offset = append_segment(
        &mut file,
        b"ZISRAWMETADATA",
        metadata_data(b"<ImageDocument/>", 0),
        None,
    );
    let subblock_offset = append_segment(
        &mut file,
        b"ZISRAWSUBBLOCK",
        subblock_data(),
        Some(256 + 64),
    );
    let mut attachment_offset = None;
    let attachment_directory_offset = if with_attachments {
        let attachment_position = append_segment(
            &mut file,
            b"ZISRAWATTACH",
            attachment_data(0),
            Some(256 + 3),
        );
        let attachment_offset_usize =
            usize::try_from(attachment_position).expect("attachment offset");
        let attachment_entry = attachment_offset_usize + SEGMENT_HEADER_SIZE + 16;
        file[attachment_entry + 12..attachment_entry + 20]
            .copy_from_slice(&attachment_position.to_le_bytes());
        attachment_offset = Some(attachment_position);
        append_segment(
            &mut file,
            b"ZISRAWATTDIR",
            attachment_directory_data(attachment_position),
            None,
        )
    } else {
        0
    };

    let directory_offset_usize = usize::try_from(directory_offset).expect("directory offset");
    let entry_offset = directory_offset_usize + SEGMENT_HEADER_SIZE + DIRECTORY_DATA_SIZE;
    file[entry_offset + 6..entry_offset + 14].copy_from_slice(&subblock_offset.to_le_bytes());

    let subblock_offset_usize = usize::try_from(subblock_offset).expect("subblock offset");
    let inline_entry = subblock_offset_usize + SEGMENT_HEADER_SIZE + 16;
    file[inline_entry + 6..inline_entry + 14].copy_from_slice(&subblock_offset.to_le_bytes());

    let header = usize::try_from(header_offset).expect("header offset") + SEGMENT_HEADER_SIZE;
    file[header + 52..header + 60].copy_from_slice(&directory_offset.to_le_bytes());
    file[header + 60..header + 68].copy_from_slice(&metadata_offset.to_le_bytes());
    file[header + 72..header + 80].copy_from_slice(&attachment_directory_offset.to_le_bytes());
    file[header..header + 4].copy_from_slice(&1_u32.to_le_bytes());

    SyntheticFile {
        bytes: file,
        directory_offset,
        subblock_offset,
        attachment_offset,
    }
}

fn make_single_plane(file: &mut SyntheticFile, pixel_type: i32, data_size: i64) {
    for entry in [directory_entry_offset(file), inline_entry_offset(file)] {
        file.bytes[entry + 2..entry + 6].copy_from_slice(&pixel_type.to_le_bytes());
        let stored_size = entry + DV_FIXED_SIZE + 3 * DIMENSION_SIZE + 16;
        file.bytes[stored_size..stored_size + 4].copy_from_slice(&1_i32.to_le_bytes());
    }
    let data_size_offset =
        usize::try_from(file.subblock_offset).expect("offset") + SEGMENT_HEADER_SIZE + 8;
    file.bytes[data_size_offset..data_size_offset + 8].copy_from_slice(&data_size.to_le_bytes());
}

fn set_tile_compression(file: &mut SyntheticFile, compression: i32) {
    for entry in [directory_entry_offset(file), inline_entry_offset(file)] {
        file.bytes[entry + 18..entry + 22].copy_from_slice(&compression.to_le_bytes());
    }
}

fn directory_entry_offset(file: &SyntheticFile) -> usize {
    usize::try_from(file.directory_offset).expect("offset")
        + SEGMENT_HEADER_SIZE
        + DIRECTORY_DATA_SIZE
}

fn inline_entry_offset(file: &SyntheticFile) -> usize {
    usize::try_from(file.subblock_offset).expect("offset") + SEGMENT_HEADER_SIZE + 16
}

fn tile_data_range(file: &SyntheticFile) -> usize {
    usize::try_from(file.subblock_offset).expect("offset") + SEGMENT_HEADER_SIZE + 256
}

fn directory_data(_subblock_offset: u64) -> Vec<u8> {
    let mut data = vec![0; DIRECTORY_DATA_SIZE + DV_FIXED_SIZE + 4 * DIMENSION_SIZE];
    data[0..4].copy_from_slice(&1_i32.to_le_bytes());
    let entry = &mut data[DIRECTORY_DATA_SIZE..];
    entry[0..2].copy_from_slice(b"DV");
    entry[2..6].copy_from_slice(&1_i32.to_le_bytes());
    entry[18..22].copy_from_slice(&0_i32.to_le_bytes());
    entry[22] = 2;
    entry[28..32].copy_from_slice(&4_i32.to_le_bytes());
    dimension(entry, 0, *b"X\0\0\0", 0, 8, 0);
    dimension(entry, 1, *b"Y\0\0\0", 0, 4, 2);
    dimension(entry, 2, *b"M\0\0\0", 7, 1, 0);
    dimension(entry, 3, *b"Q\0\0\0", 3, 2, 0);
    data
}

fn subblock_data() -> Vec<u8> {
    let mut data = vec![0; 320];
    data[8..16].copy_from_slice(&64_i64.to_le_bytes());
    let entry = &mut data[16..16 + DV_FIXED_SIZE + 4 * DIMENSION_SIZE];
    entry[0..2].copy_from_slice(b"DV");
    entry[2..6].copy_from_slice(&1_i32.to_le_bytes());
    entry[18..22].copy_from_slice(&0_i32.to_le_bytes());
    entry[22] = 2;
    entry[28..32].copy_from_slice(&4_i32.to_le_bytes());
    dimension(entry, 0, *b"X\0\0\0", 0, 8, 0);
    dimension(entry, 1, *b"Y\0\0\0", 0, 4, 2);
    dimension(entry, 2, *b"M\0\0\0", 7, 1, 0);
    dimension(entry, 3, *b"Q\0\0\0", 3, 2, 0);
    data[256..320].fill(0);
    data[256..260].copy_from_slice(&[1, 2, 3, 4]);
    data
}

fn metadata_data(xml: &[u8], attachment_size: u32) -> Vec<u8> {
    let mut data = vec![0; 256 + xml.len()];
    data[0..4].copy_from_slice(&u32::try_from(xml.len()).expect("XML size").to_le_bytes());
    data[4..8].copy_from_slice(&attachment_size.to_le_bytes());
    data[256..].copy_from_slice(xml);
    data
}

fn attachment_data(file_position: u64) -> Vec<u8> {
    let mut data = vec![0; 259];
    data[0..4].copy_from_slice(&3_u32.to_le_bytes());
    let entry = &mut data[16..144];
    entry[0..2].copy_from_slice(b"A1");
    entry[12..20].copy_from_slice(&file_position.to_le_bytes());
    entry[40..43].copy_from_slice(b"JPG");
    entry[48..57].copy_from_slice(b"thumbnail");
    data[256..259].copy_from_slice(b"jpg");
    data
}

fn attachment_directory_data(attachment_offset: u64) -> Vec<u8> {
    let mut data = vec![0; 256 + 128];
    data[0..4].copy_from_slice(&1_i32.to_le_bytes());
    let entry = &mut data[256..];
    entry[0..2].copy_from_slice(b"A1");
    entry[12..20].copy_from_slice(&attachment_offset.to_le_bytes());
    entry[40..43].copy_from_slice(b"JPG");
    entry[48..57].copy_from_slice(b"thumbnail");
    data
}

fn distinct_dimension_starts(dataset: &CziDataset, code: DimensionCode) -> Vec<i32> {
    let mut starts = dataset
        .index()
        .tiles
        .iter()
        .flat_map(|tile| tile.entry.dimensions.iter())
        .filter(|dimension| dimension.code == code)
        .map(|dimension| dimension.start)
        .collect::<Vec<_>>();
    starts.sort_unstable();
    starts.dedup();
    if starts.is_empty() {
        starts.push(0);
    }
    starts
}
