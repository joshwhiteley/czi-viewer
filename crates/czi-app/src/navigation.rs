//! Geometry-only overview and keyboard navigation; never loads additional image tiles.
use czi_core::SpatialRect;
use eframe::egui;

use crate::Camera;

pub(super) fn keyboard_pan_zoom(
    ui: &egui::Ui,
    response: &egui::Response,
    camera: &mut Camera,
    bounds: SpatialRect,
) -> bool {
    if (ui.ctx().wants_keyboard_input() && !response.has_focus())
        || !(response.hovered() || response.has_focus())
    {
        return false;
    }
    let mut changed = false;
    if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::F)) {
        camera.fit(response.rect, bounds);
        changed = true;
    }
    if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Num1)) {
        camera.one_to_one();
        changed = true;
    }
    for (key, delta) in [
        (egui::Key::ArrowLeft, egui::vec2(48.0, 0.0)),
        (egui::Key::ArrowRight, egui::vec2(-48.0, 0.0)),
        (egui::Key::ArrowUp, egui::vec2(0.0, 48.0)),
        (egui::Key::ArrowDown, egui::vec2(0.0, -48.0)),
    ] {
        if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, key)) {
            camera.pan += delta;
            changed = true;
        }
    }
    if let Some(factor) = ui.input_mut(consume_zoom_key) {
        camera.zoom_at(response.rect.center(), factor, response.rect, bounds);
        changed = true;
    }
    changed
}

fn consume_zoom_key(input: &mut egui::InputState) -> Option<f64> {
    for (modifiers, key, factor) in [
        (egui::Modifiers::NONE, egui::Key::Plus, 1.25),
        (egui::Modifiers::SHIFT, egui::Key::Plus, 1.25),
        (egui::Modifiers::NONE, egui::Key::Equals, 1.25),
        (egui::Modifiers::SHIFT, egui::Key::Equals, 1.25),
        (egui::Modifiers::NONE, egui::Key::Minus, 0.8),
    ] {
        if input.consume_key(modifiers, key) {
            return Some(factor);
        }
    }
    None
}

pub(super) fn overview(
    ui: &egui::Ui,
    canvas: egui::Rect,
    bounds: SpatialRect,
    camera: &mut Camera,
) -> bool {
    if canvas.width() < 320.0 || canvas.height() < 240.0 {
        return false;
    }
    let outer = egui::Rect::from_min_size(
        egui::pos2(canvas.right() - 174.0, canvas.top() + 12.0),
        egui::vec2(160.0, 116.0),
    );
    let area = outer.shrink2(egui::vec2(8.0, 20.0));
    let scale = (f64::from(area.width()) / bounds.width().max(1) as f64)
        .min(f64::from(area.height()) / bounds.height().max(1) as f64);
    let image = egui::Rect::from_center_size(
        area.center(),
        egui::vec2(
            (bounds.width() as f64 * scale) as f32,
            (bounds.height() as f64 * scale) as f32,
        ),
    );
    let response = ui
        .interact(
            outer,
            ui.id().with("overview-map"),
            egui::Sense::click_and_drag(),
        )
        .on_hover_text(
            "Overview of dataset bounds. Click or drag to navigate. No extra image data is loaded.",
        );
    let painter = ui.painter().with_clip_rect(canvas);
    painter.rect_filled(outer, 6.0, egui::Color32::from_black_alpha(220));
    painter.text(
        outer.min + egui::vec2(8.0, 5.0),
        egui::Align2::LEFT_TOP,
        "Overview",
        egui::FontId::proportional(11.0),
        egui::Color32::LIGHT_GRAY,
    );
    painter.rect_filled(image, 0.0, egui::Color32::from_gray(65));
    let mut changed = false;
    if (response.clicked() || response.dragged())
        && let Some(pointer) = response.interact_pointer_pos()
    {
        let world = map_to_world(image.clamp(pointer), image, bounds);
        let center = Camera::world_center(bounds);
        camera.pan = egui::vec2(
            ((center.0 - world.0) * camera.zoom) as f32,
            ((center.1 - world.1) * camera.zoom) as f32,
        );
        changed = true;
    }
    if let Some(viewport) = camera.viewport(canvas, bounds) {
        let min = world_to_map(
            (viewport.min_x as f64, viewport.min_y as f64),
            image,
            bounds,
        );
        let max = world_to_map(
            (viewport.max_x as f64, viewport.max_y as f64),
            image,
            bounds,
        );
        let viewport = egui::Rect::from_min_max(min, max).intersect(image);
        if viewport.is_positive() {
            painter.rect_filled(
                viewport,
                0.0,
                egui::Color32::from_rgba_unmultiplied(115, 180, 255, 40),
            );
            painter.rect_stroke(
                viewport,
                0.0,
                egui::Stroke::new(1.5, egui::Color32::LIGHT_BLUE),
                egui::StrokeKind::Inside,
            );
        }
    }
    changed
}

fn map_to_world(point: egui::Pos2, map: egui::Rect, bounds: SpatialRect) -> (f64, f64) {
    (
        bounds.min_x as f64
            + f64::from((point.x - map.left()) / map.width().max(f32::EPSILON))
                * bounds.width() as f64,
        bounds.min_y as f64
            + f64::from((point.y - map.top()) / map.height().max(f32::EPSILON))
                * bounds.height() as f64,
    )
}

fn world_to_map(point: (f64, f64), map: egui::Rect, bounds: SpatialRect) -> egui::Pos2 {
    egui::pos2(
        map.left()
            + ((point.0 - bounds.min_x as f64) / bounds.width().max(1) as f64) as f32 * map.width(),
        map.top()
            + ((point.1 - bounds.min_y as f64) / bounds.height().max(1) as f64) as f32
                * map.height(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shifted_plus_is_consumed_as_zoom_without_command_shortcuts() {
        for key in [egui::Key::Plus, egui::Key::Equals] {
            let context = egui::Context::default();
            let _ = context.run(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::SHIFT,
                    }],
                    ..Default::default()
                },
                |context| {
                    assert!(
                        context
                            .input_mut(consume_zoom_key)
                            .is_some_and(|factor| factor > 1.0)
                    );
                    assert!(context.input_mut(consume_zoom_key).is_none());
                },
            );
        }
    }

    #[test]
    fn overview_roundtrips_negative_world_coordinates() {
        let bounds = SpatialRect::new(-12000, -3000, 1000, 4500).unwrap();
        let map = egui::Rect::from_min_max(egui::pos2(10.0, 20.0), egui::pos2(170.0, 110.0));
        for point in [map.min, map.center(), map.max] {
            let world = map_to_world(point, map, bounds);
            assert!(world_to_map(world, map, bounds).distance(point) < 0.001);
        }
    }
}
