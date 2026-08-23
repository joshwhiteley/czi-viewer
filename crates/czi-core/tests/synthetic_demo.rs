#[path = "support/synthetic_czi.rs"]
mod synthetic_czi;

use czi_core::{
    CziDataset, DecodedPixels, MemorySource, PlaneSelector, PyramidScale, SceneId, TileQueryIndex,
    ViewQuery,
};

#[test]
fn synthetic_demo_has_named_channels_tiled_mosaic_and_two_pyramid_scales() {
    let dataset = CziDataset::open(MemorySource::new(synthetic_czi::demo_czi()))
        .expect("synthetic demo CZI parses");
    let index = dataset.index();
    assert_eq!(index.tile_count(), 24);

    let metadata = index.metadata.as_ref().expect("safe synthetic metadata");
    let summary = czi_core::summarize_metadata(&czi_core::MetadataDocument::parse(
        &metadata.xml,
        czi_core::MetadataParseOptions::default(),
    ));
    assert_eq!(
        summary
            .channels
            .iter()
            .map(|channel| channel.label.as_str())
            .collect::<Vec<_>>(),
        ["Phase", "Blue", "Green"]
    );

    let query = TileQueryIndex::new(index).expect("query index");
    assert_eq!(query.axis_choices().c, [0, 1, 2]);
    let plane = PlaneSelector::new(1, SceneId::default(), 0, 0);
    assert_eq!(
        query.scales(plane).expect("Blue plane"),
        [
            PyramidScale::new(1, 1).expect("native scale"),
            PyramidScale::new(2, 1).expect("coarse scale"),
        ]
    );
    let request = ViewQuery::new(plane, query.world_bounds(plane).expect("bounds"), 2.0)
        .expect("valid viewport request");
    let result = query.query(&request).expect("coarse viewport query");
    assert_eq!(result.hits.len(), 4);
    assert_eq!(result.scale, PyramidScale::new(2, 1).expect("coarse scale"));

    let decoded = dataset
        .decoded_tile(result.hits[0].tile_id.index())
        .expect("Gray16 tile");
    assert_eq!((decoded.width, decoded.height), (16, 16));
    assert!(matches!(decoded.pixels, DecodedPixels::Gray16(values) if values.len() == 256));
}
