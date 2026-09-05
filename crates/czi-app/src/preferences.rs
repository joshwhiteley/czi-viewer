//! Bounded, private desktop preferences. Never stores remote paths or credentials.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use eframe::egui;
use serde::{Deserialize, Serialize};

const MAX_BYTES: u64 = 64 * 1024;
const MAX_RECENT: usize = 12;
const MAX_PATH_BYTES: usize = 4096;
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) enum Appearance {
    #[default]
    System,
    Dark,
    Light,
}

impl Appearance {
    pub(crate) fn theme(self) -> egui::ThemePreference {
        match self {
            Self::System => egui::ThemePreference::System,
            Self::Dark => egui::ThemePreference::Dark,
            Self::Light => egui::ThemePreference::Light,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Preferences {
    pub(crate) appearance: Appearance,
    pub(crate) text_scale: f32,
    pub(crate) inspector_open: bool,
    pub(crate) show_overview: bool,
    pub(crate) automatic_basic: bool,
    pub(crate) export_annotations: bool,
    pub(crate) remember_recent: bool,
    pub(crate) recent_local: Vec<PathBuf>,
    pub(crate) window_size: Option<[f32; 2]>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            text_scale: 1.0,
            inspector_open: true,
            show_overview: true,
            automatic_basic: false,
            export_annotations: true,
            remember_recent: true,
            recent_local: Vec::new(),
            window_size: None,
        }
    }
}

impl Preferences {
    fn validate(&self) -> Result<(), String> {
        if !self.text_scale.is_finite() || !(0.8..=1.6).contains(&self.text_scale) {
            return Err(String::from("Invalid preferences text scale."));
        }
        if let Some([width, height]) = self.window_size
            && (!width.is_finite()
                || !height.is_finite()
                || !(640.0..=8192.0).contains(&width)
                || !(480.0..=8192.0).contains(&height))
        {
            return Err(String::from("Invalid saved window size."));
        }
        if self.recent_local.len() > MAX_RECENT
            || self.recent_local.iter().any(|path| !valid_local_path(path))
        {
            return Err(String::from("Invalid or oversized recent-file history."));
        }
        Ok(())
    }

    pub(crate) fn remember(&mut self, path: &Path) {
        if !self.remember_recent || !valid_local_path(path) {
            return;
        }
        self.recent_local.retain(|existing| existing != path);
        self.recent_local.insert(0, path.to_path_buf());
        self.recent_local.truncate(MAX_RECENT);
    }

    pub(crate) fn clear_history(&mut self) {
        self.recent_local.clear();
    }
}

fn valid_local_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .to_str()
            .is_some_and(|text| text.len() <= MAX_PATH_BYTES && !text.chars().any(char::is_control))
}

enum Command {
    Save(Preferences),
}

pub(crate) enum Event {
    Loaded(Result<Preferences, String>),
    Saved(Result<(), String>),
}

pub(crate) struct PreferencesWorker {
    commands: Option<SyncSender<Command>>,
    results: Receiver<Event>,
    join: Option<JoinHandle<()>>,
}

impl PreferencesWorker {
    pub(crate) fn spawn(context: egui::Context) -> Self {
        let (commands, command_rx) = mpsc::sync_channel(2);
        let (sender, results) = mpsc::channel();
        let join = thread::Builder::new()
            .name(String::from("czi-preferences"))
            .spawn(move || {
                let path = default_path();
                let loaded = path.as_deref().map_err(Clone::clone).and_then(load);
                let _ = sender.send(Event::Loaded(loaded));
                context.request_repaint();
                while let Ok(Command::Save(mut preferences)) = command_rx.recv() {
                    // Persist the latest state when several UI actions arrive together.
                    while let Ok(Command::Save(newer)) = command_rx.try_recv() {
                        preferences = newer;
                    }
                    let result = path
                        .as_deref()
                        .map_err(Clone::clone)
                        .and_then(|path| save(path, &preferences));
                    let _ = sender.send(Event::Saved(result));
                    context.request_repaint();
                }
            })
            .expect("start preferences worker");
        Self {
            commands: Some(commands),
            results,
            join: Some(join),
        }
    }

    /// Returns false when the bounded queue is full; caller retains and retries latest state.
    pub(crate) fn try_save(&self, preferences: &Preferences) -> bool {
        self.commands
            .as_ref()
            .is_some_and(|sender| sender.try_send(Command::Save(preferences.clone())).is_ok())
    }

    /// Called only during application shutdown, before this worker drains and joins.
    pub(crate) fn save_on_shutdown(&self, preferences: &Preferences) {
        if let Some(sender) = &self.commands {
            let _ = sender.send(Command::Save(preferences.clone()));
        }
    }

    pub(crate) fn try_recv(&self) -> Option<Event> {
        self.results.try_recv().ok()
    }
}

impl Drop for PreferencesWorker {
    fn drop(&mut self) {
        self.commands.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn default_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| String::from("No absolute home directory for preferences."))?;
    Ok(home.join("Library/Application Support/CZI Viewer/preferences.json"))
}

fn load(path: &Path) -> Result<Preferences, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Preferences::default());
        }
        Err(error) => return Err(format!("Could not inspect preferences: {error}")),
    };
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(String::from("Preferences must be a bounded regular file."));
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("Could not read preferences: {error}"))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(String::from("Preferences exceed size limit."));
    }
    let mut preferences: Preferences = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Could not parse preferences: {error}"))?;
    preferences.validate()?;
    if !preferences.remember_recent {
        preferences.clear_history();
    }
    Ok(preferences)
}

fn save(path: &Path, preferences: &Preferences) -> Result<(), String> {
    preferences.validate()?;
    let mut preferences = preferences.clone();
    if !preferences.remember_recent {
        preferences.clear_history();
    }
    let bytes = serde_json::to_vec(&preferences).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(String::from("Preferences exceed size limit."));
    }
    let directory = path.parent().ok_or("Preferences path has no parent.")?;
    fs::create_dir_all(directory).map_err(|error| error.to_string())?;
    if !fs::symlink_metadata(directory)
        .map_err(|error| error.to_string())?
        .file_type()
        .is_dir()
    {
        return Err(String::from("Preferences directory must not be a symlink."));
    }
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())?;
    let temporary = directory.join(format!(
        ".preferences-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "czi-preferences-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn history_is_bounded_deduplicated_and_opt_out_clears_it() {
        let mut preferences = Preferences::default();
        for n in 0..20 {
            preferences.remember(Path::new(&format!("/data/{n}.czi")));
        }
        assert_eq!(preferences.recent_local.len(), MAX_RECENT);
        preferences.remember(Path::new("/data/18.czi"));
        assert_eq!(preferences.recent_local[0], Path::new("/data/18.czi"));
        assert_eq!(preferences.recent_local.len(), MAX_RECENT);
        preferences.remember(Path::new("relative.czi"));
        assert_eq!(preferences.recent_local.len(), MAX_RECENT);
        preferences.remember_recent = false;
        preferences.clear_history();
        preferences.remember(Path::new("/data/private.czi"));
        assert!(preferences.recent_local.is_empty());
    }

    #[test]
    fn settings_roundtrip_is_private_bounded_and_rejects_symlinks() {
        let root = temporary_directory();
        let path = root.join("preferences.json");
        let mut preferences = Preferences::default();
        preferences.remember(Path::new("/data/test.czi"));
        save(&path, &preferences).unwrap();
        assert_eq!(load(&path).unwrap(), preferences);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        preferences.remember_recent = false;
        save(&path, &preferences).unwrap();
        assert!(load(&path).unwrap().recent_local.is_empty());
        fs::write(&path, vec![b'x'; usize::try_from(MAX_BYTES).unwrap() + 1]).unwrap();
        assert!(load(&path).is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(root.join("missing"), &path).unwrap();
        assert!(load(&path).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn settings_reject_invalid_sizes_and_unknown_fields() {
        let mut preferences = Preferences {
            text_scale: f32::NAN,
            ..Preferences::default()
        };
        assert!(preferences.validate().is_err());
        preferences.text_scale = 1.0;
        preferences.window_size = Some([1.0, f32::INFINITY]);
        assert!(preferences.validate().is_err());
        assert!(serde_json::from_str::<Preferences>(r#"{"ssh_password":"no"}"#).is_err());
        assert_eq!(
            serde_json::from_str::<Preferences>("{}").unwrap(),
            Preferences::default()
        );
    }
}
