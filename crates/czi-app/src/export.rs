//! Native destination selection and atomic snapshot writes. Source CZIs are never opened for writing.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Call on the PNG writer thread, after the exact canvas has been captured.
pub(crate) fn save_png(default_name: &str, png: &[u8]) -> Result<Option<PathBuf>, String> {
    let Some(path) = choose_destination(default_name)? else {
        return Ok(None);
    };
    write_atomic(&path, png)?;
    Ok(Some(path))
}

#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)] // Keep the same signature as the unsupported-platform error.
fn choose_destination(default_name: &str) -> Result<Option<PathBuf>, String> {
    Ok(rfd::FileDialog::new()
        .set_title("Export Canvas as PNG")
        .add_filter("PNG image", &["png"])
        .set_file_name(default_name)
        .save_file())
}

#[cfg(not(target_os = "macos"))]
fn choose_destination(_default_name: &str) -> Result<Option<PathBuf>, String> {
    Err(String::from(
        "Native PNG export is currently supported on macOS.",
    ))
}

fn write_atomic(path: &Path, png: &[u8]) -> Result<(), String> {
    if !path.is_absolute()
        || !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
    {
        return Err(String::from(
            "Choose an absolute output filename ending in .png.",
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => {
            return Err(String::from(
                "PNG output must not be a symlink or directory.",
            ));
        }
        Ok(_) => {} // Replacement is confirmed by the native save panel.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("Could not inspect PNG destination: {error}")),
    }
    let directory = path
        .parent()
        .ok_or("PNG destination has no parent directory.")?;
    let temporary = directory.join(format!(
        ".czi-export-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(|error| format!("Could not create PNG: {error}"))?;
        file.write_all(png)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("Could not write PNG: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not finish PNG export: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn reveal(path: &Path) -> Result<(), String> {
    if !path.is_absolute() {
        return Err(String::from("Cannot reveal a relative path."));
    }
    #[cfg(target_os = "macos")]
    {
        // Fixed executable, no shell, absolute path cannot become an option.
        std::process::Command::new("/usr/bin/open")
            .arg("-R")
            .arg(path)
            .spawn()
            .map_err(|error| format!("Could not reveal export in Finder: {error}"))
            .map(|mut child| {
                let _ = std::thread::spawn(move || child.wait());
            })
    }
    #[cfg(not(target_os = "macos"))]
    Err(String::from("Reveal in Finder is only available on macOS."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_replaces_png_atomically_and_never_follows_symlinks() {
        let root = std::env::temp_dir().join(format!(
            "czi-export-test-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let path = root.join("snapshot.png");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        let czi = root.join("source.czi");
        fs::write(&czi, b"source").unwrap();
        assert!(write_atomic(&czi, b"never").is_err());
        let link = root.join("link.png");
        std::os::unix::fs::symlink(&czi, &link).unwrap();
        assert!(write_atomic(&link, b"never").is_err());
        assert_eq!(fs::read(&czi).unwrap(), b"source");
        assert!(write_atomic(Path::new("relative.png"), b"never").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
