use super::*;

impl Session {
    pub fn ds(&self) -> Result<&DocState> {
        self.docs.get(self.active).ok_or(EngineError::NoDocument)
    }

    pub fn ds_mut(&mut self) -> Result<&mut DocState> {
        self.docs.get_mut(self.active).ok_or(EngineError::NoDocument)
    }

    pub fn doc(&self) -> Result<&Document> {
        Ok(self.ds()?.doc.as_ref())
    }

    pub fn doc_mut(&mut self) -> Result<&mut Document> {
        Ok(self.ds_mut()?.doc_mut())
    }

    pub fn selection(&self) -> Vec<NodeId> {
        self.ds().map(|d| d.selection.clone()).unwrap_or_default()
    }

    pub fn set_selection(&mut self, ids: Vec<NodeId>) {
        if let Ok(ds) = self.ds_mut() {
            ds.selection = ids;
            ds.clean_selection();
            ds.revision += 1;
        }
    }

    pub fn page(&self) -> Result<NodeId> {
        Ok(self.ds()?.page)
    }

    /// A summary for status displays.
    pub fn status(&self) -> Value {
        match self.ds() {
            Ok(ds) => json!({
                "document": ds.doc.name,
                "path": ds.path,
                "dirty": ds.is_dirty(),
                "page": ds.page,
                "selection": ds.selection,
                "tool": self.tool.tool.id(),
                "zoom": ds.view.zoom,
                "undo": self.undo_depth().0,
                "redo": self.undo_depth().1,
                "documents": self.docs.len(),
            }),
            Err(_) => json!({"documents": 0}),
        }
    }
}
