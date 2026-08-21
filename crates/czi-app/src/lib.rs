use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use czi_core::{CziDataset, DecodedPixels, DecodedTile, DimensionCode, LocalFileSource, TileIndex};
use eframe::egui;

const CHANNEL_CAPACITY: usize = 8;
const METADATA_PREVIEW_CHARS: usize = 4_096;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PlaneSelection {
    c: i32,
    s: i32,
    z: i32,
    t: i32,
}

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
struct DatasetInfo {
    path: PathBuf,
    tile_count: usize,
    c: DimensionChoices,
    s: DimensionChoices,
    z: DimensionChoices,
    t: DimensionChoices,
    metadata_preview: String,
}

impl DatasetInfo {
    fn from_dataset(path: PathBuf, dataset: &CziDataset) -> Self {
        let tiles = &dataset.index().tiles;
        let metadata_preview = dataset.index().metadata.as_ref().map_or_else(
            || String::from("No global metadata XML."),
            |metadata| metadata.xml.chars().take(METADATA_PREVIEW_CHARS).collect(),
        );
        Self {
            path,
            tile_count: tiles.len(),
            c: dimension_choices(tiles, DimensionCode::C),
            s: dimension_choices(tiles, DimensionCode::S),
            z: dimension_choices(tiles, DimensionCode::Z),
            t: dimension_choices(tiles, DimensionCode::T),
            metadata_preview,
        }
    }

    fn default_selection(&self) -> PlaneSelection {
        PlaneSelection {
            c: self.c.default_value(),
            s: self.s.default_value(),
            z: self.z.default_value(),
            t: self.t.default_value(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Generations {
    source: u64,
    plane: u64,
}

impl Generations {
    fn begin_source(&mut self) -> u64 {
        self.source = self.source.wrapping_add(1);
        self.plane = self.plane.wrapping_add(1);
        self.source
    }

    fn begin_plane(&mut self) -> u64 {
        self.plane = self.plane.wrapping_add(1);
        self.plane
    }

    fn accepts_source(&self, source: u64) -> bool {
        source == self.source
    }

    fn accepts_plane(&self, source: u64, plane: u64) -> bool {
        self.accepts_source(source) && plane == self.plane
    }
}

enum WorkerCommand {
    Open {
        path: PathBuf,
        source_generation: u64,
    },
    LoadTile {
        selection: PlaneSelection,
        source_generation: u64,
        plane_generation: u64,
    },
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
        tile: DecodedTile,
        source_generation: u64,
        plane_generation: u64,
    },
    TileFailed {
        message: String,
        source_generation: u64,
        plane_generation: u64,
    },
}

struct DatasetWorker {
    commands: SyncSender<WorkerCommand>,
    events: Receiver<WorkerEvent>,
    join: Option<JoinHandle<()>>,
}

impl DatasetWorker {
    fn spawn() -> Self {
        let (commands, command_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(CHANNEL_CAPACITY);
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

fn worker_loop(commands: &Receiver<WorkerCommand>, events: &SyncSender<WorkerEvent>) {
    let mut dataset = None;
    let mut active_source_generation = 0;
    while let Ok(command) = commands.recv() {
        match command {
            WorkerCommand::Open {
                path,
                source_generation,
            } => {
                active_source_generation = source_generation;
                dataset = None;
                match LocalFileSource::open(&path)
                    .map_err(czi_core::CziError::from)
                    .and_then(CziDataset::open)
                {
                    Ok(opened) => {
                        let info = DatasetInfo::from_dataset(path, &opened);
                        dataset = Some(opened);
                        if !publish_event(
                            events,
                            WorkerEvent::Opened {
                                info,
                                source_generation,
                            },
                        ) {
                            break;
                        }
                    }
                    Err(error) => {
                        if !publish_event(
                            events,
                            WorkerEvent::OpenFailed {
                                message: error.to_string(),
                                source_generation,
                            },
                        ) {
                            break;
                        }
                    }
                }
            }
            WorkerCommand::LoadTile {
                selection,
                source_generation,
                plane_generation,
            } => {
                let event = if source_generation != active_source_generation {
                    WorkerEvent::TileFailed {
                        message: String::from("dataset was superseded before the tile request"),
                        source_generation,
                        plane_generation,
                    }
                } else if let Some(dataset) = dataset.as_ref() {
                    load_selected_tile(dataset, selection, source_generation, plane_generation)
                } else {
                    WorkerEvent::TileFailed {
                        message: String::from("no dataset is open"),
                        source_generation,
                        plane_generation,
                    }
                };
                if !publish_event(events, event) {
                    break;
                }
            }
            WorkerCommand::Shutdown => break,
        }
    }
}

fn publish_event(events: &SyncSender<WorkerEvent>, event: WorkerEvent) -> bool {
    match events.try_send(event) {
        Ok(()) | Err(TrySendError::Full(_)) => true,
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn load_selected_tile(
    dataset: &CziDataset,
    selection: PlaneSelection,
    source_generation: u64,
    plane_generation: u64,
) -> WorkerEvent {
    let Some(index) = select_tile(&dataset.index().tiles, selection) else {
        return WorkerEvent::TileFailed {
            message: String::from("no tile matches the selected C/S/Z/T coordinates"),
            source_generation,
            plane_generation,
        };
    };
    match dataset.decoded_tile(index) {
        Ok(tile) => WorkerEvent::TileLoaded {
            tile,
            source_generation,
            plane_generation,
        },
        Err(error) => WorkerEvent::TileFailed {
            message: error.to_string(),
            source_generation,
            plane_generation,
        },
    }
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

fn select_tile(tiles: &[TileIndex], selection: PlaneSelection) -> Option<usize> {
    tiles.iter().position(|tile| {
        matches_dimension(&tile.entry.dimensions, DimensionCode::C, selection.c)
            && matches_dimension(&tile.entry.dimensions, DimensionCode::S, selection.s)
            && matches_dimension(&tile.entry.dimensions, DimensionCode::Z, selection.z)
            && matches_dimension(&tile.entry.dimensions, DimensionCode::T, selection.t)
    })
}

fn matches_dimension(
    dimensions: &[czi_core::DimensionEntry],
    code: DimensionCode,
    selected: i32,
) -> bool {
    dimensions
        .iter()
        .find(|dimension| dimension.code == code)
        .is_none_or(|dimension| dimension.start == selected)
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
    fn for_tile(tile: &DecodedTile) -> Self {
        let (minimum, maximum, full_range) = match &tile.pixels {
            DecodedPixels::Gray8(values) => (
                values.iter().copied().map(u16::from).min().unwrap_or(0),
                values.iter().copied().map(u16::from).max().unwrap_or(0),
                255,
            ),
            DecodedPixels::Gray16(values) => (
                values.iter().copied().min().unwrap_or(0),
                values.iter().copied().max().unwrap_or(0),
                u16::MAX,
            ),
        };
        if minimum == maximum {
            Self {
                black: 0,
                white: full_range,
            }
        } else {
            Self {
                black: minimum,
                white: maximum,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Camera {
    zoom: f32,
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
    fn image_to_screen(
        self,
        image: egui::Pos2,
        canvas: egui::Rect,
        image_size: egui::Vec2,
    ) -> egui::Pos2 {
        canvas.center() + self.pan + (image.to_vec2() - image_size * 0.5) * self.zoom
    }

    fn screen_to_image(
        self,
        screen: egui::Pos2,
        canvas: egui::Rect,
        image_size: egui::Vec2,
    ) -> egui::Pos2 {
        let image = (screen - canvas.center() - self.pan) / self.zoom + image_size * 0.5;
        egui::pos2(image.x, image.y)
    }

    fn zoom_at(
        &mut self,
        cursor: egui::Pos2,
        factor: f32,
        canvas: egui::Rect,
        image_size: egui::Vec2,
    ) {
        let anchor = self.screen_to_image(cursor, canvas, image_size);
        self.zoom = (self.zoom * factor).clamp(0.05, 64.0);
        self.pan = cursor - canvas.center() - (anchor.to_vec2() - image_size * 0.5) * self.zoom;
    }

    fn fit(&mut self, canvas: egui::Rect, image_size: egui::Vec2) {
        self.zoom = (canvas.width() / image_size.x)
            .min(canvas.height() / image_size.y)
            .clamp(0.05, 64.0);
        self.pan = egui::Vec2::ZERO;
    }

    fn one_to_one(&mut self) {
        *self = Self::default();
    }
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

/// The first local, single-tile CZI viewer.
pub struct ViewerApp {
    worker: DatasetWorker,
    path_input: String,
    dataset: Option<DatasetInfo>,
    selection: PlaneSelection,
    generations: Generations,
    status: Status,
    tile: Option<DecodedTile>,
    texture: Option<egui::TextureHandle>,
    texture_dirty: bool,
    levels: Levels,
    camera: Camera,
    fit_pending: bool,
}

impl ViewerApp {
    /// Create the viewer state and its dedicated dataset worker.
    #[must_use]
    pub fn new(_creation_context: &eframe::CreationContext<'_>) -> Self {
        Self {
            worker: DatasetWorker::spawn(),
            path_input: String::new(),
            dataset: None,
            selection: PlaneSelection::default(),
            generations: Generations::default(),
            status: Status::normal("Enter a local .czi path or drop a file from Finder."),
            tile: None,
            texture: None,
            texture_dirty: false,
            levels: Levels {
                black: 0,
                white: u16::MAX,
            },
            camera: Camera::default(),
            fit_pending: false,
        }
    }

    fn open_current_path(&mut self) {
        let path = PathBuf::from(self.path_input.trim());
        if self.path_input.trim().is_empty() {
            self.status = Status::error("Enter a local .czi path first.");
            return;
        }
        let source_generation = self.generations.begin_source();
        self.dataset = None;
        self.tile = None;
        self.texture = None;
        self.texture_dirty = false;
        self.fit_pending = false;
        self.status = Status::normal(format!("Opening {}…", path.display()));
        if let Err(error) = self.worker.send(WorkerCommand::Open {
            path,
            source_generation,
        }) {
            self.status = Status::error(error);
        }
    }

    fn request_selected_tile(&mut self) {
        if self.dataset.is_none() {
            return;
        }
        let plane_generation = self.generations.begin_plane();
        self.tile = None;
        self.texture = None;
        self.texture_dirty = false;
        self.fit_pending = false;
        self.status = Status::normal("Loading one matching tile…");
        if let Err(error) = self.worker.send(WorkerCommand::LoadTile {
            selection: self.selection,
            source_generation: self.generations.source,
            plane_generation,
        }) {
            self.status = Status::error(error);
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
                    self.status = Status::normal(format!(
                        "Indexed {} tile(s); loading one selected tile.",
                        info.tile_count
                    ));
                    self.dataset = Some(info);
                    self.request_selected_tile();
                }
                Ok(WorkerEvent::OpenFailed {
                    message,
                    source_generation,
                }) if self.generations.accepts_source(source_generation) => {
                    self.status = Status::error(message);
                }
                Ok(WorkerEvent::TileLoaded {
                    tile,
                    source_generation,
                    plane_generation,
                }) if self
                    .generations
                    .accepts_plane(source_generation, plane_generation) =>
                {
                    self.status = Status::normal(format!(
                        "Loaded one {} × {} tile.",
                        tile.width, tile.height
                    ));
                    self.levels = Levels::for_tile(&tile);
                    self.tile = Some(tile);
                    self.texture_dirty = true;
                    self.fit_pending = true;
                }
                Ok(WorkerEvent::TileFailed {
                    message,
                    source_generation,
                    plane_generation,
                }) if self
                    .generations
                    .accepts_plane(source_generation, plane_generation) =>
                {
                    self.status = Status::error(message);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn refresh_texture(&mut self, context: &egui::Context) {
        if !self.texture_dirty {
            return;
        }
        let Some(tile) = self.tile.as_ref() else {
            return;
        };
        match texture_image(tile, self.levels) {
            Ok(image) => {
                if let Some(texture) = self.texture.as_mut() {
                    texture.set(image, egui::TextureOptions::NEAREST);
                } else {
                    self.texture = Some(context.load_texture(
                        "czi-selected-tile",
                        image,
                        egui::TextureOptions::NEAREST,
                    ));
                }
                self.texture_dirty = false;
            }
            Err(error) => {
                self.status = Status::error(error);
                self.texture_dirty = false;
            }
        }
    }

    fn show_canvas(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Fit").clicked() {
                self.fit_pending = true;
            }
            if ui.button("1:1").clicked() {
                self.camera.one_to_one();
                self.fit_pending = false;
            }
            ui.weak("Wheel: zoom at cursor · Drag: pan · One stored tile only");
        });
        let (response, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::drag());
        painter.rect_filled(response.rect, 0.0, egui::Color32::from_gray(24));

        let Some(tile) = self.tile.as_ref() else {
            canvas_message(
                &painter,
                response.rect,
                "A single selected tile will appear here.",
            );
            return;
        };
        let Some(texture) = self.texture.as_ref() else {
            canvas_message(&painter, response.rect, "Preparing tile texture…");
            return;
        };
        let Some(image_size) = display_tile_size(tile) else {
            canvas_message(
                &painter,
                response.rect,
                "Tile dimensions are not displayable.",
            );
            return;
        };

        if self.fit_pending {
            self.camera.fit(response.rect, image_size);
            self.fit_pending = false;
        }
        if response.hovered() {
            if let Some(cursor) = ui.input(|input| input.pointer.hover_pos()) {
                let scroll_y = ui.input(|input| input.raw_scroll_delta.y);
                if scroll_y != 0.0 {
                    self.camera.zoom_at(
                        cursor,
                        (scroll_y * 0.002).exp(),
                        response.rect,
                        image_size,
                    );
                }
            }
        }
        if response.dragged() {
            self.camera.pan += ui.input(|input| input.pointer.delta());
        }

        let image_rect = egui::Rect::from_min_max(
            self.camera
                .image_to_screen(egui::Pos2::ZERO, response.rect, image_size),
            self.camera.image_to_screen(
                egui::pos2(image_size.x, image_size.y),
                response.rect,
                image_size,
            ),
        );
        painter.with_clip_rect(response.rect).image(
            texture.id(),
            image_rect,
            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
            egui::Color32::WHITE,
        );
    }
}

#[allow(clippy::cast_precision_loss)]
fn display_tile_size(tile: &DecodedTile) -> Option<egui::Vec2> {
    (tile.width > 0 && tile.height > 0).then(|| egui::vec2(tile.width as f32, tile.height as f32))
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
                        | selection_selector(ui, "S", &dataset.s, &mut self.selection.s)
                        | selection_selector(ui, "Z", &dataset.z, &mut self.selection.z)
                        | selection_selector(ui, "T", &dataset.t, &mut self.selection.t)
                } else {
                    ui.label("No dataset is open.");
                    false
                };
                if selection_changed {
                    self.request_selected_tile();
                }

                ui.separator();
                ui.heading("Display range");
                if let Some(tile) = self.tile.as_ref()
                    && level_selector(ui, tile, &mut self.levels)
                {
                    self.texture_dirty = true;
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

        self.refresh_texture(context);
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

fn level_selector(ui: &mut egui::Ui, tile: &DecodedTile, levels: &mut Levels) -> bool {
    let maximum = match &tile.pixels {
        DecodedPixels::Gray8(_) => u16::from(u8::MAX),
        DecodedPixels::Gray16(_) => u16::MAX,
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
    use czi_core::{CompressionMode, DimensionEntry, DirectoryEntry, PixelType, PyramidType};

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
    fn selection_uses_sparse_dimension_starts_without_dense_plane_requests() {
        let tiles = [
            tile(vec![
                dimension(DimensionCode::C, 2),
                dimension(DimensionCode::S, 10),
                dimension(DimensionCode::X, 0),
                dimension(DimensionCode::Y, 0),
            ]),
            tile(vec![
                dimension(DimensionCode::C, 7),
                dimension(DimensionCode::S, 42),
                dimension(DimensionCode::X, 0),
                dimension(DimensionCode::Y, 0),
            ]),
        ];
        assert_eq!(
            dimension_choices(&tiles, DimensionCode::C).values,
            vec![2, 7]
        );
        assert_eq!(
            dimension_choices(&tiles, DimensionCode::S).values,
            vec![10, 42]
        );
        assert_eq!(
            select_tile(
                &tiles,
                PlaneSelection {
                    c: 7,
                    s: 42,
                    z: 0,
                    t: 0,
                }
            ),
            Some(1)
        );
    }

    #[test]
    fn generations_drop_stale_source_and_plane_results() {
        let mut generations = Generations::default();
        let first_source = generations.begin_source();
        let first_plane = generations.begin_plane();
        assert!(generations.accepts_plane(first_source, first_plane));
        let second_plane = generations.begin_plane();
        assert!(!generations.accepts_plane(first_source, first_plane));
        assert!(generations.accepts_plane(first_source, second_plane));
        let second_source = generations.begin_source();
        assert_ne!(first_source, second_source);
        assert!(!generations.accepts_plane(first_source, second_plane));
    }

    #[test]
    fn worker_shutdown_joins_its_thread() {
        let mut worker = DatasetWorker::spawn();
        worker.shutdown();
        assert!(worker.join.is_none());
    }

    #[test]
    fn pixel_conversion_maps_black_and_white_endpoints() {
        let levels = Levels {
            black: 100,
            white: 1_000,
        };
        assert_eq!(display_intensity(0, levels), 0);
        assert_eq!(display_intensity(100, levels), 0);
        assert_eq!(display_intensity(1_000, levels), u8::MAX);
        assert_eq!(display_intensity(u16::MAX, levels), u8::MAX);
    }

    #[test]
    fn camera_fit_transforms_and_cursor_zoom_are_stable() {
        let canvas = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let image_size = egui::vec2(100.0, 50.0);
        let mut camera = Camera::default();
        camera.fit(canvas, image_size);
        assert!((camera.zoom - 2.0).abs() < f32::EPSILON);
        assert_eq!(
            camera.image_to_screen(egui::pos2(50.0, 25.0), canvas, image_size),
            canvas.center()
        );
        let cursor = egui::pos2(150.0, 80.0);
        let image_before = camera.screen_to_image(cursor, canvas, image_size);
        camera.zoom_at(cursor, 1.5, canvas, image_size);
        let image_after = camera.screen_to_image(cursor, canvas, image_size);
        assert!((image_before.x - image_after.x).abs() < 0.001);
        assert!((image_before.y - image_after.y).abs() < 0.001);
        camera.one_to_one();
        assert_eq!(camera, Camera::default());
    }
}
