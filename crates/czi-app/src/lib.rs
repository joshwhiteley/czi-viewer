use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
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
                if events.send(event).is_err() {
                    break;
                }
            }
            WorkerCommand::Shutdown => break,
        }
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

/// The first local, single-tile CZI viewer.
pub struct ViewerApp {
    worker: DatasetWorker,
    path_input: String,
    dataset: Option<DatasetInfo>,
    selection: PlaneSelection,
    generations: Generations,
    status: Status,
    tile: Option<DecodedTile>,
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
                    self.tile = Some(tile);
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

        egui::CentralPanel::default().show(context, |ui| {
            ui.heading("Canvas");
            ui.separator();
            match self.tile.as_ref() {
                Some(DecodedTile {
                    width,
                    height,
                    pixels: DecodedPixels::Gray8(_),
                }) => ui.label(format!("Gray8 tile ready: {width} × {height}")),
                Some(DecodedTile {
                    width,
                    height,
                    pixels: DecodedPixels::Gray16(_),
                }) => ui.label(format!("Gray16 tile ready: {width} × {height}")),
                None => ui.label(
                    "A single selected tile will appear here. Mosaic assembly is not available.",
                ),
            };
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
}
