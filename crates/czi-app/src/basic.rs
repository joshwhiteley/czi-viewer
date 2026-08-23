#![allow(clippy::cast_sign_loss)]

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use czi_core::{
    DatasetIndex, DecodedPixels, DecodedTile, PixelType, PlaneKey, PyramidScale, SceneId,
    SpatialRect, TileHit, TileId, TileQueryIndex,
};
use serde::{Deserialize, Serialize};

pub(crate) const GRID_WIDTH: u32 = 128;
pub(crate) const GRID_HEIGHT: u32 = 128;
pub(crate) const MAX_SAMPLES_PER_CHANNEL: usize = 512;
pub(crate) const MIN_SAMPLES_PER_CHANNEL: usize = 32;
pub(crate) const WARN_SAMPLES_PER_CHANNEL: usize = 100;
const MAX_CHANNELS: usize = 64;
const MAX_CHANNEL_NAME_CHARS: usize = 128;
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: u64 = 32 * 1024 * 1024;
const MIN_SUPPORTED_GAIN: f32 = 1.0e-6;
const MAX_SUPPORTED_GAIN: f32 = 1.0e6;
const RESPONSE_JSON_LIMIT: u64 = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
type DetectorTileIdentity = (i64, i64, i64, i64, Option<i32>);
type DetectorTileSignature = (DetectorTileIdentity, PixelType);
type AcquisitionIdentity = (SceneId, i32, i32, i64, i64, i64, i64, Option<i32>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ChannelSpec {
    pub(crate) id: String,
    pub(crate) c_index: i32,
    pub(crate) name: String,
    pub(crate) is_phase: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct SampleCandidate {
    pub(crate) tile_id: TileId,
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelSamplePlan {
    pub(crate) spec: ChannelSpec,
    pub(crate) pixel_max: u16,
    pub(crate) candidates: Vec<SampleCandidate>,
}

#[derive(Clone, Debug)]
pub(crate) struct SamplePlan {
    pub(crate) channels: Vec<ChannelSamplePlan>,
    pub(crate) verified_scales: HashMap<PlaneKey, Vec<PyramidScale>>,
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelProfile {
    pub(crate) id: String,
    pub(crate) c_index: i32,
    pub(crate) pixel_max: u16,
    pub(crate) gain: Arc<Vec<f32>>,
    pub(crate) support: Arc<Vec<u8>>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ProfileSet {
    pub(crate) channels: HashMap<i32, ChannelProfile>,
}

impl ProfileSet {
    pub(crate) fn is_ready_for(&self, channels: &[i32]) -> bool {
        self.channels.len() == channels.len()
            && channels.iter().all(|channel| {
                self.channels
                    .get(channel)
                    .is_some_and(ChannelProfile::structurally_valid)
            })
    }
}

impl ChannelProfile {
    fn structurally_valid(&self) -> bool {
        let expected = grid_pixels();
        !self.id.is_empty()
            && self.gain.len() == expected
            && self.support.len() == expected
            && self.pixel_max > 0
            && self.support.contains(&1)
            && self
                .gain
                .iter()
                .zip(self.support.iter())
                .all(|(gain, support)| {
                    gain.is_finite()
                        && matches!(support, 0 | 1)
                        && (*support == 0
                            || (*gain >= MIN_SUPPORTED_GAIN && *gain <= MAX_SUPPORTED_GAIN))
                })
    }
}

#[derive(Serialize)]
struct RequestManifest<'a> {
    version: u32,
    width: u32,
    height: u32,
    channels: Vec<RequestChannel<'a>>,
}

#[derive(Serialize)]
struct RequestChannel<'a> {
    id: &'a str,
    c_index: i32,
    name: &'a str,
    sample_count: usize,
    pixel_max: u16,
    is_phase: bool,
    file: &'a str,
}

#[derive(Deserialize)]
struct ResponseManifest {
    version: u32,
    status: String,
    darkfield_enabled: bool,
    channels: Vec<ResponseChannel>,
}

#[derive(Deserialize)]
struct ResponseChannel {
    id: String,
    method: String,
    version: u32,
    sample_count: usize,
    support_fraction: f32,
    gain_range: GainRange,
    gain_file: String,
    support_file: String,
}

#[derive(Deserialize)]
struct GainRange {
    min: f32,
    max: f32,
}

#[derive(Clone, Debug)]
struct RequestChannelOwned {
    spec: ChannelSpec,
    sample_count: usize,
    pixel_max: u16,
    file: String,
}

pub(crate) struct TempRequest {
    path: PathBuf,
    channels: Vec<RequestChannelOwned>,
}

impl TempRequest {
    pub(crate) fn create(plan: &SamplePlan) -> Result<Self, String> {
        if plan.channels.is_empty() || plan.channels.len() > MAX_CHANNELS {
            return Err(String::from("BaSiC request has an invalid channel count."));
        }
        for channel in &plan.channels {
            validate_channel_spec(&channel.spec)?;
        }
        let aggregate_bytes = plan.channels.iter().try_fold(0_u64, |total, channel| {
            expected_sample_bytes(channel.candidates.len()).and_then(|size| {
                total
                    .checked_add(size)
                    .ok_or_else(|| String::from("BaSiC aggregate sample size overflow."))
            })
        })?;
        if aggregate_bytes > MAX_INPUT_BYTES {
            return Err(String::from(
                "BaSiC samples exceed the 32 MiB protocol v1 input bound.",
            ));
        }
        let mut request = Self {
            path: create_private_temp_dir()?,
            channels: Vec::with_capacity(plan.channels.len()),
        };
        for channel in &plan.channels {
            let file = format!("channel-{}.u16le", channel.spec.id);
            create_new_file(&request.path.join(&file))?;
            request.channels.push(RequestChannelOwned {
                spec: channel.spec.clone(),
                sample_count: channel.candidates.len(),
                pixel_max: channel.pixel_max,
                file,
            });
        }
        Ok(request)
    }

    pub(crate) fn append_sample(&self, channel: usize, pixels: &[u16]) -> Result<(), String> {
        if pixels.len() != grid_pixels() {
            return Err(String::from("BaSiC sample has the wrong shape."));
        }
        let descriptor = self
            .channels
            .get(channel)
            .ok_or_else(|| String::from("BaSiC sample channel is out of range."))?;
        let path = self.path.join(&descriptor.file);
        let file = OpenOptions::new()
            .append(true)
            .open(&path)
            .map_err(|error| {
                format!("Could not append BaSiC sample {}: {error}", path.display())
            })?;
        let mut writer = BufWriter::new(file);
        for value in pixels {
            writer
                .write_all(&value.to_le_bytes())
                .map_err(|error| format!("Could not write BaSiC sample: {error}"))?;
        }
        writer
            .flush()
            .map_err(|error| format!("Could not flush BaSiC sample: {error}"))
    }

    pub(crate) fn write_manifest(&self) -> Result<(), String> {
        for channel in &self.channels {
            let actual = fs::metadata(self.path.join(&channel.file))
                .map_err(|error| format!("Could not inspect BaSiC samples: {error}"))?
                .len();
            let expected = expected_sample_bytes(channel.sample_count)?;
            if actual != expected {
                return Err(format!(
                    "BaSiC sample file has {actual} bytes; expected {expected}."
                ));
            }
        }
        let channels = self
            .channels
            .iter()
            .map(|channel| RequestChannel {
                id: &channel.spec.id,
                c_index: channel.spec.c_index,
                name: &channel.spec.name,
                sample_count: channel.sample_count,
                pixel_max: channel.pixel_max,
                is_phase: channel.spec.is_phase,
                file: &channel.file,
            })
            .collect();
        let manifest = RequestManifest {
            version: 1,
            width: GRID_WIDTH,
            height: GRID_HEIGHT,
            channels,
        };
        let encoded = serde_json::to_vec(&manifest)
            .map_err(|error| format!("Could not encode BaSiC request manifest: {error}"))?;
        if encoded.len() > MAX_MANIFEST_BYTES {
            return Err(String::from(
                "BaSiC request.json exceeds the 64 KiB protocol bound.",
            ));
        }
        let path = self.path.join("request.json");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| format!("Could not create BaSiC request manifest: {error}"))?;
        let mut writer = BufWriter::new(file);
        writer
            .write_all(&encoded)
            .and_then(|()| writer.flush())
            .map_err(|error| format!("Could not write BaSiC request manifest: {error}"))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn read_response(&self) -> Result<ProfileSet, String> {
        read_response(&self.path, &self.channels)
    }
}

impl Drop for TempRequest {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn helper_from_env() -> Result<PathBuf, String> {
    let value = std::env::var_os("CZI_BASIC_HELPER").ok_or_else(|| {
        String::from("Set CZI_BASIC_HELPER to an absolute helper executable path.")
    })?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(String::from("CZI_BASIC_HELPER must be an absolute path."));
    }
    Ok(path)
}

pub(crate) fn run_helper(
    helper: &Path,
    request: &TempRequest,
    cancelled: &AtomicBool,
) -> Result<ProfileSet, String> {
    if !helper.is_absolute() {
        return Err(String::from("BaSiC helper path is not absolute."));
    }
    let helper = fs::canonicalize(helper)
        .map_err(|error| format!("Could not resolve BaSiC helper executable: {error}"))?;
    if !helper.is_file() {
        return Err(String::from("BaSiC helper does not name a regular file."));
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(String::from("BaSiC preparation cancelled."));
    }
    let mut child = helper_command(&helper, request.path())
        .spawn()
        .map_err(|error| format!("Could not start BaSiC helper: {error}"))?;
    loop {
        if cancelled.load(Ordering::Acquire) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(String::from("BaSiC preparation cancelled."));
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => {
                return Err(format!("BaSiC helper exited unsuccessfully ({status})."));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Could not wait for BaSiC helper: {error}"));
            }
        }
    }
    if cancelled.load(Ordering::Acquire) {
        return Err(String::from("BaSiC preparation cancelled."));
    }
    request.read_response()
}

fn helper_command(helper: &Path, request_path: &Path) -> Command {
    let mut command = Command::new(helper);
    command
        .arg("--request-dir")
        .arg(request_path)
        .current_dir(request_path)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[allow(clippy::too_many_lines)]
pub(crate) fn plan_samples(
    index: &DatasetIndex,
    query: &TileQueryIndex,
    specs: &[ChannelSpec],
    cancelled: &AtomicBool,
) -> Result<SamplePlan, String> {
    check_cancelled(cancelled)?;
    if specs.is_empty() || specs.len() > MAX_CHANNELS {
        return Err(String::from("CZI has an invalid sparse channel count."));
    }
    let expected_channels = &query.axis_choices().c;
    if specs.len() != expected_channels.len()
        || specs
            .iter()
            .zip(expected_channels)
            .any(|(spec, expected)| spec.c_index != *expected)
    {
        return Err(String::from(
            "BaSiC request does not exactly match the CZI sparse channels.",
        ));
    }
    let verified_scales = verified_scales(index, query, cancelled)?;
    let mut native_channels = Vec::with_capacity(specs.len());
    for spec in specs {
        check_cancelled(cancelled)?;
        validate_channel_spec(spec)?;
        let mut native_hits = Vec::new();
        for plane in query.planes().filter(|plane| plane.key.c == spec.c_index) {
            check_cancelled(cancelled)?;
            let native = *plane
                .scales
                .first()
                .ok_or_else(|| String::from("Sparse plane has no pyramid scale."))?;
            native_hits.extend(
                query
                    .query_at_scale(plane.key.into(), plane.world_bounds, native)
                    .map_err(|error| error.to_string())?
                    .hits,
            );
        }
        native_hits.sort_unstable_by_key(hit_order_key);
        if native_hits.len() < MIN_SAMPLES_PER_CHANNEL {
            return Err(format!(
                "{} has only {} acquisition positions; at least {} are required.",
                spec.name,
                native_hits.len(),
                MIN_SAMPLES_PER_CHANNEL
            ));
        }
        let pixel_max = channel_pixel_max(index, &native_hits, cancelled)?;
        native_channels.push((spec, pixel_max, native_hits));
    }
    let phase_alignment = if specs.iter().any(|spec| spec.is_phase) {
        let mut common: Option<Vec<AcquisitionIdentity>> = None;
        for (_, _, hits) in &native_channels {
            check_cancelled(cancelled)?;
            let identities = unique_acquisition_hits(hits, cancelled)?
                .keys()
                .copied()
                .collect::<Vec<_>>();
            common = Some(match common {
                None => identities,
                Some(previous) => previous
                    .into_iter()
                    .filter(|identity| identities.binary_search(identity).is_ok())
                    .collect(),
            });
        }
        common
    } else {
        None
    };
    let available_count = phase_alignment.as_ref().map_or_else(
        || {
            native_channels
                .iter()
                .map(|(_, _, hits)| hits.len())
                .min()
                .unwrap_or_default()
        },
        Vec::len,
    );
    let common_count = available_count
        .min(MAX_SAMPLES_PER_CHANNEL)
        .min(aggregate_sample_cap(native_channels.len())?);
    if common_count < MIN_SAMPLES_PER_CHANNEL {
        return Err(String::from(
            "BaSiC protocol bounds leave fewer than 32 samples per sparse channel.",
        ));
    }
    let selected_phase_identities = phase_alignment
        .as_ref()
        .map(|identities| stratified_values(identities, common_count));
    let mut channels = Vec::with_capacity(specs.len());
    for (spec, pixel_max, native_hits) in native_channels {
        check_cancelled(cancelled)?;
        let selected = if let Some(identities) = selected_phase_identities.as_ref() {
            let hits = unique_acquisition_hits(&native_hits, cancelled)?;
            identities
                .iter()
                .map(|identity| {
                    hits.get(identity).copied().ok_or_else(|| {
                        String::from("Aligned BaSiC acquisition position disappeared.")
                    })
                })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            stratified_hits(&native_hits, common_count)
        };
        let candidates = selected
            .into_iter()
            .map(|native| {
                check_cancelled(cancelled)?;
                sample_representation(query, native, &verified_scales)
                    .map(|tile_id| SampleCandidate { tile_id })
            })
            .collect::<Result<Vec<_>, _>>()?;
        channels.push(ChannelSamplePlan {
            spec: spec.clone(),
            pixel_max,
            candidates,
        });
    }
    Ok(SamplePlan {
        channels,
        verified_scales,
    })
}

pub(crate) fn verified_scales(
    index: &DatasetIndex,
    query: &TileQueryIndex,
    cancelled: &AtomicBool,
) -> Result<HashMap<PlaneKey, Vec<PyramidScale>>, String> {
    let mut result = HashMap::new();
    for plane in query.planes() {
        check_cancelled(cancelled)?;
        let native_scale = *plane
            .scales
            .first()
            .ok_or_else(|| String::from("Sparse plane has no pyramid scale."))?;
        let native = query
            .query_at_scale(plane.key.into(), plane.world_bounds, native_scale)
            .map_err(|error| error.to_string())?
            .hits;
        let native_signatures = unique_hit_signatures(index, &native, cancelled)?;
        let mut scales = Vec::new();
        if let Some(native_signatures) = native_signatures {
            for scale in &plane.scales {
                check_cancelled(cancelled)?;
                let hits = query
                    .query_at_scale(plane.key.into(), plane.world_bounds, *scale)
                    .map_err(|error| error.to_string())?
                    .hits;
                if unique_hit_signatures(index, &hits, cancelled)?.as_ref()
                    == Some(&native_signatures)
                {
                    scales.push(*scale);
                }
            }
        }
        if !scales.contains(&native_scale) {
            scales.insert(0, native_scale);
        }
        result.insert(plane.key, scales);
    }
    Ok(result)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Acquire) {
        Err(String::from("BaSiC preparation cancelled."))
    } else {
        Ok(())
    }
}

pub(crate) fn select_verified_scale(
    available: &[PyramidScale],
    verified: &[PyramidScale],
    target_downsample: f64,
) -> Option<PyramidScale> {
    let mut selected = *available.first()?;
    for scale in available {
        if verified.contains(scale) && scale.as_f64() <= target_downsample {
            selected = *scale;
        }
    }
    Some(selected)
}

pub(crate) fn downsample_tile(tile: &DecodedTile) -> Result<Vec<u16>, String> {
    let width =
        usize::try_from(tile.width).map_err(|_| String::from("Tile width is too large."))?;
    let height =
        usize::try_from(tile.height).map_err(|_| String::from("Tile height is too large."))?;
    let count = width
        .checked_mul(height)
        .ok_or_else(|| String::from("Tile shape overflows memory bounds."))?;
    let values = match &tile.pixels {
        DecodedPixels::Gray8(values) if values.len() == count => values
            .iter()
            .map(|value| u16::from(*value))
            .collect::<Vec<_>>(),
        DecodedPixels::Gray16(values) if values.len() == count => values.clone(),
        _ => return Err(String::from("Decoded BaSiC sample has an invalid shape.")),
    };
    Ok(resize_bilinear_u16(
        &values,
        width,
        height,
        GRID_WIDTH as usize,
        GRID_HEIGHT as usize,
    ))
}

pub(crate) fn correct_value(
    raw: u16,
    x: usize,
    y: usize,
    tile_width: usize,
    tile_height: usize,
    profile: &ChannelProfile,
) -> u16 {
    if raw > profile.pixel_max
        || tile_width == 0
        || tile_height == 0
        || profile.gain.len() != grid_pixels()
        || profile.support.len() != grid_pixels()
    {
        return raw;
    }
    let gx = ((x as f32 + 0.5) * GRID_WIDTH as f32 / tile_width as f32 - 0.5)
        .clamp(0.0, GRID_WIDTH.saturating_sub(1) as f32);
    let gy = ((y as f32 + 0.5) * GRID_HEIGHT as f32 / tile_height as f32 - 0.5)
        .clamp(0.0, GRID_HEIGHT.saturating_sub(1) as f32);
    let support_x = gx.round() as usize;
    let support_y = gy.round() as usize;
    if profile.support[support_y * GRID_WIDTH as usize + support_x] == 0 {
        return raw;
    }
    let Some(gain) = supported_bilinear_gain(gx, gy, profile) else {
        return raw;
    };
    let corrected = (f32::from(raw) / gain).round();
    if !corrected.is_finite() {
        return raw;
    }
    corrected.clamp(0.0, f32::from(profile.pixel_max)) as u16
}

fn supported_bilinear_gain(x: f32, y: f32, profile: &ChannelProfile) -> Option<f32> {
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(GRID_WIDTH as usize - 1);
    let y1 = (y0 + 1).min(GRID_HEIGHT as usize - 1);
    let tx = x - x0 as f32;
    let ty = y - y0 as f32;
    let points = [
        (x0, y0, (1.0 - tx) * (1.0 - ty)),
        (x1, y0, tx * (1.0 - ty)),
        (x0, y1, (1.0 - tx) * ty),
        (x1, y1, tx * ty),
    ];
    let mut total = 0.0;
    let mut weight = 0.0;
    for (px, py, point_weight) in points {
        let index = py * GRID_WIDTH as usize + px;
        if profile.support[index] == 1 {
            total += profile.gain[index] * point_weight;
            weight += point_weight;
        }
    }
    if weight <= 0.0 {
        return None;
    }
    let gain = total / weight;
    (gain.is_finite() && (MIN_SUPPORTED_GAIN..=MAX_SUPPORTED_GAIN).contains(&gain)).then_some(gain)
}

fn read_response(path: &Path, request: &[RequestChannelOwned]) -> Result<ProfileSet, String> {
    let response_path = path.join("response.json");
    let response_file = open_checked_regular(&response_path, None, RESPONSE_JSON_LIMIT)
        .map_err(|error| format!("BaSiC helper response.json is invalid: {error}"))?;
    let mut response_bytes = Vec::new();
    response_file
        .take(RESPONSE_JSON_LIMIT + 1)
        .read_to_end(&mut response_bytes)
        .map_err(|error| format!("Could not read BaSiC response.json: {error}"))?;
    if response_bytes.len() as u64 > RESPONSE_JSON_LIMIT {
        return Err(String::from("BaSiC response.json exceeds its size bound."));
    }
    let response: ResponseManifest = serde_json::from_slice(&response_bytes)
        .map_err(|error| format!("Invalid BaSiC response.json: {error}"))?;
    if response.version != 1
        || response.status != "preview-not-held-out-validated"
        || response.darkfield_enabled
    {
        return Err(String::from(
            "BaSiC response version, validation status, or darkfield mode is invalid.",
        ));
    }
    if response.channels.len() != request.len() {
        return Err(String::from(
            "BaSiC response channel count does not match the request.",
        ));
    }
    let expected = request
        .iter()
        .map(|channel| (channel.spec.id.as_str(), channel))
        .collect::<BTreeMap<_, _>>();
    let mut profiles = HashMap::with_capacity(request.len());
    for channel in response.channels {
        let descriptor = expected
            .get(channel.id.as_str())
            .ok_or_else(|| String::from("BaSiC response contains an unknown channel."))?;
        if channel.method != "BaSiC approximate"
            || channel.version != 1
            || channel.sample_count != descriptor.sample_count
            || channel.gain_file != format!("gain-{}.f32le", channel.id)
            || channel.support_file != format!("support-{}.u8", channel.id)
            || !channel.support_fraction.is_finite()
            || !(0.0..=1.0).contains(&channel.support_fraction)
            || !channel.gain_range.min.is_finite()
            || !channel.gain_range.max.is_finite()
            || channel.gain_range.min < MIN_SUPPORTED_GAIN
            || channel.gain_range.max > MAX_SUPPORTED_GAIN
            || channel.gain_range.min > channel.gain_range.max
        {
            return Err(format!(
                "BaSiC response metadata for {} is invalid.",
                channel.id
            ));
        }
        let pixel_max = descriptor.pixel_max;
        let c_index = descriptor.spec.c_index;
        if profiles.contains_key(&c_index) {
            return Err(String::from("BaSiC response repeats a channel."));
        }
        let gain_path = checked_response_file(path, &channel.gain_file)?;
        let support_path = checked_response_file(path, &channel.support_file)?;
        let gain = read_f32_file(&gain_path)?;
        let support = read_u8_file(&support_path)?;
        let profile = ChannelProfile {
            id: channel.id.clone(),
            c_index,
            pixel_max,
            gain: Arc::new(gain),
            support: Arc::new(support),
        };
        if !profile.structurally_valid() {
            return Err(format!(
                "BaSiC profile for C {} is structurally invalid.",
                profile.c_index
            ));
        }
        validate_reported_profile(&profile, &channel)?;
        profiles.insert(profile.c_index, profile);
    }
    if profiles.len() != expected.len() {
        return Err(String::from("BaSiC response omitted a requested channel."));
    }
    Ok(ProfileSet { channels: profiles })
}

fn validate_reported_profile(
    profile: &ChannelProfile,
    response: &ResponseChannel,
) -> Result<(), String> {
    let supported = profile
        .gain
        .iter()
        .zip(profile.support.iter())
        .filter_map(|(gain, support)| (*support == 1).then_some(*gain))
        .collect::<Vec<_>>();
    let fraction = supported.len() as f32 / grid_pixels() as f32;
    let minimum = supported.iter().copied().fold(f32::INFINITY, f32::min);
    let maximum = supported.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !approximately_equal(fraction, response.support_fraction)
        || !approximately_equal(minimum, response.gain_range.min)
        || !approximately_equal(maximum, response.gain_range.max)
    {
        return Err(format!(
            "BaSiC response metadata does not match profile data for {}.",
            response.id
        ));
    }
    Ok(())
}

fn approximately_equal(left: f32, right: f32) -> bool {
    (left - right).abs() <= 1.0e-5 * left.abs().max(right.abs()).max(1.0)
}

fn checked_response_file(directory: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    let mut components = relative.components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(String::from(
            "BaSiC response file path must be one relative filename.",
        ));
    }
    let path = directory.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("Could not inspect BaSiC response file: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err(String::from(
            "BaSiC response data must be a regular non-symlink file.",
        ));
    }
    Ok(path)
}

fn read_f32_file(path: &Path) -> Result<Vec<f32>, String> {
    let expected = grid_pixels()
        .checked_mul(4)
        .ok_or_else(|| String::from("BaSiC gain size overflow."))?;
    let bytes = read_exact_file(path, expected)?;
    Ok(bytes
        .chunks_exact(4)
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .collect())
}

fn read_u8_file(path: &Path) -> Result<Vec<u8>, String> {
    read_exact_file(path, grid_pixels())
}

fn read_exact_file(path: &Path, expected: usize) -> Result<Vec<u8>, String> {
    let expected_u64 = expected as u64;
    let mut file = open_checked_regular(path, Some(expected_u64), expected_u64)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(expected)
        .map_err(|_| String::from("Could not allocate BaSiC response buffer."))?;
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("Could not read BaSiC response data: {error}"))?;
    if bytes.len() != expected {
        return Err(String::from(
            "BaSiC response data changed while being read.",
        ));
    }
    Ok(bytes)
}

fn open_checked_regular(
    path: &Path,
    expected_size: Option<u64>,
    maximum_size: u64,
) -> Result<File, String> {
    let before = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect BaSiC response file: {error}"))?;
    if !before.file_type().is_file() || before.len() > maximum_size {
        return Err(String::from(
            "BaSiC response data is not a bounded regular non-symlink file.",
        ));
    }
    let file =
        File::open(path).map_err(|error| format!("Could not open BaSiC response file: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("Could not inspect opened BaSiC response file: {error}"))?;
    if !opened.file_type().is_file() || before.len() != opened.len() {
        return Err(String::from(
            "BaSiC response file changed while it was being opened.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            return Err(String::from(
                "BaSiC response file changed while it was being opened.",
            ));
        }
    }
    if let Some(expected) = expected_size
        && opened.len() != expected
    {
        return Err(format!(
            "BaSiC response data {} has {} bytes; expected {expected}.",
            path.display(),
            opened.len(),
        ));
    }
    Ok(file)
}

fn channel_pixel_max(
    index: &DatasetIndex,
    hits: &[TileHit],
    cancelled: &AtomicBool,
) -> Result<u16, String> {
    let mut maximum = None;
    for hit in hits {
        check_cancelled(cancelled)?;
        let tile = index
            .tile(hit.tile_id.index())
            .ok_or_else(|| String::from("BaSiC sample tile is missing from the CZI index."))?;
        let value = match tile.entry.pixel_type {
            PixelType::Gray8 => u16::from(u8::MAX),
            PixelType::Gray16 => u16::MAX,
            other => return Err(format!("BaSiC preview does not support {other:?} pixels.")),
        };
        if maximum.is_some_and(|existing| existing != value) {
            return Err(String::from(
                "A sparse C channel mixes Gray8 and Gray16 tiles.",
            ));
        }
        maximum = Some(value);
    }
    maximum.ok_or_else(|| String::from("BaSiC channel has no native detector tiles."))
}

fn sample_representation(
    query: &TileQueryIndex,
    native: TileHit,
    verified: &HashMap<PlaneKey, Vec<PyramidScale>>,
) -> Result<TileId, String> {
    query
        .plane(native.plane)
        .ok_or_else(|| String::from("BaSiC native plane disappeared."))?;
    let mut selected = native;
    for scale in verified.get(&native.plane).into_iter().flatten() {
        let hits = query
            .query_at_scale(native.plane.into(), native.logical_rect, *scale)
            .map_err(|error| error.to_string())?;
        if let Some(hit) = hits
            .hits
            .into_iter()
            .find(|hit| hit_key(*hit) == hit_key(native))
            && hit.physical_stored_size.width >= GRID_WIDTH
            && hit.physical_stored_size.height >= GRID_HEIGHT
            && hit.scale > selected.scale
        {
            selected = hit;
        }
    }
    Ok(selected.tile_id)
}

fn unique_hit_signatures(
    index: &DatasetIndex,
    hits: &[TileHit],
    cancelled: &AtomicBool,
) -> Result<Option<Vec<DetectorTileSignature>>, String> {
    let mut signatures = hits
        .iter()
        .map(|hit| {
            check_cancelled(cancelled)?;
            let pixel_type = index
                .tile(hit.tile_id.index())
                .ok_or_else(|| String::from("Pyramid tile is missing from the CZI index."))?
                .entry
                .pixel_type;
            Ok((
                (
                    hit.logical_rect.min_x,
                    hit.logical_rect.min_y,
                    hit.logical_rect.max_x,
                    hit.logical_rect.max_y,
                    hit.m_index,
                ),
                pixel_type,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    signatures.sort_unstable_by_key(|(identity, _)| *identity);
    let unique = signatures.windows(2).all(|pair| pair[0].0 != pair[1].0);
    Ok(unique.then_some(signatures))
}

fn hit_key(hit: TileHit) -> (SpatialRect, Option<i32>) {
    (hit.logical_rect, hit.m_index)
}

fn hit_order_key(hit: &TileHit) -> (PlaneKey, i64, i64, Option<i32>, TileId) {
    (
        hit.plane,
        hit.logical_rect.min_y,
        hit.logical_rect.min_x,
        hit.m_index,
        hit.tile_id,
    )
}

fn acquisition_identity(hit: TileHit) -> AcquisitionIdentity {
    (
        hit.plane.scene,
        hit.plane.z,
        hit.plane.t,
        hit.logical_rect.min_x,
        hit.logical_rect.min_y,
        hit.logical_rect.max_x,
        hit.logical_rect.max_y,
        hit.m_index,
    )
}

fn unique_acquisition_hits(
    hits: &[TileHit],
    cancelled: &AtomicBool,
) -> Result<BTreeMap<AcquisitionIdentity, TileHit>, String> {
    let mut indexed = BTreeMap::new();
    for hit in hits {
        check_cancelled(cancelled)?;
        if indexed.insert(acquisition_identity(*hit), *hit).is_some() {
            return Err(String::from(
                "BaSiC acquisition positions are not one-to-one within a sparse channel.",
            ));
        }
    }
    Ok(indexed)
}

fn stratified_hits(hits: &[TileHit], cap: usize) -> Vec<TileHit> {
    stratified_values(hits, cap)
}

fn stratified_values<T: Copy>(values: &[T], cap: usize) -> Vec<T> {
    if values.len() <= cap {
        return values.to_vec();
    }
    (0..cap)
        .map(|stratum| {
            let numerator = (2 * stratum + 1) * values.len();
            values[numerator / (2 * cap)]
        })
        .collect()
}

fn resize_bilinear_u16(
    source: &[u16],
    source_width: usize,
    source_height: usize,
    target_width: usize,
    target_height: usize,
) -> Vec<u16> {
    let mut output = Vec::with_capacity(target_width.saturating_mul(target_height));
    for y in 0..target_height {
        let source_y = ((y as f64 + 0.5) * source_height as f64 / target_height as f64 - 0.5)
            .clamp(0.0, source_height.saturating_sub(1) as f64);
        let y0 = source_y.floor() as usize;
        let y1 = (y0 + 1).min(source_height - 1);
        let fy = source_y - y0 as f64;
        for x in 0..target_width {
            let source_x = ((x as f64 + 0.5) * source_width as f64 / target_width as f64 - 0.5)
                .clamp(0.0, source_width.saturating_sub(1) as f64);
            let x0 = source_x.floor() as usize;
            let x1 = (x0 + 1).min(source_width - 1);
            let fx = source_x - x0 as f64;
            let top = f64::from(source[y0 * source_width + x0]) * (1.0 - fx)
                + f64::from(source[y0 * source_width + x1]) * fx;
            let bottom = f64::from(source[y1 * source_width + x0]) * (1.0 - fx)
                + f64::from(source[y1 * source_width + x1]) * fx;
            output.push((top * (1.0 - fy) + bottom * fy).round() as u16);
        }
    }
    output
}

fn validate_channel_spec(spec: &ChannelSpec) -> Result<(), String> {
    if spec.c_index < 0 {
        return Err(String::from(
            "BaSiC protocol v1 requires non-negative sparse C indices.",
        ));
    }
    if spec.id.is_empty()
        || spec.id.len() > 64
        || !spec
            .id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(String::from("BaSiC channel id is invalid."));
    }
    if spec.name.is_empty()
        || spec.name.chars().count() > MAX_CHANNEL_NAME_CHARS
        || spec.name.chars().any(char::is_control)
    {
        return Err(String::from("BaSiC channel name is invalid."));
    }
    Ok(())
}

fn expected_sample_bytes(sample_count: usize) -> Result<u64, String> {
    let count =
        u64::try_from(sample_count).map_err(|_| String::from("Sample count is too large."))?;
    count
        .checked_mul(u64::from(GRID_WIDTH))
        .and_then(|value| value.checked_mul(u64::from(GRID_HEIGHT)))
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| String::from("BaSiC sample file size overflow."))
}

fn aggregate_sample_cap(channel_count: usize) -> Result<usize, String> {
    let channels = u64::try_from(channel_count)
        .map_err(|_| String::from("BaSiC channel count is too large."))?;
    let bytes_per_channel_sample = u64::from(GRID_WIDTH)
        .checked_mul(u64::from(GRID_HEIGHT))
        .and_then(|value| value.checked_mul(2))
        .ok_or_else(|| String::from("BaSiC sample shape size overflow."))?;
    let denominator = channels
        .checked_mul(bytes_per_channel_sample)
        .ok_or_else(|| String::from("BaSiC aggregate sample size overflow."))?;
    if denominator == 0 {
        return Err(String::from("BaSiC channel count must be positive."));
    }
    usize::try_from(MAX_INPUT_BYTES / denominator)
        .map_err(|_| String::from("BaSiC aggregate sample cap does not fit memory bounds."))
}

const fn grid_pixels() -> usize {
    GRID_WIDTH as usize * GRID_HEIGHT as usize
}

fn create_private_temp_dir() -> Result<PathBuf, String> {
    let root = std::env::temp_dir();
    let root = if root.is_absolute() {
        root
    } else {
        std::env::current_dir()
            .map_err(|error| format!("Could not resolve temporary directory: {error}"))?
            .join(root)
    };
    for _ in 0..128 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!("czi-basic-{}-{sequence}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Err(error) =
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                    {
                        let _ = fs::remove_dir(&path);
                        return Err(format!("Could not secure BaSiC request directory: {error}"));
                    }
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("Could not create BaSiC request directory: {error}")),
        }
    }
    Err(String::from(
        "Could not choose a unique BaSiC request directory.",
    ))
}

fn create_new_file(path: &Path) -> Result<(), String> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
        .map_err(|error| format!("Could not create BaSiC sample file: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(gain: Vec<f32>, support: Vec<u8>, pixel_max: u16) -> ChannelProfile {
        ChannelProfile {
            id: String::from("c0"),
            c_index: 0,
            pixel_max,
            gain: Arc::new(gain),
            support: Arc::new(support),
        }
    }

    #[test]
    fn correction_uses_detector_orientation_bilinear_gain_and_nearest_support() {
        let mut gain = vec![1.0; grid_pixels()];
        for y in 0..GRID_HEIGHT as usize {
            for x in 0..GRID_WIDTH as usize {
                gain[y * GRID_WIDTH as usize + x] = 1.0 + x as f32 / 127.0 + y as f32 * 0.0;
            }
        }
        let support = vec![1; grid_pixels()];
        let profile = profile(gain, support, u16::MAX);
        assert_eq!(correct_value(1_000, 0, 0, 2, 1, &profile), 801);
        assert_eq!(correct_value(1_000, 1, 0, 2, 1, &profile), 571);
    }

    #[test]
    fn unsupported_pixels_stay_raw_and_correction_rounds_and_clips() {
        let gain = vec![0.25; grid_pixels()];
        let mut support = vec![1; grid_pixels()];
        support[0] = 0;
        let profile = profile(gain, support, 255);
        assert_eq!(correct_value(100, 0, 0, 256, 256, &profile), 100);
        assert_eq!(correct_value(100, 128, 128, 256, 256, &profile), 255);
    }

    #[test]
    fn response_rejects_paths_shapes_nonfinite_and_nonpositive_supported_gain() {
        let spec = ChannelSpec {
            id: String::from("c0"),
            c_index: 0,
            name: String::from("Phase"),
            is_phase: true,
        };
        let plan = SamplePlan {
            channels: vec![ChannelSamplePlan {
                spec,
                pixel_max: 255,
                candidates: vec![SampleCandidate { tile_id: TileId(0) }; MIN_SAMPLES_PER_CHANNEL],
            }],
            verified_scales: HashMap::new(),
        };
        let request = TempRequest::create(&plan).expect("temp request");
        assert!(
            checked_response_file(request.path(), "../gain")
                .unwrap_err()
                .contains("relative filename")
        );
        fs::write(
            request.path().join("response.json"),
            r#"{"version":1,"status":"preview-not-held-out-validated","darkfield_enabled":false,"channels":[{"id":"c0","method":"BaSiC approximate","version":1,"sample_count":32,"support_fraction":1.0,"gain_range":{"min":1.0,"max":1.0},"gain_file":"../gain","support_file":"support"}]}"#,
        )
        .expect("response");
        assert!(request.read_response().unwrap_err().contains("metadata"));

        fs::write(
            request.path().join("response.json"),
            r#"{"version":1,"status":"preview-not-held-out-validated","darkfield_enabled":true,"channels":[{"id":"c0","method":"BaSiC approximate","version":1,"sample_count":32,"support_fraction":1.0,"gain_range":{"min":1.0,"max":1.0},"gain_file":"gain-c0.f32le","support_file":"support-c0.u8"}]}"#,
        )
        .expect("response");
        assert!(request.read_response().unwrap_err().contains("darkfield"));

        fs::write(
            request.path().join("response.json"),
            r#"{"version":1,"status":"preview-not-held-out-validated","darkfield_enabled":false,"channels":[{"id":"c0","method":"BaSiC approximate","version":1,"sample_count":32,"support_fraction":1.0,"gain_range":{"min":1.0,"max":1.0},"gain_file":"gain-c0.f32le","support_file":"support-c0.u8"}]}"#,
        )
        .expect("response");
        fs::write(request.path().join("gain-c0.f32le"), b"short").expect("short gain");
        fs::write(request.path().join("support-c0.u8"), vec![1; grid_pixels()]).expect("support");
        assert!(request.read_response().unwrap_err().contains("expected"));

        let mut gains = vec![1.0_f32; grid_pixels()];
        gains[7] = f32::NAN;
        fs::write(
            request.path().join("gain-c0.f32le"),
            gains
                .iter()
                .flat_map(|gain| gain.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .expect("gain");
        assert!(
            request
                .read_response()
                .unwrap_err()
                .contains("structurally invalid")
        );

        fs::write(
            request.path().join("gain-c0.f32le"),
            vec![0.0_f32; grid_pixels()]
                .iter()
                .flat_map(|gain| gain.to_le_bytes())
                .collect::<Vec<_>>(),
        )
        .expect("gain");
        fs::write(
            request.path().join("response.json"),
            r#"{"version":1,"status":"preview-not-held-out-validated","darkfield_enabled":false,"channels":[{"id":"c0","method":"BaSiC approximate","version":1,"sample_count":32,"support_fraction":1.0,"gain_range":{"min":0.0,"max":0.0},"gain_file":"gain-c0.f32le","support_file":"support-c0.u8"}]}"#,
        )
        .expect("response");
        assert!(request.read_response().unwrap_err().contains("metadata"));
    }

    #[test]
    fn all_channel_readiness_requires_exact_structurally_valid_set() {
        let valid = profile(vec![1.0; grid_pixels()], vec![1; grid_pixels()], 255);
        let profiles = ProfileSet {
            channels: HashMap::from([(0, valid)]),
        };
        assert!(profiles.is_ready_for(&[0]));
        assert!(!profiles.is_ready_for(&[0, 4]));
        assert!(!profiles.is_ready_for(&[]));
    }

    #[test]
    fn stratified_selection_is_deterministic_and_bounded() {
        let hits = (0..1_000)
            .map(|index| TileHit {
                tile_id: TileId(index),
                plane: PlaneKey::new(0, czi_core::SceneId::Implicit, 0, 0),
                logical_rect: SpatialRect::new(
                    i64::try_from(index).expect("small index"),
                    0,
                    i64::try_from(index + 1).expect("small index"),
                    1,
                )
                .expect("rect"),
                physical_stored_size: czi_core::PhysicalSize {
                    width: 256,
                    height: 256,
                },
                scale: PyramidScale::new(1, 1).expect("scale"),
                m_index: None,
                paint_order: index,
            })
            .collect::<Vec<_>>();
        let first = stratified_hits(&hits, MAX_SAMPLES_PER_CHANNEL);
        let second = stratified_hits(&hits, MAX_SAMPLES_PER_CHANNEL);
        assert_eq!(first, second);
        assert_eq!(first.len(), MAX_SAMPLES_PER_CHANNEL);
        assert!(first.first().unwrap().tile_id.0 < 2);
        assert!(first.last().unwrap().tile_id.0 > 997);
    }

    #[test]
    fn aggregate_protocol_bound_reduces_the_common_sample_count() {
        assert_eq!(aggregate_sample_cap(3).expect("3 channels"), 341);
        assert_eq!(aggregate_sample_cap(17).expect("17 channels"), 60);
        assert_eq!(aggregate_sample_cap(32).expect("32 channels"), 32);
        assert_eq!(aggregate_sample_cap(64).expect("64 channels"), 16);
        let bytes =
            expected_sample_bytes(aggregate_sample_cap(32).expect("cap")).expect("bytes") * 32;
        assert_eq!(bytes, MAX_INPUT_BYTES);
        assert_eq!(expected_sample_bytes(300).expect("bytes") * 3, 29_491_200);
    }

    #[test]
    fn request_manifest_contains_only_sample_metadata_not_source_handles() {
        let request = RequestManifest {
            version: 1,
            width: GRID_WIDTH,
            height: GRID_HEIGHT,
            channels: vec![RequestChannel {
                id: "c0",
                c_index: 0,
                name: "Phase",
                sample_count: 32,
                pixel_max: 255,
                is_phase: true,
                file: "channel-c0.u16le",
            }],
        };
        let json = serde_json::to_string(&request).expect("serialize");
        assert!(!json.contains(".czi"));
        assert!(!json.contains("ssh"));
        assert!(!json.contains("profile"));
        assert!(!json.contains("credential"));
    }

    #[test]
    fn helper_command_receives_only_the_app_owned_request_directory() {
        let helper = Path::new("/opt/local/bin/basic-helper");
        let request = Path::new("/private/tmp/czi-basic-test");
        let command = helper_command(helper, request);
        assert_eq!(command.get_program(), helper.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            vec![std::ffi::OsStr::new("--request-dir"), request.as_os_str()]
        );
        assert_eq!(command.get_current_dir(), Some(request));
        let rendered = format!("{command:?}");
        assert!(!rendered.contains(".czi"));
        assert!(!rendered.contains("ssh-profile"));
        assert!(!rendered.contains("credential"));
        assert!(!rendered.contains("sftp"));
    }

    #[test]
    fn configured_helper_protocol_v1_round_trip() {
        let Some(helper) = std::env::var_os("CZI_BASIC_HELPER_INTEGRATION") else {
            return;
        };
        let plan = SamplePlan {
            channels: vec![ChannelSamplePlan {
                spec: ChannelSpec {
                    id: String::from("c0"),
                    c_index: 0,
                    name: String::from("Fluorescence"),
                    is_phase: false,
                },
                pixel_max: u16::MAX,
                candidates: vec![SampleCandidate { tile_id: TileId(0) }; MIN_SAMPLES_PER_CHANNEL],
            }],
            verified_scales: HashMap::new(),
        };
        let request = TempRequest::create(&plan).expect("request");
        for sample in 0..MIN_SAMPLES_PER_CHANNEL {
            let mut pixels = vec![0_u16; grid_pixels()];
            for (index, pixel) in pixels.iter_mut().enumerate() {
                *pixel = 1_000 + u16::try_from((index + sample) % 1_000).expect("bounded value");
            }
            request.append_sample(0, &pixels).expect("sample");
        }
        request.write_manifest().expect("manifest");
        let profiles = run_helper(Path::new(&helper), &request, &AtomicBool::new(false))
            .expect("protocol-compatible helper response");
        assert!(profiles.is_ready_for(&[0]));
    }
}
