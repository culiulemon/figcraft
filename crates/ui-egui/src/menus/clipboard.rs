use super::{FigApp, dev_mode_refuses, json, trn};

/// Run a menu item: some open dialogs or use platform services, the rest go to the engine.
/// Paste: the system clipboard wins when it holds something newer than our own copy (layers copied
/// from another design app, an image, SVG or text from another app); otherwise our copied layers.
/// `event_text` is the text the platform handed over with the paste event (the browser has no
/// other way to read it). `at`: the page point to paste at (Paste here); `None` pastes in the
/// middle of the view.
/// In Dev Mode only a comment's link pastes (it opens the thread); everything else would edit.
pub fn paste(app: &mut FigApp, event_text: Option<&str>, at: Option<(f64, f64)>) {
    // A gesture injected through the control channel pastes only our own copied layers: it must not
    // read (and so expose) the user's system clipboard.
    if app.control_synthetic {
        // Synthetic input is user-like: Dev Mode refuses the paste here too, and only the engine's
        // own copied layers are pasted (the system clipboard is never read).
        if !dev_mode_refuses(app, "edit.paste") {
            app.run("edit.paste", at.map_or(json!({}), |(x, y)| json!({"x": x, "y": y})));
        }
        return;
    }
    // Only HTML that holds readable design data takes this path; other HTML pastes as before.
    if let Some(html) = app.services.clipboard_get_html().filter(|h| figcraft_engine::cmd::edit::design_clipboard_buffer(h).is_some()) {
        if dev_mode_refuses(app, "edit.paste") {
            return;
        }
        let mut params = json!({"html": html});
        if let Some((x, y)) = at {
            params["x"] = json!(x);
            params["y"] = json!(y);
        }
        if let Some(r) = app.run("edit.pasteDesignLayers", params) {
            // The pasted layers are now our copy: text another app puts on the clipboard later is newer.
            app.ui.last_copy = Some(app.services.clipboard_get_text().unwrap_or_default());
            app.ui.last_copy_generation = app.session.clipboard.layers_generation;
            let left_out = r["warnings"].as_array().map_or(0, Vec::len);
            if left_out > 0 {
                app.toast(trn("Pasted design layers; {n} kinds of content were left out", left_out, &[]));
            }
        }
        return;
    }
    let sys_text = app.services.clipboard_get_text().or_else(|| event_text.map(str::to_string)).filter(|t| !t.is_empty());
    // The text matching `last_copy` means the system clipboard still holds what we mirrored only
    // while the engine clipboard is the one we mirrored. A control-channel copy changes the engine
    // clipboard without writing the system clipboard, so its generation no longer matches and the
    // user's own text is pasted instead of the programmatic selection.
    let ours = sys_text.is_some() && sys_text == app.ui.last_copy && app.ui.last_copy_generation == app.session.clipboard.layers_generation;
    let (cx, cy) = at.or_else(|| app.session.ds().ok().map(|d| (d.view.cx, d.view.cy))).unwrap_or((0.0, 0.0));
    if !ours {
        if let Some((png, w, h)) = clipboard_png(app) {
            if dev_mode_refuses(app, "edit.paste") {
                return;
            }
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD.encode(png);
            // One selected layer that can have fills takes the image as its top fill.
            if let [target] = app.session.selection()[..]
                && at.is_none()
                && app.session.doc().ok().and_then(|d| d.node(target)).is_some_and(|n| figcraft_engine::cmd::create::accepts_image_fill(&n.kind))
            {
                app.run("image.place", json!({"data": b64, "target": target}));
                return;
            }
            // Centred in the view; Paste here puts its top-left at the point, as it does layers.
            let (x, y) = if at.is_some() { (cx, cy) } else { ((cx - f64::from(w) / 2.0).round(), (cy - f64::from(h) / 2.0).round()) };
            app.run("file.import", json!({"data": b64, "name": "Pasted image.png", "x": x, "y": y}));
            return;
        }
        // Not ours: any other text is newer (another app's text, SVG, a comment link) and wins.
        // Our own marker for copied layers without a text/SVG mirror is never a text paste; when
        // the engine copy moved on it falls through to `edit.paste` (the only layers we still have).
        if let Some(text) = sys_text.filter(|t| !is_clipboard_marker(t)) {
            return paste_text(app, &text, (cx, cy), at.is_none());
        }
    }
    if !dev_mode_refuses(app, "edit.paste") {
        app.run("edit.paste", at.map_or(json!({}), |(x, y)| json!({"x": x, "y": y})));
    }
}

/// The marker FigCraft puts on the system clipboard for copied layers with no text/SVG mirror.
fn is_clipboard_marker(text: &str) -> bool {
    text.starts_with("figcraft://clipboard/")
}

/// Paste text from another app at `(cx, cy)` (`center`: centred there): a comment's link opens its
/// thread, SVG imports, anything else becomes a text layer.
fn paste_text(app: &mut FigApp, text: &str, (cx, cy): (f64, f64), center: bool) {
    if text.trim().starts_with("figcraft://comment/") {
        app.run("comment.open", json!({"link": text.trim()}));
    } else if dev_mode_refuses(app, "edit.paste") {
        // Dev Mode: the note says why nothing was pasted.
    } else if figcraft_engine::looks_like_svg(text) {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        app.run_user("file.import", json!({"data": b64, "name": "Pasted SVG.svg", "x": cx, "y": cy, "center": center}));
    } else {
        app.run("text.create", json!({"text": text, "x": cx, "y": cy}));
    }
}

/// The system clipboard's image as PNG bytes, with its size.
fn clipboard_png(app: &mut FigApp) -> Option<(Vec<u8>, u32, u32)> {
    let (w, h, rgba) = app.services.clipboard_get_image()?;
    let img = image::RgbaImage::from_raw(w, h, rgba)?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some((out.into_inner(), w, h))
}

/// The text of a selection made only of text layers, one layer per line (None for any other
/// selection, or when there is no text).
pub(super) fn text_layers_text(doc: &figcraft_doc::Document, ids: &[figcraft_doc::NodeId]) -> Option<String> {
    let texts: Option<Vec<&str>> = ids.iter().map(|i| doc.node(*i).and_then(|n| n.kind.text()).map(|t| t.characters.as_str())).collect();
    texts.map(|t| t.join("\n")).filter(|t| !t.is_empty())
}

/// The layers as SVG text for other apps (None when the export fails).
pub(super) fn figcraft_svg_export(doc: &figcraft_doc::Document, ids: &[figcraft_doc::NodeId]) -> Option<String> {
    let bytes = figcraft_engine::cmd::file::export_bytes(doc, ids, figcraft_doc::ExportFormat::Svg, 1.0).ok()?;
    String::from_utf8(bytes).ok()
}
