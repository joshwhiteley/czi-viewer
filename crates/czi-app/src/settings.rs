use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};

use serde::{Deserialize, Serialize};

const SETTINGS_FILE_BYTES: u64 = 4 * 1024;
const HELPER_PATH_BYTES: usize = 2 * 1024;
const SETTINGS_QUEUE_CAPACITY: usize = 4;
static SETTINGS_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HelperSettings {
    helper_path: String,
}

enum SettingsCommand {
    Save { path: PathBuf, generation: u64 },
    Clear { generation: u64 },
    Shutdown,
}

pub(crate) enum SettingsResult {
    Loaded {
        result: Result<Option<PathBuf>, String>,
        generation: u64,
    },
    Saved {
        result: Result<PathBuf, String>,
        generation: u64,
    },
    Cleared {
        result: Result<(), String>,
        generation: u64,
    },
}

pub(crate) struct HelperSettingsWorker {
    commands: Option<SyncSender<SettingsCommand>>,
    results: Receiver<SettingsResult>,
    join: Option<JoinHandle<()>>,
}

impl HelperSettingsWorker {
    pub(crate) fn spawn() -> Self {
        let (commands, command_rx) = mpsc::sync_channel(SETTINGS_QUEUE_CAPACITY);
        let (result_tx, results) = mpsc::channel();
        let join = thread::Builder::new()
            .name(String::from("czi-helper-settings"))
            .spawn(move || settings_loop(&command_rx, &result_tx))
            .expect("start helper settings worker");
        Self {
            commands: Some(commands),
            results,
            join: Some(join),
        }
    }

    pub(crate) fn save(&self, path: PathBuf, generation: u64) -> Result<(), String> {
        self.send(SettingsCommand::Save { path, generation })
    }

    pub(crate) fn clear(&self, generation: u64) -> Result<(), String> {
        self.send(SettingsCommand::Clear { generation })
    }

    pub(crate) fn try_recv(&self) -> Option<SettingsResult> {
        self.results.try_recv().ok()
    }

    fn send(&self, command: SettingsCommand) -> Result<(), String> {
        self.commands
            .as_ref()
            .ok_or_else(|| String::from("Helper settings worker is shut down."))?
            .try_send(command)
            .map_err(|error| format!("Helper settings queue is unavailable: {error}"))
    }
}

impl Drop for HelperSettingsWorker {
    fn drop(&mut self) {
        let commands = self.commands.take();
        if let Some(commands) = &commands {
            let _ = commands.try_send(SettingsCommand::Shutdown);
        }
        drop(commands);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn settings_loop(commands: &Receiver<SettingsCommand>, results: &mpsc::Sender<SettingsResult>) {
    let settings_path = default_settings_path();
    let loaded = settings_path
        .as_deref()
        .map_err(Clone::clone)
        .and_then(load_settings);
    if results
        .send(SettingsResult::Loaded {
            result: loaded,
            generation: 0,
        })
        .is_err()
    {
        return;
    }
    while let Ok(command) = commands.recv() {
        match command {
            SettingsCommand::Save { path, generation } => {
                let result = settings_path
                    .as_deref()
                    .map_err(Clone::clone)
                    .and_then(|settings_path| save_settings(settings_path, &path));
                if results
                    .send(SettingsResult::Saved { result, generation })
                    .is_err()
                {
                    return;
                }
            }
            SettingsCommand::Clear { generation } => {
                let result = settings_path
                    .as_deref()
                    .map_err(Clone::clone)
                    .and_then(clear_settings);
                if results
                    .send(SettingsResult::Cleared { result, generation })
                    .is_err()
                {
                    return;
                }
            }
            SettingsCommand::Shutdown => return,
        }
    }
}

pub(crate) fn validate_helper_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(String::from("BaSiC helper path must be absolute."));
    }
    if path.as_os_str().as_bytes().len() > HELPER_PATH_BYTES {
        return Err(format!(
            "BaSiC helper path exceeds the {HELPER_PATH_BYTES}-byte settings bound."
        ));
    }
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("Could not resolve BaSiC helper executable: {error}"))?;
    if !canonical.is_absolute() {
        return Err(String::from(
            "BaSiC helper did not resolve to an absolute path.",
        ));
    }
    let metadata = fs::metadata(&canonical)
        .map_err(|error| format!("Could not inspect BaSiC helper executable: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err(String::from("BaSiC helper must be a regular file."));
    }
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(String::from("BaSiC helper file is not executable."));
    }
    if canonical.as_os_str().as_bytes().len() > HELPER_PATH_BYTES {
        return Err(format!(
            "Canonical BaSiC helper path exceeds the {HELPER_PATH_BYTES}-byte settings bound."
        ));
    }
    Ok(canonical)
}

fn default_settings_path() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| String::from("Could not locate the macOS home directory for settings."))?;
    if !home.is_absolute() {
        return Err(String::from(
            "The macOS home directory for settings is not absolute.",
        ));
    }
    Ok(home
        .join("Library")
        .join("Application Support")
        .join("CZI Viewer")
        .join("settings.json"))
}

fn load_settings(path: &Path) -> Result<Option<PathBuf>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not inspect helper settings: {error}")),
    };
    if !metadata.file_type().is_file() {
        return Err(String::from("Helper settings must be a regular file."));
    }
    if metadata.len() > SETTINGS_FILE_BYTES {
        return Err(format!(
            "Helper settings exceed the {SETTINGS_FILE_BYTES}-byte bound."
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    File::open(path)
        .and_then(|file| file.take(SETTINGS_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("Could not read helper settings: {error}"))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > SETTINGS_FILE_BYTES {
        return Err(format!(
            "Helper settings exceed the {SETTINGS_FILE_BYTES}-byte bound."
        ));
    }
    let settings: HelperSettings = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Could not parse helper settings: {error}"))?;
    validate_helper_path(Path::new(&settings.helper_path)).map(Some)
}

fn save_settings(settings_path: &Path, helper_path: &Path) -> Result<PathBuf, String> {
    let helper_path = validate_helper_path(helper_path)?;
    let helper_path_text = helper_path
        .to_str()
        .ok_or_else(|| String::from("BaSiC helper path is not valid UTF-8 for settings."))?;
    let encoded = serde_json::to_vec(&HelperSettings {
        helper_path: helper_path_text.to_owned(),
    })
    .map_err(|error| format!("Could not encode helper settings: {error}"))?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > SETTINGS_FILE_BYTES {
        return Err(format!(
            "Helper settings exceed the {SETTINGS_FILE_BYTES}-byte bound."
        ));
    }
    let directory = settings_path
        .parent()
        .ok_or_else(|| String::from("Helper settings path has no parent directory."))?;
    create_private_directory(directory)?;
    let temp_path = unique_temp_path(settings_path);
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp_path)
            .map_err(|error| format!("Could not create private helper settings: {error}"))?;
        file.write_all(&encoded)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Could not write helper settings: {error}"))?;
        fs::rename(&temp_path, settings_path)
            .map_err(|error| format!("Could not atomically replace helper settings: {error}"))?;
        fs::set_permissions(settings_path, fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("Could not make helper settings private: {error}"))?;
        sync_directory(directory)?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result.map(|()| helper_path)
}

fn clear_settings(settings_path: &Path) -> Result<(), String> {
    match fs::remove_file(settings_path) {
        Ok(()) => {
            if let Some(directory) = settings_path.parent() {
                sync_directory(directory)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not clear helper settings: {error}")),
    }
}

fn create_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create helper settings directory: {error}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("Could not make helper settings directory private: {error}"))
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not sync helper settings directory: {error}"))
}

fn unique_temp_path(settings_path: &Path) -> PathBuf {
    let sequence = SETTINGS_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    settings_path.with_extension(format!("tmp-{}-{sequence}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "czi-settings-{name}-{}-{}",
            std::process::id(),
            SETTINGS_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("test directory");
        path
    }

    fn executable(path: &Path) {
        fs::write(path, b"#!/bin/sh\nexit 0\n").expect("helper");
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("executable mode");
    }

    #[test]
    fn helper_validation_requires_absolute_regular_executable_and_canonicalizes() {
        let root = test_dir("validation");
        let helper = root.join("helper");
        executable(&helper);
        assert_eq!(
            validate_helper_path(&helper).expect("valid helper"),
            fs::canonicalize(&helper).expect("canonical helper")
        );
        assert!(validate_helper_path(Path::new("relative-helper")).is_err());

        let plain = root.join("plain");
        fs::write(&plain, b"not executable").expect("plain file");
        assert!(
            validate_helper_path(&plain)
                .unwrap_err()
                .contains("executable")
        );
        assert!(
            validate_helper_path(&root)
                .unwrap_err()
                .contains("regular file")
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn settings_are_bounded_private_and_atomically_replaced() {
        let root = test_dir("atomic");
        let settings_path = root.join("Application Support/CZI Viewer/settings.json");
        let first = root.join("first-helper");
        let second = root.join("second-helper");
        executable(&first);
        executable(&second);

        save_settings(&settings_path, &first).expect("first save");
        save_settings(&settings_path, &second).expect("atomic replacement");
        assert_eq!(
            load_settings(&settings_path).expect("load"),
            Some(fs::canonicalize(second).expect("canonical helper"))
        );
        assert_eq!(
            fs::metadata(&settings_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let siblings = fs::read_dir(settings_path.parent().expect("parent"))
            .expect("settings directory")
            .count();
        assert_eq!(siblings, 1, "temporary settings file was left behind");

        fs::write(&settings_path, vec![b'x'; SETTINGS_FILE_BYTES as usize + 1])
            .expect("oversized settings");
        assert!(load_settings(&settings_path).unwrap_err().contains("bound"));
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn clear_removes_only_the_settings_file() {
        let root = test_dir("clear");
        let settings_path = root.join("settings.json");
        let other = root.join("keep");
        fs::write(&settings_path, b"{}").expect("settings");
        fs::write(&other, b"keep").expect("other");
        clear_settings(&settings_path).expect("clear");
        assert!(!settings_path.exists());
        assert!(other.exists());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
