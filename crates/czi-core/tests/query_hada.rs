use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use czi_core::{
    CziDataset, LocalFileSource, MetadataDocument, MetadataParseLimits, MetadataParseOptions,
    PlaneSelector, RandomAccessSource, SourceError, SourceInfo, TileQueryIndex, ViewQuery,
    summarize_metadata,
};

#[derive(Clone)]
struct CountingSource {
    inner: Arc<LocalFileSource>,
    reads: Arc<AtomicUsize>,
}

#[test]
#[ignore = "requires the local HADA fixture and CZI_RUN_FIXTURES=1"]
fn hada_metadata_summary_survives_bounded_tree_retention() {
    if std::env::var_os("CZI_RUN_FIXTURES").is_none() {
        return;
    }
    let path = hada_path();
    assert!(path.is_file(), "missing HADA fixture: {}", path.display());
    let dataset = CziDataset::open(LocalFileSource::open(path).expect("fixture source"))
        .expect("fixture index");
    let xml = &dataset
        .index()
        .metadata
        .as_ref()
        .expect("global metadata")
        .xml;
    let document = MetadataDocument::parse(
        xml,
        MetadataParseOptions {
            retain_raw_xml: true,
            limits: MetadataParseLimits {
                max_nodes: 1_000,
                ..MetadataParseLimits::default()
            },
        },
    );
    let summary = summarize_metadata(&document);

    let root = document.root.as_ref().expect("partial metadata tree");
    assert_eq!(root.name, "ImageDocument");
    assert!(document.diagnostics.iter().any(|diagnostic| {
        diagnostic.message.contains("node limit of 1000")
            && diagnostic.message.contains("structured view is partial")
    }));
    assert_eq!(summary.channels.len(), 3);
    assert_eq!(summary.channels[0].label, "Phase PH3");
    assert_eq!(summary.channels[0].fluor.as_deref(), Some("TL Phase"));
    assert_eq!(summary.channels[1].label, "AF405");
    assert_eq!(
        summary.channels[1].fluor.as_deref(),
        Some("Alexa Fluor 405")
    );
    assert_eq!(summary.channels[2].label, "Bod493");
    assert_eq!(summary.channels[2].fluor.as_deref(), Some("BODIPY FL"));
    let pixel_size = summary.pixel_size.expect("X/Y calibration");
    assert!((pixel_size.x_um - 0.103_174_603_174_603_17).abs() < 1e-12);
    assert!((pixel_size.y_um - 0.103_174_603_174_603_17).abs() < 1e-12);
    assert_eq!(
        summary.acquisition_date.as_deref(),
        Some("2025-06-02T18:23:49.6167109Z")
    );
    assert_eq!(
        summary.objective.as_deref(),
        Some("Plan-Apochromat 63x/1.40 Oil Ph 3 M27")
    );
}

impl RandomAccessSource for CountingSource {
    fn info(&self) -> SourceInfo {
        self.inner.info()
    }

    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), SourceError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_at(offset, dst)
    }
}

fn hada_path() -> PathBuf {
    PathBuf::from(
        std::env::var_os("CZI_HADA_FIXTURE")
            .expect("CZI_HADA_FIXTURE must name the private HADA fixture"),
    )
}

#[test]
#[ignore = "requires the 2,700-tile HADA fixture and CZI_RUN_FIXTURES=1"]
fn hada_query_index_is_sparse_and_payload_lazy() {
    if std::env::var_os("CZI_RUN_FIXTURES").is_none() {
        return;
    }
    let path = hada_path();
    assert!(
        path.is_file(),
        "CZI_RUN_FIXTURES is set but requested HADA fixture is missing: {}",
        path.display()
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let source = CountingSource {
        inner: Arc::new(LocalFileSource::open(path).expect("fixture source")),
        reads: Arc::clone(&reads),
    };
    let dataset = CziDataset::open(source).expect("fixture index");
    assert_eq!(dataset.index().tile_count(), 2_700);
    let after_open = reads.load(Ordering::Relaxed);
    let query = TileQueryIndex::new(dataset.index()).expect("geometry index");
    assert_eq!(query.tile_count(), 2_700);
    assert_eq!(
        reads.load(Ordering::Relaxed),
        after_open,
        "query reads pixels"
    );
    assert_eq!(query.plane_count(), 36);
    assert_eq!(query.axis_choices().scenes.len(), 12);
    assert_eq!(query.axis_choices().c.len(), 3);
    let expected_scales = [
        czi_core::PyramidScale::new(1, 1).unwrap(),
        czi_core::PyramidScale::new(2, 1).unwrap(),
        czi_core::PyramidScale::new(4, 1).unwrap(),
        czi_core::PyramidScale::new(8, 1).unwrap(),
        czi_core::PyramidScale::new(16, 1).unwrap(),
    ];
    assert!(
        query
            .planes()
            .all(|plane| plane.scales.as_slice() == expected_scales)
    );
    let level_zero_records = query
        .planes()
        .filter(|plane| {
            plane
                .scales
                .contains(&czi_core::PyramidScale::new(1, 1).unwrap())
        })
        .map(|plane| {
            let result = query
                .query(
                    &ViewQuery::new(
                        PlaneSelector::new(plane.key.c, plane.key.scene, plane.key.z, plane.key.t),
                        plane.world_bounds,
                        1.0,
                    )
                    .expect("view"),
                )
                .expect("level zero query");
            result.hits.len()
        })
        .sum::<usize>();
    assert_eq!(level_zero_records, 900);

    let plane = query.planes().next().expect("one plane");
    let result = query
        .query(
            &ViewQuery::new(
                PlaneSelector::new(plane.key.c, plane.key.scene, plane.key.z, plane.key.t),
                plane.world_bounds,
                1.0,
            )
            .expect("view"),
        )
        .expect("visible query");
    assert!(!result.hits.is_empty());
    let after_query = reads.load(Ordering::Relaxed);
    assert_eq!(
        after_query, after_open,
        "index/query performed a pixel read"
    );
    let first = result.hits[0];
    let decoded = dataset
        .decoded_tile(first.tile_id.index())
        .expect("visible tile");
    assert!(decoded.width > 0 && decoded.height > 0);
    assert!(reads.load(Ordering::Relaxed) > after_query);
}
