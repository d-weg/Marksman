/// What kind of document a hit points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Source,
    Doc,
    Config,
}

/// One indexed document: the unit every stage passes around.
#[derive(Debug, Clone)]
pub struct DocEntry {
    pub name: String,
    pub path: String,
    pub score: f32,
    pub kind: EntryKind,
}

impl DocEntry {
    pub fn display(&self) -> String {
        format!("{} ({})", self.name, self.path)
    }
}
