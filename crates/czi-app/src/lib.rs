#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use czi_core::{
    BlockCache, CziDataset, DecodedPixels, DecodedTile, DimensionCode, LocalFileSource,
    MetadataDocument, MetadataParseOptions, MetadataSummary, PhysicalSize, PixelType, PlaneInfo,
    PlaneKey, PlaneSelector, PyramidScale, SceneId, SpatialRect, TileHit, TileId, TileIndex,
    TileQueryIndex, ViewQuery, summarize_metadata,
};
use czi_ssh::{
    EmbeddedSshCancellation, OpenSshConfig, RemoteDirEntry, SftpLocation, SftpSession, SftpSource,
    SharedSftpSession, SshConsole, SshProfile,
};
use eframe::egui;

const CHANNEL_CAPACITY: usize = 8;
const TEXTURE_CACHE_LIMIT: usize = 256 * 1024 * 1024;
const MAX_REMOTE_SUGGESTIONS: usize = 200;
const MAX_REMOTE_DIRECTORY_ENTRIES: usize = 4_096;
const CONSOLE_PUMP_INPUT_CAPACITY: usize = 32;
const MAX_CONSOLE_INPUT_BYTES: usize = 4_096;
const EMBEDDED_START_TIMEOUT: Duration = Duration::from_secs(10);
const VIEWPORT_PREFETCH_PERCENT: u64 = 12;
const MAX_PREFETCH_TILES: usize = 128;
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;

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

#[derive(Clone, Debug)]
enum DatasetLocator {
    Local(PathBuf),
    Remote {
        profile: String,
        path: String,
        config: OpenSshConfig,
    },
}

impl DatasetLocator {
    fn display_label(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Remote { profile, path, .. } => format!("SSH {profile}:{path}"),
        }
    }

    fn remote_parts(&self) -> Result<(SshProfile, SftpLocation, &OpenSshConfig), String> {
        let Self::Remote {
            profile,
            path,
            config,
        } = self
        else {
            return Err(String::from("local source is not an SSH locator"));
        };
        let profile = SshProfile::new(profile.clone()).map_err(|error| error.to_string())?;
        let location = SftpLocation::new(path.clone()).map_err(|error| error.to_string())?;
        if !location.as_str().starts_with('/') {
            return Err(String::from("remote CZI path must be absolute"));
        }
        Ok((profile, location, config))
    }

    fn open_failure(error: impl std::fmt::Display) -> OpenFailure {
        OpenFailure {
            message: sanitize_error(error),
            session_usable: true,
        }
    }
}

struct OpenFailure {
    message: String,
    session_usable: bool,
}

#[derive(Clone)]
struct EmbeddedCancellationSlot {
    inner: Arc<Mutex<EmbeddedCancellationState>>,
}

struct EmbeddedCancellationState {
    current: Option<(u64, EmbeddedSshCancellation)>,
    cancelled_through: Option<u64>,
}

impl EmbeddedCancellationSlot {
    fn replace(&self, generation: u64, cancellation: &EmbeddedSshCancellation) {
        let cancel = if let Ok(mut state) = self.inner.lock() {
            state.current = Some((generation, cancellation.clone()));
            state
                .cancelled_through
                .is_some_and(|cancelled_through| generation <= cancelled_through)
        } else {
            false
        };
        if cancel {
            let _ = cancellation.cancel();
        }
    }

    fn clear(&self, generation: u64) {
        if let Ok(mut state) = self.inner.lock() {
            if state
                .current
                .as_ref()
                .is_some_and(|(current_generation, _)| *current_generation == generation)
            {
                state.current = None;
            }
        }
    }

    fn cancel(&self, generation: u64) {
        let cancellation = self
            .inner
            .lock()
            .map(|mut state| {
                state.cancelled_through = Some(
                    state
                        .cancelled_through
                        .map_or(generation, |cancelled_through| {
                            cancelled_through.max(generation)
                        }),
                );
                state
                    .current
                    .as_ref()
                    .filter(|(current_generation, _)| *current_generation == generation)
                    .map(|(_, cancellation)| cancellation.clone())
            })
            .ok()
            .flatten();
        if let Some(cancellation) = cancellation {
            let _ = cancellation.cancel();
        }
    }

    fn cancel_active(&self) {
        let generation = self
            .inner
            .lock()
            .ok()
            .and_then(|state| state.current.as_ref().map(|(generation, _)| *generation));
        if let Some(generation) = generation {
            self.cancel(generation);
        }
    }
}

impl Default for EmbeddedCancellationSlot {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EmbeddedCancellationState {
                current: None,
                cancelled_through: None,
            })),
        }
    }
}

fn validation_failure(error: impl std::fmt::Display) -> OpenFailure {
    OpenFailure {
        message: sanitize_error(error),
        session_usable: true,
    }
}

fn remote_failure(error: impl std::fmt::Display) -> OpenFailure {
    OpenFailure {
        message: sanitize_error(error),
        session_usable: false,
    }
}

fn remote_session_failure(error: impl std::fmt::Display, session_usable: bool) -> OpenFailure {
    OpenFailure {
        message: sanitize_error(error),
        session_usable,
    }
}

fn classify_remote_open_failure(
    browse_session: &mut Option<WorkerBrowseSession>,
    error: impl std::fmt::Display,
) -> OpenFailure {
    let session_usable = browse_session
        .as_ref()
        .is_some_and(|session| session.session.is_usable());
    if !session_usable {
        *browse_session = None;
    }
    remote_session_failure(error, session_usable)
}

fn requires_remote_reauthentication(session_usable: bool) -> bool {
    !session_usable
}

fn sanitize_error(error: impl std::fmt::Display) -> String {
    const MAX_ERROR_CHARS: usize = 4_096;

    let message = error.to_string();
    let mut sanitized = message
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .take(MAX_ERROR_CHARS)
        .collect::<String>();
    if message.chars().count() > MAX_ERROR_CHARS {
        sanitized.push('…');
    }
    sanitized
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RemotePathKind {
    Directory,
    CziFile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RemotePathSuggestion {
    name: String,
    path: String,
    kind: RemotePathKind,
    size: Option<u64>,
    modified: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RemoteBrowseTarget {
    Home,
    Directory { path: String, prefix: String },
}

#[derive(Debug)]
struct RemoteBrowseResult {
    directory: String,
    suggestions: Vec<RemotePathSuggestion>,
    home: bool,
}

fn remote_browse_target(path: &str, home: bool) -> Result<RemoteBrowseTarget, String> {
    let path = path.trim();
    if home || path.is_empty() {
        return Ok(RemoteBrowseTarget::Home);
    }
    if !path.starts_with('/') {
        return Err(String::from("remote browser path must be absolute"));
    }
    if path.ends_with('/') {
        let directory = path.trim_end_matches('/');
        return Ok(RemoteBrowseTarget::Directory {
            path: if directory.is_empty() {
                String::from("/")
            } else {
                directory.to_owned()
            },
            prefix: String::new(),
        });
    }
    let (directory, prefix) = path
        .rsplit_once('/')
        .expect("an absolute path always contains a slash");
    Ok(RemoteBrowseTarget::Directory {
        path: if directory.is_empty() {
            String::from("/")
        } else {
            directory.to_owned()
        },
        prefix: prefix.to_owned(),
    })
}

fn remote_path_suggestions(
    directory: &str,
    prefix: &str,
    entries: Vec<RemoteDirEntry>,
) -> Vec<RemotePathSuggestion> {
    let mut suggestions = entries
        .into_iter()
        .filter_map(|entry| {
            let name = entry.path.as_str();
            let kind = remote_path_kind(name, entry.attributes.permissions)?;
            (safe_remote_name(name) && name.starts_with(prefix)).then(|| RemotePathSuggestion {
                name: name.to_owned(),
                path: join_remote_path(directory, name),
                kind,
                size: entry.attributes.size,
                modified: entry
                    .attributes
                    .access_modify_time
                    .map(|(_, modified)| modified),
            })
        })
        .collect::<Vec<_>>();
    suggestions.sort_by(|left, right| {
        remote_path_kind_order(&left.kind)
            .cmp(&remote_path_kind_order(&right.kind))
            .then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            })
            .then_with(|| left.name.cmp(&right.name))
    });
    suggestions.truncate(MAX_REMOTE_SUGGESTIONS);
    suggestions
}

fn remote_path_kind_order(kind: &RemotePathKind) -> u8 {
    match kind {
        RemotePathKind::Directory => 0,
        RemotePathKind::CziFile => 1,
    }
}

fn remote_path_kind(name: &str, permissions: Option<u32>) -> Option<RemotePathKind> {
    let file_type = permissions.map(|permissions| permissions & S_IFMT);
    if file_type == Some(S_IFDIR) {
        return Some(RemotePathKind::Directory);
    }
    let is_regular_or_unknown =
        file_type.is_none() || file_type == Some(0) || file_type == Some(S_IFREG);
    (is_regular_or_unknown && name.to_ascii_lowercase().ends_with(".czi"))
        .then_some(RemotePathKind::CziFile)
}

fn safe_remote_name(name: &str) -> bool {
    !name.is_empty()
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

fn join_remote_path(directory: &str, name: &str) -> String {
    if directory == "/" {
        format!("/{name}")
    } else {
        format!("{}/{}", directory.trim_end_matches('/'), name)
    }
}

fn directory_path(path: &str) -> String {
    if path == "/" {
        String::from("/")
    } else {
        format!("{}/", path.trim_end_matches('/'))
    }
}

fn remote_parent_path(path: &str) -> String {
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/" {
        return String::from("/");
    }
    path.rsplit_once('/').map_or_else(
        || String::from("/"),
        |(parent, _)| {
            if parent.is_empty() {
                String::from("/")
            } else {
                parent.to_owned()
            }
        },
    )
}

fn filter_remote_suggestions(
    suggestions: &[RemotePathSuggestion],
    filter: &str,
) -> Vec<RemotePathSuggestion> {
    let filter = filter.trim().to_ascii_lowercase();
    suggestions
        .iter()
        .filter(|suggestion| suggestion.name.to_ascii_lowercase().contains(&filter))
        .cloned()
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RemoteSelectionAction {
    BrowseDirectory(String),
    OpenCzi(String),
}

fn remote_selection_action(
    suggestion: &RemotePathSuggestion,
    double_clicked: bool,
) -> Option<RemoteSelectionAction> {
    if !double_clicked {
        return None;
    }
    Some(match suggestion.kind {
        RemotePathKind::Directory => {
            RemoteSelectionAction::BrowseDirectory(directory_path(&suggestion.path))
        }
        RemotePathKind::CziFile => RemoteSelectionAction::OpenCzi(suggestion.path.clone()),
    })
}

#[derive(Clone, Debug, PartialEq)]
struct DatasetInfo {
    source_label: String,
    tile_count: usize,
    c: DimensionChoices,
    s: SceneChoices,
    z: DimensionChoices,
    t: DimensionChoices,
    planes: Vec<PlaneInfo>,
    pixel_type: PixelType,
    metadata: MetadataDocument,
    metadata_summary: MetadataSummary,
}

impl DatasetInfo {
    fn from_dataset(source_label: String, dataset: &CziDataset, query: &TileQueryIndex) -> Self {
        let tiles = &dataset.index().tiles;
        let mut metadata = dataset.index().metadata.as_ref().map_or_else(
            || MetadataDocument {
                root: None,
                diagnostics: vec![czi_core::MetadataDiagnostic {
                    message: String::from("This CZI has no global metadata XML."),
                }],
                raw_xml: None,
                summary: MetadataSummary::default(),
            },
            |metadata| {
                MetadataDocument::parse(
                    &metadata.xml,
                    MetadataParseOptions {
                        retain_raw_xml: true,
                        ..MetadataParseOptions::default()
                    },
                )
            },
        );
        metadata.diagnostics.extend(
            dataset
                .index()
                .metadata_diagnostics
                .iter()
                .cloned()
                .map(|message| czi_core::MetadataDiagnostic { message }),
        );
        let metadata_summary = summarize_metadata(&metadata);
        let pixel_type = tiles
            .first()
            .map_or(PixelType::Gray8, |tile| tile.entry.pixel_type);
        Self {
            source_label,
            tile_count: tiles.len(),
            c: dimension_choices(tiles, DimensionCode::C),
            s: scene_choices(query),
            z: dimension_choices(tiles, DimensionCode::Z),
            t: dimension_choices(tiles, DimensionCode::T),
            planes: query.planes().cloned().collect(),
            pixel_type,
            metadata,
            metadata_summary,
        }
    }

    fn default_selection(&self) -> PlaneSelection {
        self.planes.first().map_or_else(
            || PlaneSelection {
                c: self.c.default_value(),
                scene: self.s.default_value(),
                z: self.z.default_value(),
                t: self.t.default_value(),
            },
            |plane| plane.key.into(),
        )
    }

    fn repair_selection(&self, selection: PlaneSelection, changed: [bool; 4]) -> PlaneSelection {
        if self.plane(selection).is_some() {
            return selection;
        }
        self.planes
            .iter()
            .find(|plane| {
                (!changed[0] || plane.key.c == selection.c)
                    && (!changed[1] || plane.key.scene == selection.scene)
                    && (!changed[2] || plane.key.z == selection.z)
                    && (!changed[3] || plane.key.t == selection.t)
            })
            .or_else(|| {
                self.planes.iter().find(|plane| {
                    (changed[0] && plane.key.c == selection.c)
                        || (changed[1] && plane.key.scene == selection.scene)
                        || (changed[2] && plane.key.z == selection.z)
                        || (changed[3] && plane.key.t == selection.t)
                })
            })
            .or_else(|| self.planes.first())
            .map_or(selection, |plane| plane.key.into())
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
    browse: u64,
    connection: u64,
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

    fn begin_browse(&mut self) -> u64 {
        self.browse = self.browse.wrapping_add(1);
        self.browse
    }

    fn accepts_browse(&self, browse: u64) -> bool {
        browse == self.browse
    }

    fn begin_connection(&mut self) -> u64 {
        self.connection = self.connection.wrapping_add(1);
        self.connection
    }

    fn accepts_connection(&self, connection: u64) -> bool {
        connection == self.connection
    }
}

fn accepts_open_result(
    generations: &Generations,
    source_generation: u64,
    connection_generation: u64,
    remote: bool,
) -> bool {
    generations.accepts_source(source_generation)
        && (!remote || generations.accepts_connection(connection_generation))
}

#[derive(Clone, Debug)]
struct ViewRequest {
    source_generation: u64,
    view_generation: u64,
    planes: Vec<PlaneSelector>,
    viewport: SpatialRect,
    prefetch_viewport: SpatialRect,
    target_downsample: f64,
    resident_tile_ids: Vec<TileId>,
}

type ViewRequestKey = (Vec<PlaneKey>, SpatialRect, u64);

enum ViewSubmission {
    Sent,
    Pending(ViewRequest),
    Unavailable(String),
}

enum WorkerCommand {
    Open {
        locator: DatasetLocator,
        source_generation: u64,
        connection_generation: u64,
    },
    Browse {
        profile: String,
        path: String,
        home: bool,
        config: OpenSshConfig,
        browse_generation: u64,
        connection_generation: u64,
    },
    ClearBrowse,
    ClearDataset,
    View(ViewRequest),
    Shutdown,
}

#[derive(Clone, Default)]
struct ConsolePumpSnapshot {
    transcript: String,
    error: Option<String>,
}

enum ConsolePumpCommand {
    Input(Vec<u8>),
    Stop,
}

struct ConsolePump {
    commands: SyncSender<ConsolePumpCommand>,
    snapshot: Arc<Mutex<ConsolePumpSnapshot>>,
}

impl ConsolePump {
    fn spawn(console: SshConsole) -> Result<Self, czi_ssh::SftpError> {
        let (commands, command_rx) = mpsc::sync_channel(CONSOLE_PUMP_INPUT_CAPACITY);
        let snapshot = Arc::new(Mutex::new(ConsolePumpSnapshot::default()));
        let worker_snapshot = Arc::clone(&snapshot);
        let _pump = thread::Builder::new()
            .name(String::from("czi-ssh-console-pump"))
            .spawn(move || pump_console(console, &command_rx, &worker_snapshot))
            .map_err(|source| czi_ssh::SftpError::Spawn { source })?;
        Ok(Self { commands, snapshot })
    }

    fn snapshot(&self) -> ConsolePumpSnapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn try_send_input(&self, input: Vec<u8>) -> Result<(), String> {
        if input.len() > MAX_CONSOLE_INPUT_BYTES {
            return Err(String::from("SSH console input is too large."));
        }
        self.commands
            .try_send(ConsolePumpCommand::Input(input))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    String::from("SSH console input is busy; try again.")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    String::from("SSH console is no longer available.")
                }
            })
    }
}

impl Drop for ConsolePump {
    fn drop(&mut self) {
        let _ = self.commands.try_send(ConsolePumpCommand::Stop);
    }
}

fn pump_console(
    mut console: SshConsole,
    commands: &Receiver<ConsolePumpCommand>,
    snapshot: &Mutex<ConsolePumpSnapshot>,
) {
    loop {
        loop {
            match commands.try_recv() {
                Ok(ConsolePumpCommand::Input(input)) => {
                    if let Err(error) = console.write_input(&input) {
                        set_console_pump_error(snapshot, sanitize_error(error));
                    }
                }
                Ok(ConsolePumpCommand::Stop) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            }
        }
        if let Err(error) = console.drain_output() {
            set_console_pump_error(snapshot, sanitize_error(error));
        }
        if let Ok(mut snapshot) = snapshot.lock() {
            console.transcript().clone_into(&mut snapshot.transcript);
        }
        match commands.recv_timeout(Duration::from_millis(16)) {
            Ok(ConsolePumpCommand::Input(input)) => {
                if let Err(error) = console.write_input(&input) {
                    set_console_pump_error(snapshot, sanitize_error(error));
                }
            }
            Ok(ConsolePumpCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn set_console_pump_error(snapshot: &Mutex<ConsolePumpSnapshot>, error: String) {
    if let Ok(mut snapshot) = snapshot.lock() {
        snapshot.error = Some(error);
    }
}

enum WorkerEvent {
    Opened {
        info: Box<DatasetInfo>,
        source_generation: u64,
        connection_generation: u64,
        remote: bool,
    },
    OpenFailed {
        message: String,
        session_usable: bool,
        source_generation: u64,
        connection_generation: u64,
        remote: bool,
    },
    AuthenticationStarted {
        profile: String,
        console: ConsolePump,
        cancellation: EmbeddedSshCancellation,
        connection_generation: u64,
    },
    AuthenticationSucceeded {
        connection_generation: u64,
    },
    AuthenticationFailed {
        message: String,
        connection_generation: u64,
    },
    RemotePaths {
        directory: String,
        suggestions: Vec<RemotePathSuggestion>,
        home: bool,
        browse_generation: u64,
        connection_generation: u64,
    },
    RemotePathsFailed {
        message: String,
        recoverable_remote_status: bool,
        browse_generation: u64,
        connection_generation: u64,
    },
    TileLoaded {
        tile_id: TileId,
        plane: PlaneKey,
        logical_rect: SpatialRect,
        scale: PyramidScale,
        paint_order: usize,
        prefetch: bool,
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum DatasetOrigin {
    Local,
    Remote,
}

struct ViewInterruption {
    command: WorkerCommand,
    resume: Option<ViewRequest>,
}

enum TileDecodeResult {
    Complete,
    Interrupted(Box<ViewInterruption>),
    Failed,
}

struct WorkerBrowseSession {
    profile: String,
    config: OpenSshConfig,
    connection_generation: u64,
    session: SharedSftpSession,
}

#[derive(Clone, Copy)]
struct RemoteSessionKey<'a> {
    profile: &'a str,
    config: &'a OpenSshConfig,
    generation: u64,
}

fn matches_worker_remote_session(
    existing: RemoteSessionKey<'_>,
    requested: RemoteSessionKey<'_>,
) -> bool {
    existing.profile == requested.profile
        && existing.config == requested.config
        && existing.generation == requested.generation
}

struct ConnectedSftpSession {
    session: SftpSession,
    embedded_cancellation: Option<EmbeddedSshCancellation>,
}

struct EmbeddedConnectionContext<'a> {
    cancellation: &'a EmbeddedCancellationSlot,
    events: &'a SyncSender<WorkerEvent>,
    generation: u64,
}

struct DatasetWorker {
    commands: Option<SyncSender<WorkerCommand>>,
    events: Option<Receiver<WorkerEvent>>,
    embedded_cancellation: EmbeddedCancellationSlot,
    join: Option<JoinHandle<()>>,
}

impl DatasetWorker {
    fn spawn() -> Self {
        let (commands, command_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let embedded_cancellation = EmbeddedCancellationSlot::default();
        let worker_embedded_cancellation = embedded_cancellation.clone();
        let join = thread::Builder::new()
            .name(String::from("czi-dataset-worker"))
            .spawn(move || {
                worker_loop(&command_rx, &event_tx, &worker_embedded_cancellation);
            })
            .expect("start CZI dataset worker");
        Self {
            commands: Some(commands),
            events: Some(events),
            embedded_cancellation,
            join: Some(join),
        }
    }

    fn send(&self, command: WorkerCommand) -> Result<(), String> {
        self.commands
            .as_ref()
            .ok_or_else(|| String::from("dataset worker is shut down"))?
            .try_send(command)
            .map_err(|error| format!("dataset worker command queue is unavailable: {error}"))
    }

    fn try_send_view(&self, request: ViewRequest) -> ViewSubmission {
        let Some(commands) = self.commands.as_ref() else {
            return ViewSubmission::Unavailable(String::from("dataset worker is shut down"));
        };
        enqueue_view(commands, request)
    }

    fn shutdown(&mut self) {
        if self.join.is_none() {
            return;
        }
        self.embedded_cancellation.cancel_active();
        self.events.take();
        let commands = self.commands.take();
        if let Some(commands) = &commands {
            let _ = commands.try_send(WorkerCommand::Shutdown);
        }
        drop(commands);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn enqueue_view(commands: &SyncSender<WorkerCommand>, request: ViewRequest) -> ViewSubmission {
    match commands.try_send(WorkerCommand::View(request)) {
        Ok(()) => ViewSubmission::Sent,
        Err(mpsc::TrySendError::Full(WorkerCommand::View(request))) => {
            ViewSubmission::Pending(request)
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            ViewSubmission::Unavailable(String::from("dataset worker is shut down"))
        }
        Err(mpsc::TrySendError::Full(_)) => {
            ViewSubmission::Unavailable(String::from("dataset worker command queue is unavailable"))
        }
    }
}

fn replace_pending_view(
    pending_view: &mut Option<(ViewRequest, ViewRequestKey)>,
    request: ViewRequest,
    key: ViewRequestKey,
) {
    *pending_view = Some((request, key));
}

fn record_view_submission(
    pending_view: &mut Option<(ViewRequest, ViewRequestKey)>,
    submission: ViewSubmission,
    key: ViewRequestKey,
) -> Result<(), String> {
    match submission {
        ViewSubmission::Sent => {
            *pending_view = None;
            Ok(())
        }
        ViewSubmission::Pending(request) => {
            replace_pending_view(pending_view, request, key);
            Ok(())
        }
        ViewSubmission::Unavailable(error) => {
            *pending_view = None;
            Err(error)
        }
    }
}

impl Drop for DatasetWorker {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[allow(clippy::too_many_lines)]
fn worker_loop(
    commands: &Receiver<WorkerCommand>,
    events: &SyncSender<WorkerEvent>,
    embedded_cancellation: &EmbeddedCancellationSlot,
) {
    let mut dataset = None;
    let mut browse_session = None;
    let mut active_source_generation = 0;
    let mut pending_commands = VecDeque::new();
    loop {
        let command = match pending_commands.pop_front() {
            Some(command) => command,
            None => match commands.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };
        match command {
            WorkerCommand::Open {
                locator,
                source_generation,
                connection_generation,
            } => {
                active_source_generation = source_generation;
                let remote = matches!(&locator, DatasetLocator::Remote { .. });
                let connection = EmbeddedConnectionContext {
                    cancellation: embedded_cancellation,
                    events,
                    generation: connection_generation,
                };
                let (next_dataset, sent) = send_open_result(
                    events,
                    open_dataset(locator, &mut browse_session, &connection),
                    source_generation,
                    connection_generation,
                    remote,
                );
                if !sent {
                    break;
                }
                dataset = next_dataset;
            }
            WorkerCommand::Browse {
                profile,
                path,
                home,
                config,
                browse_generation,
                connection_generation,
            } => {
                let connection = EmbeddedConnectionContext {
                    cancellation: embedded_cancellation,
                    events,
                    generation: connection_generation,
                };
                let result = browse_remote_paths(
                    &profile,
                    &path,
                    home,
                    &config,
                    &mut browse_session,
                    &connection,
                );
                if !send_remote_browse_result(
                    events,
                    result,
                    browse_generation,
                    connection_generation,
                ) {
                    break;
                }
            }
            WorkerCommand::ClearBrowse => browse_session = None,
            WorkerCommand::ClearDataset => {
                dataset = None;
                active_source_generation = 0;
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
                    if let Some(ViewInterruption { command, resume }) =
                        process_view(commands, events, opened, &request)
                    {
                        if let Some(resume) = resume {
                            pending_commands.push_front(WorkerCommand::View(resume));
                        }
                        pending_commands.push_front(command);
                    }
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

fn send_open_result(
    events: &SyncSender<WorkerEvent>,
    result: Result<(CziDataset, TileQueryIndex, DatasetInfo), OpenFailure>,
    source_generation: u64,
    connection_generation: u64,
    remote: bool,
) -> (Option<WorkerDataset>, bool) {
    match result {
        Ok((dataset, query, info)) => {
            let sent = events
                .send(WorkerEvent::Opened {
                    info: Box::new(info),
                    source_generation,
                    connection_generation,
                    remote,
                })
                .is_ok();
            (sent.then_some(WorkerDataset { dataset, query }), sent)
        }
        Err(OpenFailure {
            message,
            session_usable,
        }) => (
            None,
            events
                .send(WorkerEvent::OpenFailed {
                    message,
                    session_usable,
                    source_generation,
                    connection_generation,
                    remote,
                })
                .is_ok(),
        ),
    }
}

fn send_remote_browse_result(
    events: &SyncSender<WorkerEvent>,
    result: Result<RemoteBrowseResult, OpenFailure>,
    browse_generation: u64,
    connection_generation: u64,
) -> bool {
    match result {
        Ok(RemoteBrowseResult {
            directory,
            suggestions,
            home,
        }) => events
            .send(WorkerEvent::RemotePaths {
                directory,
                suggestions,
                home,
                browse_generation,
                connection_generation,
            })
            .is_ok(),
        Err(OpenFailure {
            message,
            session_usable,
        }) => events
            .send(WorkerEvent::RemotePathsFailed {
                message,
                recoverable_remote_status: session_usable,
                browse_generation,
                connection_generation,
            })
            .is_ok(),
    }
}

fn browse_remote_paths(
    profile: &str,
    path: &str,
    home: bool,
    config: &OpenSshConfig,
    browse_session: &mut Option<WorkerBrowseSession>,
    connection: &EmbeddedConnectionContext<'_>,
) -> Result<RemoteBrowseResult, OpenFailure> {
    let profile = SshProfile::new(profile.to_owned()).map_err(validation_failure)?;
    let target = remote_browse_target(path, home).map_err(validation_failure)?;
    let target = match target {
        RemoteBrowseTarget::Home => None,
        RemoteBrowseTarget::Directory { path, prefix } => {
            Some((SftpLocation::new(path).map_err(validation_failure)?, prefix))
        }
    };
    let matches_existing = browse_session.as_ref().is_some_and(|existing| {
        matches_worker_remote_session(
            RemoteSessionKey {
                profile: &existing.profile,
                config: &existing.config,
                generation: existing.connection_generation,
            },
            RemoteSessionKey {
                profile: profile.as_str(),
                config,
                generation: connection.generation,
            },
        )
    });
    if !matches_existing {
        if let Some(existing) = browse_session.as_ref() {
            let _ = existing
                .session
                .cancel_embedded_connection(existing.connection_generation);
        }
        *browse_session = None;
        let session = shared_session(
            connect_embedded(&profile, config, connection).map_err(remote_failure)?,
            connection.generation,
        );
        *browse_session = Some(WorkerBrowseSession {
            profile: profile.as_str().to_owned(),
            config: config.clone(),
            connection_generation: connection.generation,
            session,
        });
    }
    let result = browse_with_session(
        &browse_session
            .as_ref()
            .expect("browse session was created or matched")
            .session,
        target,
    );
    let session_usable = browse_session
        .as_ref()
        .is_some_and(|session| session.session.is_usable());
    if result.is_err() && !session_usable {
        *browse_session = None;
    }
    result.map_err(|error| remote_session_failure(error, session_usable))
}

fn browse_with_session(
    session: &SharedSftpSession,
    target: Option<(SftpLocation, String)>,
) -> Result<RemoteBrowseResult, czi_ssh::SftpError> {
    let (directory, prefix, home, entries) = session.with_session(|session| {
        let (directory, prefix, home) = match target {
            None => {
                let current_directory = SftpLocation::new(".")
                    .expect("the fixed SFTP current-directory location is valid");
                let home = session.realpath(&current_directory)?;
                (home, String::new(), true)
            }
            Some((directory, prefix)) => (directory, prefix, false),
        };
        let entries = session.read_dir_limited(&directory, MAX_REMOTE_DIRECTORY_ENTRIES)?;
        Ok((directory, prefix, home, entries))
    })?;
    let directory = directory.as_str().trim_end_matches('/');
    let directory = if directory.is_empty() { "/" } else { directory };
    Ok(RemoteBrowseResult {
        directory: directory.to_owned(),
        suggestions: remote_path_suggestions(directory, &prefix, entries),
        home,
    })
}

fn open_dataset(
    locator: DatasetLocator,
    browse_session: &mut Option<WorkerBrowseSession>,
    connection: &EmbeddedConnectionContext<'_>,
) -> Result<(CziDataset, TileQueryIndex, DatasetInfo), OpenFailure> {
    match locator {
        DatasetLocator::Local(path) => {
            *browse_session = None;
            let source_label = path.display().to_string();
            let opened = LocalFileSource::open(&path)
                .map_err(czi_core::CziError::from)
                .and_then(CziDataset::open)
                .map_err(|error| OpenFailure {
                    message: sanitize_error(error),
                    session_usable: false,
                })?;
            finish_open(source_label, opened).map_err(|error| OpenFailure {
                message: sanitize_error(error),
                session_usable: false,
            })
        }
        remote @ DatasetLocator::Remote { .. } => {
            let source_label = remote.display_label();
            let (profile, location, config) = remote
                .remote_parts()
                .map_err(DatasetLocator::open_failure)?;
            let source = if browse_session.as_ref().is_some_and(|existing| {
                matches_worker_remote_session(
                    RemoteSessionKey {
                        profile: &existing.profile,
                        config: &existing.config,
                        generation: existing.connection_generation,
                    },
                    RemoteSessionKey {
                        profile: profile.as_str(),
                        config,
                        generation: connection.generation,
                    },
                )
            }) {
                SftpSource::open_with_shared_session(
                    browse_session
                        .as_ref()
                        .expect("matching browse session is present")
                        .session
                        .clone(),
                    &location,
                )
            } else {
                if let Some(existing) = browse_session.as_ref() {
                    let _ = existing
                        .session
                        .cancel_embedded_connection(existing.connection_generation);
                }
                *browse_session = None;
                let session = shared_session(
                    connect_embedded(&profile, config, connection).map_err(remote_failure)?,
                    connection.generation,
                );
                *browse_session = Some(WorkerBrowseSession {
                    profile: profile.as_str().to_owned(),
                    config: config.clone(),
                    connection_generation: connection.generation,
                    session: session.clone(),
                });
                SftpSource::open_with_shared_session(session, &location)
            };
            let source = match source {
                Ok(source) => source,
                Err(error) => return Err(classify_remote_open_failure(browse_session, error)),
            };
            let cache = match BlockCache::with_defaults(source) {
                Ok(cache) => cache,
                Err(error) => return Err(classify_remote_open_failure(browse_session, error)),
            };
            let opened = match CziDataset::open(cache) {
                Ok(opened) => opened,
                Err(error) => return Err(classify_remote_open_failure(browse_session, error)),
            };
            finish_open(source_label, opened)
                .map_err(|error| classify_remote_open_failure(browse_session, error))
        }
    }
}

fn connect_embedded(
    profile: &SshProfile,
    config: &OpenSshConfig,
    connection: &EmbeddedConnectionContext<'_>,
) -> Result<ConnectedSftpSession, czi_ssh::SftpError> {
    let deadline = std::time::Instant::now() + EMBEDDED_START_TIMEOUT;
    let (pending, console) = loop {
        match SftpSession::start_embedded(profile, config) {
            Ok(connection) => break connection,
            Err(error)
                if matches!(
                    &error,
                    czi_ssh::SftpError::Spawn { source }
                        if source.kind() == std::io::ErrorKind::AlreadyExists
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = connection.events.send(WorkerEvent::AuthenticationFailed {
                    message: sanitize_error(&error),
                    connection_generation: connection.generation,
                });
                return Err(error);
            }
        }
    };
    let cancellation = pending.cancellation();
    connection
        .cancellation
        .replace(connection.generation, &cancellation);
    let console = match ConsolePump::spawn(console) {
        Ok(console) => console,
        Err(error) => {
            connection.cancellation.clear(connection.generation);
            return Err(error);
        }
    };
    if connection
        .events
        .send(WorkerEvent::AuthenticationStarted {
            profile: profile.as_str().to_owned(),
            console,
            cancellation: cancellation.clone(),
            connection_generation: connection.generation,
        })
        .is_err()
    {
        connection.cancellation.cancel(connection.generation);
        return Err(czi_ssh::SftpError::Spawn {
            source: std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "viewer event receiver closed",
            ),
        });
    }
    match pending.initialize() {
        Ok(session) => {
            if connection
                .events
                .send(WorkerEvent::AuthenticationSucceeded {
                    connection_generation: connection.generation,
                })
                .is_err()
            {
                connection.cancellation.cancel(connection.generation);
                return Err(czi_ssh::SftpError::Spawn {
                    source: std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "viewer event receiver closed",
                    ),
                });
            }
            Ok(ConnectedSftpSession {
                session,
                embedded_cancellation: Some(cancellation),
            })
        }
        Err(error) => {
            connection.cancellation.clear(connection.generation);
            let _ = connection.events.send(WorkerEvent::AuthenticationFailed {
                message: sanitize_error(&error),
                connection_generation: connection.generation,
            });
            Err(error)
        }
    }
}

fn shared_session(connected: ConnectedSftpSession, generation: u64) -> SharedSftpSession {
    match connected.embedded_cancellation {
        Some(cancellation) => {
            SharedSftpSession::new_embedded(connected.session, generation, cancellation)
        }
        None => SharedSftpSession::new(connected.session),
    }
}

fn finish_open(
    source_label: String,
    opened: CziDataset,
) -> Result<(CziDataset, TileQueryIndex, DatasetInfo), String> {
    let query = TileQueryIndex::new(opened.index()).map_err(|error| error.to_string())?;
    let info = DatasetInfo::from_dataset(source_label, &opened, &query);
    Ok((opened, query, info))
}

fn take_newer_command(
    commands: &Receiver<WorkerCommand>,
    current_view: &ViewRequest,
) -> Option<ViewInterruption> {
    let mut latest_view = None;
    loop {
        match commands.try_recv() {
            Ok(WorkerCommand::View(request)) => latest_view = Some(request),
            Ok(command @ (WorkerCommand::Browse { .. } | WorkerCommand::ClearBrowse)) => {
                return Some(ViewInterruption {
                    command,
                    resume: Some(latest_view.unwrap_or_else(|| current_view.clone())),
                });
            }
            Ok(
                command @ (WorkerCommand::Open { .. }
                | WorkerCommand::ClearDataset
                | WorkerCommand::Shutdown),
            ) => {
                return Some(ViewInterruption {
                    command,
                    resume: None,
                });
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                return latest_view.map(|request| ViewInterruption {
                    command: WorkerCommand::View(request),
                    resume: None,
                });
            }
        }
    }
}

fn process_view(
    commands: &Receiver<WorkerCommand>,
    events: &SyncSender<WorkerEvent>,
    opened: &WorkerDataset,
    request: &ViewRequest,
) -> Option<ViewInterruption> {
    if let Some(newer) = take_newer_command(commands, request) {
        return Some(newer);
    }
    for plane in &request.planes {
        if let Some(interruption) = process_plane_view(commands, events, opened, request, *plane) {
            return Some(interruption);
        }
    }
    None
}

fn process_plane_view(
    commands: &Receiver<WorkerCommand>,
    events: &SyncSender<WorkerEvent>,
    opened: &WorkerDataset,
    request: &ViewRequest,
    plane: PlaneSelector,
) -> Option<ViewInterruption> {
    let query = match ViewQuery::new(plane, request.viewport, request.target_downsample)
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
    let prefetch = match ViewQuery::new(plane, request.prefetch_viewport, request.target_downsample)
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
    let visible = visible_tile_ids.iter().copied().collect::<HashSet<_>>();
    let mut visible_decode = query.hits.clone();
    sort_center_first(&mut visible_decode, request.viewport);
    match decode_tiles(
        commands,
        events,
        opened,
        request,
        &resident,
        visible_decode,
        false,
    ) {
        TileDecodeResult::Complete => {}
        TileDecodeResult::Interrupted(interruption) => return Some(*interruption),
        TileDecodeResult::Failed => return None,
    }
    if events
        .send(WorkerEvent::ViewFinished {
            plane: query.plane,
            scale: query.scale,
            visible_tile_ids,
            source_generation: request.source_generation,
            view_generation: request.view_generation,
        })
        .is_err()
    {
        return None;
    }
    let mut prefetch_decode = prefetch
        .hits
        .into_iter()
        .filter(|hit| !visible.contains(&hit.tile_id))
        .collect::<Vec<_>>();
    sort_center_first(&mut prefetch_decode, request.viewport);
    prefetch_decode.truncate(MAX_PREFETCH_TILES);
    let mut interruption = match decode_tiles(
        commands,
        events,
        opened,
        request,
        &resident,
        prefetch_decode,
        true,
    ) {
        TileDecodeResult::Complete | TileDecodeResult::Failed => return None,
        TileDecodeResult::Interrupted(interruption) => *interruption,
    };
    if interruption
        .resume
        .as_ref()
        .is_some_and(|resume| resume.view_generation == request.view_generation)
    {
        interruption.resume = None;
    }
    Some(interruption)
}

fn decode_tiles(
    commands: &Receiver<WorkerCommand>,
    events: &SyncSender<WorkerEvent>,
    opened: &WorkerDataset,
    request: &ViewRequest,
    resident: &HashSet<TileId>,
    hits: Vec<TileHit>,
    prefetch: bool,
) -> TileDecodeResult {
    for hit in hits {
        if resident.contains(&hit.tile_id) {
            continue;
        }
        let tile = match opened.dataset.decoded_tile(hit.tile_id.index()) {
            Ok(tile) => tile,
            Err(_) if prefetch => continue,
            Err(error) => {
                let _ = events.send(WorkerEvent::ViewFailed {
                    message: format!("tile {}: {error}", hit.tile_id),
                    source_generation: request.source_generation,
                    view_generation: request.view_generation,
                });
                return TileDecodeResult::Failed;
            }
        };
        let event = WorkerEvent::TileLoaded {
            tile_id: hit.tile_id,
            plane: hit.plane,
            logical_rect: hit.logical_rect,
            scale: hit.scale,
            paint_order: hit.paint_order,
            prefetch,
            tile,
            source_generation: request.source_generation,
            view_generation: request.view_generation,
        };
        if events.send(event).is_err() {
            return TileDecodeResult::Failed;
        }
        if let Some(newer) = take_newer_command(commands, request) {
            return TileDecodeResult::Interrupted(Box::new(newer));
        }
    }
    TileDecodeResult::Complete
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OpenMode {
    #[default]
    Local,
    Ssh,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DisplayMode {
    #[default]
    Single,
    Composite,
}

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord)]
enum ChannelRole {
    #[default]
    Off,
    Gray,
    Red,
    Green,
    Blue,
}

impl ChannelRole {
    const ALL: [Self; 5] = [Self::Off, Self::Gray, Self::Red, Self::Green, Self::Blue];

    const fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Gray => "Gray / Phase",
            Self::Red => "Red",
            Self::Green => "Green",
            Self::Blue => "Blue",
        }
    }
}

fn blend_channel(base: [u8; 3], intensity: u8, role: ChannelRole) -> [u8; 3] {
    if role == ChannelRole::Gray {
        return [intensity; 3];
    }
    let overlay = match role {
        ChannelRole::Red => [u8::MAX, 0, 0],
        ChannelRole::Green => [0, u8::MAX, 0],
        ChannelRole::Blue => [0, 0, u8::MAX],
        ChannelRole::Off | ChannelRole::Gray => return base,
    };
    let alpha = u16::from(intensity);
    let inverse = u16::from(u8::MAX - intensity);
    std::array::from_fn(|index| {
        let value = u16::from(overlay[index]) * alpha + u16::from(base[index]) * inverse;
        u8::try_from(value / u16::from(u8::MAX)).expect("source-over channel fits u8")
    })
}

fn default_channel_roles(
    choices: &DimensionChoices,
    summary: &MetadataSummary,
) -> HashMap<i32, ChannelRole> {
    let mut roles = HashMap::new();
    for channel in &choices.values {
        let label = channel_label(summary, *channel).to_ascii_lowercase();
        let role = if ["phase", "brightfield", "bright field", "transmitted"]
            .iter()
            .any(|needle| label.contains(needle))
        {
            ChannelRole::Gray
        } else if [
            "hada",
            "dapi",
            "hoechst",
            "af405",
            "af 405",
            "alexa fluor 405",
            "alexa 405",
        ]
        .iter()
        .any(|needle| label.contains(needle))
        {
            ChannelRole::Blue
        } else if ["bodipy", "bod493", "fitc", "gfp"]
            .iter()
            .any(|needle| label.contains(needle))
        {
            ChannelRole::Green
        } else {
            ChannelRole::Off
        };
        roles.insert(*channel, role);
    }
    if !roles.values().any(|role| *role != ChannelRole::Off)
        && let Some(first) = choices.values.first()
    {
        roles.insert(*first, ChannelRole::Gray);
    }
    roles
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
    fn world_center(bounds: SpatialRect) -> (f64, f64) {
        (
            (bounds.min_x as f64 + bounds.max_x as f64) * 0.5,
            (bounds.min_y as f64 + bounds.max_y as f64) * 0.5,
        )
    }

    fn world_to_screen_xy(
        self,
        world: (f64, f64),
        canvas: egui::Rect,
        bounds: SpatialRect,
    ) -> egui::Pos2 {
        let (center_x, center_y) = Self::world_center(bounds);
        let delta_x = ((world.0 - center_x) * self.zoom) as f32;
        let delta_y = ((world.1 - center_y) * self.zoom) as f32;
        canvas.center() + self.pan + egui::vec2(delta_x, delta_y)
    }

    fn screen_to_world_xy(
        self,
        screen: egui::Pos2,
        canvas: egui::Rect,
        bounds: SpatialRect,
    ) -> (f64, f64) {
        let (center_x, center_y) = Self::world_center(bounds);
        (
            center_x + f64::from(screen.x - canvas.center().x - self.pan.x) / self.zoom,
            center_y + f64::from(screen.y - canvas.center().y - self.pan.y) / self.zoom,
        )
    }

    fn zoom_at(
        &mut self,
        cursor: egui::Pos2,
        factor: f64,
        canvas: egui::Rect,
        bounds: SpatialRect,
    ) {
        let anchor = self.screen_to_world_xy(cursor, canvas, bounds);
        self.zoom = (self.zoom * factor).clamp(0.000_001, 1_000_000.0);
        let (center_x, center_y) = Self::world_center(bounds);
        self.pan = egui::vec2(
            cursor.x - canvas.center().x - ((anchor.0 - center_x) * self.zoom) as f32,
            cursor.y - canvas.center().y - ((anchor.1 - center_y) * self.zoom) as f32,
        );
    }

    fn fit(&mut self, canvas: egui::Rect, bounds: SpatialRect) {
        let width = bounds.width().max(1) as f64;
        let height = bounds.height().max(1) as f64;
        self.zoom = (f64::from(canvas.width()) / width)
            .min(f64::from(canvas.height()) / height)
            .clamp(0.000_001, 1_000_000.0);
        self.pan = egui::Vec2::ZERO;
    }

    fn rebase_bounds(&mut self, previous: SpatialRect, next: SpatialRect) {
        let previous_center = Self::world_center(previous);
        let next_center = Self::world_center(next);
        self.pan += egui::vec2(
            ((next_center.0 - previous_center.0) * self.zoom) as f32,
            ((next_center.1 - previous_center.1) * self.zoom) as f32,
        );
    }

    fn one_to_one(&mut self) {
        *self = Self::default();
    }

    fn viewport(self, canvas: egui::Rect, bounds: SpatialRect) -> Option<SpatialRect> {
        let minimum = self.screen_to_world_xy(canvas.min, canvas, bounds);
        let maximum = self.screen_to_world_xy(canvas.max, canvas, bounds);
        let min_x = floor_i64(minimum.0)?;
        let min_y = floor_i64(minimum.1)?;
        let max_x = ceil_i64(maximum.0)?;
        let max_y = ceil_i64(maximum.1)?;
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

fn prefetch_viewport(viewport: SpatialRect, bounds: SpatialRect) -> SpatialRect {
    let min_x = viewport.min_x.clamp(bounds.min_x, bounds.max_x);
    let min_y = viewport.min_y.clamp(bounds.min_y, bounds.max_y);
    let max_x = viewport.max_x.clamp(min_x, bounds.max_x);
    let max_y = viewport.max_y.clamp(min_y, bounds.max_y);
    let visible = SpatialRect::new(min_x, min_y, max_x, max_y)
        .expect("clamped viewport remains a valid rectangle");
    let margin_x = prefetch_margin(visible.width());
    let margin_y = prefetch_margin(visible.height());
    SpatialRect::new(
        visible.min_x.saturating_sub(margin_x).max(bounds.min_x),
        visible.min_y.saturating_sub(margin_y).max(bounds.min_y),
        visible.max_x.saturating_add(margin_x).min(bounds.max_x),
        visible.max_y.saturating_add(margin_y).min(bounds.max_y),
    )
    .expect("prefetch viewport remains a valid rectangle")
}

fn prefetch_margin(extent: u64) -> i64 {
    let margin = (u128::from(extent) * u128::from(VIEWPORT_PREFETCH_PERCENT)).div_ceil(100);
    i64::try_from(margin).unwrap_or(i64::MAX)
}

fn select_requested_scale(scales: &[PyramidScale], target_downsample: f64) -> Option<PyramidScale> {
    let mut selected = *scales.first()?;
    for scale in scales {
        if scale.as_f64() <= target_downsample {
            selected = *scale;
        }
    }
    Some(selected)
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

fn texture_image(
    tile: &DecodedTile,
    levels: Levels,
    role: ChannelRole,
) -> Result<egui::ColorImage, &'static str> {
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
    let pixels = grayscale
        .into_iter()
        .map(|value| match role {
            ChannelRole::Off => egui::Color32::TRANSPARENT,
            ChannelRole::Gray => egui::Color32::from_gray(value),
            ChannelRole::Red | ChannelRole::Green | ChannelRole::Blue => {
                let [red, green, blue] = blend_channel([0; 3], value, role);
                egui::Color32::from_rgba_premultiplied(red, green, blue, value)
            }
        })
        .collect();
    Ok(egui::ColorImage::new([width, height], pixels))
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TextureKey {
    source_generation: u64,
    plane: PlaneKey,
    tile_id: TileId,
}

fn cache_key_is_active(key: TextureKey, source_generation: u64, planes: &[PlaneKey]) -> bool {
    key.source_generation == source_generation && planes.contains(&key.plane)
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
    protected: HashSet<TextureKey>,
    bytes: usize,
    clock: u64,
    budget: usize,
}

impl TextureCache {
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            protected: HashSet::new(),
            bytes: 0,
            clock: 0,
            budget,
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.protected.clear();
        self.bytes = 0;
    }

    fn clear_visibility(&mut self) {
        for entry in self.entries.values_mut() {
            entry.visible = false;
        }
        self.protected.clear();
        self.evict_non_visible();
    }

    fn begin_view(&mut self, source_generation: u64, planes: &[PlaneKey]) -> Vec<TileId> {
        self.protected = self
            .entries
            .keys()
            .filter(|key| key.source_generation == source_generation && planes.contains(&key.plane))
            .copied()
            .collect();
        for entry in self.entries.values_mut() {
            entry.visible = false;
        }
        for key in &self.protected {
            if let Some(entry) = self.entries.get_mut(key) {
                entry.visible = true;
            }
        }
        self.evict_non_visible();
        self.entries
            .keys()
            .filter(|key| key.source_generation == source_generation && planes.contains(&key.plane))
            .map(|key| key.tile_id)
            .collect()
    }

    fn insert(
        &mut self,
        key: TextureKey,
        texture: egui::TextureHandle,
        bytes: usize,
        hit: TileHit,
        visible: bool,
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
                visible,
                logical_rect: hit.logical_rect,
                paint_order: hit.paint_order,
            },
        );
        self.evict_non_visible();
    }

    fn finish_view(&mut self, source_generation: u64, plane: PlaneKey, visible: &[TileId]) {
        self.protected
            .retain(|key| key.source_generation != source_generation || key.plane != plane);
        let visible = visible.iter().copied().collect::<HashSet<_>>();
        for (key, entry) in &mut self.entries {
            if key.source_generation == source_generation && key.plane == plane {
                entry.visible = visible.contains(&key.tile_id);
            }
        }
        self.evict_non_visible();
    }

    fn evict_non_visible(&mut self) {
        while self.bytes > self.budget {
            let candidate = self
                .entries
                .iter()
                .filter(|(key, entry)| !entry.visible && !self.protected.contains(key))
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

    fn current_counts_for_planes(
        &self,
        source_generation: u64,
        planes: &[PlaneKey],
    ) -> (usize, usize, usize) {
        let entries = self
            .entries
            .iter()
            .filter(|(key, _)| cache_key_is_active(**key, source_generation, planes));
        let mut resident = 0;
        let mut visible = 0;
        let mut bytes: usize = 0;
        for (_, entry) in entries {
            resident += 1;
            visible += usize::from(entry.visible);
            bytes = bytes.saturating_add(entry.bytes);
        }
        (visible, resident, bytes)
    }
}

fn take_worker_event_batch(events: &Receiver<WorkerEvent>) -> Vec<WorkerEvent> {
    let mut batch = Vec::with_capacity(CHANNEL_CAPACITY);
    for _ in 0..CHANNEL_CAPACITY {
        match events.try_recv() {
            Ok(event) => batch.push(event),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    batch
}

struct PendingTile {
    tile_id: TileId,
    plane: PlaneKey,
    logical_rect: SpatialRect,
    scale: PyramidScale,
    paint_order: usize,
    prefetch: bool,
    tile: DecodedTile,
    source_generation: u64,
    view_generation: u64,
}

#[derive(Clone, Copy, Default)]
struct PyramidDisplay {
    requested: Option<PyramidScale>,
    displayed: Option<PyramidScale>,
    view_generation: Option<u64>,
}

impl PyramidDisplay {
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn request(&mut self, view_generation: u64, scale: PyramidScale) {
        self.requested = Some(scale);
        self.view_generation = Some(view_generation);
    }

    fn finish(&mut self, view_generation: u64, scale: PyramidScale) -> bool {
        if self.view_generation != Some(view_generation) || self.requested != Some(scale) {
            return false;
        }
        self.displayed = Some(scale);
        true
    }
}

fn record_finished_plane(
    visible: &mut HashMap<PlaneKey, Vec<TileId>>,
    pyramid: &mut PyramidDisplay,
    view_generation: u64,
    plane: PlaneKey,
    scale: PyramidScale,
    visible_tile_ids: Vec<TileId>,
) {
    let _ = pyramid.finish(view_generation, scale);
    visible.insert(plane, visible_tile_ids);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AuthenticationStatus {
    Connecting,
    Authenticated,
    Failed,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum RemoteBrowserVisibility {
    Open,
    Hidden,
}

impl RemoteBrowserVisibility {
    fn is_open(self) -> bool {
        self == Self::Open
    }

    fn toggle(&mut self) {
        *self = match *self {
            Self::Open => Self::Hidden,
            Self::Hidden => Self::Open,
        };
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum InspectorTab {
    Display,
    Metadata,
}

#[derive(Clone, Copy, Debug)]
struct SnapshotRegion {
    rect: egui::Rect,
    pixels_per_point: f32,
}

#[derive(Clone, Debug)]
struct SnapshotRequest {
    generation: u64,
    region: SnapshotRegion,
    armed_frame: u64,
    region_frozen: bool,
    filename: String,
}

enum SnapshotWriteResult {
    Saved { generation: u64, path: PathBuf },
    Failed { generation: u64, message: String },
}

struct EmbeddedAuthentication {
    profile: String,
    console: ConsolePump,
    cancellation: EmbeddedSshCancellation,
    generation: u64,
    status: AuthenticationStatus,
}

fn reuses_remote_connection(
    profile: &str,
    connection_generation: u64,
    authentication: Option<(&str, AuthenticationStatus)>,
    session: Option<(&str, u64)>,
) -> bool {
    authentication.is_some_and(|(authenticated_profile, status)| {
        authenticated_profile == profile && status != AuthenticationStatus::Failed
    }) || session.is_some_and(|(session_profile, session_generation)| {
        session_profile == profile && session_generation == connection_generation
    })
}

/// The local and SSH CZI mosaic viewer.
pub struct ViewerApp {
    worker: DatasetWorker,
    open_mode: OpenMode,
    path_input: String,
    ssh_profile_input: String,
    remote_path_input: String,
    remote_browser_path_input: String,
    remote_browse_directory: Option<String>,
    remote_suggestions: Vec<RemotePathSuggestion>,
    remote_browse_pending: bool,
    remote_filename_filter: String,
    remote_selected_path: Option<String>,
    remote_browser_visibility: RemoteBrowserVisibility,
    remote_session: Option<(String, u64)>,
    profile_editing: bool,
    authentication_focus_request: Option<u64>,
    inspector_tab: InspectorTab,
    display_mode: DisplayMode,
    channel_roles: HashMap<i32, ChannelRole>,
    metadata_filter: String,
    dataset: Option<DatasetInfo>,
    dataset_origin: Option<DatasetOrigin>,
    opening_origin: Option<DatasetOrigin>,
    selection: PlaneSelection,
    generations: Generations,
    status: Status,
    cache: TextureCache,
    pending_tiles: Vec<PendingTile>,
    visible_tile_ids: HashMap<PlaneKey, Vec<TileId>>,
    pyramid_display: PyramidDisplay,
    levels: Levels,
    camera: Camera,
    fit_pending: bool,
    last_request: Option<ViewRequestKey>,
    pending_view: Option<(ViewRequest, ViewRequestKey)>,
    pending_open: Option<(DatasetLocator, u64)>,
    snapshot_region: Option<SnapshotRegion>,
    pending_snapshot: Option<SnapshotRequest>,
    snapshot_writing: Option<u64>,
    snapshot_generation: u64,
    ui_frame: u64,
    returned_screenshots: Vec<(u64, Arc<egui::ColorImage>)>,
    snapshot_write_sender: mpsc::Sender<SnapshotWriteResult>,
    snapshot_write_results: Receiver<SnapshotWriteResult>,
    embedded_authentication: Option<EmbeddedAuthentication>,
    retired_consoles: Vec<ConsolePump>,
}

impl ViewerApp {
    /// Create the viewer state and its dedicated dataset worker.
    #[must_use]
    pub fn new(_creation_context: &eframe::CreationContext<'_>) -> Self {
        let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
        let (snapshot_write_sender, snapshot_write_results) = mpsc::channel();
        let mut app = Self {
            worker: DatasetWorker::spawn(),
            open_mode: OpenMode::Local,
            path_input: initial_path
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            ssh_profile_input: String::new(),
            remote_path_input: String::new(),
            remote_browser_path_input: String::new(),
            remote_browse_directory: None,
            remote_suggestions: Vec::new(),
            remote_browse_pending: false,
            remote_filename_filter: String::new(),
            remote_selected_path: None,
            remote_browser_visibility: RemoteBrowserVisibility::Open,
            remote_session: None,
            profile_editing: false,
            authentication_focus_request: None,
            inspector_tab: InspectorTab::Display,
            display_mode: DisplayMode::Single,
            channel_roles: HashMap::new(),
            metadata_filter: String::new(),
            dataset: None,
            dataset_origin: None,
            opening_origin: None,
            selection: PlaneSelection::default(),
            generations: Generations::default(),
            status: Status::normal("Choose Local or SSH, then open a .czi file."),
            cache: TextureCache::new(TEXTURE_CACHE_LIMIT),
            pending_tiles: Vec::new(),
            visible_tile_ids: HashMap::new(),
            pyramid_display: PyramidDisplay::default(),
            levels: Levels::default_for(PixelType::Gray16),
            camera: Camera::default(),
            fit_pending: false,
            last_request: None,
            pending_view: None,
            pending_open: None,
            snapshot_region: None,
            pending_snapshot: None,
            snapshot_writing: None,
            snapshot_generation: 0,
            ui_frame: 0,
            returned_screenshots: Vec::new(),
            snapshot_write_sender,
            snapshot_write_results,
            embedded_authentication: None,
            retired_consoles: Vec::new(),
        };
        if initial_path.is_some() {
            app.open_local_path();
        }
        app
    }

    fn invalidate_view(&mut self) {
        self.generations.begin_view();
        self.clear_pending_view();
        self.visible_tile_ids.clear();
        self.pyramid_display.clear();
        self.cache.clear_visibility();
    }

    fn clear_pending_view(&mut self) {
        self.last_request = None;
        self.pending_view = None;
    }

    fn open_local_path(&mut self) {
        let path = PathBuf::from(self.path_input.trim());
        if self.path_input.trim().is_empty() {
            self.status = Status::error("Enter a local .czi path first.");
            return;
        }
        self.disconnect_remote_for_local_source();
        self.open_locator(DatasetLocator::Local(path));
    }

    fn open_remote_path(&mut self) {
        self.open_locator(DatasetLocator::Remote {
            profile: self.ssh_profile_input.trim().to_owned(),
            path: self.remote_path_input.trim().to_owned(),
            config: OpenSshConfig::new(),
        });
    }

    fn close_remote_dataset(&mut self) {
        let pending_remote_open = self
            .pending_open
            .as_ref()
            .is_some_and(|(locator, _)| matches!(locator, DatasetLocator::Remote { .. }));
        if self.dataset_origin != Some(DatasetOrigin::Remote)
            && self.opening_origin != Some(DatasetOrigin::Remote)
            && !pending_remote_open
        {
            return;
        }
        self.generations.begin_source();
        self.dataset = None;
        self.dataset_origin = None;
        self.opening_origin = None;
        self.pending_open = None;
        self.cache.clear();
        self.pending_tiles.clear();
        self.visible_tile_ids.clear();
        self.pyramid_display.clear();
        self.clear_pending_view();
        self.fit_pending = false;
        if let Err(error) = self.worker.send(WorkerCommand::ClearDataset) {
            self.status = Status::error(error);
        }
    }

    fn disconnect_remote_for_local_source(&mut self) {
        self.close_remote_dataset();
        self.cancel_active_remote_connection();
        self.generations.begin_browse();
        self.generations.begin_connection();
        self.retire_embedded_authentication();
        self.remote_session = None;
        self.remote_browse_pending = false;
        self.remote_browse_directory = None;
        self.remote_suggestions.clear();
        self.remote_browser_path_input.clear();
        self.remote_selected_path = None;
        let _ = self.worker.send(WorkerCommand::ClearBrowse);
    }

    fn browse_remote_path(&mut self, home: bool) {
        let browse_generation = self.generations.begin_browse();
        self.remote_browse_pending = true;
        self.status = Status::normal(if home {
            String::from("Finding remote home directory…")
        } else {
            String::from("Listing remote paths…")
        });
        let connection_generation = self.prepare_remote_connection();
        if let Err(error) = self.worker.send(WorkerCommand::Browse {
            profile: self.ssh_profile_input.trim().to_owned(),
            path: if home {
                String::new()
            } else {
                directory_path(self.remote_browser_path_input.trim())
            },
            home,
            config: OpenSshConfig::new(),
            browse_generation,
            connection_generation,
        }) {
            self.remote_browse_pending = false;
            self.status = Status::error(error);
        }
    }

    fn run_remote_selection_action(&mut self, action: RemoteSelectionAction) {
        match action {
            RemoteSelectionAction::BrowseDirectory(path) => {
                self.remote_browser_path_input = path;
                self.remote_selected_path = None;
                self.browse_remote_path(false);
            }
            RemoteSelectionAction::OpenCzi(path) => {
                self.remote_path_input = path;
                self.open_remote_path();
            }
        }
    }

    fn browse_remote_parent(&mut self) {
        let directory = self
            .remote_browse_directory
            .as_deref()
            .map_or_else(|| String::from("/"), remote_parent_path);
        self.remote_browser_path_input = directory_path(&directory);
        self.remote_selected_path = None;
        self.browse_remote_path(false);
    }

    fn refresh_remote_browser(&mut self) {
        if let Some(directory) = &self.remote_browse_directory {
            self.remote_browser_path_input = directory_path(directory);
            self.browse_remote_path(false);
        } else {
            self.browse_remote_path(true);
        }
    }

    fn open_selected_remote_czi(&mut self) {
        let Some(path) = self.remote_selected_path.as_deref() else {
            return;
        };
        let Some(suggestion) = self.remote_suggestions.iter().find(|suggestion| {
            suggestion.path == path && suggestion.kind == RemotePathKind::CziFile
        }) else {
            return;
        };
        self.run_remote_selection_action(RemoteSelectionAction::OpenCzi(suggestion.path.clone()));
    }

    fn open_locator(&mut self, locator: DatasetLocator) {
        let origin = if matches!(&locator, DatasetLocator::Remote { .. }) {
            DatasetOrigin::Remote
        } else {
            DatasetOrigin::Local
        };
        let connection_generation = match &locator {
            DatasetLocator::Remote { .. } => self.prepare_remote_connection(),
            DatasetLocator::Local(_) => self.generations.connection,
        };
        let source_label = locator.display_label();
        let source_generation = self.generations.begin_source();
        self.dataset = None;
        self.dataset_origin = None;
        self.opening_origin = Some(origin);
        self.cache.clear();
        self.pending_tiles.clear();
        self.visible_tile_ids.clear();
        self.pyramid_display.clear();
        self.clear_pending_view();
        self.fit_pending = false;
        self.status = Status::normal(format!("Opening {source_label}…"));
        let pending = (locator.clone(), source_generation);
        if let Err(error) = self.worker.send(WorkerCommand::Open {
            locator,
            source_generation,
            connection_generation,
        }) {
            self.pending_open = Some(pending);
            self.opening_origin = None;
            self.status = Status::error(error);
        } else {
            self.pending_open = None;
        }
    }

    fn retry_pending_open(&mut self) {
        let Some((locator, source_generation)) = self.pending_open.take() else {
            return;
        };
        let origin = if matches!(&locator, DatasetLocator::Remote { .. }) {
            DatasetOrigin::Remote
        } else {
            DatasetOrigin::Local
        };
        let connection_generation = match &locator {
            DatasetLocator::Remote { .. } => self.prepare_remote_connection(),
            DatasetLocator::Local(_) => self.generations.connection,
        };
        self.opening_origin = Some(origin);
        if let Err(error) = self.worker.send(WorkerCommand::Open {
            locator: locator.clone(),
            source_generation,
            connection_generation,
        }) {
            self.pending_open = Some((locator, source_generation));
            self.opening_origin = None;
            self.status = Status::error(error);
        }
    }

    fn prepare_remote_connection(&mut self) -> u64 {
        let profile = self.ssh_profile_input.trim();
        let authentication = self
            .embedded_authentication
            .as_ref()
            .map(|authentication| (authentication.profile.as_str(), authentication.status));
        let session = self
            .remote_session
            .as_ref()
            .map(|(session_profile, session_generation)| {
                (session_profile.as_str(), *session_generation)
            });
        let reuse = reuses_remote_connection(
            profile,
            self.generations.connection,
            authentication,
            session,
        );
        if !reuse {
            self.cancel_active_remote_connection();
            self.retire_embedded_authentication();
            self.remote_session = None;
            self.generations.begin_connection();
        }
        self.generations.connection
    }

    fn cancel_active_remote_connection(&mut self) {
        self.worker
            .embedded_cancellation
            .cancel(self.generations.connection);
        if let Some(authentication) = &self.embedded_authentication
            && authentication.generation == self.generations.connection
        {
            let _ = authentication.cancellation.cancel();
        }
    }

    fn cancel_embedded_authentication(&mut self) {
        self.cancel_active_remote_connection();
        self.generations.begin_browse();
        self.generations.begin_connection();
        self.retire_embedded_authentication();
        self.remote_session = None;
        self.remote_browse_pending = false;
        self.status = Status::normal("SSH authentication cancelled.");
    }

    fn reconnect_remote(&mut self) {
        self.close_remote_dataset();
        self.cancel_active_remote_connection();
        self.retire_embedded_authentication();
        self.remote_session = None;
        self.clear_remote_listing();
        self.refresh_remote_browser();
    }

    fn begin_profile_change(&mut self) {
        self.profile_editing = true;
        self.clear_remote_listing();
    }

    fn clear_remote_listing(&mut self) {
        self.remote_browse_pending = false;
        self.remote_browse_directory = None;
        self.remote_suggestions.clear();
        self.remote_selected_path = None;
    }

    fn active_planes(&self) -> Vec<PlaneSelector> {
        let Some(dataset) = self.dataset.as_ref() else {
            return Vec::new();
        };
        if self.display_mode == DisplayMode::Single {
            return dataset
                .plane(self.selection)
                .is_some()
                .then_some(self.selection)
                .into_iter()
                .collect();
        }
        dataset
            .c
            .values
            .iter()
            .filter(|channel| {
                self.channel_roles.get(channel).copied().unwrap_or_default() != ChannelRole::Off
            })
            .filter_map(|channel| {
                let plane = PlaneSelector::new(
                    *channel,
                    self.selection.scene,
                    self.selection.z,
                    self.selection.t,
                );
                dataset.plane(plane).is_some().then_some(plane)
            })
            .collect()
    }

    fn role_for_plane(&self, plane: PlaneKey) -> ChannelRole {
        if self.display_mode == DisplayMode::Single {
            ChannelRole::Gray
        } else {
            self.channel_roles
                .get(&plane.c)
                .copied()
                .unwrap_or_default()
        }
    }

    fn active_cache_counts(&self, source_generation: u64) -> (usize, usize, usize) {
        let planes = self
            .active_planes()
            .iter()
            .map(|plane| plane.key())
            .collect::<Vec<_>>();
        self.cache
            .current_counts_for_planes(source_generation, &planes)
    }

    fn retire_embedded_authentication(&mut self) {
        self.authentication_focus_request = None;
        if let Some(authentication) = self.embedded_authentication.take() {
            self.retired_consoles.push(authentication.console);
        }
    }

    fn request_view(&mut self, viewport: SpatialRect) {
        let Some((scales, bounds)) = self.dataset.as_ref().and_then(|dataset| {
            dataset
                .plane(self.selection)
                .map(|plane| (plane.scales.clone(), plane.world_bounds))
        }) else {
            return;
        };
        let planes = self.active_planes();
        if planes.is_empty() {
            return;
        }
        let target_downsample = (1.0 / self.camera.zoom).clamp(0.000_001, 1_000_000.0);
        let plane_keys = planes.iter().map(|plane| plane.key()).collect::<Vec<_>>();
        let request_key = (plane_keys.clone(), viewport, target_downsample.to_bits());
        if self.last_request.as_ref() == Some(&request_key) {
            return;
        }
        let view_generation = self.generations.begin_view();
        let requested_scale = select_requested_scale(&scales, target_downsample)
            .expect("indexed plane has at least one pyramid scale");
        self.pyramid_display
            .request(view_generation, requested_scale);
        let resident_tile_ids = self.cache.begin_view(self.generations.source, &plane_keys);
        let request = ViewRequest {
            source_generation: self.generations.source,
            view_generation,
            planes,
            viewport,
            prefetch_viewport: prefetch_viewport(viewport, bounds),
            target_downsample,
            resident_tile_ids,
        };
        self.last_request = Some(request_key.clone());
        if let Err(error) = record_view_submission(
            &mut self.pending_view,
            self.worker.try_send_view(request),
            request_key,
        ) {
            self.last_request = None;
            self.status = Status::error(error);
        }
    }

    fn flush_pending_view(&mut self) {
        let Some((request, key)) = self.pending_view.take() else {
            return;
        };
        if let Err(error) = record_view_submission(
            &mut self.pending_view,
            self.worker.try_send_view(request),
            key,
        ) {
            self.last_request = None;
            self.status = Status::error(error);
        }
    }

    fn poll_snapshot_results(&mut self) {
        loop {
            match self.snapshot_write_results.try_recv() {
                Ok(SnapshotWriteResult::Saved { generation, path })
                    if self.snapshot_writing == Some(generation) =>
                {
                    self.snapshot_writing = None;
                    self.status = Status::normal(format!("Saved PNG: {}", path.display()));
                }
                Ok(SnapshotWriteResult::Failed {
                    generation,
                    message,
                }) if self.snapshot_writing == Some(generation) => {
                    self.snapshot_writing = None;
                    self.status = Status::error(message);
                }
                Ok(SnapshotWriteResult::Saved { .. } | SnapshotWriteResult::Failed { .. }) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
    }

    fn handle_screenshot_events(&mut self, context: &egui::Context) {
        let mut screenshots = Vec::new();
        context.input_mut(|input| {
            input.events.retain(|event| {
                let egui::Event::Screenshot {
                    user_data, image, ..
                } = event
                else {
                    return true;
                };
                let Some(generation) = user_data
                    .data
                    .as_ref()
                    .and_then(|data| data.downcast_ref::<u64>())
                    .copied()
                else {
                    return true;
                };
                screenshots.push((generation, image.clone()));
                false
            });
        });
        self.returned_screenshots.extend(screenshots);
    }

    fn process_returned_screenshots(&mut self) {
        for (generation, image) in std::mem::take(&mut self.returned_screenshots) {
            self.handle_screenshot(generation, image);
        }
    }

    fn request_snapshot(&mut self, context: &egui::Context) {
        if self.pending_snapshot.is_some() || self.snapshot_writing.is_some() {
            self.status = Status::normal("A PNG snapshot is already in progress.");
            return;
        }
        let Some(region) = self.snapshot_region else {
            self.status = Status::error("The canvas is not ready to capture yet.");
            return;
        };
        self.snapshot_generation = self.snapshot_generation.wrapping_add(1);
        let filename = self.dataset.as_ref().map_or_else(
            || String::from("czi-snapshot"),
            |dataset| source_filename(&dataset.source_label),
        );
        let request = SnapshotRequest {
            generation: self.snapshot_generation,
            region,
            armed_frame: self.ui_frame,
            region_frozen: false,
            filename,
        };
        context.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
            request.generation,
        )));
        self.pending_snapshot = Some(request);
        self.status = Status::normal("Capturing annotated canvas PNG…");
    }

    fn handle_screenshot(&mut self, generation: u64, image: Arc<egui::ColorImage>) {
        let Some(request) = take_matching_snapshot_request(&mut self.pending_snapshot, generation)
        else {
            return;
        };
        let Some(crop) = screenshot_crop_bounds(image.size, request.region) else {
            self.status = Status::error("The canvas snapshot area was outside the screenshot.");
            return;
        };
        let sender = self.snapshot_write_sender.clone();
        let filename = request.filename;
        self.snapshot_writing = Some(request.generation);
        self.status = Status::normal("Writing PNG snapshot…");
        if thread::Builder::new()
            .name(String::from("czi-png-writer"))
            .spawn(move || {
                let rgba = crop_screenshot_rgba(&image, crop);
                let result = write_png_snapshot(&filename, crop.width, crop.height, &rgba)
                    .map_or_else(
                        |message| SnapshotWriteResult::Failed {
                            generation: request.generation,
                            message,
                        },
                        |path| SnapshotWriteResult::Saved {
                            generation: request.generation,
                            path,
                        },
                    );
                let _ = sender.send(result);
            })
            .is_err()
        {
            self.snapshot_writing = None;
            self.status = Status::error("Could not start the PNG writer thread.");
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
            self.open_mode = OpenMode::Local;
            self.path_input = path.display().to_string();
            self.open_local_path();
        }
    }

    fn handle_worker_events(&mut self) {
        if let Some(events) = self.worker.events.as_ref() {
            for event in take_worker_event_batch(events) {
                self.handle_worker_event(event);
            }
        }
    }

    fn show_display_inspector(&mut self, ui: &mut egui::Ui) {
        let before_selection = self.selection;
        let before_mode = self.display_mode;
        ui.horizontal(|ui| {
            ui.label("Mode");
            ui.selectable_value(&mut self.display_mode, DisplayMode::Single, "Single");
            ui.selectable_value(&mut self.display_mode, DisplayMode::Composite, "Composite");
        });
        let selection_changed = if let Some(dataset) = self.dataset.as_ref() {
            ui.weak(format!("{} indexed tile(s)", dataset.tile_count));
            ui.separator();
            let channel_changed = if self.display_mode == DisplayMode::Single {
                channel_selector(
                    ui,
                    &dataset.c,
                    &dataset.metadata_summary,
                    &mut self.selection.c,
                )
            } else {
                composite_channel_assignments(
                    ui,
                    &dataset.c,
                    &dataset.metadata_summary,
                    &mut self.channel_roles,
                )
            };
            channel_changed
                | scene_selector(ui, &dataset.s, &mut self.selection.scene)
                | selection_selector(ui, "Z", &dataset.z, &mut self.selection.z)
                | selection_selector(ui, "T", &dataset.t, &mut self.selection.t)
        } else {
            ui.label("No dataset is open.");
            false
        };
        if selection_changed || before_mode != self.display_mode {
            let changed = [
                before_selection.c != self.selection.c,
                before_selection.scene != self.selection.scene,
                before_selection.z != self.selection.z,
                before_selection.t != self.selection.t,
            ];
            let preserve_channel_fov = if changed.iter().any(|changed| *changed) {
                selection_change_preserves_fov(changed)
            } else {
                true
            };
            if let Some(dataset) = self.dataset.as_ref() {
                let previous_bounds = dataset
                    .plane(before_selection)
                    .map(|plane| plane.world_bounds);
                self.selection = dataset.repair_selection(self.selection, changed);
                let next_bounds = dataset
                    .plane(self.selection)
                    .map(|plane| plane.world_bounds);
                if preserve_channel_fov {
                    if let (Some(previous), Some(next)) = (previous_bounds, next_bounds) {
                        self.camera.rebase_bounds(previous, next);
                    }
                }
            }
            self.cache.clear();
            self.invalidate_view();
            if !preserve_channel_fov {
                self.fit_pending = true;
            }
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
        if let Some(scale) = self.pyramid_display.requested {
            ui.label(format!("Requested pyramid scale: {}×", format_scale(scale)));
        }
        if let Some(scale) = self.pyramid_display.displayed {
            ui.label(format!("Displayed pyramid scale: {}×", format_scale(scale)));
        }
        if let Some(dataset) = self.dataset.as_ref() {
            let (visible, resident, bytes) = self.active_cache_counts(self.generations.source);
            ui.label(format!(
                "Visible: {visible} · Resident: {resident} · Cache: {}",
                format_bytes(bytes)
            ));
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
    }

    fn show_metadata_inspector(&mut self, ui: &mut egui::Ui) {
        let Some(dataset) = self.dataset.as_ref() else {
            ui.label("Open a CZI to inspect metadata.");
            return;
        };
        let summary = &dataset.metadata_summary;
        ui.heading("Overview");
        metadata_overview_row(ui, "File", &dataset.source_label);
        metadata_overview_row(
            ui,
            "Document name",
            summary.name.as_deref().unwrap_or("Unavailable"),
        );
        metadata_overview_row(
            ui,
            "Acquired",
            summary.acquisition_date.as_deref().unwrap_or("Unavailable"),
        );
        metadata_overview_row(
            ui,
            "Objective",
            summary.objective.as_deref().unwrap_or("Unavailable"),
        );
        if let Some(pixel_size) = summary.pixel_size {
            metadata_overview_row(
                ui,
                "Pixel size",
                &format!("{:.6} × {:.6} µm", pixel_size.x_um, pixel_size.y_um),
            );
        } else {
            metadata_overview_row(ui, "Pixel size", "Unavailable (X/Y calibration not found)");
        }

        ui.add_space(6.0);
        ui.heading("Channels");
        if summary.channels.is_empty() {
            ui.weak("Unavailable (no named channels found)");
        } else {
            for channel in &summary.channels {
                let fluor = channel
                    .fluor
                    .as_deref()
                    .filter(|fluor| *fluor != channel.label)
                    .map_or_else(String::new, |fluor| format!(" · {fluor}"));
                ui.label(format!("C {} · {}{fluor}", channel.index, channel.label));
            }
        }
        if !dataset.metadata.diagnostics.is_empty() {
            ui.add_space(4.0);
            egui::CollapsingHeader::new("Some metadata details were unavailable")
                .id_salt("czi-metadata-diagnostics")
                .show(ui, |ui| {
                    for diagnostic in &dataset.metadata.diagnostics {
                        ui.weak(&diagnostic.message);
                    }
                });
        }
        ui.separator();
        ui.add(
            egui::TextEdit::singleline(&mut self.metadata_filter)
                .hint_text("Search metadata fields and values"),
        );
        let filter = self.metadata_filter.trim().to_ascii_lowercase();
        egui::ScrollArea::vertical().show(ui, |ui| {
            if let Some(root) = dataset.metadata.root.as_ref() {
                metadata_sections(ui, root, &filter);
            } else {
                ui.weak("No structured metadata is available.");
            }
            if let Some(raw_xml) = dataset.metadata.raw_xml.as_deref() {
                egui::CollapsingHeader::new("Raw XML")
                    .id_salt("czi-raw-metadata")
                    .show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(220.0)
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(raw_xml).monospace())
                                        .wrap(),
                                );
                            });
                    });
            }
        });
    }

    #[allow(clippy::too_many_lines)]
    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::AuthenticationStarted {
                profile,
                console,
                cancellation,
                connection_generation,
            } if self.generations.accepts_connection(connection_generation) => {
                self.retired_consoles.clear();
                self.embedded_authentication = Some(EmbeddedAuthentication {
                    profile,
                    console,
                    cancellation,
                    generation: connection_generation,
                    status: AuthenticationStatus::Connecting,
                });
                self.remote_browser_visibility = RemoteBrowserVisibility::Open;
                self.profile_editing = false;
                self.authentication_focus_request = Some(connection_generation);
                self.status = Status::normal("SSH authentication is waiting for terminal input.");
            }
            WorkerEvent::AuthenticationSucceeded {
                connection_generation,
            } if self.generations.accepts_connection(connection_generation) => {
                if let Some(authentication) = self.embedded_authentication.as_mut()
                    && authentication.generation == connection_generation
                {
                    authentication.status = AuthenticationStatus::Authenticated;
                    self.remote_session =
                        Some((authentication.profile.clone(), connection_generation));
                    self.authentication_focus_request = None;
                    self.status =
                        Status::normal("SSH authentication succeeded; opening SFTP session.");
                }
            }
            WorkerEvent::AuthenticationFailed {
                message,
                connection_generation,
            } if self.generations.accepts_connection(connection_generation) => {
                if let Some(authentication) = self.embedded_authentication.as_mut()
                    && authentication.generation == connection_generation
                {
                    authentication.status = AuthenticationStatus::Failed;
                }
                self.remote_session = None;
                self.remote_browser_visibility = RemoteBrowserVisibility::Open;
                self.authentication_focus_request = None;
                self.status = Status::error(message);
            }
            WorkerEvent::Opened {
                info,
                source_generation,
                connection_generation,
                remote,
            } if accepts_open_result(
                &self.generations,
                source_generation,
                connection_generation,
                remote,
            ) =>
            {
                self.handle_opened(*info, source_generation, remote);
            }
            WorkerEvent::OpenFailed {
                message,
                session_usable,
                source_generation,
                connection_generation,
                remote,
            } if accepts_open_result(
                &self.generations,
                source_generation,
                connection_generation,
                remote,
            ) =>
            {
                self.opening_origin = None;
                self.pending_open = None;
                if remote && requires_remote_reauthentication(session_usable) {
                    self.remote_session = None;
                    self.cancel_active_remote_connection();
                    self.retire_embedded_authentication();
                    self.generations.begin_connection();
                }
                self.status = Status::error(message);
            }
            WorkerEvent::RemotePaths {
                directory,
                suggestions,
                home,
                browse_generation,
                connection_generation,
            } if self.generations.accepts_connection(connection_generation) => {
                self.handle_remote_paths(directory, suggestions, home, browse_generation);
            }
            WorkerEvent::RemotePathsFailed {
                message,
                recoverable_remote_status,
                browse_generation,
                connection_generation,
            } if self.generations.accepts_connection(connection_generation) => self
                .handle_remote_paths_failed(message, recoverable_remote_status, browse_generation),
            WorkerEvent::TileLoaded {
                tile_id,
                plane,
                logical_rect,
                scale,
                paint_order,
                prefetch,
                tile,
                source_generation,
                view_generation,
            } if self
                .generations
                .accepts_view(source_generation, view_generation)
                && self
                    .active_planes()
                    .iter()
                    .any(|active| active.key() == plane) =>
            {
                self.pending_tiles.push(PendingTile {
                    tile_id,
                    plane,
                    logical_rect,
                    scale,
                    paint_order,
                    prefetch,
                    tile,
                    source_generation,
                    view_generation,
                });
            }
            WorkerEvent::ViewFinished {
                plane,
                scale,
                visible_tile_ids,
                source_generation,
                view_generation,
            } if self
                .generations
                .accepts_view(source_generation, view_generation)
                && self
                    .active_planes()
                    .iter()
                    .any(|active| active.key() == plane) =>
            {
                record_finished_plane(
                    &mut self.visible_tile_ids,
                    &mut self.pyramid_display,
                    view_generation,
                    plane,
                    scale,
                    visible_tile_ids,
                );
                self.cache
                    .finish_view(source_generation, plane, &self.visible_tile_ids[&plane]);
                let (visible, resident, bytes) = self.active_cache_counts(source_generation);
                self.status = Status::normal(format!(
                    "Requested {}× · Displayed {}× · {} visible · {} resident · {} cache",
                    format_scale(
                        self.pyramid_display
                            .requested
                            .expect("finished request scale")
                    ),
                    format_scale(
                        self.pyramid_display
                            .displayed
                            .expect("finished display scale")
                    ),
                    visible,
                    resident,
                    format_bytes(bytes)
                ));
            }
            WorkerEvent::ViewFailed {
                message,
                source_generation,
                view_generation,
            } if self
                .generations
                .accepts_view(source_generation, view_generation) =>
            {
                self.status = Status::error(message);
            }
            WorkerEvent::AuthenticationStarted { .. }
            | WorkerEvent::AuthenticationSucceeded { .. }
            | WorkerEvent::AuthenticationFailed { .. }
            | WorkerEvent::Opened { .. }
            | WorkerEvent::OpenFailed { .. }
            | WorkerEvent::RemotePaths { .. }
            | WorkerEvent::RemotePathsFailed { .. }
            | WorkerEvent::TileLoaded { .. }
            | WorkerEvent::ViewFinished { .. }
            | WorkerEvent::ViewFailed { .. } => {}
        }
    }

    fn handle_opened(&mut self, info: DatasetInfo, source_generation: u64, remote: bool) {
        if !self.generations.accepts_source(source_generation) {
            return;
        }
        self.selection = info.default_selection();
        self.channel_roles = default_channel_roles(&info.c, &info.metadata_summary);
        self.levels = Levels::default_for(info.pixel_type);
        self.status = Status::normal(format!(
            "Indexed {} tile(s); choose a plane or view the mosaic.",
            info.tile_count
        ));
        self.dataset = Some(info);
        self.opening_origin = None;
        self.pending_open = None;
        self.dataset_origin = Some(if remote {
            DatasetOrigin::Remote
        } else {
            DatasetOrigin::Local
        });
        self.cache.clear();
        self.pending_tiles.clear();
        self.visible_tile_ids.clear();
        self.fit_pending = true;
        self.invalidate_view();
    }

    fn handle_remote_paths(
        &mut self,
        directory: String,
        suggestions: Vec<RemotePathSuggestion>,
        _home: bool,
        browse_generation: u64,
    ) {
        if !self.generations.accepts_browse(browse_generation) {
            return;
        }
        self.remote_browser_path_input = directory_path(&directory);
        self.status = Status::normal(format!("Listed {} remote entries.", suggestions.len()));
        self.remote_browse_directory = Some(directory);
        self.remote_suggestions = suggestions;
        self.remote_selected_path = None;
        self.remote_browse_pending = false;
        self.remote_session = Some((
            self.ssh_profile_input.trim().to_owned(),
            self.generations.connection,
        ));
    }

    fn handle_remote_paths_failed(
        &mut self,
        message: String,
        session_usable: bool,
        browse_generation: u64,
    ) {
        if !self.generations.accepts_browse(browse_generation) {
            return;
        }
        self.status = Status::error(message);
        self.clear_remote_listing();
        if requires_remote_reauthentication(session_usable) {
            self.remote_session = None;
            self.cancel_active_remote_connection();
            self.retire_embedded_authentication();
            self.generations.begin_connection();
        }
        self.remote_browse_pending = false;
    }

    fn refresh_textures(&mut self, context: &egui::Context) {
        let pending = std::mem::take(&mut self.pending_tiles);
        for pending in pending {
            if !self
                .generations
                .accepts_view(pending.source_generation, pending.view_generation)
                || !self
                    .active_planes()
                    .iter()
                    .any(|active| active.key() == pending.plane)
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
            match texture_image(
                &pending.tile,
                self.levels,
                self.role_for_plane(pending.plane),
            ) {
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
                        !pending.prefetch,
                    );
                }
                Err(error) => self.status = Status::error(error),
            }
        }
    }

    fn poll_embedded_authentication(&mut self, context: &egui::Context) {
        let Some(authentication) = self.embedded_authentication.as_mut() else {
            return;
        };
        let snapshot = authentication.console.snapshot();
        if let Some(error) = snapshot.error {
            self.status = Status::error(error);
        }
        if authentication.status == AuthenticationStatus::Connecting {
            context.request_repaint_after(Duration::from_millis(16));
        }
    }

    fn show_embedded_authentication(&mut self, ui: &mut egui::Ui) {
        let Some(generation) = self
            .embedded_authentication
            .as_ref()
            .map(|authentication| authentication.generation)
        else {
            return;
        };
        let request_focus =
            take_authentication_focus_request(&mut self.authentication_focus_request, generation);
        let mut input_error = None;
        if let Some(authentication) = self.embedded_authentication.as_mut() {
            let connecting = authentication.status == AuthenticationStatus::Connecting;
            let transcript = authentication.console.snapshot().transcript;
            let (focused, inputs) = authentication_terminal(
                ui,
                authentication.generation,
                &transcript,
                connecting,
                request_focus,
            );
            ui.weak(if focused {
                "Keyboard input active. Authentication input is sent directly and never saved."
            } else {
                "Click to type. Passwords and one-time codes are never saved."
            });
            if connecting {
                for input in inputs {
                    if let Err(error) = authentication.console.try_send_input(input) {
                        input_error = Some(error);
                        break;
                    }
                }
            }
        }
        if let Some(error) = input_error {
            self.status = Status::error(error);
        }
    }

    #[allow(clippy::too_many_lines)]
    fn show_remote_browser(&mut self, ui: &mut egui::Ui) {
        let authentication_status = self
            .embedded_authentication
            .as_ref()
            .map(|authentication| authentication.status);
        let connected =
            remote_browser_connected(authentication_status, self.remote_session.is_some());
        let connecting = authentication_status == Some(AuthenticationStatus::Connecting);
        let failed = authentication_status == Some(AuthenticationStatus::Failed);
        let profile_ready = !self.ssh_profile_input.trim().is_empty();
        let profile_locked = !profile_is_editable(
            self.profile_editing,
            authentication_status,
            self.remote_session.is_some(),
        );

        ui.horizontal(|ui| {
            ui.heading("Remote inspector");
            let (badge, color) = if connecting {
                ("Connecting", egui::Color32::from_rgb(112, 180, 230))
            } else if connected {
                ("Connected", egui::Color32::from_rgb(112, 210, 154))
            } else if failed {
                ("Failed", egui::Color32::from_rgb(235, 120, 120))
            } else {
                ("Offline", egui::Color32::GRAY)
            };
            ui.colored_label(color, badge);
            if ui.button("Hide").clicked() {
                self.remote_browser_visibility = RemoteBrowserVisibility::Hidden;
            }
        });

        egui::Frame::group(ui.style())
            .fill(egui::Color32::from_gray(30))
            .show(ui, |ui| {
                ui.label("Connection");
                ui.horizontal(|ui| {
                    ui.label("Profile");
                    ui.add_enabled(
                        !profile_locked,
                        egui::TextEdit::singleline(&mut self.ssh_profile_input)
                            .hint_text("my-ssh-profile")
                            .desired_width(f32::INFINITY),
                    );
                });
                ui.horizontal(|ui| {
                    if profile_locked {
                        if ui.button("Change").clicked() {
                            self.begin_profile_change();
                        }
                    } else {
                        let action = if failed {
                            "Try again"
                        } else if connected {
                            "Reconnect"
                        } else {
                            "Connect"
                        };
                        if ui
                            .add_enabled(profile_ready && !connecting, egui::Button::new(action))
                            .clicked()
                        {
                            self.profile_editing = false;
                            if connected || self.embedded_authentication.is_some() {
                                self.reconnect_remote();
                            } else {
                                self.browse_remote_path(true);
                            }
                        }
                    }
                    if connecting {
                        ui.spinner();
                    }
                    ui.weak("Read-only SFTP. SSH input stays in the terminal below.");
                });
            });

        if connecting || failed {
            ui.add_space(4.0);
            egui::Frame::group(ui.style())
                .fill(egui::Color32::from_gray(24))
                .show(ui, |ui| {
                    ui.strong("Authentication");
                    self.show_embedded_authentication(ui);
                    ui.horizontal(|ui| {
                        if connecting {
                            if ui.button("Cancel").clicked() {
                                self.cancel_embedded_authentication();
                            }
                        } else if ui
                            .add_enabled(profile_ready, egui::Button::new("Try again"))
                            .clicked()
                        {
                            self.profile_editing = false;
                            self.reconnect_remote();
                        }
                    });
                });
            return;
        }

        if !connected {
            ui.add_space(8.0);
            ui.weak("Enter an SSH profile, then select Connect to browse remote CZI files.");
            return;
        }

        ui.separator();
        ui.label("Current directory");
        let directory = self
            .remote_browse_directory
            .as_deref()
            .unwrap_or("Not listed yet");
        ui.add(egui::Label::new(egui::RichText::new(directory).monospace()).selectable(true));
        let browse_enabled =
            remote_actions_enabled(connected, self.profile_editing, self.remote_browse_pending);

        ui.horizontal(|ui| {
            if ui
                .add_enabled(browse_enabled, egui::Button::new("Home"))
                .clicked()
            {
                self.remote_browser_path_input.clear();
                self.remote_selected_path = None;
                self.browse_remote_path(true);
            }
            if ui
                .add_enabled(
                    browse_enabled && self.remote_browse_directory.is_some(),
                    egui::Button::new("Up"),
                )
                .clicked()
            {
                self.browse_remote_parent();
            }
            if ui
                .add_enabled(browse_enabled, egui::Button::new("Refresh"))
                .clicked()
            {
                self.refresh_remote_browser();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Path:");
            let input_width = (ui.available_width() - 46.0).max(64.0);
            let path_response = ui.add(
                egui::TextEdit::singleline(&mut self.remote_browser_path_input)
                    .hint_text("/absolute/directory/")
                    .desired_width(input_width),
            );
            let go = ui
                .add_enabled_ui(browse_enabled, |ui| {
                    ui.add_sized([42.0, 22.0], egui::Button::new("Go"))
                })
                .inner
                .clicked()
                || (path_response.lost_focus()
                    && browse_enabled
                    && ui.input(|input| input.key_pressed(egui::Key::Enter)));
            if go {
                self.remote_selected_path = None;
                self.browse_remote_path(false);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Filter:");
            let input_width = ui.available_width().max(64.0);
            let filter_response = ui.add(
                egui::TextEdit::singleline(&mut self.remote_filename_filter)
                    .hint_text("filename")
                    .desired_width(input_width),
            );
            if filter_response.changed() {
                self.remote_selected_path = None;
            }
        });

        if self.remote_browse_pending {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Listing directory…");
            });
        }

        let visible =
            filter_remote_suggestions(&self.remote_suggestions, &self.remote_filename_filter);
        let mut selected_path = None;
        let mut action = None;
        ui.horizontal(|ui| {
            ui.weak("Name");
            ui.weak("Type · Size · Modified");
        });
        egui::ScrollArea::vertical()
            .id_salt("remote-file-list")
            .max_height((ui.available_height() - 54.0).max(120.0))
            .show(ui, |ui| {
                for suggestion in &visible {
                    ui.push_id(&suggestion.path, |ui| {
                        ui.horizontal(|ui| {
                            let selected = self.remote_selected_path.as_deref()
                                == Some(suggestion.path.as_str());
                            let label = match suggestion.kind {
                                RemotePathKind::Directory => format!("DIR  {}/", suggestion.name),
                                RemotePathKind::CziFile => format!("CZI  {}", suggestion.name),
                            };
                            let response = ui
                                .selectable_label(selected, label)
                                .on_hover_text(&suggestion.path);
                            if response.clicked() {
                                selected_path = Some(suggestion.path.clone());
                            }
                            if response.double_clicked() && browse_enabled {
                                action = remote_selection_action(suggestion, true);
                            }
                            ui.weak(match suggestion.kind {
                                RemotePathKind::Directory => "Folder".to_owned(),
                                RemotePathKind::CziFile => "CZI".to_owned(),
                            });
                            ui.weak(
                                suggestion
                                    .size
                                    .map_or_else(|| String::from("—"), format_byte_count),
                            );
                            ui.weak(format_remote_modified_time(suggestion.modified));
                        });
                    });
                }
                if visible.is_empty() {
                    ui.weak("No matching directories or .czi files.");
                }
            });
        if let Some(selected_path) = selected_path {
            self.remote_selected_path = Some(selected_path);
        }
        if let Some(action) = action {
            self.run_remote_selection_action(action);
        }
        let selected_czi = self
            .remote_selected_path
            .as_deref()
            .is_some_and(|selected_path| {
                self.remote_suggestions.iter().any(|suggestion| {
                    suggestion.path == selected_path && suggestion.kind == RemotePathKind::CziFile
                })
            });
        if ui
            .add_enabled(
                browse_enabled && selected_czi,
                egui::Button::new("Open selected CZI"),
            )
            .clicked()
        {
            self.open_selected_remote_czi();
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
            if ui
                .add_enabled(
                    self.snapshot_region.is_some()
                        && self.pending_snapshot.is_none()
                        && self.snapshot_writing.is_none(),
                    egui::Button::new("Save PNG"),
                )
                .clicked()
            {
                self.request_snapshot(ui.ctx());
            }
            ui.weak("Wheel: zoom at cursor · Drag: pan · logical world coordinates");
        });
        let (title_response, title_painter) =
            ui.allocate_painter(egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
        draw_canvas_title(
            &title_painter,
            title_response.rect,
            self.dataset.as_ref(),
            self.selection,
            self.display_mode,
            self.pyramid_display,
        );
        let (response, painter) = ui.allocate_painter(ui.available_size(), egui::Sense::drag());
        let snapshot_region = SnapshotRegion {
            rect: title_response.rect.union(response.rect),
            pixels_per_point: ui.ctx().pixels_per_point(),
        };
        self.snapshot_region = Some(snapshot_region);
        if let Some(request) = self.pending_snapshot.as_mut() {
            freeze_snapshot_region(request, snapshot_region, self.ui_frame);
        }
        self.process_returned_screenshots();
        painter.rect_filled(
            response.rect,
            0.0,
            if self.display_mode == DisplayMode::Composite {
                egui::Color32::BLACK
            } else {
                egui::Color32::from_gray(24)
            },
        );

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
        let pixel_size_um = dataset.metadata_summary.pixel_size.map(|size| size.x_um);
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

        let mut active = self.active_planes();
        active.sort_by_key(|plane| self.role_for_plane(plane.key()));
        let mut visible_keys = Vec::new();
        for plane in active {
            let role = self.role_for_plane(plane.key());
            for tile_id in self
                .visible_tile_ids
                .get(&plane.key())
                .into_iter()
                .flatten()
            {
                let key = TextureKey {
                    source_generation: self.generations.source,
                    plane: plane.key(),
                    tile_id: *tile_id,
                };
                self.cache.touch(key);
                visible_keys.push((role, key));
            }
        }
        let mut visible = visible_keys
            .iter()
            .filter_map(|(role, key)| {
                self.cache
                    .entries
                    .get(key)
                    .map(|entry| (*role, key.plane.c, entry.paint_order, entry))
            })
            .collect::<Vec<_>>();
        visible.sort_unstable_by_key(|(role, channel, paint_order, _)| {
            (*role, *channel, *paint_order)
        });
        let has_visible = !visible.is_empty();
        for (_, _, _, entry) in &visible {
            let image_rect = egui::Rect::from_min_max(
                self.camera.world_to_screen_xy(
                    (
                        entry.logical_rect.min_x as f64,
                        entry.logical_rect.min_y as f64,
                    ),
                    response.rect,
                    bounds,
                ),
                self.camera.world_to_screen_xy(
                    (
                        entry.logical_rect.max_x as f64,
                        entry.logical_rect.max_y as f64,
                    ),
                    response.rect,
                    bounds,
                ),
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
        draw_scale_bar(&painter, response.rect, self.camera.zoom, pixel_size_um);
    }
}

impl Drop for ViewerApp {
    fn drop(&mut self) {
        self.cancel_active_remote_connection();
        self.worker.shutdown();
    }
}

fn format_bytes(bytes: usize) -> String {
    format_byte_count(u64::try_from(bytes).unwrap_or(u64::MAX))
}

fn format_byte_count(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

fn format_remote_modified_time(modified: Option<u32>) -> String {
    let Some(modified) = modified else {
        return String::from("—");
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let age = now.saturating_sub(u64::from(modified));
    if age < 60 {
        String::from("just now")
    } else if age < 60 * 60 {
        format!("{} min ago", age / 60)
    } else if age < 24 * 60 * 60 {
        format!("{} h ago", age / (60 * 60))
    } else if age < 365 * 24 * 60 * 60 {
        format!("{} d ago", age / (24 * 60 * 60))
    } else {
        format!("{} y ago", age / (365 * 24 * 60 * 60))
    }
}

fn format_scale(scale: PyramidScale) -> String {
    if scale.denominator == 1 {
        format!("{}", scale.numerator)
    } else {
        format!("{}/{}", scale.numerator, scale.denominator)
    }
}

fn draw_canvas_title(
    painter: &egui::Painter,
    rect: egui::Rect,
    dataset: Option<&DatasetInfo>,
    selection: PlaneSelection,
    display_mode: DisplayMode,
    pyramid: PyramidDisplay,
) {
    painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(20, 24, 29));
    let text = dataset.map_or_else(
        || String::from("No CZI loaded"),
        |dataset| {
            let requested = pyramid
                .requested
                .map_or_else(|| String::from("—"), format_scale);
            let displayed = pyramid
                .displayed
                .map_or_else(|| String::from("—"), format_scale);
            let channel = if display_mode == DisplayMode::Composite {
                String::from("Composite")
            } else {
                channel_label(&dataset.metadata_summary, selection.c)
            };
            format!(
                "{}  ·  Scene {}  ·  {}  ·  Z {}  ·  T {}  ·  Requested {}×  ·  Displayed {}×",
                source_filename(&dataset.source_label),
                scene_label(selection.scene),
                channel,
                selection.z,
                selection.t,
                requested,
                displayed,
            )
        },
    );
    painter.text(
        rect.left_center() + egui::vec2(8.0, 0.0),
        egui::Align2::LEFT_CENTER,
        text,
        egui::FontId::proportional(13.0),
        egui::Color32::from_rgb(232, 236, 241),
    );
}

fn source_filename(source: &str) -> String {
    Path::new(source)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .map_or_else(
            || source.rsplit('/').next().unwrap_or(source).to_owned(),
            str::to_owned,
        )
}

#[derive(Clone, Debug, PartialEq)]
struct ScaleBar {
    points: f32,
    label: String,
}

fn scale_bar_spec_for_width(
    zoom: f64,
    pixel_size_um: Option<f64>,
    maximum_points: f64,
) -> Option<ScaleBar> {
    if !zoom.is_finite() || zoom <= 0.0 {
        return None;
    }
    if !maximum_points.is_finite() || maximum_points <= 0.0 {
        return None;
    }
    let (units_per_point, suffix) = match pixel_size_um {
        Some(size) if size.is_finite() && size > 0.0 => (size / zoom, "µm"),
        _ => (1.0 / zoom, "px"),
    };
    let target_units = maximum_points.min(100.0) * units_per_point;
    let length = nice_scale_length(target_units)?;
    let points = (length / units_per_point) as f32;
    (points.is_finite() && points > 0.0).then(|| ScaleBar {
        points,
        label: format!("{} {suffix}", format_scale_length(length)),
    })
}

fn nice_scale_length(target: f64) -> Option<f64> {
    if !target.is_finite() || target <= 0.0 {
        return None;
    }
    let base = 10.0_f64.powf(target.log10().floor());
    for factor in [5.0, 2.0, 1.0] {
        let candidate = factor * base;
        if candidate <= target * (1.0 + 1e-12) {
            return Some(candidate);
        }
    }
    Some(base / 10.0)
}

fn format_scale_length(length: f64) -> String {
    if length >= 1.0 {
        format!("{length:.0}")
    } else if length >= 0.1 {
        format!("{length:.1}")
    } else if length >= 0.01 {
        format!("{length:.2}")
    } else {
        format!("{length:.1e}")
    }
}

fn draw_scale_bar(
    painter: &egui::Painter,
    canvas: egui::Rect,
    zoom: f64,
    pixel_size_um: Option<f64>,
) {
    let available_points = f64::from((canvas.width() - 36.0).max(0.0));
    let Some(bar) = scale_bar_spec_for_width(zoom, pixel_size_um, available_points) else {
        return;
    };
    let start = egui::pos2(canvas.left() + 18.0, canvas.bottom() - 18.0);
    let end = start + egui::vec2(bar.points, 0.0);
    let black = egui::Stroke::new(5.0, egui::Color32::BLACK);
    let white = egui::Stroke::new(2.0, egui::Color32::WHITE);
    painter.line_segment([start, end], black);
    painter.line_segment([start, end], white);
    for point in [start, end] {
        painter.line_segment(
            [point - egui::vec2(0.0, 5.0), point + egui::vec2(0.0, 5.0)],
            black,
        );
        painter.line_segment(
            [point - egui::vec2(0.0, 5.0), point + egui::vec2(0.0, 5.0)],
            white,
        );
    }
    let text_position = start + egui::vec2(0.0, -8.0);
    painter.text(
        text_position + egui::vec2(1.0, 1.0),
        egui::Align2::LEFT_BOTTOM,
        &bar.label,
        egui::FontId::proportional(12.0),
        egui::Color32::BLACK,
    );
    painter.text(
        text_position,
        egui::Align2::LEFT_BOTTOM,
        bar.label,
        egui::FontId::proportional(12.0),
        egui::Color32::WHITE,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PixelCrop {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

fn take_matching_snapshot_request(
    request: &mut Option<SnapshotRequest>,
    generation: u64,
) -> Option<SnapshotRequest> {
    request
        .as_ref()
        .is_some_and(|request| request.generation == generation)
        .then(|| request.take())
        .flatten()
}

fn freeze_snapshot_region(
    request: &mut SnapshotRequest,
    region: SnapshotRegion,
    ui_frame: u64,
) -> bool {
    if request.region_frozen || ui_frame <= request.armed_frame {
        return false;
    }
    request.region = region;
    request.region_frozen = true;
    true
}

fn screenshot_crop_bounds(image_size: [usize; 2], region: SnapshotRegion) -> Option<PixelCrop> {
    let pixels_per_point = region.pixels_per_point;
    if !pixels_per_point.is_finite() || pixels_per_point <= 0.0 {
        return None;
    }
    let to_pixel = |point: f32| (point * pixels_per_point) as f64;
    let (min_x, min_y, max_x, max_y) = (
        to_pixel(region.rect.min.x),
        to_pixel(region.rect.min.y),
        to_pixel(region.rect.max.x),
        to_pixel(region.rect.max.y),
    );
    if !min_x.is_finite()
        || !min_y.is_finite()
        || !max_x.is_finite()
        || !max_y.is_finite()
        || min_x < 0.0
        || min_y < 0.0
    {
        return None;
    }
    let min_x = pixel_bound(min_x.floor());
    let min_y = pixel_bound(min_y.floor());
    let max_x = pixel_bound(max_x.ceil());
    let max_y = pixel_bound(max_y.ceil());
    if min_x > image_size[0]
        || min_y > image_size[1]
        || max_x > image_size[0]
        || max_y > image_size[1]
        || max_x <= min_x
        || max_y <= min_y
    {
        return None;
    }
    Some(PixelCrop {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    })
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn pixel_bound(value: f64) -> usize {
    if !value.is_finite() || value <= 0.0 {
        0
    } else if value >= usize::MAX as f64 {
        usize::MAX
    } else {
        value as usize
    }
}

fn crop_screenshot_rgba(image: &egui::ColorImage, crop: PixelCrop) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(
        crop.width
            .saturating_mul(crop.height)
            .saturating_mul(usize::from(4_u8)),
    );
    for y in crop.y..crop.y + crop.height {
        let row_start = y.saturating_mul(image.size[0]).saturating_add(crop.x);
        for pixel in &image.pixels[row_start..row_start + crop.width] {
            rgba.extend_from_slice(&pixel.to_srgba_unmultiplied());
        }
    }
    rgba
}

fn encode_png_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let width = u32::try_from(width).map_err(|_| String::from("PNG width is too large."))?;
    let height = u32::try_from(height).map_err(|_| String::from("PNG height is too large."))?;
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| String::from("PNG dimensions are too large."))?;
    if rgba.len() != expected {
        return Err(String::from(
            "PNG pixels did not match the crop dimensions.",
        ));
    }
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("Could not encode PNG header: {error}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|error| format!("Could not encode PNG pixels: {error}"))?;
    drop(writer);
    Ok(bytes)
}

fn write_png_snapshot(
    filename: &str,
    width: usize,
    height: usize,
    rgba: &[u8],
) -> Result<PathBuf, String> {
    let png = encode_png_rgba(width, height, rgba)?;
    let directory = snapshot_directory();
    let timestamp = unix_timestamp();
    for sequence in 0_u16..1_000 {
        let path = directory.join(snapshot_output_filename_with_sequence(
            filename, timestamp, sequence,
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!("Could not save PNG to {}: {error}", path.display()));
            }
        };
        if let Err(error) = std::io::Write::write_all(&mut file, &png) {
            let _ = std::fs::remove_file(&path);
            return Err(format!("Could not save PNG to {}: {error}", path.display()));
        }
        return Ok(path);
    }
    Err(String::from(
        "Could not choose an unused PNG snapshot filename.",
    ))
}

fn snapshot_directory() -> PathBuf {
    let desktop = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Desktop"));
    desktop
        .filter(|path| path.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

fn snapshot_output_filename_with_sequence(source: &str, timestamp: u64, sequence: u16) -> String {
    let suffix = if sequence > 0 {
        format!("-{sequence}")
    } else {
        String::new()
    };
    format!(
        "{}-{timestamp}{}.png",
        sanitize_snapshot_filename(source),
        suffix
    )
}

fn sanitize_snapshot_filename(source: &str) -> String {
    let stem = Path::new(source)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("czi-snapshot");
    let sanitized = stem
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(80)
        .collect::<String>();
    let sanitized = sanitized.trim_matches('_');
    if sanitized.is_empty() {
        String::from("czi-snapshot")
    } else {
        sanitized.to_owned()
    }
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn console_key_input(key: egui::Key, modifiers: egui::Modifiers) -> Option<Vec<u8>> {
    if modifiers.ctrl {
        let control = match key {
            egui::Key::A => 1,
            egui::Key::C => 3,
            egui::Key::D => 4,
            egui::Key::U => 21,
            egui::Key::W => 23,
            _ => return None,
        };
        return Some(vec![control]);
    }
    let sequence = match key {
        egui::Key::Enter => b"\r".as_slice(),
        egui::Key::Backspace => b"\x7f".as_slice(),
        egui::Key::Tab => b"\t".as_slice(),
        egui::Key::Escape => b"\x1b".as_slice(),
        egui::Key::ArrowUp => b"\x1b[A".as_slice(),
        egui::Key::ArrowDown => b"\x1b[B".as_slice(),
        egui::Key::ArrowRight => b"\x1b[C".as_slice(),
        egui::Key::ArrowLeft => b"\x1b[D".as_slice(),
        egui::Key::Home => b"\x1b[H".as_slice(),
        egui::Key::End => b"\x1b[F".as_slice(),
        egui::Key::Delete => b"\x1b[3~".as_slice(),
        egui::Key::PageUp => b"\x1b[5~".as_slice(),
        egui::Key::PageDown => b"\x1b[6~".as_slice(),
        _ => return None,
    };
    Some(sequence.to_vec())
}

fn console_event_input(event: &egui::Event) -> Option<Vec<u8>> {
    match event {
        egui::Event::Text(text) | egui::Event::Paste(text) if !text.is_empty() => {
            Some(text.as_bytes().to_vec())
        }
        egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } => console_key_input(*key, *modifiers),
        _ => None,
    }
}

fn authentication_terminal(
    ui: &mut egui::Ui,
    generation: u64,
    transcript: &str,
    connecting: bool,
    request_focus: bool,
) -> (bool, Vec<Vec<u8>>) {
    let terminal_id = ui.make_persistent_id(("ssh-auth-terminal", generation));
    let (_, rect) = ui.allocate_space(egui::vec2(ui.available_width(), 96.0));
    let response = ui.interact(rect, terminal_id, egui::Sense::click());
    if connecting && (request_focus || response.clicked()) {
        response.request_focus();
    }
    if !connecting {
        response.surrender_focus();
    }
    let focused = connecting && response.has_focus();
    let painter = ui.painter_at(rect);
    let background = egui::Color32::from_rgb(15, 18, 22);
    let accent = if focused {
        egui::Color32::from_rgb(90, 174, 220)
    } else {
        egui::Color32::from_gray(70)
    };
    painter.rect_filled(rect, 4.0, background);
    painter.rect_stroke(
        rect,
        4.0,
        egui::Stroke::new(if focused { 2.0 } else { 1.0 }, accent),
        egui::StrokeKind::Inside,
    );
    let text_rect = rect.shrink(8.0);
    let text = terminal_display_text(transcript);
    let galley = painter.layout(
        text,
        egui::FontId::monospace(12.0),
        egui::Color32::from_rgb(214, 221, 230),
        text_rect.width(),
    );
    painter.galley(
        text_rect.min,
        galley,
        egui::Color32::from_rgb(214, 221, 230),
    );
    let inputs = if focused {
        ui.ctx().input_mut(|input| {
            let mut inputs = Vec::new();
            input.events.retain(|event| {
                if let Some(bytes) = console_event_input(event) {
                    inputs.push(bytes);
                    false
                } else {
                    true
                }
            });
            inputs
        })
    } else {
        Vec::new()
    };
    (focused, inputs)
}

fn terminal_display_text(transcript: &str) -> String {
    const MAX_TERMINAL_DISPLAY_BYTES: usize = 4_096;

    if transcript.len() <= MAX_TERMINAL_DISPLAY_BYTES {
        return transcript.to_owned();
    }
    let mut start = transcript.len() - MAX_TERMINAL_DISPLAY_BYTES;
    while !transcript.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &transcript[start..])
}

fn take_authentication_focus_request(request: &mut Option<u64>, generation: u64) -> bool {
    if *request == Some(generation) {
        *request = None;
        true
    } else {
        false
    }
}

fn remote_browser_connected(
    authentication_status: Option<AuthenticationStatus>,
    has_remote_session: bool,
) -> bool {
    authentication_status == Some(AuthenticationStatus::Authenticated) || has_remote_session
}

fn remote_profile_is_locked(
    authentication_status: Option<AuthenticationStatus>,
    has_remote_session: bool,
) -> bool {
    matches!(
        authentication_status,
        Some(AuthenticationStatus::Connecting | AuthenticationStatus::Authenticated)
    ) || has_remote_session
}

fn profile_is_editable(
    profile_editing: bool,
    authentication_status: Option<AuthenticationStatus>,
    has_remote_session: bool,
) -> bool {
    profile_editing || !remote_profile_is_locked(authentication_status, has_remote_session)
}

fn remote_actions_enabled(connected: bool, profile_editing: bool, browse_pending: bool) -> bool {
    connected && !profile_editing && !browse_pending
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
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui_frame = self.ui_frame.wrapping_add(1);
        self.handle_screenshot_events(context);
        self.poll_snapshot_results();
        self.handle_dropped_files(context);
        self.handle_worker_events();
        self.poll_embedded_authentication(context);
        self.retry_pending_open();
        self.flush_pending_view();

        egui::TopBottomPanel::top("top_toolbar_v2")
            .exact_height(40.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    let local_mode =
                        ui.selectable_value(&mut self.open_mode, OpenMode::Local, "Local");
                    let ssh_mode = ui.selectable_value(&mut self.open_mode, OpenMode::Ssh, "SSH");
                    if local_mode.changed()
                        && self.open_mode == OpenMode::Local
                        && self
                            .embedded_authentication
                            .as_ref()
                            .is_some_and(|authentication| {
                                authentication.status == AuthenticationStatus::Connecting
                            })
                    {
                        self.cancel_embedded_authentication();
                    }
                    if ssh_mode.changed() && self.open_mode == OpenMode::Ssh {
                        self.remote_browser_visibility = RemoteBrowserVisibility::Open;
                    }
                    ui.separator();
                    match self.open_mode {
                        OpenMode::Local => {
                            ui.label("CZI");
                            let field_width = (ui.available_width() - 146.0).max(120.0);
                            let response = ui.add_sized(
                                [field_width, 24.0],
                                egui::TextEdit::singleline(&mut self.path_input)
                                    .hint_text("/path/to/image.czi"),
                            );
                            let open = ui.button("Open").clicked()
                                || (response.lost_focus()
                                    && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                            if open {
                                self.open_local_path();
                            }
                        }
                        OpenMode::Ssh => {
                            let browser_label = if self.remote_browser_visibility.is_open() {
                                "Remote"
                            } else {
                                "Show remote"
                            };
                            if ui.button(browser_label).clicked() {
                                self.remote_browser_visibility.toggle();
                            }
                            if let Some(path) = self.remote_selected_path.as_deref() {
                                ui.weak(format!("Selected: {path}"));
                            }
                        }
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let color = if self.status.is_error {
                            egui::Color32::LIGHT_RED
                        } else {
                            egui::Color32::LIGHT_GRAY
                        };
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&self.status.message).color(color),
                            )
                            .truncate(),
                        );
                    });
                });
            });

        if self.open_mode == OpenMode::Ssh && self.remote_browser_visibility.is_open() {
            egui::SidePanel::right("remote_inspector_v2")
                .resizable(true)
                .default_width(360.0)
                .min_width(320.0)
                .max_width(420.0)
                .show(context, |ui| self.show_remote_browser(ui));
        }

        egui::SidePanel::left("dataset_panel_v2")
            .resizable(true)
            .default_width(260.0)
            .min_width(220.0)
            .max_width(320.0)
            .show(context, |ui| {
                ui.heading("Dataset");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.inspector_tab, InspectorTab::Display, "Display");
                    ui.selectable_value(
                        &mut self.inspector_tab,
                        InspectorTab::Metadata,
                        "Metadata",
                    );
                });
                ui.separator();
                match self.inspector_tab {
                    InspectorTab::Display => self.show_display_inspector(ui),
                    InspectorTab::Metadata => self.show_metadata_inspector(ui),
                }
            });

        self.refresh_textures(context);
        egui::CentralPanel::default().show(context, |ui| {
            self.show_canvas(ui);
        });

        context.request_repaint_after(Duration::from_millis(100));
    }
}

fn selection_change_preserves_fov(changed: [bool; 4]) -> bool {
    changed == [true, false, false, false]
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

fn channel_selector(
    ui: &mut egui::Ui,
    choices: &DimensionChoices,
    summary: &MetadataSummary,
    selected: &mut i32,
) -> bool {
    if !choices.present {
        ui.horizontal(|ui| {
            ui.label("C");
            ui.weak("not present (0)");
        });
        return false;
    }
    let label = |channel| channel_label(summary, channel);
    let before = *selected;
    egui::ComboBox::from_label("C")
        .selected_text(label(*selected))
        .show_ui(ui, |ui| {
            for value in &choices.values {
                ui.selectable_value(selected, *value, label(*value));
            }
        });
    *selected != before
}

fn composite_channel_assignments(
    ui: &mut egui::Ui,
    choices: &DimensionChoices,
    summary: &MetadataSummary,
    roles: &mut HashMap<i32, ChannelRole>,
) -> bool {
    let mut changed = false;
    ui.heading("Channel assignments");
    for channel in &choices.values {
        ui.horizontal(|ui| {
            ui.label(channel_label(summary, *channel));
            let role = roles.entry(*channel).or_default();
            egui::ComboBox::from_id_salt(("channel-role", channel))
                .selected_text(role.label())
                .show_ui(ui, |ui| {
                    for choice in ChannelRole::ALL {
                        changed |= ui.selectable_value(role, choice, choice.label()).changed();
                    }
                });
        });
    }
    changed
}

fn channel_label(summary: &MetadataSummary, channel: i32) -> String {
    summary
        .channels
        .iter()
        .find(|metadata| metadata.index == channel)
        .map_or_else(
            || format!("C {channel} · Channel {channel}"),
            |metadata| format!("C {channel} · {}", metadata.label),
        )
}

fn metadata_tree(ui: &mut egui::Ui, node: &czi_core::MetadataNode, filter: &str, depth: usize) {
    if !metadata_matches(node, filter) {
        return;
    }
    let text = (!node.text.is_empty()).then(|| format!(" = {}", value_preview(&node.text, 72)));
    let label = text.map_or_else(
        || node.name.clone(),
        |text| format!("{}{}", node.name, text),
    );
    ui.push_id((depth, &node.name), |ui| {
        match metadata_node_presentation(filter) {
            MetadataNodePresentation::LazyCollapse => {
                egui::CollapsingHeader::new(label)
                    .default_open(depth < 2)
                    .show(ui, |ui| metadata_node_contents(ui, node, filter, depth));
            }
            MetadataNodePresentation::ExpandedSearch => {
                ui.strong(label);
                ui.indent("matching-metadata", |ui| {
                    metadata_node_contents(ui, node, filter, depth);
                });
            }
        }
    });
}

fn metadata_node_contents(
    ui: &mut egui::Ui,
    node: &czi_core::MetadataNode,
    filter: &str,
    depth: usize,
) {
    if !node.text.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.weak("Value");
            ui.add(egui::Label::new(egui::RichText::new(&node.text).monospace()).wrap());
        });
    }
    for attribute in &node.attributes {
        ui.horizontal_wrapped(|ui| {
            ui.weak(&attribute.name);
            ui.add(egui::Label::new(egui::RichText::new(&attribute.value).monospace()).wrap());
        });
    }
    for (index, child) in node.children.iter().enumerate() {
        ui.push_id(index, |ui| metadata_tree(ui, child, filter, depth + 1));
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataNodePresentation {
    LazyCollapse,
    ExpandedSearch,
}

fn metadata_node_presentation(filter: &str) -> MetadataNodePresentation {
    if filter.is_empty() {
        MetadataNodePresentation::LazyCollapse
    } else {
        MetadataNodePresentation::ExpandedSearch
    }
}

fn metadata_sections(ui: &mut egui::Ui, root: &czi_core::MetadataNode, filter: &str) {
    let Some(metadata) = root
        .children
        .iter()
        .find(|node| node.name.eq_ignore_ascii_case("metadata"))
    else {
        metadata_tree(ui, root, filter, 0);
        return;
    };
    let useful = ["Information", "Scaling", "DisplaySetting"];
    for name in useful {
        for node in metadata
            .children
            .iter()
            .filter(|node| node.name.eq_ignore_ascii_case(name))
        {
            if metadata_matches(node, filter) {
                metadata_tree(ui, node, filter, 0);
            }
        }
    }
    let vendor_matches = metadata.children.iter().any(|node| {
        !useful
            .iter()
            .any(|name| node.name.eq_ignore_ascii_case(name))
            && metadata_matches(node, filter)
    });
    let show_vendor_nodes = |ui: &mut egui::Ui| {
        for node in &metadata.children {
            if !useful
                .iter()
                .any(|name| node.name.eq_ignore_ascii_case(name))
            {
                metadata_tree(ui, node, filter, 1);
            }
        }
    };
    match vendor_details_presentation(filter, vendor_matches) {
        VendorDetailsPresentation::Hidden => {}
        VendorDetailsPresentation::Collapsed => {
            egui::CollapsingHeader::new("Vendor details")
                .id_salt("czi-vendor-metadata")
                .show(ui, show_vendor_nodes);
        }
        VendorDetailsPresentation::SearchResults => {
            ui.strong("Vendor details");
            show_vendor_nodes(ui);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VendorDetailsPresentation {
    Hidden,
    Collapsed,
    SearchResults,
}

fn vendor_details_presentation(filter: &str, has_matches: bool) -> VendorDetailsPresentation {
    match (filter.is_empty(), has_matches) {
        (_, false) => VendorDetailsPresentation::Hidden,
        (true, true) => VendorDetailsPresentation::Collapsed,
        (false, true) => VendorDetailsPresentation::SearchResults,
    }
}

fn metadata_overview_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.weak(label);
        ui.label(value);
    });
}

fn value_preview(value: &str, maximum_chars: usize) -> String {
    let mut characters = value.chars();
    let preview = characters.by_ref().take(maximum_chars).collect::<String>();
    if characters.next().is_some() {
        format!("{preview}…")
    } else {
        preview
    }
}

fn metadata_matches(node: &czi_core::MetadataNode, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    node.name.to_ascii_lowercase().contains(filter)
        || node.text.to_ascii_lowercase().contains(filter)
        || node.attributes.iter().any(|attribute| {
            attribute.name.to_ascii_lowercase().contains(filter)
                || attribute.value.to_ascii_lowercase().contains(filter)
        })
        || node
            .children
            .iter()
            .any(|child| metadata_matches(child, filter))
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
    use czi_core::{ChannelMetadata, CompressionMode, DimensionEntry, DirectoryEntry, PyramidType};

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
    fn metadata_preview_is_width_bounded_and_unicode_safe() {
        assert_eq!(value_preview("short", 8), "short");
        assert_eq!(value_preview("αβγδε", 3), "αβγ…");
    }

    #[test]
    fn metadata_search_matches_nested_values_and_attributes() {
        let node = czi_core::MetadataNode {
            name: String::from("Information"),
            attributes: Vec::new(),
            text: String::new(),
            children: vec![czi_core::MetadataNode {
                name: String::from("Channel"),
                attributes: vec![czi_core::MetadataAttribute {
                    name: String::from("Name"),
                    value: String::from("Alexa Fluor 405"),
                }],
                text: String::new(),
                children: Vec::new(),
            }],
        };
        assert!(metadata_matches(&node, "fluor"));
        assert!(!metadata_matches(&node, "hardware"));
    }

    #[test]
    fn metadata_nodes_render_expanded_for_searches_and_lazy_without_one() {
        assert_eq!(
            metadata_node_presentation(""),
            MetadataNodePresentation::LazyCollapse
        );
        assert_eq!(
            metadata_node_presentation("fluor"),
            MetadataNodePresentation::ExpandedSearch
        );
    }

    #[test]
    fn vendor_details_render_directly_for_searches_and_collapsed_without_one() {
        assert_eq!(
            vendor_details_presentation("", true),
            VendorDetailsPresentation::Collapsed
        );
        assert_eq!(
            vendor_details_presentation("channel", true),
            VendorDetailsPresentation::SearchResults
        );
        assert_eq!(
            vendor_details_presentation("hardware", false),
            VendorDetailsPresentation::Hidden
        );
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
    fn channel_role_defaults_use_labels_and_sparse_channel_values() {
        let choices = DimensionChoices {
            present: true,
            values: vec![2, 7, 11, 19, 23, 29],
        };
        let summary = MetadataSummary {
            channels: vec![
                ChannelMetadata {
                    index: 2,
                    id: None,
                    label: String::from("Phase Contrast"),
                },
                ChannelMetadata {
                    index: 7,
                    id: None,
                    label: String::from("HADA"),
                },
                ChannelMetadata {
                    index: 11,
                    id: None,
                    label: String::from("BODIPY 493/503"),
                },
                ChannelMetadata {
                    index: 23,
                    id: None,
                    label: String::from("AF405 (fluor Alexa Fluor 405)"),
                },
                ChannelMetadata {
                    index: 29,
                    id: None,
                    label: String::from("Bod493 (fluor BODIPY FL)"),
                },
            ],
            pixel_size: None,
        };
        let roles = default_channel_roles(&choices, &summary);
        assert_eq!(roles[&2], ChannelRole::Gray);
        assert_eq!(roles[&7], ChannelRole::Blue);
        assert_eq!(roles[&11], ChannelRole::Green);
        assert_eq!(roles[&19], ChannelRole::Off);
        assert_eq!(roles[&23], ChannelRole::Blue);
        assert_eq!(roles[&29], ChannelRole::Green);

        let fallback = default_channel_roles(&choices, &MetadataSummary::default());
        assert_eq!(fallback[&2], ChannelRole::Gray);
        assert!(
            choices.values[1..]
                .iter()
                .all(|channel| fallback[channel] == ChannelRole::Off)
        );
    }

    #[test]
    fn paper_blend_uses_gray_base_and_ordered_source_over_colors() {
        let gray = blend_channel([0; 3], 64, ChannelRole::Gray);
        assert_eq!(gray, [64; 3]);
        let red = blend_channel(gray, 128, ChannelRole::Red);
        assert_eq!(red, [159, 31, 31]);
        let green = blend_channel(red, 128, ChannelRole::Green);
        assert_eq!(green, [79, 143, 15]);
        assert_eq!(blend_channel([0; 3], 200, ChannelRole::Blue), [0, 0, 200]);
        assert_eq!(blend_channel(gray, 255, ChannelRole::Off), gray);
    }

    #[test]
    fn role_textures_keep_one_tile_allocation_and_expected_alpha() {
        let tile = DecodedTile {
            width: 2,
            height: 1,
            pixels: DecodedPixels::Gray8(vec![0, 255]),
        };
        let image = texture_image(
            &tile,
            Levels::default_for(PixelType::Gray8),
            ChannelRole::Blue,
        )
        .expect("blue texture");
        assert_eq!(image.size, [2, 1]);
        assert_eq!(image.pixels[0], egui::Color32::TRANSPARENT);
        assert_eq!(image.pixels[1], egui::Color32::BLUE);
    }

    #[test]
    fn remote_browser_generations_drop_stale_results() {
        let mut generations = Generations::default();
        let first = generations.begin_browse();
        assert!(generations.accepts_browse(first));
        let second = generations.begin_browse();
        assert!(!generations.accepts_browse(first));
        assert!(generations.accepts_browse(second));
    }

    #[test]
    fn connection_generations_drop_stale_authentication_events() {
        let mut generations = Generations::default();
        let first = generations.begin_connection();
        assert!(generations.accepts_connection(first));
        let second = generations.begin_connection();
        assert!(!generations.accepts_connection(first));
        assert!(generations.accepts_connection(second));
    }

    #[test]
    fn authentication_focus_request_is_generation_scoped_and_single_use() {
        let mut request = Some(7);
        assert!(!take_authentication_focus_request(&mut request, 6));
        assert_eq!(request, Some(7));
        assert!(take_authentication_focus_request(&mut request, 7));
        assert_eq!(request, None);
        assert!(!take_authentication_focus_request(&mut request, 7));
    }

    #[test]
    fn profile_editing_and_remote_actions_are_explicitly_gated() {
        assert!(remote_profile_is_locked(
            Some(AuthenticationStatus::Connecting),
            false
        ));
        assert!(remote_profile_is_locked(
            Some(AuthenticationStatus::Authenticated),
            false
        ));
        assert!(!profile_is_editable(
            false,
            Some(AuthenticationStatus::Authenticated),
            false
        ));
        assert!(profile_is_editable(
            true,
            Some(AuthenticationStatus::Authenticated),
            false
        ));
        assert!(!remote_profile_is_locked(
            Some(AuthenticationStatus::Failed),
            false
        ));
        assert!(remote_browser_connected(
            Some(AuthenticationStatus::Authenticated),
            false
        ));
        assert!(remote_browser_connected(None, true));
        assert!(!remote_browser_connected(
            Some(AuthenticationStatus::Connecting),
            false
        ));
        assert!(!remote_browser_connected(None, false));
        assert!(remote_actions_enabled(true, false, false));
        assert!(!remote_actions_enabled(true, true, false));
        assert!(!remote_actions_enabled(true, false, true));
        assert!(!remote_actions_enabled(false, false, false));
    }

    #[test]
    fn usable_remote_file_failures_do_not_require_reauthentication() {
        let content_error = remote_session_failure("invalid CZI bytes", true);
        let transport_error = remote_session_failure("SFTP READ failed", false);
        assert!(content_error.session_usable);
        assert!(!requires_remote_reauthentication(
            content_error.session_usable
        ));
        assert!(requires_remote_reauthentication(
            transport_error.session_usable
        ));
    }

    #[test]
    fn local_open_results_ignore_ssh_generation_changes() {
        let mut generations = Generations::default();
        let source = generations.begin_source();
        let remote_connection = generations.begin_connection();
        let _new_connection = generations.begin_connection();
        assert!(accepts_open_result(
            &generations,
            source,
            remote_connection,
            false,
        ));
        assert!(!accepts_open_result(
            &generations,
            source,
            remote_connection,
            true,
        ));
    }

    #[test]
    fn canceled_connection_generation_stays_sticky_for_late_worker_commands() {
        let slot = EmbeddedCancellationSlot::default();
        slot.cancel(4);
        slot.clear(4);
        let state = slot.inner.lock().expect("cancellation state");
        assert_eq!(state.cancelled_through, Some(4));
        assert!(
            state
                .cancelled_through
                .is_some_and(|cancelled_through| 4 <= cancelled_through)
        );
        assert!(
            state
                .cancelled_through
                .is_none_or(|cancelled_through| 5 > cancelled_through)
        );
    }

    fn test_view_request(view_generation: u64) -> ViewRequest {
        ViewRequest {
            source_generation: 1,
            view_generation,
            planes: vec![PlaneSelector::default()],
            viewport: SpatialRect::new(0, 0, 1, 1).expect("unit viewport"),
            prefetch_viewport: SpatialRect::new(0, 0, 1, 1).expect("unit prefetch viewport"),
            target_downsample: 1.0,
            resident_tile_ids: Vec::new(),
        }
    }

    #[test]
    fn browse_interleaved_with_views_requeues_the_latest_view() {
        let (sender, receiver) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let current = test_view_request(1);
        sender
            .send(WorkerCommand::View(test_view_request(2)))
            .expect("queue newer view");
        sender
            .send(WorkerCommand::ClearBrowse)
            .expect("queue browse clear");

        let ViewInterruption { command, resume } =
            take_newer_command(&receiver, &current).expect("interrupted view");
        assert!(matches!(command, WorkerCommand::ClearBrowse));
        assert_eq!(
            resume
                .expect("newest view must be requeued")
                .view_generation,
            2
        );
    }

    #[test]
    fn saturated_view_queue_keeps_only_the_latest_pending_view_without_an_error() {
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(WorkerCommand::View(test_view_request(1)))
            .expect("saturate command queue");
        let mut second = test_view_request(2);
        second.viewport = SpatialRect::new(2, 0, 3, 1).expect("second viewport");
        let mut third = test_view_request(3);
        third.viewport = SpatialRect::new(3, 0, 4, 1).expect("third viewport");
        let key = |request: &ViewRequest| {
            (
                request.planes.iter().map(|plane| plane.key()).collect(),
                request.viewport,
                request.target_downsample.to_bits(),
            )
        };
        let mut pending = None;
        let second_key = key(&second);
        assert!(
            record_view_submission(&mut pending, enqueue_view(&sender, second), second_key,)
                .is_ok()
        );
        assert_eq!(
            pending.as_ref().map(|(request, _)| request.view_generation),
            Some(2)
        );
        let third_key = key(&third);

        assert!(matches!(
            receiver.recv().expect("queued first view"),
            WorkerCommand::View(ViewRequest {
                view_generation: 1,
                ..
            })
        ));
        assert!(
            record_view_submission(&mut pending, enqueue_view(&sender, third), third_key,).is_ok()
        );
        assert!(
            pending.is_none(),
            "a submitted newest view clears stale pending work"
        );
        assert!(matches!(
            receiver.recv().expect("latest view eventually submits"),
            WorkerCommand::View(ViewRequest {
                view_generation: 3,
                ..
            })
        ));
    }

    #[test]
    fn composite_request_and_cache_identity_include_sparse_plane_sets() {
        let mut request = test_view_request(1);
        request.planes = vec![
            PlaneSelector::new(2, SceneId::Implicit, 0, 0),
            PlaneSelector::new(9, SceneId::Implicit, 0, 0),
        ];
        let key = (
            request
                .planes
                .iter()
                .map(|plane| plane.key())
                .collect::<Vec<_>>(),
            request.viewport,
            request.target_downsample.to_bits(),
        );
        assert_ne!(key.0[0], key.0[1]);
        assert_ne!(
            TextureKey {
                source_generation: 1,
                plane: key.0[0],
                tile_id: TileId(4),
            },
            TextureKey {
                source_generation: 1,
                plane: key.0[1],
                tile_id: TileId(4),
            }
        );
        assert!(cache_key_is_active(
            TextureKey {
                source_generation: 1,
                plane: key.0[1],
                tile_id: TileId(4),
            },
            1,
            &key.0,
        ));
        assert!(!cache_key_is_active(
            TextureKey {
                source_generation: 2,
                plane: key.0[1],
                tile_id: TileId(4),
            },
            1,
            &key.0,
        ));

        let mut replacement = request.clone();
        replacement.view_generation = 2;
        replacement.planes.pop();
        let mut pending = None;
        replace_pending_view(&mut pending, request, key);
        let replacement_key = (
            replacement.planes.iter().map(|plane| plane.key()).collect(),
            replacement.viewport,
            replacement.target_downsample.to_bits(),
        );
        replace_pending_view(&mut pending, replacement, replacement_key.clone());
        assert_eq!(pending.as_ref().map(|(_, key)| key), Some(&replacement_key));
    }

    #[test]
    fn prefetch_viewport_expands_and_clamps_to_plane_bounds() {
        let bounds = SpatialRect::new(0, 0, 1_000, 1_000).expect("bounds");
        let viewport = SpatialRect::new(900, 900, 1_000, 1_000).expect("viewport");
        assert_eq!(
            prefetch_viewport(viewport, bounds),
            SpatialRect::new(888, 888, 1_000, 1_000).expect("clamped prefetch")
        );
        let outside = SpatialRect::new(-50, 200, 50, 300).expect("outside viewport");
        assert_eq!(
            prefetch_viewport(outside, bounds),
            SpatialRect::new(0, 188, 56, 312).expect("left-clamped prefetch")
        );
    }

    #[test]
    fn adaptive_pyramid_requests_finest_scale_and_retains_fallback_until_completion() {
        let one = PyramidScale::new(1, 1).expect("one scale");
        let two = PyramidScale::new(2, 1).expect("two scale");
        let four = PyramidScale::new(4, 1).expect("four scale");
        let scales = [one, two, four];
        assert_eq!(select_requested_scale(&scales, 4.0), Some(four));
        assert_eq!(select_requested_scale(&scales, 2.0), Some(two));
        assert_eq!(select_requested_scale(&scales, 0.01), Some(one));

        let mut display = PyramidDisplay::default();
        display.request(4, four);
        assert!(display.finish(4, four));
        display.request(5, two);
        assert_eq!(display.requested, Some(two));
        assert_eq!(display.displayed, Some(four));
        assert!(!display.finish(4, four));
        assert!(display.finish(5, two));
        display.request(6, one);
        assert_eq!(display.displayed, Some(two));
        assert!(display.finish(6, one));
        assert_eq!(display.displayed, Some(one));
    }

    #[test]
    fn unequal_channel_scale_still_records_plane_completion() {
        let one = PyramidScale::new(1, 1).expect("one scale");
        let two = PyramidScale::new(2, 1).expect("two scale");
        let plane = PlaneKey::new(7, SceneId::Implicit, 0, 0);
        let mut display = PyramidDisplay::default();
        display.request(9, one);
        let mut visible = HashMap::new();

        record_finished_plane(
            &mut visible,
            &mut display,
            9,
            plane,
            two,
            vec![TileId(3), TileId(8)],
        );

        assert_eq!(visible[&plane], vec![TileId(3), TileId(8)]);
        assert_eq!(display.requested, Some(one));
        assert_eq!(display.displayed, None);
    }

    #[test]
    fn scale_bar_uses_nice_physical_lengths_and_pixel_fallback() {
        assert_eq!(
            scale_bar_spec_for_width(2.0, Some(0.5), 100.0),
            Some(ScaleBar {
                points: 80.0,
                label: String::from("20 µm"),
            })
        );
        assert_eq!(
            scale_bar_spec_for_width(0.5, None, 100.0),
            Some(ScaleBar {
                points: 100.0,
                label: String::from("200 px"),
            })
        );
        assert_eq!(nice_scale_length(0.37), Some(0.2));
        assert_eq!(
            scale_bar_spec_for_width(2.0, Some(0.5), 30.0),
            Some(ScaleBar {
                points: 20.0,
                label: String::from("5 µm"),
            })
        );
        assert_eq!(
            scale_bar_spec_for_width(1_000_000.0, None, 100.0),
            Some(ScaleBar {
                points: 100.0,
                label: String::from("1.0e-4 px"),
            })
        );
        assert_eq!(scale_bar_spec_for_width(0.0, Some(0.5), 100.0), None);
    }

    #[test]
    fn screenshot_crop_handles_one_and_two_pixel_displays() {
        let rect = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(110.0, 70.0));
        assert_eq!(
            screenshot_crop_bounds(
                [160, 100],
                SnapshotRegion {
                    rect,
                    pixels_per_point: 1.0,
                }
            ),
            Some(PixelCrop {
                x: 10,
                y: 20,
                width: 100,
                height: 50,
            })
        );
        assert_eq!(
            screenshot_crop_bounds(
                [320, 200],
                SnapshotRegion {
                    rect,
                    pixels_per_point: 2.0,
                }
            ),
            Some(PixelCrop {
                x: 20,
                y: 40,
                width: 200,
                height: 100,
            })
        );
        assert_eq!(
            screenshot_crop_bounds(
                [100, 100],
                SnapshotRegion {
                    rect: egui::Rect::from_min_max(egui::pos2(90.0, 0.0), egui::pos2(110.0, 10.0),),
                    pixels_per_point: 1.0,
                }
            ),
            None,
            "a mismatched screenshot is rejected instead of silently clamped"
        );
    }

    #[test]
    fn snapshot_filename_encoder_and_stale_request_are_safe() {
        assert_eq!(
            snapshot_output_filename_with_sequence("My slide?.czi", 42, 0),
            "My_slide-42.png"
        );
        let png = encode_png_rgba(1, 1, &[255, 0, 0, 255]).expect("encode png");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(encode_png_rgba(2, 1, &[255, 0, 0, 255]).is_err());

        let mut request = Some(SnapshotRequest {
            generation: 2,
            region: SnapshotRegion {
                rect: egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1.0, 1.0)),
                pixels_per_point: 1.0,
            },
            armed_frame: 1,
            region_frozen: false,
            filename: String::from("test"),
        });
        assert!(take_matching_snapshot_request(&mut request, 1).is_none());
        assert!(request.is_some());
        let capture_region = SnapshotRegion {
            rect: egui::Rect::from_min_size(egui::pos2(2.0, 3.0), egui::vec2(4.0, 5.0)),
            pixels_per_point: 2.0,
        };
        assert!(!freeze_snapshot_region(
            request.as_mut().expect("pending request"),
            capture_region,
            1,
        ));
        assert!(freeze_snapshot_region(
            request.as_mut().expect("pending request"),
            capture_region,
            2,
        ));
        assert!(!freeze_snapshot_region(
            request.as_mut().expect("frozen request"),
            SnapshotRegion {
                rect: egui::Rect::NOTHING,
                pixels_per_point: 1.0,
            },
            3,
        ));
        assert_eq!(
            take_matching_snapshot_request(&mut request, 2)
                .expect("matching screenshot request")
                .generation,
            2
        );
    }

    #[test]
    fn console_control_keys_use_terminal_bytes_without_input_state() {
        assert_eq!(
            console_key_input(egui::Key::Enter, egui::Modifiers::default()),
            Some(vec![b'\r'])
        );
        assert_eq!(
            console_key_input(egui::Key::Backspace, egui::Modifiers::default()),
            Some(vec![0x7f])
        );
        assert_eq!(
            console_key_input(egui::Key::ArrowUp, egui::Modifiers::default()),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            console_key_input(
                egui::Key::C,
                egui::Modifiers {
                    ctrl: true,
                    ..Default::default()
                }
            ),
            Some(vec![3])
        );
        assert_eq!(
            console_key_input(egui::Key::F1, egui::Modifiers::default()),
            None
        );
        assert_eq!(
            console_event_input(&egui::Event::Paste(String::from("one-time-code"))),
            Some(b"one-time-code".to_vec())
        );
        assert_eq!(
            console_event_input(&egui::Event::Text(String::from("p"))),
            Some(vec![b'p'])
        );
        assert_eq!(console_event_input(&egui::Event::Copy), None);
    }

    #[test]
    fn remote_browser_splits_parent_prefix_and_home_targets() {
        assert_eq!(
            remote_browse_target("", false),
            Ok(RemoteBrowseTarget::Home)
        );
        assert_eq!(
            remote_browse_target("/ignored", true),
            Ok(RemoteBrowseTarget::Home)
        );
        assert_eq!(
            remote_browse_target("/data/images/", false),
            Ok(RemoteBrowseTarget::Directory {
                path: String::from("/data/images"),
                prefix: String::new(),
            })
        );
        assert_eq!(
            remote_browse_target("/data/images/sample", false),
            Ok(RemoteBrowseTarget::Directory {
                path: String::from("/data/images"),
                prefix: String::from("sample"),
            })
        );
        assert_eq!(
            remote_browse_target("sample", false),
            Err(String::from("remote browser path must be absolute"))
        );
    }

    #[test]
    fn remote_browser_parent_filter_and_selection_actions_are_pure() {
        let directory = RemotePathSuggestion {
            name: String::from("images"),
            path: String::from("/data/images"),
            kind: RemotePathKind::Directory,
            size: None,
            modified: Some(1),
        };
        let file = RemotePathSuggestion {
            name: String::from("sample.czi"),
            path: String::from("/data/sample.czi"),
            kind: RemotePathKind::CziFile,
            size: Some(1_572_864),
            modified: Some(1),
        };
        assert_eq!(remote_parent_path("/"), "/");
        assert_eq!(remote_parent_path("/data"), "/");
        assert_eq!(remote_parent_path("/data/images"), "/data");
        assert_eq!(
            filter_remote_suggestions(&[directory.clone(), file.clone()], "SAMPLE"),
            vec![file.clone()]
        );
        assert_eq!(remote_selection_action(&directory, false), None);
        assert_eq!(
            remote_selection_action(&directory, true),
            Some(RemoteSelectionAction::BrowseDirectory(String::from(
                "/data/images/"
            )))
        );
        assert_eq!(
            remote_selection_action(&file, true),
            Some(RemoteSelectionAction::OpenCzi(String::from(
                "/data/sample.czi"
            )))
        );
        assert_eq!(format_byte_count(1_572_864), "1.5 MiB");
    }

    #[test]
    fn authenticated_remote_browse_reuses_only_the_matching_session() {
        assert!(reuses_remote_connection(
            "lab-czi",
            7,
            Some(("lab-czi", AuthenticationStatus::Authenticated)),
            None,
        ));
        assert!(reuses_remote_connection(
            "lab-czi",
            7,
            None,
            Some(("lab-czi", 7)),
        ));
        assert!(!reuses_remote_connection(
            "lab-czi",
            8,
            None,
            Some(("lab-czi", 7)),
        ));
        assert!(!reuses_remote_connection(
            "lab-czi",
            7,
            Some(("other-host", AuthenticationStatus::Authenticated)),
            Some(("other-host", 7)),
        ));
        assert!(!reuses_remote_connection(
            "lab-czi",
            7,
            Some(("lab-czi", AuthenticationStatus::Failed)),
            None,
        ));
    }

    #[test]
    fn worker_session_reuse_requires_matching_profile_config_and_generation() {
        let config = OpenSshConfig::new();
        assert!(matches_worker_remote_session(
            RemoteSessionKey {
                profile: "lab-czi",
                config: &config,
                generation: 7,
            },
            RemoteSessionKey {
                profile: "lab-czi",
                config: &config,
                generation: 7,
            },
        ));
        assert!(!matches_worker_remote_session(
            RemoteSessionKey {
                profile: "lab-czi",
                config: &config,
                generation: 7,
            },
            RemoteSessionKey {
                profile: "lab-czi",
                config: &config,
                generation: 8,
            },
        ));
    }

    #[test]
    fn remote_browser_filters_sorts_and_bounds_safe_suggestions() {
        fn entry(name: &str, permissions: Option<u32>) -> RemoteDirEntry {
            RemoteDirEntry {
                path: SftpLocation::new(name).expect("test path"),
                long_name: String::new(),
                attributes: czi_ssh::SftpAttributes {
                    permissions,
                    ..Default::default()
                },
            }
        }

        let suggestions = remote_path_suggestions(
            "/data",
            "",
            vec![
                entry("zeta.czi", Some(S_IFREG | 0o644)),
                entry("folder.czi", Some(S_IFDIR | 0o755)),
                entry("beta.CZI", None),
                entry("not-a-czi.txt", Some(S_IFREG | 0o644)),
                entry("socket.czi", Some(0o140_000)),
                entry(".", Some(S_IFDIR | 0o755)),
                entry("..", Some(S_IFDIR | 0o755)),
                entry("nested/name.czi", Some(S_IFREG | 0o644)),
                entry("line\nbreak.czi", Some(S_IFREG | 0o644)),
            ],
        );
        assert_eq!(
            suggestions,
            vec![
                RemotePathSuggestion {
                    name: String::from("folder.czi"),
                    path: String::from("/data/folder.czi"),
                    kind: RemotePathKind::Directory,
                    size: None,
                    modified: None,
                },
                RemotePathSuggestion {
                    name: String::from("beta.CZI"),
                    path: String::from("/data/beta.CZI"),
                    kind: RemotePathKind::CziFile,
                    size: None,
                    modified: None,
                },
                RemotePathSuggestion {
                    name: String::from("zeta.czi"),
                    path: String::from("/data/zeta.czi"),
                    kind: RemotePathKind::CziFile,
                    size: None,
                    modified: None,
                },
            ]
        );

        let entries = (0..=MAX_REMOTE_SUGGESTIONS)
            .map(|index| entry(&format!("{index:03}.czi"), Some(S_IFREG | 0o644)))
            .collect();
        let bounded = remote_path_suggestions("/data", "", entries);
        assert_eq!(bounded.len(), MAX_REMOTE_SUGGESTIONS);
        assert_eq!(
            bounded.first().map(|entry| entry.name.as_str()),
            Some("000.czi")
        );
        assert_eq!(
            bounded.last().map(|entry| entry.name.as_str()),
            Some("199.czi")
        );
    }

    #[test]
    fn remote_browse_paths_never_enter_openssh_argv() {
        let remote_path = "/data/; never-an-ssh-argument.czi";
        let profile = SshProfile::new("research-cluster").expect("profile");
        let argv = OpenSshConfig::new()
            .sftp_argv(&profile)
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert!(!argv.iter().any(|argument| argument == remote_path));
    }

    #[test]
    fn worker_shutdown_joins_its_thread() {
        let mut worker = DatasetWorker::spawn();
        worker.shutdown();
        assert!(worker.join.is_none());
    }

    #[test]
    fn worker_shutdown_disconnects_a_saturated_event_channel() {
        let (commands, command_rx) = mpsc::sync_channel(1);
        let (event_tx, events) = mpsc::sync_channel(1);
        event_tx
            .send(WorkerEvent::ViewFailed {
                message: String::from("fill the event channel"),
                source_generation: 0,
                view_generation: 0,
            })
            .expect("fill bounded event channel");
        let embedded_cancellation = EmbeddedCancellationSlot::default();
        let worker_cancellation = embedded_cancellation.clone();
        let join = thread::spawn(move || {
            worker_loop(&command_rx, &event_tx, &worker_cancellation);
        });
        commands
            .send(WorkerCommand::Open {
                locator: DatasetLocator::Local(PathBuf::from("/missing.czi")),
                source_generation: 1,
                connection_generation: 0,
            })
            .expect("queue blocked event producer");
        let mut worker = DatasetWorker {
            commands: Some(commands),
            events: Some(events),
            embedded_cancellation,
            join: Some(join),
        };
        worker.shutdown();
        assert!(worker.join.is_none());
    }

    #[test]
    fn remote_locator_validates_profile_location_and_absolute_path() {
        let config = OpenSshConfig::new();
        let valid = DatasetLocator::Remote {
            profile: String::from("research-cluster"),
            path: String::from("/data/image.czi"),
            config: config.clone(),
        };
        assert!(valid.remote_parts().is_ok());

        let invalid_profile = DatasetLocator::Remote {
            profile: String::from("-oProxyCommand=bad"),
            path: String::from("/data/image.czi"),
            config: config.clone(),
        };
        assert!(
            invalid_profile
                .remote_parts()
                .is_err_and(|error| error.contains("must not begin"))
        );

        let invalid_location = DatasetLocator::Remote {
            profile: String::from("research-cluster"),
            path: String::from("/data\0image.czi"),
            config: config.clone(),
        };
        assert!(
            invalid_location
                .remote_parts()
                .is_err_and(|error| error.contains("must not contain NUL"))
        );

        let relative_location = DatasetLocator::Remote {
            profile: String::from("research-cluster"),
            path: String::from("data/image.czi"),
            config,
        };
        assert_eq!(
            relative_location
                .remote_parts()
                .expect_err("relative remote path"),
            "remote CZI path must be absolute"
        );
    }

    #[test]
    fn worker_preserves_events_from_a_bounded_command_burst() {
        let mut worker = DatasetWorker::spawn();
        let count = u64::try_from(CHANNEL_CAPACITY + 1).expect("channel capacity fits u64");
        for source_generation in 1..=count {
            worker
                .commands
                .as_ref()
                .expect("command sender")
                .send(WorkerCommand::Open {
                    locator: DatasetLocator::Local(PathBuf::from(format!(
                        "/dev/null/czi-viewer-missing-{source_generation}.czi"
                    ))),
                    source_generation,
                    connection_generation: 0,
                })
                .expect("bounded command burst");
        }

        let mut observed = Vec::new();
        for _ in 0..count {
            match worker
                .events
                .as_ref()
                .expect("event receiver")
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
    fn worker_event_batch_is_bounded_per_ui_update() {
        let (sender, events) = mpsc::sync_channel(CHANNEL_CAPACITY);
        for source_generation in 0..CHANNEL_CAPACITY {
            sender
                .send(WorkerEvent::OpenFailed {
                    message: source_generation.to_string(),
                    session_usable: false,
                    source_generation: u64::try_from(source_generation).expect("generation"),
                    connection_generation: 0,
                    remote: false,
                })
                .expect("event channel capacity");
        }
        assert_eq!(take_worker_event_batch(&events).len(), CHANNEL_CAPACITY);
        sender
            .send(WorkerEvent::OpenFailed {
                message: String::from("next"),
                session_usable: false,
                source_generation: 0,
                connection_generation: 0,
                remote: false,
            })
            .expect("event channel capacity");
        assert_eq!(take_worker_event_batch(&events).len(), 1);
    }

    #[test]
    fn camera_world_round_trips_negative_coordinates_and_cursor_zoom() {
        let canvas = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let bounds = SpatialRect::new(-100, -50, 100, 50).unwrap();
        let mut camera = Camera::default();
        camera.fit(canvas, bounds);
        let world = egui::pos2(-37.0, 21.0);
        let screen =
            camera.world_to_screen_xy((f64::from(world.x), f64::from(world.y)), canvas, bounds);
        let (round_trip_x, round_trip_y) = camera.screen_to_world_xy(screen, canvas, bounds);
        assert!((round_trip_x - f64::from(world.x)).abs() < 0.001);
        assert!((round_trip_y - f64::from(world.y)).abs() < 0.001);
        let cursor = egui::pos2(150.0, 80.0);
        let before = camera.screen_to_world_xy(cursor, canvas, bounds);
        camera.zoom_at(cursor, 1.5, canvas, bounds);
        let after = camera.screen_to_world_xy(cursor, canvas, bounds);
        assert!((before.0 - after.0).abs() < 0.001);
        assert!((before.1 - after.1).abs() < 0.001);
        camera.one_to_one();
        assert_eq!(camera, Camera::default());
    }

    #[test]
    fn channel_change_preserves_camera_fov_across_plane_bounds() {
        assert!(selection_change_preserves_fov([true, false, false, false]));
        assert!(!selection_change_preserves_fov([false, true, false, false]));
        assert!(!selection_change_preserves_fov([false, false, true, false]));
        assert!(!selection_change_preserves_fov([false, false, false, true]));
        assert!(!selection_change_preserves_fov([true, false, true, false]));

        let canvas = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let previous = SpatialRect::new(-100, -50, 100, 50).unwrap();
        let next = SpatialRect::new(300, 150, 700, 450).unwrap();
        let mut camera = Camera {
            zoom: 3.25,
            pan: egui::vec2(47.0, -19.0),
        };
        let original_camera = camera;
        let center_before = camera.screen_to_world_xy(canvas.center(), canvas, previous);

        camera.rebase_bounds(previous, next);

        let center_after = camera.screen_to_world_xy(canvas.center(), canvas, next);
        assert!((center_before.0 - center_after.0).abs() < 0.001);
        assert!((center_before.1 - center_after.1).abs() < 0.001);
        assert!((camera.zoom - original_camera.zoom).abs() < f64::EPSILON);

        camera.rebase_bounds(next, previous);
        assert!((camera.pan.x - original_camera.pan.x).abs() < f32::EPSILON);
        assert!((camera.pan.y - original_camera.pan.y).abs() < f32::EPSILON);
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
            metadata_diagnostics: Vec::new(),
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
