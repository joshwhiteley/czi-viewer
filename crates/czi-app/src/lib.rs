#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use czi_core::{
    CziDataset, DecodedPixels, DecodedTile, DimensionCode, LocalFileSource, PhysicalSize,
    PixelType, PlaneInfo, PlaneKey, PlaneSelector, PyramidScale, SceneId, SpatialRect, TileHit,
    TileId, TileIndex, TileQueryIndex, ViewQuery,
};
use eframe::egui;

const CHANNEL_CAPACITY: usize = 8;
const METADATA_PREVIEW_CHARS: usize = 4_096;
const TEXTURE_CACHE_LIMIT: usize = 256 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DimensionChoices {
    present: bool,
    values: Vec<i32>,
}

impl DimensionChoices {
    fn default_value(&self) -> i32 {
        self.values.first().copied().unwrap_or(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SceneChoices {
    present: bool,
    values: Vec<SceneId>,
}

impl SceneChoices {
    fn default_value(&self) -> SceneId {
        self.values.first().copied().unwrap_or_default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct DatasetInfo {
    path: PathBuf,
    tile_count: usize,
    c: DimensionChoices,
    s: SceneChoices,
    z: DimensionChoices,
    t: DimensionChoices,
    planes: Vec<PlaneInfo>,
    pixel_type: PixelType,
    metadata_preview: String,
}

impl DatasetInfo {
    fn from_dataset(path: PathBuf, dataset: &CziDataset, query: &TileQueryIndex) -> Self {
        let tiles = &dataset.index().tiles;
        let metadata_preview = dataset.index().metadata.as_ref().map_or_else(
            || String::from("No global metadata XML."),
            |metadata| metadata.xml.chars().take(METADATA_PREVIEW_CHARS).collect(),
        );
        let pixel_type = tiles
            .first()
            .map_or(PixelType::Gray8, |tile| tile.entry.pixel_type);
        Self {
            path,
            tile_count: tiles.len(),
            c: dimension_choices(tiles, DimensionCode::C),
            s: scene_choices(query),
            z: dimension_choices(tiles, DimensionCode::Z),
            t: dimension_choices(tiles, DimensionCode::T),
            planes: query.planes().cloned().collect(),
            pixel_type,
            metadata_preview,
        }
    }

    fn default_selection(&self) -> PlaneSelection {
        PlaneSelection {
            c: self.c.default_value(),
            scene: self.s.default_value(),
            z: self.z.default_value(),
            t: self.t.default_value(),
        }
    }

    fn plane(&self, selection: PlaneSelection) -> Option<&PlaneInfo> {
        self.planes
            .iter()
            .find(|plane| plane.key == selection.key())
    }
}

type PlaneSelection = PlaneSelector;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Generations {
    source: u64,
    view: u64,
}

impl Generations {
    fn begin_source(&mut self) -> u64 {
        self.source = self.source.wrapping_add(1);
        self.view = self.view.wrapping_add(1);
        self.source
    }

    fn begin_view(&mut self) -> u64 {
        self.view = self.view.wrapping_add(1);
        self.view
    }

    fn accepts_source(&self, source: u64) -> bool {
        source == self.source
    }

    fn accepts_view(&self, source: u64, view: u64) -> bool {
        self.accepts_source(source) && view == self.view
    }
}

#[derive(Clone, Debug)]
struct ViewRequest {
    source_generation: u64,
    view_generation: u64,
    plane: PlaneSelector,
    viewport: SpatialRect,
    target_downsample: f64,
    resident_tile_ids: Vec<TileId>,
}

enum WorkerCommand {
    Open {
        path: PathBuf,
        source_generation: u64,
    },
    View(ViewRequest),
    Shutdown,
}

enum WorkerEvent {
    Opened {
        info: DatasetInfo,
        source_generation: u64,
    },
    OpenFailed {
        message: String,
        source_generation: u64,
    },
    TileLoaded {
        tile_id: TileId,
        plane: PlaneKey,
        logical_rect: SpatialRect,
        scale: PyramidScale,
        paint_order: usize,
        tile: DecodedTile,
        source_generation: u64,
        view_generation: u64,
    },
    ViewFinished {
        plane: PlaneKey,
        scale: PyramidScale,
        visible_tile_ids: Vec<TileId>,
        source_generation: u64,
        view_generation: u64,
    },
    ViewFailed {
        message: String,
        source_generation: u64,
        view_generation: u64,
    },
}

struct WorkerDataset {
    dataset: CziDataset,
    query: TileQueryIndex,
}

struct DatasetWorker {
    commands: SyncSender<WorkerCommand>,
    events: Receiver<WorkerEvent>,
    join: Option<JoinHandle<()>>,
}

impl DatasetWorker {
    fn spawn() -> Self {
        let (commands, command_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (event_tx, events) = mpsc::channel();
        let join = thread::Builder::new()
            .name(String::from("czi-dataset-worker"))
            .spawn(move || worker_loop(&command_rx, &event_tx))
            .expect("start CZI dataset worker");
        Self {
            commands,
            events,
            join: Some(join),
        }
    }

    fn send(&self, command: WorkerCommand) -> Result<(), String> {
        self.commands
            .try_send(command)
            .map_err(|error| format!("dataset worker command queue is unavailable: {error}"))
    }

    fn shutdown(&mut self) {
        if self.join.is_none() {
            return;
        }
        let _ = self.commands.send(WorkerCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for DatasetWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn worker_loop(commands: &Receiver<WorkerCommand>, events: &Sender<WorkerEvent>) {
    let mut dataset = None;
    let mut active_source_generation = 0;
    let mut pending_command = None;
    loop {
        let command = match pending_command.take() {
            Some(command) => command,
            None => match commands.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        match command {
            WorkerCommand::Open {
                path,
                source_generation,
            } => {
                active_source_generation = source_generation;
                dataset = None;
                let result = LocalFileSource::open(&path)
                    .map_err(czi_core::CziError::from)
                    .and_then(CziDataset::open)
                    .and_then(|opened| {
                        let query = TileQueryIndex::new(opened.index()).map_err(|error| {
                            czi_core::CziError::Missing {
                                what: "query geometry",
                                offset: u64::try_from(error.to_string().len()).unwrap_or(u64::MAX),
                            }
                        })?;
                        let info = DatasetInfo::from_dataset(path, &opened, &query);
                        Ok((opened, query, info))
                    });
                match result {
                    Ok((opened, query, info)) => {
                        dataset = Some(WorkerDataset {
                            dataset: opened,
                            query,
                        });
                        if events
                            .send(WorkerEvent::Opened {
                                info,
                                source_generation,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        if events
                            .send(WorkerEvent::OpenFailed {
                                message: error.to_string(),
                                source_generation,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            WorkerCommand::View(request) => {
                if request.source_generation != active_source_generation {
                    if events
                        .send(WorkerEvent::ViewFailed {
                            message: String::from("dataset was superseded before the view request"),
                            source_generation: request.source_generation,
                            view_generation: request.view_generation,
                        })
                        .is_err()
                    {
                        break;
                    }
                } else if let Some(opened) = dataset.as_ref() {
                    pending_command = process_view(commands, events, opened, &request);
                } else if events
                    .send(WorkerEvent::ViewFailed {
                        message: String::from("no dataset is open"),
                        source_generation: request.source_generation,
                        view_generation: request.view_generation,
                    })
                    .is_err()
                {
                    break;
                }
            }
            WorkerCommand::Shutdown => break,
        }
    }
}

fn process_view(
    commands: &Receiver<WorkerCommand>,
    events: &Sender<WorkerEvent>,
    opened: &WorkerDataset,
    request: &ViewRequest,
) -> Option<WorkerCommand> {
    let query = match ViewQuery::new(request.plane, request.viewport, request.target_downsample)
        .map_err(|error| error.to_string())
        .and_then(|view| opened.query.query(&view).map_err(|error| error.to_string()))
    {
        Ok(query) => query,
        Err(message) => {
            let _ = events.send(WorkerEvent::ViewFailed {
                message,
                source_generation: request.source_generation,
                view_generation: request.view_generation,
            });
            return None;
        }
    };

    let visible_tile_ids = query.hits.iter().map(|hit| hit.tile_id).collect::<Vec<_>>();
    let resident = request
        .resident_tile_ids
        .iter()
        .copied()
        .collect::<HashSet<_>>();
    let mut decode_order = query.hits.clone();
    sort_center_first(&mut decode_order, request.viewport);
    for hit in decode_order {
        if resident.contains(&hit.tile_id) {
            continue;
        }
        let event = match opened.dataset.decoded_tile(hit.tile_id.index()) {
            Ok(tile) => WorkerEvent::TileLoaded {
                tile_id: hit.tile_id,
                plane: hit.plane,
                logical_rect: hit.logical_rect,
                scale: hit.scale,
                paint_order: hit.paint_order,
                tile,
                source_generation: request.source_generation,
                view_generation: request.view_generation,
            },
            Err(error) => WorkerEvent::ViewFailed {
                message: format!("tile {}: {error}", hit.tile_id),
                source_generation: request.source_generation,
                view_generation: request.view_generation,
            },
        };
        if events.send(event).is_err() {
            return None;
        }
        match commands.try_recv() {
            Ok(newer) => return Some(newer),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return None,
        }
    }
    let _ = events.send(WorkerEvent::ViewFinished {
        plane: query.plane,
        scale: query.scale,
        visible_tile_ids,
        source_generation: request.source_generation,
        view_generation: request.view_generation,
    });
    None
}

#[allow(clippy::cast_precision_loss)]
fn sort_center_first(hits: &mut [TileHit], viewport: SpatialRect) {
    let center_x = (viewport.min_x as f64 + viewport.max_x as f64) * 0.5;
    let center_y = (viewport.min_y as f64 + viewport.max_y as f64) * 0.5;
    hits.sort_by(|left, right| {
        let left_x = (left.logical_rect.min_x as f64 + left.logical_rect.max_x as f64) * 0.5;
        let left_y = (left.logical_rect.min_y as f64 + left.logical_rect.max_y as f64) * 0.5;
        let right_x = (right.logical_rect.min_x as f64 + right.logical_rect.max_x as f64) * 0.5;
        let right_y = (right.logical_rect.min_y as f64 + right.logical_rect.max_y as f64) * 0.5;
        let left_distance =
            (left_x - center_x).mul_add(left_x - center_x, (left_y - center_y).powi(2));
        let right_distance =
            (right_x - center_x).mul_add(right_x - center_x, (right_y - center_y).powi(2));
        left_distance
            .partial_cmp(&right_distance)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.paint_order.cmp(&right.paint_order))
            .then_with(|| left.tile_id.cmp(&right.tile_id))
    });
}

fn dimension_choices(tiles: &[TileIndex], code: DimensionCode) -> DimensionChoices {
    let mut values = tiles
        .iter()
        .flat_map(|tile| tile.entry.dimensions.iter())
        .filter(|dimension| dimension.code == code)
        .map(|dimension| dimension.start)
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    let present = !values.is_empty();
    if !present {
        values.push(0);
    }
    DimensionChoices { present, values }
}

fn scene_choices(query: &TileQueryIndex) -> SceneChoices {
    let mut values = query.axis_choices().scenes.clone();
    let present = values
        .iter()
        .any(|scene| matches!(scene, SceneId::Explicit(_)));
    if values.is_empty() {
        values.push(SceneId::Implicit);
    }
    SceneChoices { present, values }
}

#[derive(Clone, Debug)]
struct Status {
    message: String,
    is_error: bool,
}

impl Status {
    fn normal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_error: false,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            is_error: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Levels {
    black: u16,
    white: u16,
}

impl Levels {
    fn default_for(pixel_type: PixelType) -> Self {
        Self {
            black: 0,
            white: match pixel_type {
                PixelType::Gray8 => u16::from(u8::MAX),
                _ => u16::MAX,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Camera {
    zoom: f64,
    pan: egui::Vec2,
}

impl Default for Camera {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan: egui::Vec2::ZERO,
        }
    }
}

impl Camera {
    #[allow(clippy::cast_precision_loss)]
    fn world_center(bounds: SpatialRect) -> egui::Pos2 {
        egui::pos2(
            ((bounds.min_x as f64 + bounds.max_x as f64) * 0.5) as f32,
            ((bounds.min_y as f64 + bounds.max_y as f64) * 0.5) as f32,
        )
    }

    #[allow(clippy::cast_precision_loss)]
    fn world_to_screen(
        self,
        world: egui::Pos2,
        canvas: egui::Rect,
        bounds: SpatialRect,
    ) -> egui::Pos2 {
        let center = Self::world_center(bounds);
        canvas.center() + self.pan + (world - center) * self.zoom as f32
    }

    fn screen_to_world(
        self,
        screen: egui::Pos2,
        canvas: egui::Rect,
        bounds: SpatialRect,
    ) -> egui::Pos2 {
        let center = Self::world_center(bounds);
        let world = (screen - canvas.center() - self.pan) / self.zoom as f32 + center.to_vec2();
        egui::pos2(world.x, world.y)
    }

    fn zoom_at(
        &mut self,
        cursor: egui::Pos2,
        factor: f64,
        canvas: egui::Rect,
        bounds: SpatialRect,
    ) {
        let anchor = self.screen_to_world(cursor, canvas, bounds);
        self.zoom = (self.zoom * factor).clamp(0.000_001, 1_000_000.0);
        let center = Self::world_center(bounds);
        self.pan = cursor - canvas.center() - (anchor - center) * self.zoom as f32;
    }

    fn fit(&mut self, canvas: egui::Rect, bounds: SpatialRect) {
        let width = bounds.width().max(1) as f64;
        let height = bounds.height().max(1) as f64;
        self.zoom = (f64::from(canvas.width()) / width)
            .min(f64::from(canvas.height()) / height)
            .clamp(0.000_001, 1_000_000.0);
        self.pan = egui::Vec2::ZERO;
    }

    fn one_to_one(&mut self) {
        *self = Self::default();
    }

    #[allow(clippy::cast_precision_loss)]
    fn viewport(self, canvas: egui::Rect, bounds: SpatialRect) -> Option<SpatialRect> {
        let minimum = self.screen_to_world(canvas.min, canvas, bounds);
        let maximum = self.screen_to_world(canvas.max, canvas, bounds);
        let min_x = floor_i64(minimum.x as f64)?;
        let min_y = floor_i64(minimum.y as f64)?;
        let max_x = ceil_i64(maximum.x as f64)?;
        let max_y = ceil_i64(maximum.y as f64)?;
        SpatialRect::new(min_x, min_y, max_x.max(min_x), max_y.max(min_y)).ok()
    }
}

fn floor_i64(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let value = value.floor();
    (value >= i64::MIN as f64 && value <= i64::MAX as f64).then_some(value as i64)
}

fn ceil_i64(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let value = value.ceil();
    (value >= i64::MIN as f64 && value <= i64::MAX as f64).then_some(value as i64)
}

fn display_intensity(value: u16, levels: Levels) -> u8 {
    if value <= levels.black {
        return 0;
    }
    if value >= levels.white {
        return u8::MAX;
    }
    let span = u32::from(levels.white - levels.black);
    let scaled = u32::from(value - levels.black) * u32::from(u8::MAX) / span;
    u8::try_from(scaled).expect("scaled grayscale intensity fits in u8")
}

fn texture_image(tile: &DecodedTile, levels: Levels) -> Result<egui::ColorImage, &'static str> {
    let width = usize::try_from(tile.width).map_err(|_| "tile width does not fit usize")?;
    let height = usize::try_from(tile.height).map_err(|_| "tile height does not fit usize")?;
    let pixel_count = width
        .checked_mul(height)
        .ok_or("tile dimensions overflow display size")?;
    let mut grayscale = Vec::new();
    grayscale
        .try_reserve_exact(pixel_count)
        .map_err(|_| "cannot allocate display grayscale buffer")?;
    match &tile.pixels {
        DecodedPixels::Gray8(values) if values.len() == pixel_count => {
            grayscale.extend(
                values
                    .iter()
                    .copied()
                    .map(|value| display_intensity(u16::from(value), levels)),
            );
        }
        DecodedPixels::Gray16(values) if values.len() == pixel_count => {
            grayscale.extend(
                values
                    .iter()
                    .copied()
                    .map(|value| display_intensity(value, levels)),
            );
        }
        _ => return Err("decoded pixel count does not match the tile dimensions"),
    }
    Ok(egui::ColorImage::from_gray([width, height], &grayscale))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TextureKey {
    source_generation: u64,
    plane: PlaneKey,
    tile_id: TileId,
}

struct TextureEntry {
    texture: egui::TextureHandle,
    bytes: usize,
    last_used: u64,
    visible: bool,
    logical_rect: SpatialRect,
    paint_order: usize,
}

struct TextureCache {
    entries: HashMap<TextureKey, TextureEntry>,
    bytes: usize,
    clock: u64,
    budget: usize,
}

impl TextureCache {
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            bytes: 0,
            clock: 0,
            budget,
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    fn clear_visibility(&mut self) {
        for entry in self.entries.values_mut() {
            entry.visible = false;
        }
        self.evict_non_visible();
    }

    fn resident_tile_ids(&self, source_generation: u64, plane: PlaneKey) -> Vec<TileId> {
        self.entries
            .keys()
            .filter(|key| key.source_generation == source_generation && key.plane == plane)
            .map(|key| key.tile_id)
            .collect()
    }

    fn insert(
        &mut self,
        key: TextureKey,
        texture: egui::TextureHandle,
        bytes: usize,
        hit: TileHit,
    ) {
        self.clock = self.clock.wrapping_add(1);
        if let Some(previous) = self.entries.remove(&key) {
            self.bytes = self.bytes.saturating_sub(previous.bytes);
        }
        self.bytes = self.bytes.saturating_add(bytes);
        self.entries.insert(
            key,
            TextureEntry {
                texture,
                bytes,
                last_used: self.clock,
                visible: true,
                logical_rect: hit.logical_rect,
                paint_order: hit.paint_order,
            },
        );
        self.evict_non_visible();
    }

    fn finish_view(&mut self, source_generation: u64, plane: PlaneKey, visible: &[TileId]) {
        let visible = visible.iter().copied().collect::<HashSet<_>>();
        for (key, entry) in &mut self.entries {
            entry.visible = key.source_generation == source_generation
                && key.plane == plane
                && visible.contains(&key.tile_id);
        }
        self.evict_non_visible();
    }

    fn evict_non_visible(&mut self) {
        while self.bytes > self.budget {
            let candidate = self
                .entries
                .iter()
                .filter(|(_, entry)| !entry.visible)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key);
            let Some(key) = candidate else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
            }
        }
    }

    fn touch(&mut self, key: TextureKey) {
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = self.clock;
        }
    }

    fn current_counts(&self, source_generation: u64, plane: PlaneKey) -> (usize, usize) {
        let entries = self
            .entries
            .iter()
            .filter(|(key, _)| key.source_generation == source_generation && key.plane == plane);
        let mut resident = 0;
        let mut visible = 0;
        for (_, entry) in entries {
            resident += 1;
            visible += usize::from(entry.visible);
        }
        (visible, resident)
    }
}

struct PendingTile {
    tile_id: TileId,
    plane: PlaneKey,
    logical_rect: SpatialRect,
    scale: PyramidScale,
    paint_order: usize,
    tile: DecodedTile,
    source_generation: u64,
    view_generation: u64,
}

/// The local CZI mosaic viewer.
pub struct ViewerApp {
    worker: DatasetWorker,
    path_input: String,
    dataset: Option<DatasetInfo>,
    selection: PlaneSelection,
    generations: Generations,
    status: Status,
    cache: TextureCache,
    pending_tiles: Vec<PendingTile>,
    visible_tile_ids: Vec<TileId>,
    selected_scale: Option<PyramidScale>,
    levels: Levels,
    camera: Camera,
    fit_pending: bool,
    last_request: Option<(PlaneKey, SpatialRect, u64)>,
}

impl ViewerApp {
    /// Create the viewer state and its dedicated dataset worker.
    #[must_use]
    pub fn new(_creation_context: &eframe::CreationContext<'_>) -> Self {
        let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
        let mut app = Self {
            worker: DatasetWorker::spawn(),
            path_input: initial_path
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            dataset: None,
            selection: PlaneSelection::default(),
            generations: Generations::default(),
            status: Status::normal("Enter a local .czi path or drop a file from Finder."),
            cache: TextureCache::new(TEXTURE_CACHE_LIMIT),
            pending_tiles: Vec::new(),
            visible_tile_ids: Vec::new(),
            selected_scale: None,
            levels: Levels::default_for(PixelType::Gray16),
            camera: Camera::default(),
            fit_pending: false,
            last_request: None,
        };
        if initial_path.is_some() {
            app.open_current_path();
        }
        app
    }

    fn invalidate_view(&mut self) {
        self.generations.begin_view();
        self.last_request = None;
        self.visible_tile_ids.clear();
        self.selected_scale = None;
        self.cache.clear_visibility();
    }

    fn open_current_path(&mut self) {
        let path = PathBuf::from(self.path_input.trim());
        if self.path_input.trim().is_empty() {
            self.status = Status::error("Enter a local .czi path first.");
            return;
        }
        let source_generation = self.generations.begin_source();
        self.dataset = None;
        self.cache.clear();
        self.pending_tiles.clear();
        self.visible_tile_ids.clear();
        self.selected_scale = None;
        self.last_request = None;
        self.fit_pending = false;
        self.status = Status::normal(format!("Opening {}…", path.display()));
        if let Err(error) = self.worker.send(WorkerCommand::Open {
            path,
            source_generation,
        }) {
            self.status = Status::error(error);
        }
    }

    fn request_view(&mut self, viewport: SpatialRect) {
        if self.dataset.is_none() {
            return;
        }
        let plane = self.selection;
        let target_downsample = (1.0 / self.camera.zoom).clamp(0.000_001, 1_000_000.0);
        let request_key = (plane.key(), viewport, target_downsample.to_bits());
        if self.last_request == Some(request_key) {
            return;
        }
        let view_generation = self.generations.begin_view();
        self.cache.clear_visibility();
        let resident_tile_ids = self
            .cache
            .resident_tile_ids(self.generations.source, plane.key());
        let request = ViewRequest {
            source_generation: self.generations.source,
            view_generation,
            plane,
            viewport,
            target_downsample,
            resident_tile_ids,
        };
        if let Err(error) = self.worker.send(WorkerCommand::View(request)) {
            self.status = Status::error(error);
        } else {
            self.last_request = Some(request_key);
            if let Some(info) = self.dataset.as_ref()
                && info.plane(plane).is_none()
            {
                self.status = Status::error("The selected sparse plane has no indexed geometry.");
            }
        }
    }

    fn handle_dropped_files(&mut self, context: &egui::Context) {
        let path = context.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .find_map(|file| file.path.clone())
        });
        if let Some(path) = path {
            self.path_input = path.display().to_string();
            self.open_current_path();
        }
    }

    fn handle_worker_events(&mut self) {
        loop {
            match self.worker.events.try_recv() {
                Ok(WorkerEvent::Opened {
                    info,
                    source_generation,
                }) if self.generations.accepts_source(source_generation) => {
                    self.selection = info.default_selection();
                    self.levels = Levels::default_for(info.pixel_type);
                    self.status = Status::normal(format!(
                        "Indexed {} tile(s); choose a plane or view the mosaic.",
                        info.tile_count
                    ));
                    self.dataset = Some(info);
                    self.cache.clear();
                    self.pending_tiles.clear();
                    self.visible_tile_ids.clear();
                    self.fit_pending = true;
                    self.invalidate_view();
                }
                Ok(WorkerEvent::OpenFailed {
                    message,
                    source_generation,
                }) if self.generations.accepts_source(source_generation) => {
                    self.status = Status::error(message);
                }
                Ok(WorkerEvent::TileLoaded {
                    tile_id,
                    plane,
                    logical_rect,
                    scale,
                    paint_order,
                    tile,
                    source_generation,
                    view_generation,
                }) if self
                    .generations
                    .accepts_view(source_generation, view_generation)
                    && plane == self.selection.key() =>
                {
                    self.pending_tiles.push(PendingTile {
                        tile_id,
                        plane,
                        logical_rect,
                        scale,
                        paint_order,
                        tile,
                        source_generation,
                        view_generation,
                    });
                }
                Ok(WorkerEvent::ViewFinished {
                    plane,
                    scale,
                    visible_tile_ids,
                    source_generation,
                    view_generation,
                }) if self
                    .generations
                    .accepts_view(source_generation, view_generation)
                    && plane == self.selection.key() =>
                {
                    self.selected_scale = Some(scale);
                    self.visible_tile_ids = visible_tile_ids;
                    self.cache
                        .finish_view(source_generation, plane, &self.visible_tile_ids);
                    let (visible, resident) = self.cache.current_counts(source_generation, plane);
                    self.status = Status::normal(format!(
                        "Scale {}× · {} visible · {} resident",
                        format_scale(scale),
                        visible,
                        resident
                    ));
                }
                Ok(WorkerEvent::ViewFailed {
                    message,
                    source_generation,
                    view_generation,
                }) if self
                    .generations
                    .accepts_view(source_generation, view_generation) =>
                {
                    self.status = Status::error(message);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn refresh_textures(&mut self, context: &egui::Context) {
        let pending = std::mem::take(&mut self.pending_tiles);
        for pending in pending {
            if !self
                .generations
                .accepts_view(pending.source_generation, pending.view_generation)
                || pending.plane != self.selection.key()
            {
                continue;
            }
            let hit = TileHit {
                tile_id: pending.tile_id,
                plane: pending.plane,
                logical_rect: pending.logical_rect,
                physical_stored_size: PhysicalSize {
                    width: pending.tile.width,
                    height: pending.tile.height,
                },
                scale: pending.scale,
                m_index: None,
                paint_order: pending.paint_order,
            };
            match texture_image(&pending.tile, self.levels) {
                Ok(image) => {
                    let bytes = image
                        .pixels
                        .len()
                        .saturating_mul(std::mem::size_of::<egui::Color32>());
                    let texture = context.load_texture(
                        format!("czi-{}-{}", pending.source_generation, pending.tile_id),
                        image,
                        egui::TextureOptions::NEAREST,
                    );
                    self.cache.insert(
                        TextureKey {
                            source_generation: pending.source_generation,
                            plane: pending.plane,
                            tile_id: pending.tile_id,
                        },
                        texture,
                        bytes,
                        hit,
                    );
                }
                Err(error) => self.status = Status::error(error),
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn show_canvas(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Fit").clicked() {
                self.fit_pending = true;
                self.last_request = None;
            }
            if ui.button("1:1").clicked() {
                self.camera.one_to_one();
                self.last_request = None;
            }
            ui.weak("Wheel: zoom at cursor · Drag: pan · logical world coordinates");
        });
        let (response, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::drag());
        painter.rect_filled(response.rect, 0.0, egui::Color32::from_gray(24));

        let Some(dataset) = self.dataset.as_ref() else {
            canvas_message(&painter, response.rect, "Open a CZI to view its mosaic.");
            return;
        };
        let Some(plane) = dataset.plane(self.selection) else {
            canvas_message(
                &painter,
                response.rect,
                "The selected sparse plane has no geometry.",
            );
            return;
        };
        let bounds = plane.world_bounds;
        if self.fit_pending {
            self.camera.fit(response.rect, bounds);
            self.fit_pending = false;
            self.last_request = None;
        }
        let mut changed = false;
        if response.hovered() {
            if let Some(cursor) = ui.input(|input| input.pointer.hover_pos()) {
                let scroll_y = ui.input(|input| input.raw_scroll_delta.y);
                if scroll_y != 0.0 {
                    self.camera.zoom_at(
                        cursor,
                        f64::from(scroll_y * 0.002).exp(),
                        response.rect,
                        bounds,
                    );
                    changed = true;
                }
            }
        }
        if response.dragged() {
            self.camera.pan += ui.input(|input| input.pointer.delta());
            changed = true;
        }
        if changed {
            self.last_request = None;
        }
        if let Some(viewport) = self.camera.viewport(response.rect, bounds) {
            self.request_view(viewport);
        }

        for tile_id in &self.visible_tile_ids {
            self.cache.touch(TextureKey {
                source_generation: self.generations.source,
                plane: self.selection.key(),
                tile_id: *tile_id,
            });
        }
        let mut visible = self
            .visible_tile_ids
            .iter()
            .filter_map(|tile_id| {
                let key = TextureKey {
                    source_generation: self.generations.source,
                    plane: self.selection.key(),
                    tile_id: *tile_id,
                };
                self.cache
                    .entries
                    .get(&key)
                    .map(|entry| (entry.paint_order, entry))
            })
            .collect::<Vec<_>>();
        visible.sort_unstable_by_key(|(paint_order, _)| *paint_order);
        let has_visible = !visible.is_empty();
        for (_, entry) in &visible {
            let min = egui::pos2(
                entry.logical_rect.min_x as f32,
                entry.logical_rect.min_y as f32,
            );
            let max = egui::pos2(
                entry.logical_rect.max_x as f32,
                entry.logical_rect.max_y as f32,
            );
            let image_rect = egui::Rect::from_min_max(
                self.camera.world_to_screen(min, response.rect, bounds),
                self.camera.world_to_screen(max, response.rect, bounds),
            );
            painter.with_clip_rect(response.rect).image(
                entry.texture.id(),
                image_rect,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
        if !has_visible {
            canvas_message(&painter, response.rect, "Loading visible tiles…");
        }
    }
}

fn format_scale(scale: PyramidScale) -> String {
    if scale.denominator == 1 {
        format!("{}", scale.numerator)
    } else {
        format!("{}/{}", scale.numerator, scale.denominator)
    }
}

fn scene_label(scene: SceneId) -> String {
    match scene {
        SceneId::Implicit => String::from("implicit"),
        SceneId::Explicit(value) => value.to_string(),
    }
}

fn canvas_message(painter: &egui::Painter, rect: egui::Rect, message: &str) {
    painter.text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        message,
        egui::FontId::proportional(16.0),
        egui::Color32::LIGHT_GRAY,
    );
}

impl eframe::App for ViewerApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_dropped_files(context);
        self.handle_worker_events();

        egui::TopBottomPanel::top("open_bar").show(context, |ui| {
            ui.horizontal(|ui| {
                ui.label("CZI path:");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.path_input)
                        .hint_text("/path/to/image.czi")
                        .desired_width(f32::INFINITY),
                );
                let open = ui.button("Open").clicked()
                    || (response.lost_focus()
                        && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                if open {
                    self.open_current_path();
                }
            });
            let color = if self.status.is_error {
                egui::Color32::LIGHT_RED
            } else {
                egui::Color32::LIGHT_GRAY
            };
            ui.colored_label(color, &self.status.message);
        });

        egui::SidePanel::left("dataset_panel")
            .resizable(true)
            .default_width(300.0)
            .show(context, |ui| {
                ui.heading("Dataset");
                let selection_changed = if let Some(dataset) = self.dataset.as_ref() {
                    ui.label(dataset.path.display().to_string());
                    ui.label(format!("{} indexed tile(s)", dataset.tile_count));
                    ui.separator();
                    selection_selector(ui, "C", &dataset.c, &mut self.selection.c)
                        | scene_selector(ui, &dataset.s, &mut self.selection.scene)
                        | selection_selector(ui, "Z", &dataset.z, &mut self.selection.z)
                        | selection_selector(ui, "T", &dataset.t, &mut self.selection.t)
                } else {
                    ui.label("No dataset is open.");
                    false
                };
                if selection_changed {
                    self.cache.clear();
                    self.invalidate_view();
                    self.fit_pending = true;
                }

                ui.separator();
                ui.heading("Display range");
                let pixel_type = self
                    .dataset
                    .as_ref()
                    .map_or(PixelType::Gray16, |dataset| dataset.pixel_type);
                if level_selector(ui, pixel_type, &mut self.levels) {
                    self.cache.clear();
                    self.invalidate_view();
                }
                if let Some(scale) = self.selected_scale {
                    ui.label(format!("Selected pyramid scale: {}×", format_scale(scale)));
                }
                if let Some(dataset) = self.dataset.as_ref() {
                    let (visible, resident) = self
                        .cache
                        .current_counts(self.generations.source, self.selection.key());
                    ui.label(format!("Visible: {visible} · Resident: {resident}"));
                    if let Some(plane) = dataset.plane(self.selection) {
                        ui.label(format!(
                            "World bounds: [{}, {})..[{}, {})",
                            plane.world_bounds.min_x,
                            plane.world_bounds.min_y,
                            plane.world_bounds.max_x,
                            plane.world_bounds.max_y
                        ));
                    }
                }

                ui.separator();
                ui.heading("Raw metadata preview");
                egui::ScrollArea::vertical()
                    .max_height(260.0)
                    .show(ui, |ui| {
                        if let Some(dataset) = self.dataset.as_ref() {
                            ui.monospace(&dataset.metadata_preview);
                        } else {
                            ui.label("Open a CZI to inspect its metadata.");
                        }
                    });
            });

        self.refresh_textures(context);
        egui::CentralPanel::default().show(context, |ui| {
            ui.heading("Canvas");
            ui.separator();
            self.show_canvas(ui);
        });

        context.request_repaint_after(Duration::from_millis(100));
    }
}

fn selection_selector(
    ui: &mut egui::Ui,
    label: &str,
    choices: &DimensionChoices,
    selected: &mut i32,
) -> bool {
    if !choices.present {
        ui.horizontal(|ui| {
            ui.label(label);
            ui.weak("not present (0)");
        });
        return false;
    }
    let before = *selected;
    egui::ComboBox::from_label(label)
        .selected_text(selected.to_string())
        .show_ui(ui, |ui| {
            for value in &choices.values {
                ui.selectable_value(selected, *value, value.to_string());
            }
        });
    *selected != before
}

fn scene_selector(ui: &mut egui::Ui, choices: &SceneChoices, selected: &mut SceneId) -> bool {
    if !choices.present {
        ui.horizontal(|ui| {
            ui.label("S");
            ui.weak("not present (implicit)");
        });
        return false;
    }
    let before = *selected;
    egui::ComboBox::from_label("S")
        .selected_text(scene_label(*selected))
        .show_ui(ui, |ui| {
            for value in &choices.values {
                ui.selectable_value(selected, *value, scene_label(*value));
            }
        });
    *selected != before
}

fn level_selector(ui: &mut egui::Ui, pixel_type: PixelType, levels: &mut Levels) -> bool {
    let maximum = match pixel_type {
        PixelType::Gray8 => u16::from(u8::MAX),
        _ => u16::MAX,
    };
    let before = *levels;
    ui.add(egui::Slider::new(&mut levels.black, 0..=maximum).text("Black"));
    ui.add(egui::Slider::new(&mut levels.white, 0..=maximum).text("White"));
    if levels.black >= levels.white {
        if levels.black < maximum {
            levels.white = levels.black + 1;
        } else {
            levels.black = levels.white.saturating_sub(1);
        }
    }
    *levels != before
}

/// Run the macOS-native local viewer.
///
/// # Errors
///
/// Returns the native window or graphics-backend error from eframe.
pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1440.0, 900.0]),
        ..Default::default()
    };
    eframe::run_native(
        "CZI Viewer",
        options,
        Box::new(|creation_context| Ok(Box::new(ViewerApp::new(creation_context)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use czi_core::{CompressionMode, DimensionEntry, DirectoryEntry, PyramidType};

    fn dimension(code: DimensionCode, start: i32) -> DimensionEntry {
        DimensionEntry {
            code,
            start,
            logical_size: 1,
            start_coordinate: 0.0,
            stored_size: 1,
            stored_size_raw: 1,
        }
    }

    fn tile(dimensions: Vec<DimensionEntry>) -> TileIndex {
        TileIndex {
            entry: DirectoryEntry {
                schema_type: *b"DV",
                pixel_type: PixelType::Gray8,
                file_position: 0,
                file_part: 0,
                compression: CompressionMode::Uncompressed,
                pyramid_type: PyramidType::None,
                dimensions,
            },
        }
    }

    #[test]
    fn generations_drop_stale_source_and_view_results() {
        let mut generations = Generations::default();
        let first_source = generations.begin_source();
        let first_view = generations.begin_view();
        assert!(generations.accepts_view(first_source, first_view));
        let second_view = generations.begin_view();
        assert!(!generations.accepts_view(first_source, first_view));
        assert!(generations.accepts_view(first_source, second_view));
        let second_source = generations.begin_source();
        assert_ne!(first_source, second_source);
        assert!(!generations.accepts_view(first_source, second_view));
    }

    #[test]
    fn worker_shutdown_joins_its_thread() {
        let mut worker = DatasetWorker::spawn();
        worker.shutdown();
        assert!(worker.join.is_none());
    }

    #[test]
    fn worker_preserves_events_from_a_bounded_command_burst() {
        let mut worker = DatasetWorker::spawn();
        let count = u64::try_from(CHANNEL_CAPACITY + 1).expect("channel capacity fits u64");
        for source_generation in 1..=count {
            worker
                .commands
                .send(WorkerCommand::Open {
                    path: PathBuf::from(format!(
                        "/dev/null/czi-viewer-missing-{source_generation}.czi"
                    )),
                    source_generation,
                })
                .expect("bounded command burst");
        }

        let mut observed = Vec::new();
        for _ in 0..count {
            match worker
                .events
                .recv_timeout(Duration::from_secs(1))
                .expect("worker event")
            {
                WorkerEvent::OpenFailed {
                    source_generation, ..
                } => observed.push(source_generation),
                _ => panic!("missing-path open should fail"),
            }
        }
        observed.sort_unstable();
        assert_eq!(observed, (1..=count).collect::<Vec<_>>());
        worker.shutdown();
    }

    #[test]
    fn camera_world_round_trips_negative_coordinates_and_cursor_zoom() {
        let canvas = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let bounds = SpatialRect::new(-100, -50, 100, 50).unwrap();
        let mut camera = Camera::default();
        camera.fit(canvas, bounds);
        let world = egui::pos2(-37.0, 21.0);
        let screen = camera.world_to_screen(world, canvas, bounds);
        let round_trip = camera.screen_to_world(screen, canvas, bounds);
        assert!((round_trip.x - world.x).abs() < 0.001);
        assert!((round_trip.y - world.y).abs() < 0.001);
        let cursor = egui::pos2(150.0, 80.0);
        let before = camera.screen_to_world(cursor, canvas, bounds);
        camera.zoom_at(cursor, 1.5, canvas, bounds);
        let after = camera.screen_to_world(cursor, canvas, bounds);
        assert!((before.x - after.x).abs() < 0.001);
        assert!((before.y - after.y).abs() < 0.001);
        camera.one_to_one();
        assert_eq!(camera, Camera::default());
    }

    #[test]
    fn scene_selector_preserves_implicit_and_explicit_zero() {
        let query = TileQueryIndex::new(&czi_core::DatasetIndex {
            source: czi_core::SourceInfo {
                length: 0,
                version: 0,
            },
            file_header: czi_core::FileHeader {
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
            tiles: vec![
                tile(vec![
                    dimension(DimensionCode::X, 0),
                    dimension(DimensionCode::Y, 0),
                ]),
                tile(vec![
                    dimension(DimensionCode::X, 1),
                    dimension(DimensionCode::Y, 0),
                    dimension(DimensionCode::S, 0),
                ]),
            ],
            metadata: None,
            attachments: Vec::new(),
        })
        .expect("query index");
        assert_eq!(
            query.axis_choices().scenes,
            vec![SceneId::Implicit, SceneId::Explicit(0)]
        );
        let choices = scene_choices(&query);
        assert_eq!(
            choices.values,
            vec![SceneId::Implicit, SceneId::Explicit(0)]
        );
        assert!(choices.present);
        assert_eq!(PlaneSelector::default().scene, SceneId::Implicit);
    }

    #[test]
    fn byte_cache_evicts_oldest_non_visible_entries_under_budget() {
        #[derive(Clone, Copy)]
        struct Record {
            bytes: usize,
            last_used: u64,
            visible: bool,
        }
        fn evict(records: &mut Vec<Record>, budget: usize) {
            while records.iter().map(|record| record.bytes).sum::<usize>() > budget {
                let Some(index) = records
                    .iter()
                    .enumerate()
                    .filter(|(_, record)| !record.visible)
                    .min_by_key(|(_, record)| record.last_used)
                    .map(|(index, _)| index)
                else {
                    break;
                };
                records.remove(index);
            }
        }
        let mut records = vec![
            Record {
                bytes: 60,
                last_used: 1,
                visible: false,
            },
            Record {
                bytes: 60,
                last_used: 2,
                visible: false,
            },
            Record {
                bytes: 60,
                last_used: 3,
                visible: true,
            },
        ];
        evict(&mut records, 120);
        assert_eq!(records.len(), 2);
        assert!(records.iter().any(|record| record.visible));
        assert_eq!(records[0].last_used, 2);
    }
}
