//! Bounded CPU-side rendering resources shared by the dataset worker and UI.
//!
//! Decoding and display conversion are deliberately separate. The worker keeps a small weighted
//! cache of decoded source tiles, while the UI owns GPU texture handles and uploads a bounded
//! number of rendered images per frame.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Condvar, Mutex};

use czi_core::{DecodedPixels, DecodedTile, PhysicalSize, TileId};
use eframe::egui;

use crate::{ChannelRole, Levels, basic, blend_channel, channel_display, display_intensity};

/// Weighted bound for rendered images waiting for UI upload.
pub(crate) const RENDER_INFLIGHT_LIMIT: usize = 64 * 1024 * 1024;
/// Practical upper bound for one GPU texture dimension.
pub(crate) const MAX_TEXTURE_DIMENSION: u32 = 16_384;
/// Practical upper bound for one rendered tile, independent of the backend's device limit.
/// This leaves one maximum-size tile representable in the weighted RGBA handoff budget.
pub(crate) const MAX_TEXTURE_PIXELS: usize = 16 * 1024 * 1024;
/// Weighted bound for source tiles retained by the dataset worker.
pub(crate) const DECODED_TILE_CACHE_LIMIT: usize = 256 * 1024 * 1024;
/// Count limits also bound cache bookkeeping and backend handles for tiny tiles.
pub(crate) const MAX_DECODED_TILES: usize = 8_192;
pub(crate) const MAX_TEXTURE_TILES: usize = 4_096;
/// Maximum number of GPU uploads attempted during one UI frame.
pub(crate) const UPLOADS_PER_FRAME: usize = 4;
/// Maximum normal upload work attempted during one UI frame.
pub(crate) const UPLOAD_BYTES_PER_FRAME: usize = 32 * 1024 * 1024;

/// Return the checked byte size of one decoded tile.
pub(crate) fn decoded_tile_bytes(tile: &DecodedTile) -> Option<usize> {
    let pixel_count = checked_pixel_count(tile.width, tile.height).ok()?;
    let bytes_per_pixel = match &tile.pixels {
        DecodedPixels::Gray8(values) if values.len() == pixel_count => 1,
        DecodedPixels::Gray16(values) if values.len() == pixel_count => 2,
        _ => return None,
    };
    pixel_count.checked_mul(bytes_per_pixel)
}

/// Validate the physical dimensions advertised by a geometry query before decoding it.
pub(crate) fn validate_physical_size(size: PhysicalSize) -> Result<usize, &'static str> {
    validate_dimensions(size.width, size.height)
}

/// Return the checked RGBA byte size for one geometry hit.
pub(crate) fn rendered_bytes_for_physical_size(size: PhysicalSize) -> Result<usize, &'static str> {
    validate_physical_size(size)?
        .checked_mul(std::mem::size_of::<egui::Color32>())
        .ok_or("rendered tile byte count overflows")
}

/// Validate a rendered image before handing it to the backend.
pub(crate) fn validate_image(image: &egui::ColorImage) -> Result<usize, &'static str> {
    let width = image.size[0];
    let height = image.size[1];
    let max_dimension = usize::try_from(MAX_TEXTURE_DIMENSION).unwrap_or(usize::MAX);
    if width == 0 || height == 0 {
        return Err("texture dimensions must be non-zero");
    }
    if width > max_dimension || height > max_dimension {
        return Err("texture dimension exceeds the renderer limit");
    }
    let pixels = checked_pixel_count_usize(width, height)?;
    if image.pixels.len() != pixels {
        return Err("rendered image pixel count does not match its dimensions");
    }
    pixels
        .checked_mul(std::mem::size_of::<egui::Color32>())
        .ok_or("rendered image byte count overflows")
}

/// Convert one decoded grayscale tile into a checked RGBA image.
///
/// This function is called by the dataset worker, never by the egui frame callback. It writes the
/// final pixel vector directly so there is no temporary per-pixel grayscale allocation.
pub(crate) fn texture_image(
    tile: &DecodedTile,
    levels: Levels,
    role: ChannelRole,
    basic_profile: Option<&basic::ChannelProfile>,
) -> Result<egui::ColorImage, &'static str> {
    let width = usize::try_from(tile.width).map_err(|_| "tile width does not fit usize")?;
    let height = usize::try_from(tile.height).map_err(|_| "tile height does not fit usize")?;
    let pixel_count = validate_dimensions(tile.width, tile.height)?;
    debug_assert_eq!(pixel_count, width.saturating_mul(height));
    let gamma = channel_display::gamma_lut(levels.gamma_milli);
    let mut pixels = Vec::new();
    pixels
        .try_reserve_exact(pixel_count)
        .map_err(|_| "cannot allocate rendered tile buffer")?;

    match &tile.pixels {
        DecodedPixels::Gray8(values) if values.len() == pixel_count => {
            for (index, value) in values.iter().copied().enumerate() {
                let raw = u16::from(value);
                let corrected = basic_profile
                    .filter(|profile| profile.pixel_max == u16::from(u8::MAX))
                    .map_or(raw, |profile| {
                        basic::correct_value(
                            raw,
                            index % width,
                            index / width,
                            width,
                            height,
                            profile,
                        )
                    });
                pixels.push(render_pixel(
                    display_intensity(corrected, levels, &gamma),
                    role,
                ));
            }
        }
        DecodedPixels::Gray16(values) if values.len() == pixel_count => {
            for (index, raw) in values.iter().copied().enumerate() {
                let corrected = basic_profile
                    .filter(|profile| profile.pixel_max == u16::MAX)
                    .map_or(raw, |profile| {
                        basic::correct_value(
                            raw,
                            index % width,
                            index / width,
                            width,
                            height,
                            profile,
                        )
                    });
                pixels.push(render_pixel(
                    display_intensity(corrected, levels, &gamma),
                    role,
                ));
            }
        }
        _ => return Err("decoded pixel count does not match the tile dimensions"),
    }
    Ok(egui::ColorImage::new([width, height], pixels))
}

fn render_pixel(value: u8, role: ChannelRole) -> egui::Color32 {
    match role {
        ChannelRole::Off => egui::Color32::TRANSPARENT,
        ChannelRole::Gray => egui::Color32::from_gray(value),
        ChannelRole::Red | ChannelRole::Green | ChannelRole::Blue => {
            let [red, green, blue] = blend_channel([0; 3], value, role);
            egui::Color32::from_rgba_premultiplied(red, green, blue, value)
        }
    }
}

fn checked_pixel_count(width: u32, height: u32) -> Result<usize, &'static str> {
    validate_dimensions(width, height)
}

fn checked_pixel_count_usize(width: usize, height: usize) -> Result<usize, &'static str> {
    let pixels = width
        .checked_mul(height)
        .ok_or("texture dimensions overflow display size")?;
    if pixels > MAX_TEXTURE_PIXELS {
        return Err("texture pixel count exceeds the renderer limit");
    }
    Ok(pixels)
}

fn validate_dimensions(width: u32, height: u32) -> Result<usize, &'static str> {
    if width == 0 || height == 0 {
        return Err("texture dimensions must be non-zero");
    }
    if width > MAX_TEXTURE_DIMENSION || height > MAX_TEXTURE_DIMENSION {
        return Err("texture dimension exceeds the renderer limit");
    }
    checked_pixel_count_usize(
        usize::try_from(width).map_err(|_| "texture width does not fit usize")?,
        usize::try_from(height).map_err(|_| "texture height does not fit usize")?,
    )
}

struct DecodedTileEntry {
    tile: Arc<DecodedTile>,
    bytes: usize,
    last_used: u64,
}

/// Weighted LRU cache for decoded source tiles.
pub(crate) struct DecodedTileCache {
    entries: HashMap<TileId, DecodedTileEntry>,
    bytes: usize,
    clock: u64,
    budget: usize,
}

impl DecodedTileCache {
    pub(crate) fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            budget,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    pub(crate) fn get(&mut self, tile_id: TileId) -> Option<Arc<DecodedTile>> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(&tile_id)?;
        entry.last_used = self.clock;
        Some(Arc::clone(&entry.tile))
    }

    /// Insert a tile if it fits. An oversized tile is returned to the caller but not retained.
    pub(crate) fn insert(&mut self, tile_id: TileId, tile: Arc<DecodedTile>) -> bool {
        let Some(bytes) = decoded_tile_bytes(&tile) else {
            return false;
        };
        if bytes > self.budget {
            return false;
        }
        let previous = self.entries.remove(&tile_id);
        if let Some(previous) = &previous {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
        }
        while self.bytes.saturating_add(bytes) > self.budget
            || self.entries.len() >= MAX_DECODED_TILES
        {
            let Some(candidate) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(tile_id, _)| *tile_id)
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&candidate) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
        if self.bytes.saturating_add(bytes) > self.budget {
            if let Some(previous) = previous {
                self.bytes = self.bytes.saturating_add(previous.bytes);
                self.entries.insert(tile_id, previous);
            }
            return false;
        }
        self.clock = self.clock.wrapping_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.insert(
            tile_id,
            DecodedTileEntry {
                tile,
                bytes,
                last_used: self.clock,
            },
        );
        true
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

struct InFlightState {
    bytes: usize,
    closed: bool,
}

/// A weighted RAII reservation for a rendered image waiting for UI upload.
pub(crate) struct RenderLease {
    budget: Arc<InFlightBudget>,
    bytes: usize,
}

impl Drop for RenderLease {
    fn drop(&mut self) {
        let mut state = self
            .budget
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.bytes = state.bytes.saturating_sub(self.bytes);
        self.budget.wake.notify_all();
    }
}

/// Bounded render-image handoff budget. The worker waits instead of creating an unbounded queue.
pub(crate) struct InFlightBudget {
    state: Mutex<InFlightState>,
    wake: Condvar,
    limit: usize,
}

impl InFlightBudget {
    pub(crate) fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(InFlightState {
                bytes: 0,
                closed: false,
            }),
            wake: Condvar::new(),
            limit,
        })
    }

    pub(crate) fn reserve(self: &Arc<Self>, bytes: usize) -> Result<RenderLease, BudgetError> {
        if bytes > self.limit {
            return Err(BudgetError::TooLarge {
                bytes,
                limit: self.limit,
            });
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.closed && state.bytes.saturating_add(bytes) > self.limit {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if state.closed {
            return Err(BudgetError::Closed);
        }
        state.bytes = state.bytes.saturating_add(bytes);
        Ok(RenderLease {
            budget: Arc::clone(self),
            bytes,
        })
    }

    pub(crate) fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        self.wake.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn bytes(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BudgetError {
    TooLarge { bytes: usize, limit: usize },
    Closed,
}

impl fmt::Display for BudgetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, limit } => {
                write!(
                    formatter,
                    "rendered tile needs {bytes} bytes; limit is {limit}"
                )
            }
            Self::Closed => formatter.write_str("render handoff is closed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(width: u32, height: u32) -> Arc<DecodedTile> {
        Arc::new(DecodedTile {
            width,
            height,
            pixels: DecodedPixels::Gray8(vec![0; usize::try_from(width * height).unwrap()]),
        })
    }

    #[test]
    fn decoded_cache_is_weighted_and_reuses_resident_tiles() {
        let mut cache = DecodedTileCache::new(8);
        assert!(cache.insert(TileId(1), tile(2, 2)));
        assert!(cache.get(TileId(1)).is_some());
        assert_eq!(cache.bytes(), 4);
        assert_eq!(cache.len(), 1);
        assert!(cache.insert(TileId(2), tile(2, 2)));
        assert_eq!(cache.bytes(), 8);
        assert!(cache.get(TileId(1)).is_some());
        assert!(cache.insert(TileId(3), tile(2, 2)));
        assert!(cache.get(TileId(1)).is_some());
        assert!(cache.get(TileId(2)).is_none());
        assert_eq!(cache.bytes(), 8);
    }

    #[test]
    fn decoded_cache_bounds_tiny_tile_bookkeeping() {
        let mut cache = DecodedTileCache::new(DECODED_TILE_CACHE_LIMIT);
        for index in 0..=MAX_DECODED_TILES {
            assert!(cache.insert(TileId(index), tile(1, 1)));
        }
        assert_eq!(cache.len(), MAX_DECODED_TILES);
        assert!(cache.get(TileId(0)).is_none());
    }

    #[test]
    fn decoded_cache_does_not_admit_oversized_tile() {
        let mut cache = DecodedTileCache::new(3);
        assert!(!cache.insert(TileId(1), tile(2, 2)));
        assert_eq!(cache.bytes(), 0);
        assert!(cache.get(TileId(1)).is_none());
    }

    #[test]
    fn display_changes_reuse_the_same_cached_raw_tile() {
        let mut cache = DecodedTileCache::new(16);
        assert!(cache.insert(TileId(1), tile(2, 2)));
        let first = cache.get(TileId(1)).expect("cached raw tile");
        let _gray = texture_image(
            first.as_ref(),
            Levels {
                black: 0,
                white: u16::from(u8::MAX),
                gamma_milli: 1_000,
            },
            ChannelRole::Gray,
            None,
        )
        .expect("gray display");
        let second = cache.get(TileId(1)).expect("cached raw tile after redraw");
        let _blue = texture_image(
            second.as_ref(),
            Levels {
                black: 1,
                white: u16::from(u8::MAX),
                gamma_milli: 1_000,
            },
            ChannelRole::Blue,
            None,
        )
        .expect("blue display");
        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn renderer_rejects_dimension_and_pixel_caps() {
        assert!(
            validate_physical_size(PhysicalSize {
                width: MAX_TEXTURE_DIMENSION + 1,
                height: 1,
            })
            .is_err()
        );
        assert!(
            validate_physical_size(PhysicalSize {
                width: 8_193,
                height: 8_193,
            })
            .is_err()
        );
        assert!(
            validate_physical_size(PhysicalSize {
                width: 2_048,
                height: 2_048,
            })
            .is_ok()
        );
    }

    #[test]
    fn rendered_byte_size_is_checked_against_rgba_budget() {
        let bytes = rendered_bytes_for_physical_size(PhysicalSize {
            width: 2_048,
            height: 2_048,
        })
        .expect("bounded RGBA size");
        assert_eq!(bytes, 2_048 * 2_048 * std::mem::size_of::<egui::Color32>());
        assert!(
            rendered_bytes_for_physical_size(PhysicalSize {
                width: MAX_TEXTURE_DIMENSION + 1,
                height: 1,
            })
            .is_err()
        );
    }

    #[test]
    fn inflight_budget_is_exact_and_released_by_lease_drop() {
        let budget = InFlightBudget::new(8);
        let lease = budget.reserve(8).expect("exact reservation");
        assert_eq!(budget.bytes(), 8);
        assert!(matches!(
            budget.reserve(9),
            Err(BudgetError::TooLarge { .. })
        ));
        drop(lease);
        assert_eq!(budget.bytes(), 0);
        budget.close();
        assert!(matches!(budget.reserve(1), Err(BudgetError::Closed)));
    }
}
