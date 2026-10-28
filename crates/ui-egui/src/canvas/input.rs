use super::*;

/// The zoom tool while Z is held (Alt: zoom out): a click zooms a step about the pointer, a drag
/// zooms to the dragged area. Takes the pointer from the other tools while it's active.
fn zoom_tool(app: &mut FigApp, ui: &Ui, resp: &egui::Response, pos: Pos2, mods: egui::Modifiers, typing: bool) -> bool {
    let held = !typing && !mods.command && ui.input(|i| i.key_down(egui::Key::Z));
    app.canvas.zoom_tool = held.then_some(mods.alt);
    let Some(start) = app.canvas.zoom_drag.or_else(|| (held && resp.hovered() && ui.input(|i| i.pointer.primary_pressed())).then_some(pos)) else {
        return held;
    };
    app.canvas.zoom_drag = Some(start);
    let area = Rect::from_two_pos(start, pos);
    if ui.input(|i| i.pointer.primary_down()) {
        return true;
    }
    app.canvas.zoom_drag = None;
    let (a, b) = (app.to_page(area.min), app.to_page(area.max));
    let params = if area.width() > 4.0 && area.height() > 4.0 {
        json!({"x": a.x, "y": a.y, "width": b.x - a.x, "height": b.y - a.y})
    } else {
        let p = app.to_page(pos);
        json!({"x": p.x, "y": p.y, "out": mods.alt})
    };
    app.run("view.zoomAt", params);
    true
}

/// The pointer and keys on the canvas. False when something on it with a cursor of its own has
/// the pointer (a comment pin, a ruler guide, a gradient or padding handle): the tool's cursor
/// then doesn't show.
pub(super) fn input(app: &mut FigApp, ui: &mut Ui, resp: &egui::Response) -> bool {
    if crate::proto_canvas::input(app, ui, resp) {
        // A prototype gesture is a press on the canvas too: the Pages list gives up the keyboard.
        app.ui.pages_focus = None;
        return true;
    }
    let ctx = ui.ctx().clone();
    // Two fingers pan as well as pinch (a touch screen has no scroll wheel).
    let (mods, scroll, zoom_delta, hover_pos, space) = ctx.input(|i| {
        let pan = i.multi_touch().map_or(egui::Vec2::ZERO, |m| m.translation_delta);
        (i.modifiers, i.smooth_scroll_delta + pan, i.zoom_delta(), i.pointer.hover_pos(), i.key_down(egui::Key::Space))
    });
    let editing_text = app.session.ds().map(|d| d.text_edit.is_some()).unwrap_or(false);
    let typing_elsewhere = crate::a11y::field_focused(&ctx);
    app.canvas.space_pan = space && !editing_text && !typing_elsewhere;
    if !app.canvas.dragging && crate::selection_spacing::input(app, &ctx, hover_pos, resp.hovered(), app.canvas.space_pan) {
        return false;
    }
    // Dropped files import onto the canvas, behind a card while they do ([`crate::pending_open`]);
    // a dropped document opens as a file of its own, as on the home screen. Dev Mode imports
    // nothing into the design, and says so once for the whole drop.
    let dropped = ctx.input(|i| i.raw.dropped_files.clone());
    let mut refused = false;
    for f in dropped {
        let path = f.path().to_path_buf();
        if crate::is_document(&path) {
            crate::pending_open::open_dropped(app, &f);
            continue;
        }
        if refused || crate::menus::dev_mode_refuses(app, "file.import") {
            refused = true;
            continue;
        }
        let pt = hover_pos.map(|h| app.to_page(h)).unwrap_or_default();
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped.png".into());
        let (params, read) = if path.exists() {
            (json!({"path": path.to_string_lossy(), "x": pt.x, "y": pt.y}), None)
        } else {
            (json!({"name": name, "x": pt.x, "y": pt.y}), Some(crate::dropped_bytes(&f, &name)))
        };
        let Some(target) =
            app.session.ds().ok().map(|doc| crate::pending_open::ImportTarget { uid: doc.uid, page: doc.page, name: doc.doc.name.clone() })
        else {
            continue;
        };
        app.ui.pending_opens.push(crate::pending_open::PendingOpen { label: name, params, command: "file.import", target: Some(target), read });
    }
    let Some(pos) = hover_pos.filter(|p| app.canvas.rect.contains(*p)) else {
        // A drag released off the canvas, or off the window, ends where the pointer was last seen.
        if app.canvas.dragging && !ctx.input(|i| i.pointer.primary_down()) {
            app.canvas.dragging = false;
            if let Some(at) = hover_pos.or_else(|| app.canvas.last_pointer.map(|p| app.to_screen(p.x, p.y))) {
                send(app, PointerKind::Up, at, &mods);
            }
        }
        // Leaving the canvas: nothing is hovered there any more.
        if !app.canvas.dragging && std::mem::take(&mut app.canvas.hovering) {
            app.session.pointer_left();
        }
        // A pan whose button was released off the canvas is over.
        if !ctx.input(|i| i.pointer.button_down(egui::PointerButton::Middle) || i.pointer.primary_down()) {
            app.canvas.middle_pan = None;
        }
        return true;
    };
    // A press on the canvas gives the keyboard back to it (arrows nudge again).
    if resp.hovered() && ctx.input(|i| i.pointer.any_pressed()) {
        app.ui.layers_focus = false;
        app.ui.pages_focus = None;
        app.ui.key_box = None;
    }
    // Zoom (pinch, Cmd/Ctrl+scroll) about the pointer; plain scroll pans.
    if resp.hovered() {
        if (zoom_delta - 1.0).abs() > 1e-4 {
            let anchor = app.to_page(pos);
            if let Ok(ds) = app.session.ds_mut() {
                let z = ds.view.zoom * f64::from(zoom_delta);
                ds.view.set_zoom_at(z, (anchor.x, anchor.y));
                ds.revision += 1;
            }
        } else if scroll != egui::Vec2::ZERO
            && let Ok(ds) = app.session.ds_mut()
        {
            ds.view.scroll(f64::from(scroll.x), f64::from(scroll.y), mods.shift);
            ds.revision += 1;
        }
    }
    if zoom_tool(app, ui, resp, pos, mods, editing_text || typing_elsewhere) {
        return true;
    }
    // Middle-button / space panning.
    let middle_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Middle));
    if (middle_down || (app.canvas.space_pan && ctx.input(|i| i.pointer.primary_down()))) && resp.hovered() || app.canvas.middle_pan.is_some() {
        if let Ok(ds) = app.session.ds_mut() {
            match app.canvas.middle_pan {
                None => app.canvas.middle_pan = Some((pos, ds.view.cx, ds.view.cy)),
                Some((p0, cx, cy)) => {
                    let z = ds.view.zoom.max(1e-6);
                    ds.view.cx = cx - f64::from(pos.x - p0.x) / z;
                    ds.view.cy = cy - f64::from(pos.y - p0.y) / z;
                    ds.revision += 1;
                }
            }
        }
        if !middle_down && !ctx.input(|i| i.pointer.primary_down()) {
            app.canvas.middle_pan = None;
        }
        return true;
    }
    if !app.canvas.dragging && (resp.hovered() || app.ui.comments.dragging()) && crate::comments::input(app, &ctx, pos) {
        return false;
    }
    if !app.canvas.dragging && resp.hovered() && crate::guides::input(app, &ctx, pos) {
        return false;
    }
    if !app.canvas.dragging && resp.hovered() && crate::handles::input(app, &ctx, pos) {
        return false;
    }
    if resp.secondary_clicked() {
        open_context_menu(app, pos, mods.command);
        return true;
    }
    // A third quick click is counted as a triple, not a double (a click elsewhere in between still
    // leaves its time in the sequence): it means the same here — edit, enter, open — unless a
    // vector or text is being edited, where a triple already has its own meaning. A triple on
    // empty canvas still means a double in vector edit mode: the double-click that entered edit
    // mode counts on, so leaving it right away would otherwise take a fourth click or a pause.
    let ds = app.session.ds().ok();
    let editing = ds.is_some_and(|d| d.vector_edit.is_some() || d.text_edit.is_some());
    // Only a triple click asks (it walks the parts' render bounds).
    let empty_canvas = || {
        ds.is_some_and(|d| {
            let Some(ve) = d.vector_edit.as_ref().filter(|v| v.anchor.is_none() && d.text_edit.is_none()) else {
                return false;
            };
            let p = app.to_page(pos);
            let tol = 6.0 / d.view.zoom.max(1e-6);
            ve.parts().iter().all(|part| d.doc.world_render_bounds(part.id).is_none_or(|b| !b.inflate(tol * 4.0, tol * 4.0).contains(p)))
        })
    };
    if resp.double_clicked() || (resp.triple_clicked() && (!editing || empty_canvas())) {
        let ev = pointer_event(app, PointerKind::DoubleClick, pos, &mods);
        if let Err(e) = app.session.pointer(&ev) {
            app.toast(e.to_string());
        }
        app.canvas.dragging = false;
        return true;
    }
    let primary_pressed = ctx.input(|i| i.pointer.button_pressed(egui::PointerButton::Primary));
    let primary_down = ctx.input(|i| i.pointer.primary_down());
    let primary_released = ctx.input(|i| i.pointer.button_released(egui::PointerButton::Primary));
    let kind = if primary_pressed && resp.hovered() {
        app.canvas.dragging = true;
        app.ui.context_menu = None;
        app.ui.tool_flyout = None;
        // Clicking the canvas takes focus from panel fields.
        ctx.memory_mut(|m| {
            if let Some(f) = m.focused() {
                m.surrender_focus(f);
            }
        });
        Some(PointerKind::Down)
    } else if app.canvas.dragging && primary_released {
        app.canvas.dragging = false;
        Some(PointerKind::Up)
    } else if app.canvas.dragging && primary_down {
        Some(PointerKind::Move)
    } else if resp.hovered() {
        app.canvas.hovering = true;
        Some(PointerKind::Hover)
    } else {
        // Something over the canvas took the pointer.
        if std::mem::take(&mut app.canvas.hovering) {
            app.session.pointer_left();
        }
        None
    };
    // Pen pressure comes with touch events (a pen on Windows, a stylus in a browser); a mouse,
    // macOS and Linux tablets report none and draw at the stroke weight.
    let force = ctx.input(|i| {
        i.events.iter().rev().find_map(|e| match e {
            egui::Event::Touch { force: Some(f), .. } if f.is_finite() && *f > 0.0 => Some(f.clamp(0.0, 1.0)),
            _ => None,
        })
    });
    if force.is_some() {
        app.canvas.pressure = force;
    }
    if let Some(k) = kind {
        send(app, k, pos, &mods);
        // The pointer also let go within this frame (a quick click, or a slow frame): the tools get
        // the release too, so the press's gesture ends — a quick click with a shape tool still
        // draws and returns to Move, and a double-click right after drawing finds the Move tool.
        if matches!(k, PointerKind::Down) && primary_released {
            app.canvas.dragging = false;
            send(app, PointerKind::Up, pos, &mods);
        }
    }
    true
}

/// Pointer event `kind` at screen point `pos`, to the tools.
fn send(app: &mut FigApp, kind: PointerKind, pos: Pos2, m: &egui::Modifiers) {
    let ev = pointer_event(app, kind, pos, m);
    if kind == PointerKind::Up {
        app.canvas.pressure = None;
    }
    app.canvas.last_pointer = Some(Point::new(ev.x, ev.y));
    if let Err(e) = app.session.pointer(&ev) {
        app.toast(e.to_string());
    }
    app.handle_ui_requests();
}

/// Right-click at screen `pos`: select what's under the pointer (`deep`: the nested layer), then
/// open the context menu there.
pub fn open_context_menu(app: &mut FigApp, pos: Pos2, deep: bool) {
    let p = app.to_page(pos);
    if let Ok(ds) = app.session.ds() {
        let (doc, page, sel, zoom) = (ds.doc.clone(), ds.page, ds.selection.clone(), ds.view.zoom.max(1e-6));
        if let Some(leaf) = app.session.hit(&doc, page, p, 2.0 / zoom) {
            let target = figcraft_doc::geometry::select_target(&doc, leaf, &sel, deep);
            if !sel.contains(&target) {
                app.session.set_selection(vec![target]);
            }
        }
    }
    app.ui.context_menu = Some((pos, app.session.selection()));
    app.ui.context_menu_shown = false;
}
