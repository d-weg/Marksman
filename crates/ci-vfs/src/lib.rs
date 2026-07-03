//! ci-vfs — a virtual file system overlay that makes an edit batch a transaction.
//!
//! Edits stage into an in-memory overlay (disk is untouched). The gate (LSP
//! diagnostics over the overlay buffers) decides: [`Vfs::commit`] flushes the
//! overlay to disk, or you simply drop the `Vfs` to roll back (nothing was written).
//! This is the write-side spine: stage → gate → commit-or-discard, atomic.
use ci_core::text::byte_offset;
use ci_core::{Error, Range, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone)]
enum FileState {
    Present(String),
    Deleted,
}

pub struct Vfs {
    root: PathBuf,
    /// Pending changes keyed by repo-relative path. Absent ⇒ falls back to disk.
    overlay: HashMap<PathBuf, FileState>,
}

impl Vfs {
    pub fn new(root: &Path) -> Self {
        Self { root: root.to_path_buf(), overlay: HashMap::new() }
    }

    fn abs(&self, rel: &Path) -> PathBuf {
        self.root.join(rel)
    }

    /// Current content (overlay wins, else disk). `None` if deleted or absent.
    pub fn read(&self, rel: &Path) -> Option<String> {
        match self.overlay.get(rel) {
            Some(FileState::Present(s)) => Some(s.clone()),
            Some(FileState::Deleted) => None,
            None => std::fs::read_to_string(self.abs(rel)).ok(),
        }
    }

    pub fn write(&mut self, rel: &Path, content: String) {
        self.overlay.insert(rel.to_path_buf(), FileState::Present(content));
    }

    pub fn create(&mut self, rel: &Path, content: String) -> Result<()> {
        // A file created EARLIER IN THIS BATCH (overlay-present, nothing on disk — e.g. a
        // move_file that supplied its parent module file automatically) makes a redundant
        // create with the same intent SATISFIED, not an error: agents pair move_file with a
        // helper create_file, and rejecting the pair over our own automation cost real turns
        // (bench move-rust). Genuinely different content still conflicts, loudly.
        if !self.abs(rel).exists() {
            if let Some(FileState::Present(existing)) = self.overlay.get(rel) {
                if existing.trim() == content.trim() {
                    return Ok(());
                }
                return Err(Error::Other(format!(
                    "create: {} was already created by an earlier op in this batch (a move_file                      supplies its parent module file automatically) with DIFFERENT content — drop                      this create_file, or match what the batch already wrote:\n{existing}",
                    rel.display()
                )));
            }
        }
        if self.read(rel).is_some() {
            return Err(Error::Other(format!("create: {} already exists", rel.display())));
        }
        self.write(rel, content);
        Ok(())
    }

    pub fn delete(&mut self, rel: &Path) {
        self.overlay.insert(rel.to_path_buf(), FileState::Deleted);
    }

    pub fn move_file(&mut self, from: &Path, to: &Path) -> Result<()> {
        let content =
            self.read(from).ok_or_else(|| Error::Other(format!("move: {} missing", from.display())))?;
        self.delete(from);
        self.write(to, content);
        Ok(())
    }

    /// Replace the precise span `range` with `new_text`, resolving the 1-based-line /
    /// 0-based-byte-column bounds via [`ci_core::text::byte_offset`]. This is the primitive
    /// `replace_node` builds on.
    pub fn replace_range(&mut self, rel: &Path, range: &Range, new_text: &str) -> Result<()> {
        let content =
            self.read(rel).ok_or_else(|| Error::Other(format!("replace: {} missing", rel.display())))?;
        let start = byte_offset(&content, range.start_line, range.start_char)
            .ok_or_else(|| Error::Other("replace: start out of bounds".into()))?;
        let end = byte_offset(&content, range.end_line, range.end_char)
            .ok_or_else(|| Error::Other("replace: end out of bounds".into()))?;
        if end < start {
            return Err(Error::Other("replace: end before start".into()));
        }
        let mut out = String::with_capacity(content.len() - (end - start) + new_text.len());
        out.push_str(&content[..start]);
        out.push_str(new_text);
        out.push_str(&content[end..]);
        self.write(rel, out);
        Ok(())
    }

    /// Exact text of a span (e.g. a node's `enclosing_range`), overlay-aware.
    pub fn read_range(&self, rel: &Path, range: &Range) -> Option<String> {
        let content = self.read(rel)?;
        let s = byte_offset(&content, range.start_line, range.start_char)?;
        let e = byte_offset(&content, range.end_line, range.end_char)?;
        content.get(s..e).map(str::to_string)
    }

    /// Insert `text` immediately before `range` (e.g. a new declaration before a node).
    pub fn insert_before(&mut self, rel: &Path, range: &Range, text: &str) -> Result<()> {
        let content =
            self.read(rel).ok_or_else(|| Error::Other(format!("insert: {} missing", rel.display())))?;
        let at = byte_offset(&content, range.start_line, range.start_char)
            .ok_or_else(|| Error::Other("insert: position out of bounds".into()))?;
        let mut out = String::with_capacity(content.len() + text.len());
        out.push_str(&content[..at]);
        out.push_str(text);
        out.push_str(&content[at..]);
        self.write(rel, out);
        Ok(())
    }

    /// Paths touched by the transaction (for the diagnostics scope + reindex).
    pub fn changed(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.overlay.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn is_empty(&self) -> bool {
        self.overlay.is_empty()
    }

    /// Flush the overlay to disk — the transaction commits. Dropping the `Vfs`
    /// instead is the rollback (nothing was written before this call).
    pub fn commit(&self) -> Result<()> {
        for (rel, state) in &self.overlay {
            let abs = self.abs(rel);
            match state {
                FileState::Present(s) => {
                    if let Some(parent) = abs.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&abs, s)?;
                }
                FileState::Deleted => {
                    let _ = std::fs::remove_file(&abs);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn r(sl: u32, sc: u32, el: u32, ec: u32) -> Range {
        Range { start_line: sl, start_char: sc, end_line: el, end_char: ec }
    }

    #[test]
    fn overlay_read_write_without_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "hello\n").unwrap();
        let mut vfs = Vfs::new(root);

        assert_eq!(vfs.read(Path::new("a.ts")).as_deref(), Some("hello\n"));
        vfs.write(Path::new("a.ts"), "changed\n".into());
        assert_eq!(vfs.read(Path::new("a.ts")).as_deref(), Some("changed\n"));
        // disk is untouched until commit
        assert_eq!(fs::read_to_string(root.join("a.ts")).unwrap(), "hello\n");

        vfs.commit().unwrap();
        assert_eq!(fs::read_to_string(root.join("a.ts")).unwrap(), "changed\n");
    }

    #[test]
    fn replace_range_is_char_precise() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // "  return a + b;" — replace the body expression precisely.
        fs::write(root.join("m.ts"), "function add() {\n  return a + b;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        // line 2 (1-based), chars 9..14 = "a + b"
        vfs.replace_range(Path::new("m.ts"), &r(2, 9, 2, 14), "a + b + 1").unwrap();
        assert_eq!(
            vfs.read(Path::new("m.ts")).unwrap(),
            "function add() {\n  return a + b + 1;\n}\n"
        );
    }

    #[test]
    fn replace_range_on_a_line_with_multibyte_chars() {
        // A line with a 2-byte char ("é") before the edit column: tree-sitter reports byte columns,
        // so the range must resolve as bytes. Char-counting (the old bug) would slice mid-character
        // and either panic or corrupt the "é". Here `const café = "x";` — replace the string.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("m.ts"), "const café = \"x\";\n").unwrap();
        let mut vfs = Vfs::new(root);
        // byte columns: c=0..; "café" is 5 bytes (é=2), ` = ` then `"x"` at bytes 13..16.
        let line = "const café = \"x\";";
        let s = line.find("\"x\"").unwrap() as u32;
        vfs.replace_range(Path::new("m.ts"), &r(1, s, 1, s + 3), "\"yy\"").unwrap();
        assert_eq!(vfs.read(Path::new("m.ts")).unwrap(), "const café = \"yy\";\n");
    }

    #[test]
    fn replace_whole_node_via_enclosing_range() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("m.ts"), "function add() {\n  return 1;\n}\n").unwrap();
        let mut vfs = Vfs::new(root);
        // enclosing_range of add: line1 col0 .. line3 col1
        vfs.replace_range(Path::new("m.ts"), &r(1, 0, 3, 1), "function add() {\n  return 2;\n}").unwrap();
        assert_eq!(vfs.read(Path::new("m.ts")).unwrap(), "function add() {\n  return 2;\n}\n");
    }

    #[test]
    fn move_and_delete_and_changed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "A\n").unwrap();
        fs::write(root.join("b.ts"), "B\n").unwrap();
        let mut vfs = Vfs::new(root);

        vfs.move_file(Path::new("a.ts"), Path::new("c.ts")).unwrap();
        vfs.delete(Path::new("b.ts"));
        vfs.create(Path::new("d.ts"), "D\n".into()).unwrap();

        let changed = vfs.changed();
        assert!(changed.contains(&PathBuf::from("a.ts")));
        assert!(changed.contains(&PathBuf::from("c.ts")));

        vfs.commit().unwrap();
        assert!(!root.join("a.ts").exists(), "moved-from deleted");
        assert_eq!(fs::read_to_string(root.join("c.ts")).unwrap(), "A\n");
        assert!(!root.join("b.ts").exists(), "deleted");
        assert_eq!(fs::read_to_string(root.join("d.ts")).unwrap(), "D\n");
    }

    #[test]
    fn rollback_is_just_dropping_the_vfs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "orig\n").unwrap();
        {
            let mut vfs = Vfs::new(root);
            vfs.write(Path::new("a.ts"), "bad\n".into());
            // gate fails -> never commit; drop here
        }
        assert_eq!(fs::read_to_string(root.join("a.ts")).unwrap(), "orig\n");
    }
}
