//! Bounded raw-pixel display summaries. These are preview statistics, not quantitative measurements.
use czi_core::{DecodedPixels, DecodedTile};

pub(super) const MAX_SAMPLES_PER_TILE: usize = 65_536;
pub(super) const MAX_TILES_PER_PLANE: usize = 64;

#[derive(Clone, Debug)]
pub(super) struct Histogram {
    pub(super) bins: [u64; 256],
    pub(super) pixel_max: u16,
}

impl Histogram {
    pub(super) fn from_tile(tile: &DecodedTile) -> Self {
        match &tile.pixels {
            DecodedPixels::Gray8(values) => {
                let mut histogram = Self {
                    bins: [0; 256],
                    pixel_max: 255,
                };
                let stride = values.len().div_ceil(MAX_SAMPLES_PER_TILE).max(1);
                for value in values.iter().step_by(stride) {
                    histogram.bins[usize::from(*value)] += 1;
                }
                histogram
            }
            DecodedPixels::Gray16(values) => {
                let mut histogram = Self {
                    bins: [0; 256],
                    pixel_max: u16::MAX,
                };
                let stride = values.len().div_ceil(MAX_SAMPLES_PER_TILE).max(1);
                for value in values.iter().step_by(stride) {
                    histogram.bins[usize::from(*value >> 8)] += 1;
                }
                histogram
            }
        }
    }

    pub(super) fn merge(&mut self, other: &Self) -> bool {
        if self.pixel_max != other.pixel_max {
            return false;
        }
        for (sum, count) in self.bins.iter_mut().zip(other.bins.iter()) {
            *sum = sum.saturating_add(*count);
        }
        true
    }

    pub(super) fn sample_count(&self) -> u64 {
        self.bins.iter().sum()
    }

    /// Approximate 1st–99th percentile range of available sampled visible raw tiles.
    pub(super) fn auto_range(&self) -> Option<(u16, u16)> {
        let total: u64 = self.bins.iter().sum();
        if total == 0 {
            return None;
        }
        let low_target = (total / 100).max(1);
        let high_target = total.saturating_sub(total / 100);
        let mut cumulative: u64 = 0;
        let mut low = None;
        let mut high = 255;
        for (index, count) in self.bins.iter().enumerate() {
            cumulative = cumulative.saturating_add(*count);
            if low.is_none() && cumulative >= low_target {
                low = Some(index);
            }
            if cumulative >= high_target {
                high = index;
                break;
            }
        }
        let span = (u32::from(self.pixel_max) + 1) / 256;
        let black = u16::try_from(u32::try_from(low?).ok()? * span).ok()?;
        let white =
            u16::try_from(((u32::try_from(high).ok()? + 1) * span).saturating_sub(1)).ok()?;
        (white > black).then_some((black, white))
    }
}

/// Integer gamma state stays equality-comparable in render request/cache identities.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // Rounded output is in 0..=255.
pub(super) fn gamma_lut(gamma_milli: u16) -> [u8; 256] {
    let gamma = f64::from(gamma_milli.clamp(100, 4000)) / 1000.0;
    std::array::from_fn(|index| {
        if gamma_milli == 1000 {
            return u8::try_from(index).expect("256-element LUT");
        }
        ((f64::from(u8::try_from(index).expect("256-element LUT")) / 255.0).powf(1.0 / gamma)
            * 255.0)
            .round() as u8
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histograms_are_bounded_and_do_not_mix_pixel_types() {
        let tile = DecodedTile {
            width: 512,
            height: 512,
            pixels: DecodedPixels::Gray16(vec![32768; 512 * 512]),
        };
        let mut histogram = Histogram::from_tile(&tile);
        assert_eq!(
            histogram.bins.iter().sum::<u64>(),
            MAX_SAMPLES_PER_TILE as u64
        );
        assert_eq!(histogram.auto_range(), Some((32768, 33023)));
        let byte = Histogram::from_tile(&DecodedTile {
            width: 1,
            height: 1,
            pixels: DecodedPixels::Gray8(vec![100]),
        });
        assert!(!histogram.merge(&byte));
        assert_eq!(
            histogram.bins.iter().sum::<u64>(),
            MAX_SAMPLES_PER_TILE as u64
        );
    }

    #[test]
    fn auto_range_excludes_sparse_outliers_and_empty_data() {
        let mut histogram = Histogram {
            bins: [0; 256],
            pixel_max: 255,
        };
        assert_eq!(histogram.auto_range(), None);
        histogram.bins[0] = 1;
        histogram.bins[64] = 499;
        histogram.bins[128] = 499;
        histogram.bins[255] = 1;
        assert_eq!(histogram.auto_range(), Some((64, 128)));
    }

    #[test]
    fn gamma_is_identity_at_one_bounded_and_monotonic() {
        let identity = gamma_lut(1000);
        assert!(
            identity
                .iter()
                .enumerate()
                .all(|(i, v)| i == usize::from(*v))
        );
        for gamma in [0, 100, 500, 2000, 4000, u16::MAX] {
            let lut = gamma_lut(gamma);
            assert_eq!((lut[0], lut[255]), (0, 255));
            assert!(lut.windows(2).all(|pair| pair[0] <= pair[1]));
        }
        assert!(gamma_lut(2000)[128] > 128);
        assert!(gamma_lut(500)[128] < 128);
    }
}
