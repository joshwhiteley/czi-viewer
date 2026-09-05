//! Desktop workflow UI kept separate from dataset/transport/rendering code.
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui;

use crate::preferences::{Appearance, Event, Preferences, PreferencesWorker};
use crate::{BasicPhase, Camera, DatasetOrigin, OpenMode, PlaneSelection, Status, ViewerApp};

#[allow(clippy::struct_excessive_bools)] // Independent window visibility and persistence flags.
pub(super) struct DesktopState {
    pub(super) context: egui::Context,
    pub(super) preferences: Preferences,
    worker: PreferencesWorker,
    loaded: bool,
    preferences_writable: bool,
    dirty: bool,
    resize_at: Option<Instant>,
    retry_save_after: Option<Instant>,
    pending_recent: Option<PathBuf>,
    pub(super) settings_open: bool,
    pub(super) shortcuts_open: bool,
    pub(super) diagnostics_open: bool,
    pub(super) last_error: Option<String>,
    pub(super) last_export: Option<PathBuf>,
    pub(super) opening_local: Option<PathBuf>,
    pub(super) copy_snapshot: bool,
    pub(super) bookmarks: Vec<Bookmark>,
}

#[derive(Clone)]
pub(super) struct Bookmark {
    selection: PlaneSelection,
    camera: Camera,
}

impl DesktopState {
    pub(super) fn new(context: egui::Context) -> Self {
        Self {
            worker: PreferencesWorker::spawn(context.clone()),
            context,
            preferences: Preferences::default(),
            loaded: false,
            preferences_writable: false,
            dirty: false,
            resize_at: None,
            retry_save_after: None,
            pending_recent: None,
            settings_open: false,
            shortcuts_open: false,
            diagnostics_open: false,
            last_error: None,
            last_export: None,
            opening_local: None,
            copy_snapshot: false,
            bookmarks: Vec::new(),
        }
    }

    pub(super) fn remember_local(&mut self, path: PathBuf) {
        if self.loaded {
            self.preferences.remember(&path);
            self.dirty = true;
        } else {
            self.pending_recent = Some(path);
        }
    }

    pub(super) fn poll(&mut self, context: &egui::Context) {
        while let Some(event) = self.worker.try_recv() {
            match event {
                Event::Loaded(result) => {
                    self.loaded = true;
                    let (preferences, writable, error) = loaded_preferences(result);
                    self.preferences = preferences;
                    self.preferences_writable = writable;
                    if let Some(error) = error {
                        self.pending_recent = None;
                        self.last_error = Some(error);
                    }
                    context.set_theme(self.preferences.appearance.theme());
                    context.set_zoom_factor(self.preferences.text_scale);
                    if let Some(size) = self.preferences.window_size {
                        let monitor = context.input(|input| input.viewport().monitor_size);
                        context.send_viewport_cmd(egui::ViewportCommand::InnerSize(
                            restore_window_size(size, context.zoom_factor(), monitor),
                        ));
                    }
                    if let Some(path) = self.pending_recent.take() {
                        self.remember_local(path);
                    }
                }
                Event::Saved(Err(error)) => {
                    self.last_error = Some(error);
                    self.dirty = true;
                    self.retry_save_after = Some(Instant::now() + Duration::from_secs(10));
                    context.request_repaint_after(Duration::from_secs(10));
                }
                Event::Saved(Ok(())) => {}
            }
        }
        if self.loaded {
            let size = current_window_size(context);
            if let Some([width, height]) = size
                && (640.0..=8192.0).contains(&width)
                && (480.0..=8192.0).contains(&height)
                && self.preferences.window_size != size
            {
                self.preferences.window_size = size;
                self.resize_at = Some(Instant::now());
            }
            if let Some(at) = self.resize_at {
                if at.elapsed() >= Duration::from_secs(1) {
                    self.dirty = true;
                    self.resize_at = None;
                } else {
                    context.request_repaint_after(Duration::from_secs(1));
                }
            }
            self.flush_preferences();
        }
    }

    pub(super) fn flush_preferences(&mut self) {
        if self.loaded && self.preferences_writable && self.dirty {
            if let Some(retry_at) = self.retry_save_after {
                if let Some(wait) = retry_at.checked_duration_since(Instant::now()) {
                    self.context.request_repaint_after(wait);
                    return;
                }
                self.retry_save_after = None;
            }
            if self.worker.try_save(&self.preferences) {
                self.dirty = false;
            } else {
                self.context
                    .request_repaint_after(Duration::from_millis(100));
            }
        }
    }
}

fn loaded_preferences(result: Result<Preferences, String>) -> (Preferences, bool, Option<String>) {
    match result {
        Ok(preferences) => (preferences, true, None),
        Err(error) => (
            Preferences {
                remember_recent: false,
                ..Preferences::default()
            },
            false,
            Some(error),
        ),
    }
}

fn current_window_size(context: &egui::Context) -> Option<[f32; 2]> {
    // input() takes a write lock: do not call another Context accessor from its closure.
    let zoom = context.zoom_factor();
    context.input(|input| {
        let viewport = input.viewport();
        if viewport.maximized == Some(true) || viewport.fullscreen == Some(true) {
            None
        } else {
            viewport
                .inner_rect
                .map(|rect| native_window_size(rect.size(), zoom))
        }
    })
}

// Persist native logical window points, not zoom-dependent egui points.
fn native_window_size(size: egui::Vec2, zoom: f32) -> [f32; 2] {
    [(size.x * zoom).round(), (size.y * zoom).round()]
}

fn restore_window_size(size: [f32; 2], zoom: f32, monitor: Option<egui::Vec2>) -> egui::Vec2 {
    let requested = egui::vec2(size[0], size[1]) / zoom;
    monitor.map_or(requested, |monitor| requested.min(monitor))
}

impl Drop for DesktopState {
    fn drop(&mut self) {
        if self.loaded && self.preferences_writable && (self.dirty || self.resize_at.is_some()) {
            self.worker.save_on_shutdown(&self.preferences);
        }
    }
}

impl ViewerApp {
    #[allow(clippy::too_many_lines)] // Keep the three compact menu definitions together.
    pub(super) fn show_desktop_menu(&mut self, context: &egui::Context) {
        let preferences_were_loaded = self.desktop.loaded;
        self.desktop.poll(context);
        if !preferences_were_loaded
            && self.desktop.loaded
            && self.desktop.preferences.automatic_basic
            && self.dataset.is_some()
            && self.basic_preview.phase == BasicPhase::Idle
        {
            self.prepare_basic_preview();
        }
        if self.status.is_error {
            self.desktop.last_error = Some(self.status.message.clone());
        }
        self.desktop_shortcuts(context);
        egui::TopBottomPanel::top("desktop-menu").show(context, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open CZI…     ⌘O").clicked() {
                        self.choose_local_file();
                        ui.close();
                    }
                    ui.menu_button("Open Recent", |ui| {
                        let recent = self.desktop.preferences.recent_local.clone();
                        if recent.is_empty() {
                            ui.weak("No recent local files.");
                        }
                        for path in recent {
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            if ui
                                .button(name)
                                .on_hover_text(path.display().to_string())
                                .clicked()
                            {
                                self.open_mode = OpenMode::Local;
                                self.path_input = path.display().to_string();
                                self.open_local_path_value(path);
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.button("Clear Recent Files").clicked() {
                            self.desktop.preferences.clear_history();
                            self.desktop.dirty = true;
                            ui.close();
                        }
                    });
                    ui.separator();
                    let ready =
                        self.dataset.is_some() && self.export_unavailable_message().is_none();
                    if ui
                        .add_enabled(ready, egui::Button::new("Export PNG…     ⌘S"))
                        .clicked()
                    {
                        self.desktop.copy_snapshot = false;
                        self.request_snapshot(context);
                        ui.close();
                    }
                    if ui
                        .add_enabled(ready, egui::Button::new("Copy Canvas     ⌘⇧C"))
                        .clicked()
                    {
                        self.copy_canvas(context);
                        ui.close();
                    }
                    if let Some(path) = self.desktop.last_export.clone()
                        && ui.button("Reveal Last Export in Finder").clicked()
                    {
                        if let Err(error) = crate::export::reveal(&path) {
                            self.status = Status::error(error);
                        }
                        ui.close();
                    }
                    ui.separator();
                    if ui
                        .add_enabled(self.dataset.is_some(), egui::Button::new("Close Dataset"))
                        .clicked()
                    {
                        self.close_desktop_dataset();
                        ui.close();
                    }
                    if ui
                        .add_enabled(self.desktop.loaded, egui::Button::new("Settings…     ⌘,"))
                        .clicked()
                    {
                        self.desktop.settings_open = true;
                        ui.close();
                    }
                });
                ui.menu_button("View", |ui| {
                    if ui
                        .checkbox(&mut self.desktop.preferences.inspector_open, "Inspector")
                        .changed()
                        | ui.checkbox(&mut self.desktop.preferences.show_overview, "Overview map")
                            .changed()
                    {
                        self.desktop.dirty = true;
                    }
                    ui.separator();
                    if ui.button("Fit Image     F").clicked() {
                        self.fit_pending = true;
                        self.last_request = None;
                        ui.close();
                    }
                    if ui.button("1:1 (Logical Pixels)     1").clicked() {
                        self.camera.one_to_one();
                        self.last_request = None;
                        ui.close();
                    }
                    ui.separator();
                    if ui.button("Diagnostics…").clicked() {
                        self.desktop.diagnostics_open = true;
                        ui.close();
                    }
                });
                ui.menu_button("Bookmarks", |ui| {
                    if ui
                        .add_enabled(
                            self.dataset.is_some() && self.desktop.bookmarks.len() < 12,
                            egui::Button::new("Bookmark This View"),
                        )
                        .clicked()
                    {
                        self.desktop.bookmarks.push(Bookmark {
                            selection: self.selection,
                            camera: self.camera,
                        });
                    }
                    let mut restore = None;
                    for (index, bookmark) in self.desktop.bookmarks.iter().enumerate() {
                        if ui
                            .button(format!(
                                "View {} · C{} Z{} T{}",
                                index + 1,
                                bookmark.selection.c,
                                bookmark.selection.z,
                                bookmark.selection.t
                            ))
                            .clicked()
                        {
                            restore = Some(bookmark.clone());
                        }
                    }
                    if let Some(bookmark) = restore {
                        self.selection = bookmark.selection;
                        self.camera = bookmark.camera;
                        self.fit_pending = false;
                        self.invalidate_view();
                        ui.close();
                    }
                    if !self.desktop.bookmarks.is_empty() && ui.button("Clear Bookmarks").clicked()
                    {
                        self.desktop.bookmarks.clear();
                    }
                    ui.weak("Bookmarks apply to this open dataset only.");
                });
                if ui.button("Keyboard Shortcuts").clicked() {
                    self.desktop.shortcuts_open = true;
                }
            });
        });
        self.show_desktop_windows(context);
    }

    fn choose_local_file(&mut self) {
        if !self.native_choosers.czi_pending()
            && let Err(error) = self.native_choosers.choose_czi()
        {
            self.status = Status::error(error);
        }
    }

    pub(super) fn active_world_bounds(&self) -> Option<czi_core::SpatialRect> {
        let dataset = self.dataset.as_ref()?;
        self.active_planes()
            .into_iter()
            .filter_map(|plane| dataset.plane(plane).map(|info| info.world_bounds))
            .reduce(czi_core::SpatialRect::union)
    }

    pub(super) fn copy_canvas(&mut self, context: &egui::Context) {
        if self.pending_snapshot.is_none()
            && self.snapshot_writing.is_none()
            && self.export_unavailable_message().is_none()
        {
            self.desktop.copy_snapshot = true;
            self.request_snapshot(context);
        }
    }

    fn close_desktop_dataset(&mut self) {
        self.close_dataset();
        self.desktop.bookmarks.clear();
        self.status = Status::normal("Dataset closed. Open a CZI to begin.");
    }

    fn desktop_shortcuts(&mut self, context: &egui::Context) {
        // Never consume authentication/password terminal input.
        if self.embedded_authentication.is_some() {
            return;
        }
        let command = egui::Modifiers::COMMAND;
        let open = context.input_mut(|input| input.consume_key(command, egui::Key::O));
        let save = context.input_mut(|input| input.consume_key(command, egui::Key::S));
        let settings = context.input_mut(|input| input.consume_key(command, egui::Key::Comma));
        let copy = context.input_mut(|input| {
            input.consume_key(
                egui::Modifiers {
                    shift: true,
                    ..command
                },
                egui::Key::C,
            )
        });
        if open {
            self.choose_local_file();
        }
        if save {
            self.desktop.copy_snapshot = false;
            self.request_snapshot(context);
        }
        if copy {
            self.copy_canvas(context);
        }
        if settings && self.desktop.loaded {
            self.desktop.settings_open = true;
        }
        if !context.wants_keyboard_input() {
            if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F)) {
                self.fit_pending = true;
                self.last_request = None;
            }
            if context.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Num1))
            {
                self.camera.one_to_one();
                self.last_request = None;
            }
            if context
                .input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                self.desktop.settings_open = false;
                self.desktop.shortcuts_open = false;
                self.desktop.diagnostics_open = false;
            }
        }
    }

    fn show_desktop_windows(&mut self, context: &egui::Context) {
        let before = self.desktop.preferences.clone();
        egui::Window::new("Settings").open(&mut self.desktop.settings_open).resizable(false).show(context, |ui| {
            if !self.desktop.preferences_writable {
                ui.colored_label(egui::Color32::LIGHT_RED, "Saved settings could not be loaded. Changes are temporary until you reset them.");
                if ui.button("Reset Saved Preferences").clicked() {
                    self.desktop.preferences_writable = true;
                    self.desktop.dirty = true;
                }
                ui.separator();
            }
            let preferences = &mut self.desktop.preferences;
            ui.heading("Appearance");
            ui.horizontal(|ui| {
                ui.selectable_value(&mut preferences.appearance, Appearance::System, "System");
                ui.selectable_value(&mut preferences.appearance, Appearance::Dark, "Dark");
                ui.selectable_value(&mut preferences.appearance, Appearance::Light, "Light");
            });
            ui.add(egui::Slider::new(&mut preferences.text_scale, 0.8..=1.6).text("Interface scale"));
            ui.separator();
            ui.heading("Background processing");
            ui.checkbox(&mut preferences.automatic_basic, "Prepare BaSiC automatically on future opens");
            ui.weak("Off by default. Prepare on demand in the Display inspector.");
            ui.separator();
            ui.heading("Export");
            ui.checkbox(&mut preferences.export_annotations, "Include title, channel legend, and scale bar");
            ui.weak("BaSiC validation warnings remain visible on corrected exports.");
            ui.separator();
            ui.heading("Privacy");
            ui.checkbox(&mut preferences.remember_recent, "Remember recent local files");
            ui.weak("Stores local paths on this Mac only. Never stores SSH credentials or remote paths.");
            if !preferences.remember_recent || ui.button("Clear Recent Files").clicked() { preferences.clear_history(); }
        });
        if before != self.desktop.preferences {
            context.set_theme(self.desktop.preferences.appearance.theme());
            context.set_zoom_factor(self.desktop.preferences.text_scale);
            self.desktop.dirty = true;
        }
        egui::Window::new("Keyboard Shortcuts")
            .open(&mut self.desktop.shortcuts_open)
            .resizable(false)
            .show(context, |ui| {
                egui::Grid::new("shortcut-grid")
                    .spacing([30.0, 8.0])
                    .show(ui, |ui| {
                        for (keys, action) in [
                            ("⌘O", "Open CZI"),
                            ("⌘S", "Export canvas PNG"),
                            ("⌘⇧C", "Copy canvas image"),
                            ("⌘,", "Settings"),
                            ("F", "Fit image"),
                            ("1", "One logical pixel per UI point"),
                            ("Arrow keys", "Pan image"),
                            ("+ / −", "Zoom"),
                            ("Drag", "Pan image"),
                            ("Wheel / pinch", "Zoom at cursor"),
                        ] {
                            ui.label(keys);
                            ui.label(action);
                            ui.end_row();
                        }
                    });
            });
        let counts = self.active_cache_counts(self.generations.source);
        egui::Window::new("Diagnostics").open(&mut self.desktop.diagnostics_open).show(context, |ui| {
            ui.label(format!("CZI Viewer {}", env!("CARGO_PKG_VERSION")));
            ui.label(format!("Visible tiles: {} · Resident textures: {} · {:.1} MiB", counts.0, counts.1, counts.2 as f64 / 1_048_576.0));
            ui.label("Source access is read-only. Display adjustments are not quantitative analysis.");
            if let Some(error) = &self.desktop.last_error {
                ui.separator(); ui.strong("Last error");
                ui.label(error);
                if ui.button("Copy Error Details").clicked() { context.copy_text(error.clone()); }
            } else { ui.weak("No errors recorded this session."); }
        });
    }

    pub(super) fn show_desktop_status(&mut self, context: &egui::Context) {
        egui::TopBottomPanel::bottom("desktop-status").show(context, |ui| {
            ui.horizontal(|ui| {
                if self.opening_origin.is_some() {
                    ui.spinner();
                    ui.label("Opening dataset…");
                } else if self.dataset.is_some() {
                    ui.label(match self.dataset_origin {
                        Some(DatasetOrigin::Remote) => "SSH · Read-only",
                        _ => "Local · Read-only",
                    });
                } else {
                    ui.label("Ready");
                }
                ui.separator();
                ui.add(egui::Label::new(&self.status.message).truncate());
                if self.desktop.last_error.is_some() && ui.button("Details…").clicked() {
                    self.desktop.diagnostics_open = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if matches!(
                        self.basic_preview.phase,
                        BasicPhase::Sampling | BasicPhase::Fitting
                    ) {
                        ui.spinner();
                        ui.label("BaSiC preparing");
                    }
                    if self.snapshot_writing.is_some() {
                        ui.label("Export in progress…");
                    }
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unreadable_preferences_do_not_enable_history_or_allow_overwrite() {
        let (mut preferences, writable, error) =
            loaded_preferences(Err(String::from("newer schema")));
        assert!(!writable);
        assert!(error.is_some());
        preferences.remember(std::path::Path::new("/private/sample.czi"));
        assert!(preferences.recent_local.is_empty());
        assert!(!preferences.automatic_basic);
    }

    #[test]
    fn window_size_is_stable_across_interface_scaling_and_clamped_to_monitor() {
        let size = egui::vec2(1440.0, 900.0);
        for zoom in [0.8, 1.0, 1.25, 1.6] {
            let saved = native_window_size(size / zoom, zoom);
            assert!((egui::vec2(saved[0], saved[1]) - size).length() < 0.01);
            let restored = restore_window_size([1440.0, 900.0], zoom, None);
            assert!((restored * zoom - size).length() < 0.01);
        }
        assert_eq!(restore_window_size([8192.0, 8192.0], 1.0, Some(size)), size);
    }

    #[test]
    fn window_size_read_does_not_reenter_the_context_lock() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let join = std::thread::spawn(move || {
            let context = egui::Context::default();
            let mut input = egui::RawInput::default();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .inner_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1440.0, 900.0),
            ));
            let _ = context.run(input, |context| {
                sender.send(current_window_size(context)).unwrap();
            });
        });
        let size = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("window-size query deadlocked")
            .unwrap();
        assert!((size[0] - 1440.0).abs() < 0.01);
        join.join().unwrap();
    }

    #[test]
    fn capture_geometry_rejects_layout_and_retina_scale_changes() {
        let frozen = crate::SnapshotRegion {
            rect: egui::Rect::from_min_size(egui::pos2(100.0, 80.0), egui::vec2(900.0, 600.0)),
            pixels_per_point: 2.0,
        };
        assert!(crate::snapshot_geometry_matches(frozen, Some(frozen)));
        assert!(!crate::snapshot_geometry_matches(frozen, None));
        assert!(!crate::snapshot_geometry_matches(
            frozen,
            Some(crate::SnapshotRegion {
                rect: frozen.rect.translate(egui::vec2(100.0, 0.0)),
                ..frozen
            })
        ));
        assert!(!crate::snapshot_geometry_matches(
            frozen,
            Some(crate::SnapshotRegion {
                pixels_per_point: 1.0,
                ..frozen
            })
        ));
    }
}
