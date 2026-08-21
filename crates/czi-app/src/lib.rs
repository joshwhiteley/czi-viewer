#![allow(
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
)]

mod bridge;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use czi_core::{
    BlockCache, CziDataset, DecodedPixels, DecodedTile, DimensionCode, LocalFileSource,
    PhysicalSize, PixelType, PlaneInfo, PlaneKey, PlaneSelector, PyramidScale, SceneId,
    SpatialRect, TileHit, TileId, TileIndex, TileQueryIndex, ViewQuery,
};
use czi_ssh::{
    BridgeCancellation, ControlPath, OpenSshConfig, RemoteDirEntry, SftpLocation, SftpSession,
    SftpSource, SshProfile,
};
use eframe::egui;

use bridge::BRIDGE_MODE;

const CHANNEL_CAPACITY: usize = 8;
const METADATA_PREVIEW_CHARS: usize = 4_096;
const TEXTURE_CACHE_LIMIT: usize = 256 * 1024 * 1024;
const MAX_REMOTE_SUGGESTIONS: usize = 200;
const MAX_REMOTE_DIRECTORY_ENTRIES: usize = 4_096;
const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFREG: u32 = 0o100_000;

/// Run the hidden interactive SFTP bridge when requested by its exact CLI mode.
///
/// Returns `Ok(false)` for a normal GUI invocation.
///
/// # Errors
///
/// Returns malformed bridge-argument or local bridge I/O errors.
pub fn run_interactive_sftp_bridge_if_requested() -> Result<bool, Box<dyn std::error::Error>> {
    bridge::run_if_requested()
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

    fn open_failure(&self, error: impl std::fmt::Display) -> OpenFailure {
        match self {
            Self::Local(_) => OpenFailure {
                message: sanitize_error(error),
                terminal_bootstrap_command: None,
            },
            Self::Remote {
                profile, config, ..
            } => remote_failure(profile, config, error),
        }
    }
}

struct OpenFailure {
    message: String,
    terminal_bootstrap_command: Option<String>,
}

fn validation_failure(error: impl std::fmt::Display) -> OpenFailure {
    OpenFailure {
        message: sanitize_error(error),
        terminal_bootstrap_command: None,
    }
}

fn remote_failure(
    profile: &str,
    config: &OpenSshConfig,
    error: impl std::fmt::Display,
) -> OpenFailure {
    let terminal_bootstrap_command = SshProfile::new(profile.to_owned())
        .ok()
        .and_then(|profile| {
            std::env::current_exe().ok().and_then(|executable| {
                config
                    .terminal_bridge_command(&executable, BRIDGE_MODE, &profile)
                    .ok()
            })
        });
    OpenFailure {
        message: sanitize_error(error),
        terminal_bootstrap_command,
    }
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
            })
        })
        .collect::<Vec<_>>();
    suggestions.sort_by(|left, right| left.name.cmp(&right.name));
    suggestions.truncate(MAX_REMOTE_SUGGESTIONS);
    suggestions
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct DatasetInfo {
    source_label: String,
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
    fn from_dataset(source_label: String, dataset: &CziDataset, query: &TileQueryIndex) -> Self {
        let tiles = &dataset.index().tiles;
        let metadata_preview = dataset.index().metadata.as_ref().map_or_else(
            || String::from("No global metadata XML."),
            |metadata| metadata.xml.chars().take(METADATA_PREVIEW_CHARS).collect(),
        );
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
            metadata_preview,
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
        locator: DatasetLocator,
        source_generation: u64,
    },
    Browse {
        profile: String,
        path: String,
        home: bool,
        config: OpenSshConfig,
        browse_generation: u64,
    },
    ClearBrowse,
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
        terminal_bootstrap_command: Option<String>,
        source_generation: u64,
    },
    RemotePaths {
        directory: String,
        suggestions: Vec<RemotePathSuggestion>,
        home: bool,
        browse_generation: u64,
    },
    RemotePathsFailed {
        message: String,
        terminal_bootstrap_command: Option<String>,
        browse_generation: u64,
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

struct WorkerBrowseSession {
    profile: String,
    config: OpenSshConfig,
    session: SftpSession,
}

struct DatasetWorker {
    commands: SyncSender<WorkerCommand>,
    events: Receiver<WorkerEvent>,
    bridge_cancellation: BridgeCancellation,
    join: Option<JoinHandle<()>>,
}

impl DatasetWorker {
    fn spawn() -> Self {
        let (commands, command_rx) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let (event_tx, events) = mpsc::sync_channel(CHANNEL_CAPACITY);
        let bridge_cancellation = BridgeCancellation::default();
        let worker_cancellation = bridge_cancellation.clone();
        let join = thread::Builder::new()
            .name(String::from("czi-dataset-worker"))
            .spawn(move || worker_loop(&command_rx, &event_tx, &worker_cancellation))
            .expect("start CZI dataset worker");
        Self {
            commands,
            events,
            bridge_cancellation,
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
        self.bridge_cancellation.cancel();
        let mut sent = self.commands.try_send(WorkerCommand::Shutdown).is_ok();
        while self.events.try_recv().is_ok() {}
        if !sent {
            sent = self.commands.send(WorkerCommand::Shutdown).is_ok();
        }
        if sent {
            while self.events.try_recv().is_ok() {}
        }
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

fn worker_loop(
    commands: &Receiver<WorkerCommand>,
    events: &SyncSender<WorkerEvent>,
    bridge_cancellation: &BridgeCancellation,
) {
    let mut dataset = None;
    let mut browse_session = None;
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
                locator,
                source_generation,
            } => {
                active_source_generation = source_generation;
                let (next_dataset, sent) = send_open_result(
                    events,
                    open_dataset(locator, &mut browse_session, bridge_cancellation),
                    source_generation,
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
            } => {
                let result = browse_remote_paths(
                    &profile,
                    &path,
                    home,
                    &config,
                    &mut browse_session,
                    bridge_cancellation,
                );
                if !send_remote_browse_result(events, result, browse_generation) {
                    break;
                }
            }
            WorkerCommand::ClearBrowse => browse_session = None,
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

fn send_open_result(
    events: &SyncSender<WorkerEvent>,
    result: Result<(CziDataset, TileQueryIndex, DatasetInfo), OpenFailure>,
    source_generation: u64,
) -> (Option<WorkerDataset>, bool) {
    match result {
        Ok((dataset, query, info)) => {
            let sent = events
                .send(WorkerEvent::Opened {
                    info,
                    source_generation,
                })
                .is_ok();
            (sent.then_some(WorkerDataset { dataset, query }), sent)
        }
        Err(OpenFailure {
            message,
            terminal_bootstrap_command,
        }) => (
            None,
            events
                .send(WorkerEvent::OpenFailed {
                    message,
                    terminal_bootstrap_command,
                    source_generation,
                })
                .is_ok(),
        ),
    }
}

fn send_remote_browse_result(
    events: &SyncSender<WorkerEvent>,
    result: Result<RemoteBrowseResult, OpenFailure>,
    browse_generation: u64,
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
            })
            .is_ok(),
        Err(OpenFailure {
            message,
            terminal_bootstrap_command,
        }) => events
            .send(WorkerEvent::RemotePathsFailed {
                message,
                terminal_bootstrap_command,
                browse_generation,
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
    bridge_cancellation: &BridgeCancellation,
) -> Result<RemoteBrowseResult, OpenFailure> {
    let profile = SshProfile::new(profile.to_owned()).map_err(validation_failure)?;
    let target = remote_browse_target(path, home).map_err(validation_failure)?;
    let target = match target {
        RemoteBrowseTarget::Home => None,
        RemoteBrowseTarget::Directory { path, prefix } => {
            Some((SftpLocation::new(path).map_err(validation_failure)?, prefix))
        }
    };
    let matches_existing = browse_session
        .as_ref()
        .is_some_and(|existing| existing.profile == profile.as_str() && existing.config == *config);
    if !matches_existing {
        *browse_session = None;
        let session =
            SftpSession::connect_preferred_with_cancellation(&profile, config, bridge_cancellation)
                .map_err(|error| remote_failure(profile.as_str(), config, error))?;
        *browse_session = Some(WorkerBrowseSession {
            profile: profile.as_str().to_owned(),
            config: config.clone(),
            session,
        });
    }
    let result = browse_with_session(
        &mut browse_session
            .as_mut()
            .expect("browse session was created or matched")
            .session,
        target,
        &profile,
        config,
    );
    if result.is_err() {
        *browse_session = None;
    }
    result
}

fn browse_with_session(
    session: &mut SftpSession,
    target: Option<(SftpLocation, String)>,
    profile: &SshProfile,
    config: &OpenSshConfig,
) -> Result<RemoteBrowseResult, OpenFailure> {
    let (directory, prefix, home) = match target {
        None => {
            let current_directory =
                SftpLocation::new(".").expect("the fixed SFTP current-directory location is valid");
            let home = session
                .realpath(&current_directory)
                .map_err(|error| remote_failure(profile.as_str(), config, error))?;
            (home, String::new(), true)
        }
        Some((directory, prefix)) => (directory, prefix, false),
    };
    let entries = session
        .read_dir_limited(&directory, MAX_REMOTE_DIRECTORY_ENTRIES)
        .map_err(|error| remote_failure(profile.as_str(), config, error))?;
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
    bridge_cancellation: &BridgeCancellation,
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
                    terminal_bootstrap_command: None,
                })?;
            finish_open(source_label, opened).map_err(|error| OpenFailure {
                message: sanitize_error(error),
                terminal_bootstrap_command: None,
            })
        }
        remote @ DatasetLocator::Remote { .. } => {
            let source_label = remote.display_label();
            let (profile, location, config) = remote
                .remote_parts()
                .map_err(|error| remote.open_failure(error))?;
            let source = if browse_session.as_ref().is_some_and(|existing| {
                existing.profile == profile.as_str() && existing.config == *config
            }) {
                let session = browse_session
                    .take()
                    .expect("matching browse session is present")
                    .session;
                SftpSource::open_with_session(session, &location)
            } else {
                *browse_session = None;
                SftpSession::connect_preferred_with_cancellation(
                    &profile,
                    config,
                    bridge_cancellation,
                )
                .and_then(|session| SftpSource::open_with_session(session, &location))
            }
            .map_err(|error| remote.open_failure(error))?;
            let cache =
                BlockCache::with_defaults(source).map_err(|error| remote.open_failure(error))?;
            let opened = CziDataset::open(cache).map_err(|error| remote.open_failure(error))?;
            finish_open(source_label, opened).map_err(|error| remote.open_failure(error))
        }
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

fn take_newer_command(commands: &Receiver<WorkerCommand>) -> Option<WorkerCommand> {
    let mut latest_view = None;
    loop {
        match commands.try_recv() {
            Ok(WorkerCommand::View(request)) => latest_view = Some(WorkerCommand::View(request)),
            Ok(
                command @ (WorkerCommand::Open { .. }
                | WorkerCommand::Browse { .. }
                | WorkerCommand::ClearBrowse
                | WorkerCommand::Shutdown),
            ) => {
                return Some(command);
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return latest_view,
        }
    }
}

fn process_view(
    commands: &Receiver<WorkerCommand>,
    events: &SyncSender<WorkerEvent>,
    opened: &WorkerDataset,
    request: &ViewRequest,
) -> Option<WorkerCommand> {
    if let Some(newer) = take_newer_command(commands) {
        return Some(newer);
    }
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
        if let Some(newer) = take_newer_command(commands) {
            return Some(newer);
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum OpenMode {
    #[default]
    Local,
    Ssh,
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

    fn begin_view(&mut self, source_generation: u64, plane: PlaneKey) -> Vec<TileId> {
        self.protected = self
            .entries
            .keys()
            .filter(|key| key.source_generation == source_generation && key.plane == plane)
            .copied()
            .collect();
        for entry in self.entries.values_mut() {
            entry.visible = false;
        }
        self.evict_non_visible();
        self.protected.iter().map(|key| key.tile_id).collect()
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
        self.protected.clear();
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

    fn current_counts(&self, source_generation: u64, plane: PlaneKey) -> (usize, usize, usize) {
        let entries = self
            .entries
            .iter()
            .filter(|(key, _)| key.source_generation == source_generation && key.plane == plane);
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
    tile: DecodedTile,
    source_generation: u64,
    view_generation: u64,
}

/// The local and SSH CZI mosaic viewer.
pub struct ViewerApp {
    worker: DatasetWorker,
    open_mode: OpenMode,
    path_input: String,
    ssh_profile_input: String,
    remote_path_input: String,
    ssh_config: Option<OpenSshConfig>,
    remote_browse_directory: Option<String>,
    remote_suggestions: Vec<RemotePathSuggestion>,
    remote_browse_pending: bool,
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
    pending_open: Option<(DatasetLocator, u64)>,
    terminal_bootstrap_command: Option<String>,
}

impl ViewerApp {
    /// Create the viewer state and its dedicated dataset worker.
    #[must_use]
    pub fn new(_creation_context: &eframe::CreationContext<'_>) -> Self {
        let initial_path = std::env::args_os().nth(1).map(PathBuf::from);
        let mut app = Self {
            worker: DatasetWorker::spawn(),
            open_mode: OpenMode::Local,
            path_input: initial_path
                .as_ref()
                .map_or_else(String::new, |path| path.display().to_string()),
            ssh_profile_input: String::new(),
            remote_path_input: String::new(),
            ssh_config: None,
            remote_browse_directory: None,
            remote_suggestions: Vec::new(),
            remote_browse_pending: false,
            dataset: None,
            selection: PlaneSelection::default(),
            generations: Generations::default(),
            status: Status::normal("Choose Local or SSH, then open a .czi file."),
            cache: TextureCache::new(TEXTURE_CACHE_LIMIT),
            pending_tiles: Vec::new(),
            visible_tile_ids: Vec::new(),
            selected_scale: None,
            levels: Levels::default_for(PixelType::Gray16),
            camera: Camera::default(),
            fit_pending: false,
            last_request: None,
            pending_open: None,
            terminal_bootstrap_command: None,
        };
        if initial_path.is_some() {
            app.open_local_path();
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

    fn open_local_path(&mut self) {
        let path = PathBuf::from(self.path_input.trim());
        if self.path_input.trim().is_empty() {
            self.status = Status::error("Enter a local .czi path first.");
            return;
        }
        self.open_locator(DatasetLocator::Local(path));
    }

    fn open_remote_path(&mut self) {
        let config = match self.ssh_config() {
            Ok(config) => config,
            Err(error) => {
                self.status = Status::error(error);
                return;
            }
        };
        self.open_locator(DatasetLocator::Remote {
            profile: self.ssh_profile_input.trim().to_owned(),
            path: self.remote_path_input.trim().to_owned(),
            config,
        });
    }

    fn ssh_config(&mut self) -> Result<OpenSshConfig, String> {
        if let Some(config) = &self.ssh_config {
            return Ok(config.clone());
        }
        let control_path = ControlPath::create_private().map_err(sanitize_error)?;
        let config = OpenSshConfig::new().with_control_path(control_path);
        self.ssh_config = Some(config.clone());
        Ok(config)
    }

    fn invalidate_remote_browse(&mut self) {
        self.generations.begin_browse();
        self.remote_browse_directory = None;
        self.remote_suggestions.clear();
        self.remote_browse_pending = false;
        self.terminal_bootstrap_command = None;
        let _ = self.worker.send(WorkerCommand::ClearBrowse);
    }

    fn browse_remote_path(&mut self, home: bool) {
        let config = match self.ssh_config() {
            Ok(config) => config,
            Err(error) => {
                self.status = Status::error(error);
                return;
            }
        };
        let browse_generation = self.generations.begin_browse();
        self.remote_browse_directory = None;
        self.remote_suggestions.clear();
        self.remote_browse_pending = true;
        self.terminal_bootstrap_command = None;
        self.status = Status::normal(if home {
            String::from("Finding remote home directory…")
        } else {
            String::from("Listing remote paths…")
        });
        if let Err(error) = self.worker.send(WorkerCommand::Browse {
            profile: self.ssh_profile_input.trim().to_owned(),
            path: self.remote_path_input.trim().to_owned(),
            home,
            config,
            browse_generation,
        }) {
            self.remote_browse_pending = false;
            self.status = Status::error(error);
        }
    }

    fn select_remote_suggestion(&mut self, suggestion: RemotePathSuggestion) {
        match suggestion.kind {
            RemotePathKind::Directory => {
                self.remote_path_input = directory_path(&suggestion.path);
                self.browse_remote_path(false);
            }
            RemotePathKind::CziFile => {
                self.remote_path_input = suggestion.path;
                self.generations.begin_browse();
                self.remote_browse_directory = None;
                self.remote_suggestions.clear();
                self.remote_browse_pending = false;
            }
        }
    }

    fn open_locator(&mut self, locator: DatasetLocator) {
        let source_label = locator.display_label();
        let source_generation = self.generations.begin_source();
        self.dataset = None;
        self.cache.clear();
        self.pending_tiles.clear();
        self.visible_tile_ids.clear();
        self.selected_scale = None;
        self.last_request = None;
        self.fit_pending = false;
        self.terminal_bootstrap_command = None;
        self.status = Status::normal(format!("Opening {source_label}…"));
        let pending = (locator.clone(), source_generation);
        if let Err(error) = self.worker.send(WorkerCommand::Open {
            locator,
            source_generation,
        }) {
            self.pending_open = Some(pending);
            self.status = Status::error(error);
        } else {
            self.pending_open = None;
        }
    }

    fn retry_pending_open(&mut self) {
        let Some((locator, source_generation)) = self.pending_open.take() else {
            return;
        };
        if let Err(error) = self.worker.send(WorkerCommand::Open {
            locator: locator.clone(),
            source_generation,
        }) {
            self.pending_open = Some((locator, source_generation));
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
        let resident_tile_ids = self.cache.begin_view(self.generations.source, plane.key());
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
            self.open_mode = OpenMode::Local;
            self.path_input = path.display().to_string();
            self.open_local_path();
        }
    }

    fn handle_worker_events(&mut self) {
        for event in take_worker_event_batch(&self.worker.events) {
            self.handle_worker_event(event);
        }
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::Opened {
                info,
                source_generation,
            } => self.handle_opened(info, source_generation),
            WorkerEvent::OpenFailed {
                message,
                terminal_bootstrap_command,
                source_generation,
            } if self.generations.accepts_source(source_generation) => {
                self.status = Status::error(message);
                self.terminal_bootstrap_command = terminal_bootstrap_command;
            }
            WorkerEvent::RemotePaths {
                directory,
                suggestions,
                home,
                browse_generation,
            } => self.handle_remote_paths(directory, suggestions, home, browse_generation),
            WorkerEvent::RemotePathsFailed {
                message,
                terminal_bootstrap_command,
                browse_generation,
            } => self.handle_remote_paths_failed(
                message,
                terminal_bootstrap_command,
                browse_generation,
            ),
            WorkerEvent::TileLoaded {
                tile_id,
                plane,
                logical_rect,
                scale,
                paint_order,
                tile,
                source_generation,
                view_generation,
            } if self
                .generations
                .accepts_view(source_generation, view_generation)
                && plane == self.selection.key() =>
            {
                if !self.visible_tile_ids.contains(&tile_id) {
                    self.visible_tile_ids.push(tile_id);
                }
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
            WorkerEvent::ViewFinished {
                plane,
                scale,
                visible_tile_ids,
                source_generation,
                view_generation,
            } if self
                .generations
                .accepts_view(source_generation, view_generation)
                && plane == self.selection.key() =>
            {
                self.selected_scale = Some(scale);
                self.visible_tile_ids = visible_tile_ids;
                self.cache
                    .finish_view(source_generation, plane, &self.visible_tile_ids);
                let (visible, resident, bytes) =
                    self.cache.current_counts(source_generation, plane);
                self.status = Status::normal(format!(
                    "Scale {}× · {} visible · {} resident · {} cache",
                    format_scale(scale),
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
            WorkerEvent::OpenFailed { .. }
            | WorkerEvent::TileLoaded { .. }
            | WorkerEvent::ViewFinished { .. }
            | WorkerEvent::ViewFailed { .. } => {}
        }
    }

    fn handle_opened(&mut self, info: DatasetInfo, source_generation: u64) {
        if !self.generations.accepts_source(source_generation) {
            return;
        }
        self.terminal_bootstrap_command = None;
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

    fn handle_remote_paths(
        &mut self,
        directory: String,
        suggestions: Vec<RemotePathSuggestion>,
        home: bool,
        browse_generation: u64,
    ) {
        if !self.generations.accepts_browse(browse_generation) {
            return;
        }
        if home {
            self.remote_path_input = directory_path(&directory);
        }
        self.status = Status::normal(format!(
            "Listed {} remote path suggestion(s).",
            suggestions.len()
        ));
        self.remote_browse_directory = Some(directory);
        self.remote_suggestions = suggestions;
        self.remote_browse_pending = false;
        self.terminal_bootstrap_command = None;
    }

    fn handle_remote_paths_failed(
        &mut self,
        message: String,
        terminal_bootstrap_command: Option<String>,
        browse_generation: u64,
    ) {
        if !self.generations.accepts_browse(browse_generation) {
            return;
        }
        self.status = Status::error(message);
        self.terminal_bootstrap_command = terminal_bootstrap_command;
        self.remote_browse_pending = false;
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
    }
}

impl Drop for ViewerApp {
    fn drop(&mut self) {
        self.worker.shutdown();
        if let Some(control_path) = self
            .ssh_config
            .as_ref()
            .and_then(OpenSshConfig::control_path)
        {
            let _ = std::fs::remove_dir_all(control_path.directory());
        }
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
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
    #[allow(clippy::too_many_lines)]
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_dropped_files(context);
        self.handle_worker_events();
        self.retry_pending_open();

        egui::TopBottomPanel::top("open_bar").show(context, |ui| {
            ui.horizontal(|ui| {
                let local_mode = ui.selectable_value(&mut self.open_mode, OpenMode::Local, "Local");
                ui.selectable_value(&mut self.open_mode, OpenMode::Ssh, "SSH");
                if local_mode.changed() && self.open_mode == OpenMode::Local {
                    self.invalidate_remote_browse();
                }
            });
            match self.open_mode {
                OpenMode::Local => {
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
                            self.open_local_path();
                        }
                    });
                }
                OpenMode::Ssh => {
                    ui.horizontal(|ui| {
                        ui.label("Profile / host alias:");
                        let profile_response = ui.add(
                            egui::TextEdit::singleline(&mut self.ssh_profile_input)
                                .hint_text("my-ssh-profile")
                                .desired_width(220.0),
                        );
                        ui.label("Remote CZI path:");
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.remote_path_input)
                                .hint_text("/absolute/path/image.czi")
                                .desired_width(360.0),
                        );
                        if profile_response.changed() || response.changed() {
                            self.invalidate_remote_browse();
                        }
                        if ui.button("Home").clicked() {
                            self.browse_remote_path(true);
                        }
                        if ui.button("Browse").clicked() {
                            self.browse_remote_path(false);
                        }
                        let connect = ui.button("Connect").clicked()
                            || (response.lost_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)));
                        if connect {
                            self.open_remote_path();
                        }
                        if ui.button("Retry").clicked() {
                            self.open_remote_path();
                        }
                    });
                    ui.weak(
                        "Read-only SFTP range reads · 1 MiB blocks · 256 MiB source cache · GUI connections never prompt.",
                    );
                    if self.remote_browse_pending {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.weak("Listing remote paths…");
                        });
                    }
                    if let Some(directory) = &self.remote_browse_directory {
                        ui.label(format!("Remote suggestions in {directory}"));
                        let mut selected_suggestion = None;
                        egui::ScrollArea::vertical()
                            .max_height(180.0)
                            .show(ui, |ui| {
                                for suggestion in &self.remote_suggestions {
                                    let label = match suggestion.kind {
                                        RemotePathKind::Directory => {
                                            format!("{}/", suggestion.name)
                                        }
                                        RemotePathKind::CziFile => suggestion.name.clone(),
                                    };
                                    if ui.button(label).clicked() {
                                        selected_suggestion = Some(suggestion.clone());
                                    }
                                }
                                if self.remote_suggestions.is_empty() {
                                    ui.weak("No matching directories or .czi files.");
                                }
                            });
                        if let Some(suggestion) = selected_suggestion {
                            self.select_remote_suggestion(suggestion);
                        }
                    }
                    if let Some(command) = &self.terminal_bootstrap_command {
                        ui.separator();
                        ui.label("SSH needs an interactive SFTP bridge:");
                        ui.add(
                            egui::Label::new(egui::RichText::new(command).monospace())
                                .selectable(true)
                                .wrap(),
                        );
                        if ui.button("Copy command").clicked() {
                            context.copy_text(command.clone());
                        }
                        ui.weak(
                            "Paste and run this command in Terminal. It waits for Retry, Home, or Browse; finish password, 2FA, or host-key prompts there. Keep Terminal open while the remote file is in use.",
                        );
                        ui.weak(
                            "The private bridge socket accepts one SFTP stream and closes when that stream ends. Closing the viewer removes its local bridge directory. The viewer never writes to the remote host.",
                        );
                    }
                }
            }
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
                let before_selection = self.selection;
                let selection_changed = if let Some(dataset) = self.dataset.as_ref() {
                    ui.label(&dataset.source_label);
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
                    let changed = [
                        before_selection.c != self.selection.c,
                        before_selection.scene != self.selection.scene,
                        before_selection.z != self.selection.z,
                        before_selection.t != self.selection.t,
                    ];
                    if let Some(dataset) = self.dataset.as_ref() {
                        self.selection = dataset.repair_selection(self.selection, changed);
                    }
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
                    let (visible, resident, bytes) = self
                        .cache
                        .current_counts(self.generations.source, self.selection.key());
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
    fn browse_generations_drop_stale_results() {
        let mut generations = Generations::default();
        let first = generations.begin_browse();
        assert!(generations.accepts_browse(first));
        let second = generations.begin_browse();
        assert!(!generations.accepts_browse(first));
        assert!(generations.accepts_browse(second));
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
                    name: String::from("beta.CZI"),
                    path: String::from("/data/beta.CZI"),
                    kind: RemotePathKind::CziFile,
                },
                RemotePathSuggestion {
                    name: String::from("folder.czi"),
                    path: String::from("/data/folder.czi"),
                    kind: RemotePathKind::Directory,
                },
                RemotePathSuggestion {
                    name: String::from("zeta.czi"),
                    path: String::from("/data/zeta.czi"),
                    kind: RemotePathKind::CziFile,
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
    fn bridge_command_is_only_returned_for_remote_open_failures() {
        let control_path = ControlPath::create_private().expect("private control path");
        let directory = control_path.directory().to_path_buf();
        let remote = DatasetLocator::Remote {
            profile: String::from("research-cluster"),
            path: String::from("/data/image.czi"),
            config: OpenSshConfig::new().with_control_path(control_path),
        };

        let local_failure = DatasetLocator::Local(PathBuf::from("/missing/image.czi"))
            .open_failure("local open failed");
        assert_eq!(local_failure.message, "local open failed");
        assert!(local_failure.terminal_bootstrap_command.is_none());

        let remote_failure = remote.open_failure("remote open failed");
        assert_eq!(remote_failure.message, "remote open failed");
        let command = remote_failure
            .terminal_bootstrap_command
            .expect("remote bridge command");
        assert!(command.contains("'research-cluster'"));
        assert!(command.contains("'--czi-sftp-bridge'"));
        assert!(!command.contains("/data/image.czi"));
        drop(remote);
        std::fs::remove_dir_all(directory).expect("remove private control path");
    }

    #[test]
    fn worker_preserves_events_from_a_bounded_command_burst() {
        let mut worker = DatasetWorker::spawn();
        let count = u64::try_from(CHANNEL_CAPACITY + 1).expect("channel capacity fits u64");
        for source_generation in 1..=count {
            worker
                .commands
                .send(WorkerCommand::Open {
                    locator: DatasetLocator::Local(PathBuf::from(format!(
                        "/dev/null/czi-viewer-missing-{source_generation}.czi"
                    ))),
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
    fn worker_event_batch_is_bounded_per_ui_update() {
        let (sender, events) = mpsc::sync_channel(CHANNEL_CAPACITY);
        for source_generation in 0..CHANNEL_CAPACITY {
            sender
                .send(WorkerEvent::OpenFailed {
                    message: source_generation.to_string(),
                    terminal_bootstrap_command: None,
                    source_generation: u64::try_from(source_generation).expect("generation"),
                })
                .expect("event channel capacity");
        }
        assert_eq!(take_worker_event_batch(&events).len(), CHANNEL_CAPACITY);
        sender
            .send(WorkerEvent::OpenFailed {
                message: String::from("next"),
                terminal_bootstrap_command: None,
                source_generation: 0,
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
