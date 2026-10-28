use super::fields::menu_item_hover;
use super::*;

/// Type settings ▸ Variable: the font's named instances, then a slider and a field per visible
/// axis (values the style name implies show until changed; "Mixed" when the shown styles differ).
/// The canvas previews a hovered instance and a dragged slider or field live; letting go commits
/// one undo step, Escape cancels.
pub(super) fn variable_axes(app: &mut FigApp, ui: &mut Ui, ids: &[NodeId], styles: &[TextStyle], face: &figcraft_text::Face) {
    let t = Tokens::get(ui.ctx());
    let range = figcraft_engine::textedit::selected_range(&app.session);
    widgets::label(ui, &face.family, t.text_secondary);
    named_instances(app, ui, ids, range, styles, face);
    // Each style's axis values; a style drawn by another face (e.g. a separate italic file) is mixed.
    let per_style: Vec<Option<Vec<f64>>> = styles
        .iter()
        .map(|s| figcraft_text::find_face(&s.font_family, &s.font_style).filter(|f| f.id == face.id).map(|_| face.axis_values(s)))
        .collect();
    let w = ui.available_width();
    let field_w = 52.0;
    let label_w = 84.0;
    let slider_w = (w - label_w - field_w - 12.0).max(40.0);
    let cancel_id = Id::unique("ty_axis_cancelled");
    // A cancelled gesture ends when the pointer is let go.
    if !ui.input(|i| i.pointer.any_down()) {
        ui.data_mut(|d| d.remove::<bool>(cancel_id));
    }
    for (i, axis) in face.axes.iter().enumerate().filter(|(_, a)| !a.hidden) {
        let first = per_style.first().cloned().flatten().and_then(|v| v.get(i).copied());
        let value = first.filter(|v0| per_style.iter().all(|vs| vs.as_ref().and_then(|v| v.get(i)).is_some_and(|v| (v - v0).abs() < 1e-9)));
        // While a drag previews, the controls show the dragged value (the panel shows the document).
        let live_id = Id::unique(("ty_axis_live", &axis.tag));
        if app.ui.text_preview.is_none() {
            ui.data_mut(|d| d.remove::<f64>(live_id));
        }
        let shown = ui.data(|d| d.get_temp::<f64>(live_id)).or(value).map(|v| round_axis(axis, v));
        // (value, gesture ended)
        let mut set: Option<(Value, bool)> = None;
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let (lr, _) = ui.allocate_exact_size(vec2(label_w, FIELD_H), Sense::hover());
            let name = ui.painter().layout(axis.name.clone(), regular(11.0), t.text, label_w);
            let tip = trf(
                "{name} ({tag}): {min} – {max}, default {default}. Double-click the slider to go back to the style's value; Escape while dragging cancels.",
                &[
                    ("name", &axis.name),
                    ("tag", &axis.tag),
                    ("min", &trim_num(axis.min)),
                    ("max", &trim_num(axis.max)),
                    ("default", &trim_num(axis.default)),
                ],
            );
            ui.interact(lr, Id::unique(("ty_axis_label", &axis.tag)), Sense::hover()).on_hover_text(tip);
            ui.painter().galley(pos2(lr.min.x, lr.center().y - name.size().y / 2.0), name, t.text);
            match axis_slider(ui, Id::unique(("ty_axis_slider", &axis.tag)), axis, shown, slider_w) {
                Some((Some(v), done)) => set = Some((json!(round_axis(axis, v)), done)),
                Some((None, _)) => set = Some((Value::Null, true)),
                None => {}
            }
            let step = if axis.max - axis.min > 20.0 { 1.0 } else { 0.1 };
            match widgets::number_field(ui, Id::unique(("ty_axis_field", &axis.tag)), Lead::None, shown, "", field_w, step) {
                Some(FieldEdit::Set(v)) => set = Some((json!(axis.clamp(round_axis(axis, v))), true)),
                Some(FieldEdit::Scrub(v, done)) => set = Some((json!(axis.clamp(round_axis(axis, v))), done)),
                None => {}
            }
        });
        let Some((v, done)) = set else { continue };
        // Escape cancels a previewing gesture (egui may end the drag on it too): the preview ends
        // and the rest of the gesture is ignored until the pointer is let go.
        let previewing = ui.data(|d| d.get_temp::<f64>(live_id)).is_some();
        if previewing && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            ui.data_mut(|d| {
                d.insert_temp(cancel_id, true);
                d.remove::<f64>(live_id);
            });
            app.end_text_preview(ui.ctx());
            continue;
        }
        if ui.data(|d| d.get_temp::<bool>(cancel_id)).unwrap_or(false) {
            continue;
        }
        let params = json!({"ids": ids, "variations": {axis.tag.clone(): v}});
        if done {
            ui.data_mut(|d| d.remove::<f64>(live_id));
            app.run("text.setVariations", params);
        } else {
            if let Some(x) = v.as_f64() {
                ui.data_mut(|d| d.insert_temp(live_id, x));
            }
            app.preview_text(ui.ctx(), "text.setVariations", params);
        }
    }
    if styles.iter().any(|s| !s.variations.is_empty()) {
        ui.add_space(8.0);
        let r = ui.add(egui::Label::new(egui::RichText::new(tr("Reset to style")).font(regular(11.0)).color(t.accent_text)).sense(Sense::click()));
        if r.on_hover_text(tr("Back to the values the font style implies")).clicked() {
            app.run("text.setVariations", json!({"ids": ids, "variations": {}, "replace": true}));
        }
    }
}

/// The variable font's named instances as a menu: the one the text shows ("Custom" once an axis
/// moved), hovering one previews it on the canvas, clicking applies it (its custom axis values go).
fn named_instances(
    app: &mut FigApp,
    ui: &mut Ui,
    ids: &[NodeId],
    range: Option<(NodeId, usize, usize)>,
    styles: &[TextStyle],
    face: &figcraft_text::Face,
) {
    if face.instances.is_empty() {
        return;
    }
    let current = uniform(styles, |s| (s.variations.is_empty(), s.font_style.clone()));
    let label = match &current {
        Some((true, st)) if face.instance(st).is_some() => st.clone(),
        Some(_) => tr("Custom").to_string(),
        None => tr(MIXED).to_string(),
    };
    ui.add_space(4.0);
    // An inline list (a menu would close the Type settings popover it opens from).
    let open_id = Id::unique("ty_instances_open");
    let mut open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false);
    if widgets::dropdown(ui, &label, ui.available_width(), None).on_hover_text(tr("Named instance")).clicked() {
        open = !open;
    }
    if open {
        ui.add_space(2.0);
        egui::ScrollArea::vertical().id_salt("ty_instances").max_height(220.0).show(ui, |ui| {
            for inst in &face.instances {
                let mark = if label == inst.name { "✓ " } else { "   " };
                let (clicked, hovered) = menu_item_hover(ui, &format!("{mark}{}", inst.name));
                let props = json!({"fontStyle": inst.name, "fontVariations": null});
                if clicked {
                    commit_text(app, ids, range, props);
                    open = false;
                } else if hovered {
                    app.preview_text(ui.ctx(), "text.style", text_style_params(ids, range, props));
                }
            }
        });
    }
    ui.data_mut(|d| d.insert_temp(open_id, open));
    ui.add_space(2.0);
}

/// Axis values snap to whole numbers on wide axes (weight, width…) and tenths on narrow ones.
pub(super) fn round_axis(axis: &figcraft_text::Axis, v: f64) -> f64 {
    if axis.max - axis.min > 20.0 { v.round() } else { (v * 10.0).round() / 10.0 }
}

/// A horizontal slider over an axis's range with a tick at its default. `value` None = mixed.
/// Returns the new value (None = double-click: back to the style's value) and whether the
/// gesture ended.
fn axis_slider(ui: &mut Ui, id: Id, axis: &figcraft_text::Axis, value: Option<f64>, width: f32) -> Option<(Option<f64>, bool)> {
    let t = Tokens::get(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(width, FIELD_H), Sense::hover());
    let resp = ui.interact(rect, id, Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), vec2(width - 12.0, 4.0));
    let span = (axis.max - axis.min).max(1e-9);
    let x_of = |v: f64| track.min.x + ((v - axis.min) / span).clamp(0.0, 1.0) as f32 * track.width();
    let p = ui.painter();
    p.rect_filled(track, CornerRadius::same(2), t.border);
    let dx = x_of(axis.default);
    p.line_segment([pos2(dx, track.min.y - 3.0), pos2(dx, track.max.y + 3.0)], Stroke::new(1.0, t.border_strong));
    if let Some(v) = value {
        let x = x_of(v);
        p.rect_filled(Rect::from_min_max(track.min, pos2(x, track.max.y)), CornerRadius::same(2), t.accent);
        p.circle_filled(pos2(x, track.center().y), 6.0, Color32::WHITE);
        let ring = if resp.hovered() || resp.dragged() { crate::theme::ACCENT } else { t.border_strong };
        p.circle_stroke(pos2(x, track.center().y), 6.0, Stroke::new(1.0, ring));
    }
    if resp.double_clicked() {
        return Some((None, true));
    }
    if (resp.dragged() || resp.clicked() || resp.drag_started())
        && let Some(pos) = resp.interact_pointer_pos()
    {
        let f = f64::from(((pos.x - track.min.x) / track.width().max(1.0)).clamp(0.0, 1.0));
        return Some((Some(axis.min + f * span), resp.drag_stopped() || resp.clicked()));
    }
    if resp.drag_stopped() {
        return value.map(|v| (Some(v), true));
    }
    None
}
