use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use crate::settings::validate_helper_path;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ChooserKind {
    Czi,
    Helper,
}

#[derive(Debug)]
pub(crate) struct ChooserResult {
    pub(crate) kind: ChooserKind,
    pub(crate) generation: u64,
    pub(crate) selection: Result<Option<PathBuf>, String>,
}

#[derive(Default)]
struct ChooserGenerations {
    czi: u64,
    helper: u64,
}

impl ChooserGenerations {
    fn begin(&mut self, kind: ChooserKind) -> u64 {
        let generation = match kind {
            ChooserKind::Czi => &mut self.czi,
            ChooserKind::Helper => &mut self.helper,
        };
        *generation = generation.wrapping_add(1);
        *generation
    }

    fn accepts(&self, kind: ChooserKind, generation: u64) -> bool {
        generation
            == match kind {
                ChooserKind::Czi => self.czi,
                ChooserKind::Helper => self.helper,
            }
    }
}

pub(crate) struct NativeChoosers {
    results: Receiver<ChooserResult>,
    sender: Sender<ChooserResult>,
    generations: ChooserGenerations,
    czi_pending: bool,
    helper_pending: bool,
}

impl Default for NativeChoosers {
    fn default() -> Self {
        let (sender, results) = mpsc::channel();
        Self {
            results,
            sender,
            generations: ChooserGenerations::default(),
            czi_pending: false,
            helper_pending: false,
        }
    }
}

impl NativeChoosers {
    pub(crate) fn choose_czi(&mut self) -> Result<(), String> {
        self.spawn(ChooserKind::Czi)
    }

    pub(crate) fn choose_helper(&mut self) -> Result<(), String> {
        self.spawn(ChooserKind::Helper)
    }

    pub(crate) const fn czi_pending(&self) -> bool {
        self.czi_pending
    }

    pub(crate) const fn helper_pending(&self) -> bool {
        self.helper_pending
    }

    pub(crate) fn try_recv(&mut self) -> Option<ChooserResult> {
        while let Ok(result) = self.results.try_recv() {
            if self.generations.accepts(result.kind, result.generation) {
                match result.kind {
                    ChooserKind::Czi => self.czi_pending = false,
                    ChooserKind::Helper => self.helper_pending = false,
                }
                return Some(result);
            }
        }
        None
    }

    fn spawn(&mut self, kind: ChooserKind) -> Result<(), String> {
        let pending = match kind {
            ChooserKind::Czi => &mut self.czi_pending,
            ChooserKind::Helper => &mut self.helper_pending,
        };
        if *pending {
            return Ok(());
        }
        let generation = self.generations.begin(kind);
        *pending = true;
        let sender = self.sender.clone();
        thread::Builder::new()
            .name(match kind {
                ChooserKind::Czi => String::from("czi-file-chooser"),
                ChooserKind::Helper => String::from("czi-helper-chooser"),
            })
            .spawn(move || {
                let selection = choose_file(kind);
                let _ = sender.send(ChooserResult {
                    kind,
                    generation,
                    selection,
                });
            })
            .map(|_| ())
            .map_err(|error| {
                *pending = false;
                format!("Could not start the macOS file chooser: {error}")
            })
    }
}

#[cfg(target_os = "macos")]
fn choose_file(kind: ChooserKind) -> Result<Option<PathBuf>, String> {
    let dialog = match kind {
        ChooserKind::Czi => rfd::FileDialog::new()
            .set_title("Choose CZI…")
            .add_filter("ZEISS CZI", &["czi"]),
        ChooserKind::Helper => rfd::FileDialog::new().set_title("Choose BaSiC helper…"),
    };
    let selected = dialog.pick_file();
    match (kind, selected) {
        (ChooserKind::Helper, Some(path)) => validate_helper_path(&path).map(Some),
        (_, selected) => Ok(selected),
    }
}

#[cfg(not(target_os = "macos"))]
fn choose_file(_kind: ChooserKind) -> Result<Option<PathBuf>, String> {
    Err(String::from("Native file choosers are available on macOS."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chooser_generations_reject_late_results_per_chooser() {
        let mut generations = ChooserGenerations::default();
        let old_czi = generations.begin(ChooserKind::Czi);
        let helper = generations.begin(ChooserKind::Helper);
        let current_czi = generations.begin(ChooserKind::Czi);

        assert!(!generations.accepts(ChooserKind::Czi, old_czi));
        assert!(generations.accepts(ChooserKind::Czi, current_czi));
        assert!(generations.accepts(ChooserKind::Helper, helper));
        assert!(!generations.accepts(ChooserKind::Helper, helper.wrapping_add(1)));
    }
}
