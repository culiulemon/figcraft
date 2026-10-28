use super::connectors::{end_json, other_end};
use super::derived::ends_now;
use super::edit::label_of;
use super::items::edit_text;
use super::*;

// ---------------------------------------------------------------------------------------------
// The board tools.

impl Session {
    /// A press with a board tool: place a sticky, shape or stamp, start a connector or a stroke,
    /// or start erasing.
    pub(crate) fn board_down(&mut self, ev: &PointerEvent) -> Result<()> {
        // A board tool kept while a design file came to the front: back to Move for this press.
        if !is_board(self.doc()?) {
            self.tool.tool = ToolId::Move;
            return self.pointer(ev);
        }
        // A sticky still being typed in ends first, so the next object is an undo step of its own.
        if self.ds()?.text_edit.is_some() {
            crate::textedit::end(self)?;
        }
        match self.tool.tool {
            ToolId::Sticky => {
                let half = STICKY / 2.0;
                let r = self.execute("board.sticky", &json!({"x": ev.x - half, "y": ev.y - half}))?;
                self.board_placed();
                if let Some(id) = r.get("id").and_then(id_of) {
                    edit_text(self, id)?;
                }
                Ok(())
            }
            ToolId::BoardShape => {
                let half = SHAPE / 2.0;
                self.execute("board.shape", &json!({"x": ev.x - half, "y": ev.y - half}))?;
                self.board_placed();
                Ok(())
            }
            ToolId::Stamp => {
                // Stamps go on one after another: the tool stays.
                self.execute("board.stamp", &json!({"x": ev.x, "y": ev.y}))?;
                Ok(())
            }
            ToolId::Marker | ToolId::Highlighter => {
                self.tool.gesture = Gesture::Pencil { points: vec![ev.p()], pressure: vec![ev.pressure] };
                Ok(())
            }
            ToolId::Connector => {
                let from = self.end_at(ev);
                self.tool.gesture = Gesture::Connect { from, start: ev.p() };
                Ok(())
            }
            ToolId::Eraser => {
                self.begin_interaction("Erase")?;
                self.tool.gesture = Gesture::Erase { gone: Vec::new() };
                self.erase_at(ev)
            }
            _ => Ok(()),
        }
    }

    /// After placing an object: back to the Move tool unless tools stay on.
    fn board_placed(&mut self) {
        if !self.prefs.keep_tool {
            self.set_tool(ToolId::Move);
        }
    }

    /// What a connector end at the pointer attaches to: the object there, or the point.
    fn end_at(&self, ev: &PointerEvent) -> End {
        let hit = (|| {
            let doc = self.doc().ok()?;
            let leaf = self.hit(doc, self.page().ok()?, ev.p(), 2.0 / ev.zoom.max(1e-6))?;
            let t = figcraft_doc::geometry::select_target(doc, leaf, &[], false);
            let n = doc.node(t)?;
            (!matches!(n.board, Some(BoardItem::Connector(_)) | Some(BoardItem::Label { .. }) | Some(BoardItem::Ink))).then_some(t)
        })();
        match hit {
            Some(node) => End::Node { node, side: Side::Auto },
            None => End::Point { x: ev.x, y: ev.y },
        }
    }

    pub(crate) fn connect_drag(&mut self, ev: &PointerEvent, from: End, start: Point) {
        let a = match from {
            End::Point { x, y } => Point::new(x, y),
            End::Node { .. } => start,
        };
        self.tool.overlay.rubber = Some([a.x, a.y, ev.x, ev.y]);
        self.tool.overlay.drop_target = from.node();
        self.tool.gesture = Gesture::Connect { from, start };
        if let Ok(ds) = self.ds_mut() {
            ds.revision += 1;
        }
    }

    pub(crate) fn connect_up(&mut self, ev: &PointerEvent, from: End, start: Point) -> Result<()> {
        self.tool.overlay.rubber = None;
        self.tool.overlay.drop_target = None;
        // A click, not a drag: nothing to join.
        if (ev.p() - start).hypot() * ev.zoom.max(1e-6) < 4.0 {
            return Ok(());
        }
        let to = self.end_at(ev);
        if to.node().is_some() && to.node() == from.node() {
            return Ok(());
        }
        let style = self.tool.board.routing.id();
        self.execute("board.connector", &json!({"from": end_json(from), "to": end_json(to), "style": style}))?;
        self.board_placed();
        Ok(())
    }

    /// Dragging the selected connector `id`'s end (`from`: its start): a line from its other end
    /// to the pointer, and the object it would attach to outlined.
    pub(crate) fn reattach_drag(&mut self, ev: &PointerEvent, id: NodeId, from: bool, start: Point) {
        self.tool.gesture = Gesture::Reattach { id, from, start };
        let Ok(doc) = self.doc() else { return };
        let fixed = ends_now(doc, id).map(|(a, b)| if from { b } else { a });
        let other = other_end(doc, id, from).and_then(|e| e.node());
        self.tool.overlay.rubber = fixed.map(|f| [f.x, f.y, ev.x, ev.y]);
        self.tool.overlay.drop_target = self.end_at(ev).node().filter(|n| Some(*n) != other);
        if let Ok(ds) = self.ds_mut() {
            ds.revision += 1;
        }
    }

    /// The end let go: attached to the object under the pointer, or free there (`board.reattach`).
    /// A click, or a drop on the object the other end is on, changes nothing.
    pub(crate) fn reattach_up(&mut self, ev: &PointerEvent, id: NodeId, from: bool, start: Point) -> Result<()> {
        self.tool.overlay.rubber = None;
        if (ev.p() - start).hypot() * ev.zoom.max(1e-6) < 4.0 {
            return Ok(());
        }
        let target = self.end_at(ev);
        let other = other_end(self.doc()?, id, from).and_then(|e| e.node());
        if target.node().is_some() && target.node() == other {
            return Ok(());
        }
        self.execute("board.reattach", &json!({"id": id, "end": if from { "from" } else { "to" }, "target": end_json(target)}))?;
        Ok(())
    }

    /// The eraser at the pointer: the marker and highlighter strokes it touches go (until the
    /// gesture ends, which is one undo step).
    pub(crate) fn erase_at(&mut self, ev: &PointerEvent) -> Result<()> {
        let Gesture::Erase { mut gone } = std::mem::take(&mut self.tool.gesture) else { return Ok(()) };
        let tol = ERASER_PX / ev.zoom.max(1e-6);
        self.tool.overlay.brush = Some([ev.x, ev.y, tol]);
        let (doc, page) = (self.doc()?.clone(), self.page()?);
        if let Some(hit) =
            doc.hit_test(page, ev.p(), tol, None).filter(|h| doc.node(*h).is_some_and(|n| n.board == Some(BoardItem::Ink)) && !gone.contains(h))
        {
            gone.push(hit);
            let ids = gone.clone();
            self.preview(|d, sel| {
                for i in &ids {
                    let _ = d.remove(*i);
                }
                sel.retain(|s| !ids.contains(s));
                Ok(())
            })?;
        }
        self.tool.gesture = Gesture::Erase { gone };
        if let Ok(ds) = self.ds_mut() {
            ds.revision += 1;
        }
        Ok(())
    }

    pub(crate) fn erase_up(&mut self) -> Result<()> {
        self.tool.overlay.brush = None;
        self.commit_interaction()
    }

    /// A finished marker or highlighter stroke (page points).
    pub(crate) fn ink_finish(&mut self, path: &str) -> Result<()> {
        let high = self.tool.tool == ToolId::Highlighter;
        self.execute("board.ink", &json!({"path": path, "highlighter": high, "select": false}))?;
        // Strokes come one after another: the selection stays as it was.
        Ok(())
    }
}

impl Session {
    /// A double-click with the Move tool on a board object: type in its text (a connector gets a
    /// label to type in). True when it was one.
    pub(crate) fn board_double_click(&mut self, doc: &Document, leaf: NodeId, sel: &[NodeId]) -> Result<bool> {
        let whole = figcraft_doc::geometry::select_target(doc, leaf, sel, false);
        let editing = self.ds()?.text_edit.as_ref().is_some_and(|te| doc.is_ancestor(whole, te.id));
        let Some(item) = doc.node(whole).and_then(|n| n.board.clone()).filter(|_| !editing) else { return Ok(false) };
        self.tool.gesture = Gesture::None;
        self.cancel_interaction()?;
        match item {
            BoardItem::Connector(_) => {
                let label = match label_of(doc, whole) {
                    Some(l) => l,
                    None => {
                        let r = self.execute("board.label", &json!({"id": whole, "text": "Label"}))?;
                        r.get("label").and_then(id_of).ok_or_else(|| EngineError::Other("no label made".into()))?
                    }
                };
                edit_text(self, label)
            }
            BoardItem::Table => crate::board_table::edit_cell(self, doc, leaf),
            b if b.has_text() => edit_text(self, whole),
            _ => Ok(false),
        }
    }
}
