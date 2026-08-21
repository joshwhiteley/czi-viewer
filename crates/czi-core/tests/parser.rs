use std::path::PathBuf;

use czi_core::{
    AttachmentIndex, CompressionMode, CziDataset, CziError, DimensionCode, LocalFileSource,
    MemorySource, ParseOptions, PixelType, PyramidType, RandomAccessSource, SourceError,
};

const SEGMENT_HEADER_SIZE: usize = 32;
const FILE_HEADER_DATA_SIZE: usize = 512;
const DIRECTORY_DATA_SIZE: usize = 128;
const DV_FIXED_SIZE: usize = 32;
const DIMENSION_SIZE: usize = 20;

#[derive(Debug)]
struct SyntheticFile {
    bytes: Vec<u8>,
    directory_offset: u64,
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

    let mut payload = [0; 4];
    assert_eq!(dataset.read_tile_data(0, &mut payload).expect("payload"), 4);
    assert_eq!(payload, [1, 2, 3, 4]);
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
    let error = CziDataset::open(MemorySource::new(truncated)).expect_err("truncated");
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
fn rejects_metadata_over_limit() {
    let file = synthetic_file(false);
    let error = CziDataset::open_with_options(
        MemorySource::new(file.bytes),
        ParseOptions::default().with_max_metadata_bytes(2),
    )
    .expect_err("metadata limit");
    assert!(matches!(error, CziError::MetadataTooLarge { .. }));
}

#[test]
fn checks_overflow_and_random_access_bounds() {
    let mut file = synthetic_file(false);
    let header_offset = 0;
    file.bytes[header_offset + SEGMENT_HEADER_SIZE + 52..header_offset + SEGMENT_HEADER_SIZE + 60]
        .copy_from_slice(&(u64::MAX - 10).to_le_bytes());
    let error = CziDataset::open(MemorySource::new(file.bytes)).expect_err("overflow");
    assert!(matches!(
        error,
        CziError::Source(SourceError::RangeOverflow { .. })
    ));

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
            "/Users/josh/Downloads/czi-tests/tf_HADA_BOD_d1_bridge_060225-02.czi",
            12_731_678_336_u64,
            2_700_usize,
            12_usize,
            3_usize,
        ),
        (
            "CZI_PLATE_FIXTURE",
            "/Users/josh/Downloads/ts_04042026_Bb_plate1_rep1_ML-01 (1).czi",
            32_498_112_u64,
            3_usize,
            1_usize,
            3_usize,
        ),
    ];
    for (
        environment,
        default_path,
        expected_length,
        expected_tiles,
        expected_scenes,
        expected_channels,
    ) in fixtures
    {
        let path = std::env::var_os(environment)
            .map_or_else(|| PathBuf::from(default_path), PathBuf::from);
        if !path.exists() {
            eprintln!("skipping missing fixture {}", path.display());
            continue;
        }
        let source = LocalFileSource::open(path).expect("fixture source");
        assert_eq!(source.info().length, expected_length);
        let dataset = CziDataset::open(source).expect("fixture index");
        assert_eq!(dataset.index().tile_count(), expected_tiles);
        assert!(dataset.index().tiles.iter().all(|tile| tile.data_size > 0));
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
    let subblock_offset =
        append_segment(&mut file, b"ZISRAWSUBBLOCK", subblock_data(), Some(256 + 4));
    let attachment_directory_offset = if with_attachments {
        let attachment_offset =
            append_segment(&mut file, b"ZISRAWATTACH", attachment_data(), Some(256 + 3));
        append_segment(
            &mut file,
            b"ZISRAWATTDIR",
            attachment_directory_data(attachment_offset),
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
    }
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
    let mut data = vec![0; 260];
    data[8..16].copy_from_slice(&4_u64.to_le_bytes());
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

fn attachment_data() -> Vec<u8> {
    let mut data = vec![0; 259];
    data[0..4].copy_from_slice(&3_u32.to_le_bytes());
    let entry = &mut data[16..144];
    entry[0..2].copy_from_slice(b"A1");
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

fn dimension(entry: &mut [u8], index: usize, code: [u8; 4], start: i32, logical: i32, stored: i32) {
    let offset = DV_FIXED_SIZE + index * DIMENSION_SIZE;
    entry[offset..offset + 4].copy_from_slice(&code);
    entry[offset + 4..offset + 8].copy_from_slice(&start.to_le_bytes());
    entry[offset + 8..offset + 12].copy_from_slice(&logical.to_le_bytes());
    entry[offset + 12..offset + 16].copy_from_slice(&0.0_f32.to_le_bytes());
    entry[offset + 16..offset + 20].copy_from_slice(&stored.to_le_bytes());
}

fn append_segment(file: &mut Vec<u8>, id: &[u8], mut data: Vec<u8>, used: Option<usize>) -> u64 {
    while data.len() % 32 != 0 {
        data.push(0);
    }
    let offset = u64::try_from(file.len()).expect("file offset");
    let used = used.unwrap_or(data.len());
    let mut header = [0; SEGMENT_HEADER_SIZE];
    header[..id.len()].copy_from_slice(id);
    header[16..24].copy_from_slice(
        &u64::try_from(data.len())
            .expect("allocated size")
            .to_le_bytes(),
    );
    header[24..32].copy_from_slice(&u64::try_from(used).expect("used size").to_le_bytes());
    file.extend_from_slice(&header);
    file.extend_from_slice(&data);
    offset
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
