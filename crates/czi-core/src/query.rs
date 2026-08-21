//! Geometry-only sparse tile indexing and viewport queries.
//!
//! This module intentionally reads only [`DatasetIndex`] metadata. It never resolves a tile
//! payload, allocates pixel buffers, or assembles a dense plane.

#![allow(clippy::missing_errors_doc)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use thiserror::Error;

use crate::{DatasetIndex, DimensionCode, DimensionEntry, TileIndex};

/// Stable identity of a tile in the summary-directory order.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TileId(pub usize);

impl From<usize> for TileId {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

impl fmt::Display for TileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl TileId {
    /// Return the summary-directory index represented by this id.
    #[must_use]
    pub const fn index(self) -> usize {
        self.0
    }
}

/// Scene identity distinguishes an absent S dimension from an explicit S=0 scene.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SceneId {
    /// The tile has no S dimension.
    #[default]
    Implicit,
    /// The tile contains an S dimension with this sparse start value.
    Explicit(i32),
}

/// Exact sparse C/S/Z/T identity of a plane.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlaneKey {
    /// Channel start, normalized to zero when C is absent.
    pub c: i32,
    /// Scene identity; absent S remains [`SceneId::Implicit`].
    pub scene: SceneId,
    /// Z start, normalized to zero when Z is absent.
    pub z: i32,
    /// Time start, normalized to zero when T is absent.
    pub t: i32,
}

impl PlaneKey {
    /// Construct an exact plane key.
    #[must_use]
    pub const fn new(c: i32, scene: SceneId, z: i32, t: i32) -> Self {
        Self { c, scene, z, t }
    }
}

/// Exact plane selected by a viewport request.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PlaneSelector {
    /// Channel start, or zero for an absent C dimension.
    pub c: i32,
    /// Exact scene identity.
    pub scene: SceneId,
    /// Z start, or zero for an absent Z dimension.
    pub z: i32,
    /// Time start, or zero for an absent T dimension.
    pub t: i32,
}

impl PlaneSelector {
    /// Construct a selector for one exact sparse plane.
    #[must_use]
    pub const fn new(c: i32, scene: SceneId, z: i32, t: i32) -> Self {
        Self { c, scene, z, t }
    }

    /// Return the key represented by this selector.
    #[must_use]
    pub const fn key(self) -> PlaneKey {
        PlaneKey::new(self.c, self.scene, self.z, self.t)
    }
}

impl From<PlaneKey> for PlaneSelector {
    fn from(value: PlaneKey) -> Self {
        Self::new(value.c, value.scene, value.z, value.t)
    }
}

impl From<PlaneSelector> for PlaneKey {
    fn from(value: PlaneSelector) -> Self {
        value.key()
    }
}

/// A checked half-open rectangle in logical world coordinates.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SpatialRect {
    /// Inclusive minimum X.
    pub min_x: i64,
    /// Inclusive minimum Y.
    pub min_y: i64,
    /// Exclusive maximum X.
    pub max_x: i64,
    /// Exclusive maximum Y.
    pub max_y: i64,
}

impl SpatialRect {
    /// Construct a checked half-open rectangle.
    ///
    /// Empty rectangles (`min == max`) are valid and never intersect a non-empty rectangle.
    pub const fn new(
        min_x: i64,
        min_y: i64,
        max_x: i64,
        max_y: i64,
    ) -> Result<Self, TileQueryError> {
        if max_x < min_x || max_y < min_y {
            return Err(TileQueryError::InvalidRect {
                min_x,
                min_y,
                max_x,
                max_y,
            });
        }
        Ok(Self {
            min_x,
            min_y,
            max_x,
            max_y,
        })
    }

    /// Construct a rectangle from an origin and unsigned extent with checked i64 arithmetic.
    pub fn from_start_size(
        min_x: i64,
        min_y: i64,
        width: u32,
        height: u32,
    ) -> Result<Self, TileQueryError> {
        let max_x =
            min_x
                .checked_add(i64::from(width))
                .ok_or(TileQueryError::RectCoordinateOverflow {
                    axis: DimensionCode::X,
                })?;
        let max_y =
            min_y
                .checked_add(i64::from(height))
                .ok_or(TileQueryError::RectCoordinateOverflow {
                    axis: DimensionCode::Y,
                })?;
        Self::new(min_x, min_y, max_x, max_y)
    }

    /// Return true when two half-open rectangles overlap with non-zero area.
    #[must_use]
    pub const fn intersects(self, other: Self) -> bool {
        self.min_x < other.max_x
            && other.min_x < self.max_x
            && self.min_y < other.max_y
            && other.min_y < self.max_y
    }

    /// Return the checked union of two rectangles.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            min_x: if self.min_x < other.min_x {
                self.min_x
            } else {
                other.min_x
            },
            min_y: if self.min_y < other.min_y {
                self.min_y
            } else {
                other.min_y
            },
            max_x: if self.max_x > other.max_x {
                self.max_x
            } else {
                other.max_x
            },
            max_y: if self.max_y > other.max_y {
                self.max_y
            } else {
                other.max_y
            },
        }
    }

    /// Return whether this rectangle has non-zero area.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.min_x == self.max_x || self.min_y == self.max_y
    }

    /// Return the width when it fits in an unsigned 64-bit value.
    #[must_use]
    pub const fn width(self) -> u64 {
        (self.max_x - self.min_x).unsigned_abs()
    }

    /// Return the height when it fits in an unsigned 64-bit value.
    #[must_use]
    pub const fn height(self) -> u64 {
        (self.max_y - self.min_y).unsigned_abs()
    }
}

/// Physical (stored) X/Y dimensions of one tile.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PhysicalSize {
    /// Stored X pixels.
    pub width: u32,
    /// Stored Y pixels.
    pub height: u32,
}

/// Reduced logical-to-physical pyramid ratio.
///
/// A value of `4/1` means a tile covers four logical pixels per stored pixel in each axis.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PyramidScale {
    /// Logical extent per stored pixel after reduction.
    pub numerator: u32,
    /// Stored extent per logical extent after reduction.
    pub denominator: u32,
}

impl PyramidScale {
    /// Construct and reduce a positive rational scale.
    pub const fn new(numerator: u32, denominator: u32) -> Result<Self, TileQueryError> {
        if numerator == 0 || denominator == 0 {
            return Err(TileQueryError::InvalidScale {
                numerator,
                denominator,
            });
        }
        let divisor = gcd(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    /// Derive one isotropic scale from logical/stored X/Y dimensions.
    pub fn from_xy(
        logical_width: u32,
        stored_width: u32,
        logical_height: u32,
        stored_height: u32,
    ) -> Result<Self, TileQueryError> {
        if logical_width == 0 || stored_width == 0 || logical_height == 0 || stored_height == 0 {
            return Err(TileQueryError::InvalidScale {
                numerator: logical_width.max(logical_height),
                denominator: stored_width.max(stored_height),
            });
        }
        if u64::from(logical_width) * u64::from(stored_height)
            != u64::from(logical_height) * u64::from(stored_width)
        {
            return Err(TileQueryError::NonUniformPyramidScale {
                logical_width,
                stored_width,
                logical_height,
                stored_height,
            });
        }
        Self::new(logical_width, stored_width)
    }

    /// Return the scale as a floating-point downsample factor.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        f64::from(self.numerator) / f64::from(self.denominator)
    }
}

impl Ord for PyramidScale {
    fn cmp(&self, other: &Self) -> Ordering {
        (u64::from(self.numerator) * u64::from(other.denominator))
            .cmp(&(u64::from(other.numerator) * u64::from(self.denominator)))
    }
}

impl PartialOrd for PyramidScale {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

/// One geometry-only tile hit returned for a viewport.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TileHit {
    /// Stable summary-directory tile identity.
    pub tile_id: TileId,
    /// Exact plane containing the tile.
    pub plane: PlaneKey,
    /// Logical world-space coverage, half-open.
    pub logical_rect: SpatialRect,
    /// Physical stored dimensions used by the decoder.
    pub physical_stored_size: PhysicalSize,
    /// Reduced logical/stored scale.
    pub scale: PyramidScale,
    /// Optional M start used only for paint order.
    pub m_index: Option<i32>,
    /// Stable order in which this hit should be painted.
    pub paint_order: usize,
}

/// A viewport request for one exact plane and one target downsample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewQuery {
    /// Exact sparse plane selector.
    pub plane: PlaneSelector,
    /// Logical world-space viewport, half-open.
    pub viewport: SpatialRect,
    /// Desired logical pixels per stored pixel.
    pub target_downsample: f64,
}

impl ViewQuery {
    /// Construct a viewport query, rejecting unsafe target values.
    pub fn new(
        plane: PlaneSelector,
        viewport: SpatialRect,
        target_downsample: f64,
    ) -> Result<Self, TileQueryError> {
        if !target_downsample.is_finite() || target_downsample <= 0.0 {
            return Err(TileQueryError::InvalidTargetDownsample(target_downsample));
        }
        Ok(Self {
            plane,
            viewport,
            target_downsample,
        })
    }
}

/// The selected level and visible tile hits for a viewport query.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewQueryResult {
    /// Exact plane selected by the request.
    pub plane: PlaneKey,
    /// Pyramid scale selected from the levels available for this plane.
    pub scale: PyramidScale,
    /// Visible tiles, sorted by M paint order and then stable tile id.
    pub hits: Vec<TileHit>,
}

impl ViewQueryResult {
    /// Return the visible tile hits.
    #[must_use]
    pub fn hits(&self) -> &[TileHit] {
        &self.hits
    }
}

/// Observed sparse values for the modeled plane axes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SparseAxisChoices {
    /// Observed channel starts.
    pub c: Vec<i32>,
    /// Observed scene identities, preserving implicit-vs-explicit semantics.
    pub scenes: Vec<SceneId>,
    /// Observed Z starts.
    pub z: Vec<i32>,
    /// Observed time starts.
    pub t: Vec<i32>,
}

/// Geometry and levels available for one exact plane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaneInfo {
    /// Exact plane identity.
    pub key: PlaneKey,
    /// Union of all tile logical bounds for the plane.
    pub world_bounds: SpatialRect,
    /// Available reduced pyramid scales in ascending order.
    pub scales: Vec<PyramidScale>,
}

/// Immutable geometry index over [`DatasetIndex::tiles`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TileQueryIndex {
    tiles: Vec<IndexedTile>,
    levels: BTreeMap<(PlaneKey, PyramidScale), Vec<TileId>>,
    planes: BTreeMap<PlaneKey, PlaneInfo>,
    axes: SparseAxisChoices,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct IndexedTile {
    plane: PlaneKey,
    logical_rect: SpatialRect,
    physical_stored_size: PhysicalSize,
    scale: PyramidScale,
    m_index: Option<i32>,
}

/// Errors found while converting summary-directory geometry into a safe query index.
#[derive(Clone, Debug, Error, PartialEq)]
pub enum TileQueryError {
    /// A tile has no X dimension.
    #[error("tile {tile_id} is missing the X dimension")]
    MissingX { tile_id: TileId },
    /// A tile has no Y dimension.
    #[error("tile {tile_id} is missing the Y dimension")]
    MissingY { tile_id: TileId },
    /// A tile dimension has a zero or otherwise invalid extent.
    #[error("tile {tile_id} has invalid {code} extent logical={logical_size} stored={stored_size}")]
    InvalidDimension {
        tile_id: TileId,
        code: DimensionCode,
        logical_size: u32,
        stored_size: u32,
    },
    /// A modeled plane dimension spans multiple sparse values and cannot be selected exactly.
    #[error(
        "tile {tile_id} has variable modeled dimension {code} logical={logical_size} stored={stored_size}"
    )]
    VariableModeledDimension {
        tile_id: TileId,
        code: DimensionCode,
        logical_size: u32,
        stored_size: u32,
    },
    /// An unmodeled dimension spans values and would mix planes.
    #[error(
        "tile {tile_id} has variable unmodeled dimension {code} logical={logical_size} stored={stored_size}"
    )]
    VariableUnmodeledDimension {
        tile_id: TileId,
        code: DimensionCode,
        logical_size: u32,
        stored_size: u32,
    },
    /// A logical tile coordinate plus extent overflowed i64.
    #[error("tile {tile_id} {axis} coordinate overflows i64")]
    CoordinateOverflow {
        tile_id: TileId,
        axis: DimensionCode,
    },
    /// Constructing a public rectangle overflowed i64.
    #[error("{axis} coordinate overflows i64")]
    RectCoordinateOverflow { axis: DimensionCode },
    /// A public rectangle has inverted bounds.
    #[error("invalid half-open rectangle [{min_x},{min_y})..[{max_x},{max_y})")]
    InvalidRect {
        min_x: i64,
        min_y: i64,
        max_x: i64,
        max_y: i64,
    },
    /// A scale has a zero numerator or denominator.
    #[error("invalid pyramid scale {numerator}/{denominator}")]
    InvalidScale { numerator: u32, denominator: u32 },
    /// X and Y imply different downsample ratios.
    #[error(
        "non-uniform pyramid scale X={logical_width}/{stored_width}, Y={logical_height}/{stored_height}"
    )]
    NonUniformPyramidScale {
        logical_width: u32,
        stored_width: u32,
        logical_height: u32,
        stored_height: u32,
    },
    /// A viewport target is not finite and positive.
    #[error("invalid target downsample {0}")]
    InvalidTargetDownsample(f64),
    /// No plane matching a selector exists.
    #[error("no indexed plane matches {selector}")]
    MissingPlane { selector: PlaneSelector },
}

impl fmt::Display for PlaneSelector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "C={} S={:?} Z={} T={}",
            self.c, self.scene, self.z, self.t
        )
    }
}

impl TileQueryIndex {
    /// Return the number of geometry-indexed tiles.
    #[must_use]
    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// Return the number of exact sparse planes.
    #[must_use]
    pub fn plane_count(&self) -> usize {
        self.planes.len()
    }

    /// Build an immutable geometry index without reading any tile payloads.
    pub fn new(index: &DatasetIndex) -> Result<Self, TileQueryError> {
        let mut tiles = Vec::with_capacity(index.tiles.len());
        let mut levels: BTreeMap<(PlaneKey, PyramidScale), Vec<TileId>> = BTreeMap::new();
        let mut plane_bounds: BTreeMap<PlaneKey, SpatialRect> = BTreeMap::new();

        for (raw_id, tile) in index.tiles.iter().enumerate() {
            let tile_id = TileId(raw_id);
            let indexed = index_tile(tile_id, tile)?;
            plane_bounds
                .entry(indexed.plane)
                .and_modify(|bounds| *bounds = bounds.union(indexed.logical_rect))
                .or_insert(indexed.logical_rect);
            levels
                .entry((indexed.plane, indexed.scale))
                .or_default()
                .push(tile_id);
            tiles.push(indexed);
        }

        let mut planes = BTreeMap::new();
        for (plane, world_bounds) in plane_bounds {
            let mut scales = levels
                .keys()
                .filter_map(|(key, scale)| (*key == plane).then_some(*scale))
                .collect::<Vec<_>>();
            scales.sort_unstable();
            scales.dedup();
            planes.insert(
                plane,
                PlaneInfo {
                    key: plane,
                    world_bounds,
                    scales,
                },
            );
        }

        let mut axes = SparseAxisChoices {
            c: planes.keys().map(|key| key.c).collect(),
            scenes: planes.keys().map(|key| key.scene).collect(),
            z: planes.keys().map(|key| key.z).collect(),
            t: planes.keys().map(|key| key.t).collect(),
        };
        axes.c.sort_unstable();
        axes.c.dedup();
        axes.scenes.sort_unstable();
        axes.scenes.dedup();
        axes.z.sort_unstable();
        axes.z.dedup();
        axes.t.sort_unstable();
        axes.t.dedup();

        Ok(Self {
            tiles,
            levels,
            planes,
            axes,
        })
    }

    /// Alias for [`Self::new`].
    pub fn from_dataset_index(index: &DatasetIndex) -> Result<Self, TileQueryError> {
        Self::new(index)
    }

    /// Return observed sparse C/S/Z/T choices.
    #[must_use]
    pub fn axis_choices(&self) -> &SparseAxisChoices {
        &self.axes
    }

    /// Return all exact plane descriptions in stable key order.
    pub fn planes(&self) -> impl Iterator<Item = &PlaneInfo> {
        self.planes.values()
    }

    /// Return one exact plane description.
    #[must_use]
    pub fn plane(&self, selector: impl Into<PlaneSelector>) -> Option<&PlaneInfo> {
        self.planes.get(&selector.into().key())
    }

    /// Return one exact plane's world bounds.
    #[must_use]
    pub fn world_bounds(&self, selector: impl Into<PlaneSelector>) -> Option<SpatialRect> {
        self.plane(selector).map(|info| info.world_bounds)
    }

    /// Return all available levels for one exact plane.
    #[must_use]
    pub fn scales(&self, selector: impl Into<PlaneSelector>) -> Option<&[PyramidScale]> {
        self.plane(selector).map(|info| info.scales.as_slice())
    }

    /// Query only tiles from the selected exact plane and selected pyramid level.
    pub fn query(&self, query: &ViewQuery) -> Result<ViewQueryResult, TileQueryError> {
        let plane = query.plane.key();
        let info = self
            .planes
            .get(&plane)
            .ok_or(TileQueryError::MissingPlane {
                selector: query.plane,
            })?;
        let scale = choose_scale(&info.scales, query.target_downsample);
        let mut hits = self
            .levels
            .get(&(plane, scale))
            .into_iter()
            .flatten()
            .filter_map(|tile_id| {
                let tile = self.tiles[tile_id.0];
                query
                    .viewport
                    .intersects(tile.logical_rect)
                    .then_some(TileHit {
                        tile_id: *tile_id,
                        plane: tile.plane,
                        logical_rect: tile.logical_rect,
                        physical_stored_size: tile.physical_stored_size,
                        scale: tile.scale,
                        m_index: tile.m_index,
                        paint_order: 0,
                    })
            })
            .collect::<Vec<_>>();
        hits.sort_unstable_by_key(|hit| {
            (
                hit.m_index.is_none(),
                hit.m_index.unwrap_or_default(),
                hit.tile_id,
            )
        });
        for (paint_order, hit) in hits.iter_mut().enumerate() {
            hit.paint_order = paint_order;
        }
        Ok(ViewQueryResult { plane, scale, hits })
    }

    /// Alias for [`Self::query`] with a name that makes viewport filtering explicit.
    pub fn query_viewport(&self, query: &ViewQuery) -> Result<ViewQueryResult, TileQueryError> {
        self.query(query)
    }

    /// Return the level that would be selected for a target downsample.
    pub fn select_scale(
        &self,
        selector: impl Into<PlaneSelector>,
        target_downsample: f64,
    ) -> Result<PyramidScale, TileQueryError> {
        if !target_downsample.is_finite() || target_downsample <= 0.0 {
            return Err(TileQueryError::InvalidTargetDownsample(target_downsample));
        }
        let selector = selector.into();
        let info = self
            .planes
            .get(&selector.key())
            .ok_or(TileQueryError::MissingPlane { selector })?;
        Ok(choose_scale(&info.scales, target_downsample))
    }
}

fn choose_scale(scales: &[PyramidScale], target_downsample: f64) -> PyramidScale {
    // `TileQueryIndex` only creates planes with at least one tile, so this is safe.
    let mut selected = scales[0];
    for scale in scales {
        if scale.as_f64() <= target_downsample {
            selected = *scale;
        }
    }
    selected
}

fn index_tile(tile_id: TileId, tile: &TileIndex) -> Result<IndexedTile, TileQueryError> {
    let dimensions = &tile.entry.dimensions;
    let x = dimension(dimensions, DimensionCode::X).ok_or(TileQueryError::MissingX { tile_id })?;
    let y = dimension(dimensions, DimensionCode::Y).ok_or(TileQueryError::MissingY { tile_id })?;
    validate_extent(tile_id, x)?;
    validate_extent(tile_id, y)?;

    for dimension in dimensions {
        if matches!(
            dimension.code,
            DimensionCode::C
                | DimensionCode::S
                | DimensionCode::Z
                | DimensionCode::T
                | DimensionCode::M
        ) {
            if dimension.logical_size != 1 || dimension.stored_size != 1 {
                return Err(TileQueryError::VariableModeledDimension {
                    tile_id,
                    code: dimension.code,
                    logical_size: dimension.logical_size,
                    stored_size: dimension.stored_size,
                });
            }
        } else if !matches!(dimension.code, DimensionCode::X | DimensionCode::Y)
            && (dimension.logical_size != 1 || dimension.stored_size != 1)
        {
            return Err(TileQueryError::VariableUnmodeledDimension {
                tile_id,
                code: dimension.code,
                logical_size: dimension.logical_size,
                stored_size: dimension.stored_size,
            });
        }
    }

    let plane = PlaneKey::new(
        dimension_start(dimensions, DimensionCode::C),
        dimension(dimensions, DimensionCode::S).map_or(SceneId::Implicit, |dimension| {
            SceneId::Explicit(dimension.start)
        }),
        dimension_start(dimensions, DimensionCode::Z),
        dimension_start(dimensions, DimensionCode::T),
    );
    let logical_rect = SpatialRect::from_start_size(
        i64::from(x.start),
        i64::from(y.start),
        x.logical_size,
        y.logical_size,
    )
    .map_err(|error| match error {
        TileQueryError::RectCoordinateOverflow { axis } => {
            TileQueryError::CoordinateOverflow { tile_id, axis }
        }
        other => other,
    })?;
    let scale =
        PyramidScale::from_xy(x.logical_size, x.stored_size, y.logical_size, y.stored_size)?;
    Ok(IndexedTile {
        plane,
        logical_rect,
        physical_stored_size: PhysicalSize {
            width: x.stored_size,
            height: y.stored_size,
        },
        scale,
        m_index: dimension(dimensions, DimensionCode::M).map(|dimension| dimension.start),
    })
}

fn dimension(dimensions: &[DimensionEntry], code: DimensionCode) -> Option<&DimensionEntry> {
    dimensions.iter().find(|dimension| dimension.code == code)
}

fn dimension_start(dimensions: &[DimensionEntry], code: DimensionCode) -> i32 {
    dimension(dimensions, code).map_or(0, |dimension| dimension.start)
}

fn validate_extent(tile_id: TileId, dimension: &DimensionEntry) -> Result<(), TileQueryError> {
    if dimension.logical_size == 0 || dimension.stored_size == 0 {
        return Err(TileQueryError::InvalidDimension {
            tile_id,
            code: dimension.code,
            logical_size: dimension.logical_size,
            stored_size: dimension.stored_size,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CompressionMode, DatasetIndex, DirectoryEntry, FileHeader, PixelType, PyramidType,
        SourceInfo, TileIndex,
    };

    fn dimension(code: DimensionCode, start: i32, logical: u32, stored: u32) -> DimensionEntry {
        DimensionEntry {
            code,
            start,
            logical_size: logical,
            start_coordinate: 0.0,
            stored_size: stored,
            stored_size_raw: i32::try_from(stored).expect("stored size"),
        }
    }

    fn tile(dimensions: Vec<DimensionEntry>, pyramid_type: PyramidType) -> TileIndex {
        TileIndex {
            entry: DirectoryEntry {
                schema_type: *b"DV",
                pixel_type: PixelType::Gray8,
                file_position: 0,
                file_part: 0,
                compression: CompressionMode::Uncompressed,
                pyramid_type,
                dimensions,
            },
        }
    }

    fn index(tiles: Vec<TileIndex>) -> DatasetIndex {
        DatasetIndex {
            source: SourceInfo {
                length: 0,
                version: 0,
            },
            file_header: FileHeader {
                segment: crate::SegmentHeader {
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
            attachments: Vec::new(),
        }
    }

    fn base(start_x: i32, start_y: i32, logical: u32, stored: u32) -> Vec<DimensionEntry> {
        vec![
            dimension(DimensionCode::X, start_x, logical, stored),
            dimension(DimensionCode::Y, start_y, logical, stored),
        ]
    }

    #[test]
    fn sparse_plane_keys_preserve_implicit_scene_and_normalize_missing_axes() {
        let mut implicit = base(0, 0, 10, 10);
        implicit.push(dimension(DimensionCode::M, 3, 1, 1));
        let mut explicit = base(10, 0, 10, 10);
        explicit.push(dimension(DimensionCode::S, 0, 1, 1));
        explicit.push(dimension(DimensionCode::C, 4, 1, 1));
        let query = TileQueryIndex::new(&index(vec![
            tile(implicit, PyramidType::Unknown(99)),
            tile(explicit, PyramidType::None),
        ]))
        .expect("geometry index");
        assert_eq!(query.axis_choices().c, vec![0, 4]);
        assert_eq!(
            query.axis_choices().scenes,
            vec![SceneId::Implicit, SceneId::Explicit(0)]
        );
        assert!(
            query
                .plane(PlaneSelector::new(0, SceneId::Implicit, 0, 0))
                .is_some()
        );
        assert!(
            query
                .plane(PlaneSelector::new(4, SceneId::Explicit(0), 0, 0))
                .is_some()
        );
    }

    #[test]
    fn negative_coordinates_and_half_open_intersections_are_checked() {
        let rect = SpatialRect::from_start_size(-10, -4, 10, 4).expect("rect");
        assert!(rect.intersects(SpatialRect::new(-1, -1, 1, 1).expect("rect")));
        assert!(!rect.intersects(SpatialRect::new(0, 0, 2, 2).expect("rect")));
        assert!(SpatialRect::new(1, 0, 0, 1).is_err());
        assert!(SpatialRect::from_start_size(i64::MAX, 0, 1, 1).is_err());
    }

    #[test]
    fn levels_are_ratio_derived_and_ignore_pyramid_type() {
        let mut level_1 = base(0, 0, 100, 100);
        level_1.push(dimension(DimensionCode::M, 2, 1, 1));
        let level_2 = base(0, 0, 100, 50);
        let level_4 = base(0, 0, 100, 25);
        let query = TileQueryIndex::new(&index(vec![
            tile(level_4, PyramidType::None),
            tile(level_1, PyramidType::Unknown(44)),
            tile(level_2, PyramidType::MultiSubblock),
        ]))
        .expect("geometry index");
        let selector = PlaneSelector::default();
        assert_eq!(
            query.scales(selector).expect("scales"),
            &[
                PyramidScale::new(1, 1).unwrap(),
                PyramidScale::new(2, 1).unwrap(),
                PyramidScale::new(4, 1).unwrap()
            ]
        );
        assert_eq!(
            query.select_scale(selector, 3.0).unwrap(),
            PyramidScale::new(2, 1).unwrap()
        );
        assert_eq!(
            query.select_scale(selector, 0.5).unwrap(),
            PyramidScale::new(1, 1).unwrap()
        );
    }

    #[test]
    fn query_filters_exact_plane_level_and_half_open_viewport() {
        let mut selected = base(0, 0, 10, 10);
        selected.push(dimension(DimensionCode::C, 3, 1, 1));
        let other_plane = {
            let mut dimensions = base(10, 0, 10, 10);
            dimensions.push(dimension(DimensionCode::C, 4, 1, 1));
            dimensions
        };
        let level_2 = base(0, 0, 10, 5);
        let query = TileQueryIndex::new(&index(vec![
            tile(selected, PyramidType::None),
            tile(other_plane, PyramidType::None),
            tile(level_2, PyramidType::None),
        ]))
        .expect("geometry index");
        let viewport = SpatialRect::new(0, 0, 10, 10).unwrap();
        let result = query
            .query(
                &ViewQuery::new(
                    PlaneSelector::new(3, SceneId::Implicit, 0, 0),
                    viewport,
                    1.5,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(result.scale, PyramidScale::new(1, 1).unwrap());
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].tile_id, TileId(0));
        assert_eq!(result.hits[0].physical_stored_size.width, 10);
        assert!(
            query
                .query(
                    &ViewQuery::new(
                        PlaneSelector::new(3, SceneId::Implicit, 0, 0),
                        SpatialRect::new(10, 0, 20, 10).unwrap(),
                        1.0,
                    )
                    .unwrap()
                )
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn stable_m_paint_order_precedes_missing_m() {
        let mut m_9 = base(0, 0, 10, 10);
        m_9.push(dimension(DimensionCode::M, 9, 1, 1));
        let mut m_2 = base(10, 0, 10, 10);
        m_2.push(dimension(DimensionCode::M, 2, 1, 1));
        let missing = base(20, 0, 10, 10);
        let query = TileQueryIndex::new(&index(vec![
            tile(m_9, PyramidType::None),
            tile(missing, PyramidType::None),
            tile(m_2, PyramidType::None),
        ]))
        .expect("geometry index");
        let viewport = SpatialRect::new(0, 0, 30, 10).unwrap();
        let result = query
            .query(&ViewQuery::new(PlaneSelector::default(), viewport, 1.0).unwrap())
            .unwrap();
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| hit.tile_id)
                .collect::<Vec<_>>(),
            vec![TileId(2), TileId(0), TileId(1)]
        );
        assert_eq!(
            result
                .hits
                .iter()
                .map(|hit| hit.paint_order)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn variable_unmodeled_and_missing_xy_are_reported() {
        let mut unknown = base(0, 0, 10, 10);
        unknown.push(dimension(DimensionCode::Unknown(*b"Q\0\0\0"), 0, 2, 2));
        assert!(matches!(
            TileQueryIndex::new(&index(vec![tile(unknown, PyramidType::None)])),
            Err(TileQueryError::VariableUnmodeledDimension { .. })
        ));
        let missing_y = vec![dimension(DimensionCode::X, 0, 10, 10)];
        assert!(matches!(
            TileQueryIndex::new(&index(vec![tile(missing_y, PyramidType::None)])),
            Err(TileQueryError::MissingY { .. })
        ));
    }
}
