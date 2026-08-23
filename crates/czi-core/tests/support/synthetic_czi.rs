//! Small, deterministic CZI builders shared by parser tests and the demo generator.
//!
//! These builders write only the bounded, uncompressed subset exercised by this project. They
//! are test and demo support, not a general CZI authoring library.

#![allow(dead_code)]

pub const SEGMENT_HEADER_SIZE: usize = 32;
const FILE_HEADER_DATA_SIZE: usize = 512;
const DIRECTORY_DATA_SIZE: usize = 128;
const DV_FIXED_SIZE: usize = 32;
const DIMENSION_SIZE: usize = 20;

#[derive(Clone, Copy)]
struct TileSpec {
    channel: i32,
    mosaic: i32,
    x: i32,
    y: i32,
    logical_width: i32,
    logical_height: i32,
    stored_width: i32,
    stored_height: i32,
    pyramid_type: u8,
}

/// Build the deterministic synthetic CZI used in the documentation demo.
///
/// It contains a 2 × 2 Gray16 mosaic for each of Phase, Blue, and Green. Every channel has a
/// native scale and a 2:1 coarse scale whose tiles map one-to-one to native tiles.
pub fn demo_czi() -> Vec<u8> {
    let mut tiles = Vec::new();
    for channel in 0..3 {
        for (stored_size, pyramid_type) in [(16, 2_u8), (32, 0_u8)] {
            for (mosaic, (x, y)) in [(0, (0, 0)), (1, (32, 0)), (2, (0, 32)), (3, (32, 32))] {
                tiles.push(TileSpec {
                    channel,
                    mosaic,
                    x,
                    y,
                    logical_width: 32,
                    logical_height: 32,
                    stored_width: stored_size,
                    stored_height: stored_size,
                    pyramid_type,
                });
            }
        }
    }

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
        directory_data(&tiles),
        Some(DIRECTORY_DATA_SIZE + tiles.len() * (DV_FIXED_SIZE + 4 * DIMENSION_SIZE)),
    );
    let metadata_offset = append_segment(
        &mut file,
        b"ZISRAWMETADATA",
        metadata_data(
            br#"<ImageDocument><Metadata><Information><Image><Dimensions><Channels><Channel Id="Channel:0" Name="Phase"/><Channel Id="Channel:1" Name="Blue"/><Channel Id="Channel:2" Name="Green"/></Channels></Dimensions></Image></Information></Metadata></ImageDocument>"#,
        ),
        None,
    );

    let mut subblock_offsets = Vec::with_capacity(tiles.len());
    for (index, tile) in tiles.iter().enumerate() {
        let pixels = demo_pixels(*tile, index);
        let subblock_offset = append_segment(
            &mut file,
            b"ZISRAWSUBBLOCK",
            subblock_data(*tile, &pixels),
            Some(256 + pixels.len() * 2),
        );
        let inline_entry =
            usize::try_from(subblock_offset).expect("subblock offset") + SEGMENT_HEADER_SIZE + 16;
        file[inline_entry + 6..inline_entry + 14].copy_from_slice(&subblock_offset.to_le_bytes());
        subblock_offsets.push(subblock_offset);
    }

    let directory_start = usize::try_from(directory_offset).expect("directory offset")
        + SEGMENT_HEADER_SIZE
        + DIRECTORY_DATA_SIZE;
    for (index, subblock_offset) in subblock_offsets.into_iter().enumerate() {
        let entry = directory_start + index * (DV_FIXED_SIZE + 4 * DIMENSION_SIZE);
        file[entry + 6..entry + 14].copy_from_slice(&subblock_offset.to_le_bytes());
    }
    let header = usize::try_from(header_offset).expect("header offset") + SEGMENT_HEADER_SIZE;
    file[header..header + 4].copy_from_slice(&1_u32.to_le_bytes());
    file[header + 52..header + 60].copy_from_slice(&directory_offset.to_le_bytes());
    file[header + 60..header + 68].copy_from_slice(&metadata_offset.to_le_bytes());
    file
}

fn directory_data(tiles: &[TileSpec]) -> Vec<u8> {
    let entry_size = DV_FIXED_SIZE + 4 * DIMENSION_SIZE;
    let mut data = vec![0; DIRECTORY_DATA_SIZE + tiles.len() * entry_size];
    data[..4].copy_from_slice(
        &i32::try_from(tiles.len())
            .expect("tile count")
            .to_le_bytes(),
    );
    for (index, tile) in tiles.iter().enumerate() {
        write_dv_entry(
            &mut data[DIRECTORY_DATA_SIZE + index * entry_size..][..entry_size],
            *tile,
        );
    }
    data
}

fn subblock_data(tile: TileSpec, pixels: &[u16]) -> Vec<u8> {
    let mut data = vec![0; 256 + pixels.len() * 2];
    data[8..16].copy_from_slice(
        &i64::try_from(pixels.len() * 2)
            .expect("payload size")
            .to_le_bytes(),
    );
    write_dv_entry(&mut data[16..16 + DV_FIXED_SIZE + 4 * DIMENSION_SIZE], tile);
    for (destination, pixel) in data[256..].chunks_exact_mut(2).zip(pixels) {
        destination.copy_from_slice(&pixel.to_le_bytes());
    }
    data
}

fn write_dv_entry(entry: &mut [u8], tile: TileSpec) {
    entry[..2].copy_from_slice(b"DV");
    entry[2..6].copy_from_slice(&1_i32.to_le_bytes()); // Gray16
    entry[18..22].copy_from_slice(&0_i32.to_le_bytes()); // uncompressed
    entry[22] = tile.pyramid_type;
    entry[28..32].copy_from_slice(&4_i32.to_le_bytes());
    dimension(
        entry,
        0,
        *b"X\0\0\0",
        tile.x,
        tile.logical_width,
        tile.stored_width,
    );
    dimension(
        entry,
        1,
        *b"Y\0\0\0",
        tile.y,
        tile.logical_height,
        tile.stored_height,
    );
    dimension(entry, 2, *b"C\0\0\0", tile.channel, 1, 1);
    dimension(entry, 3, *b"M\0\0\0", tile.mosaic, 1, 1);
}

fn demo_pixels(tile: TileSpec, tile_index: usize) -> Vec<u16> {
    let width = usize::try_from(tile.stored_width).expect("positive width");
    let height = usize::try_from(tile.stored_height).expect("positive height");
    (0..width * height)
        .map(|index| {
            let x = index % width;
            let y = index / width;
            let channel = u32::try_from(tile.channel).expect("nonnegative channel") * 14_000;
            let mosaic = u32::try_from(tile.mosaic).expect("nonnegative mosaic") * 2_000;
            let pattern = u32::try_from((x * 257 + y * 521 + tile_index * 97) % 2_000)
                .expect("bounded pattern");
            u16::try_from(4_000 + channel + mosaic + pattern).expect("Gray16 value")
        })
        .collect()
}

fn metadata_data(xml: &[u8]) -> Vec<u8> {
    let mut data = vec![0; 256 + xml.len()];
    data[..4].copy_from_slice(&u32::try_from(xml.len()).expect("XML size").to_le_bytes());
    data[256..].copy_from_slice(xml);
    data
}

/// Write a padded CZI segment and return its file offset.
pub fn append_segment(
    file: &mut Vec<u8>,
    id: &[u8],
    mut data: Vec<u8>,
    used: Option<usize>,
) -> u64 {
    while data.len() % SEGMENT_HEADER_SIZE != 0 {
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

/// Add one dimension record to a DV entry.
pub fn dimension(
    entry: &mut [u8],
    index: usize,
    code: [u8; 4],
    start: i32,
    logical: i32,
    stored: i32,
) {
    let offset = DV_FIXED_SIZE + index * DIMENSION_SIZE;
    entry[offset..offset + 4].copy_from_slice(&code);
    entry[offset + 4..offset + 8].copy_from_slice(&start.to_le_bytes());
    entry[offset + 8..offset + 12].copy_from_slice(&logical.to_le_bytes());
    entry[offset + 12..offset + 16].copy_from_slice(&0.0_f32.to_le_bytes());
    entry[offset + 16..offset + 20].copy_from_slice(&stored.to_le_bytes());
}
