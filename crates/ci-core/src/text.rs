//! Text position ↔ byte-offset conversion — the one place the edit path turns a
//! [`Range`](crate::Range)'s (line, column) into a byte index into the file.
//!
//! **Column convention: 0-based UTF-8 *byte* offset within the line.** This matches tree-sitter
//! (`Point.column` is a byte offset) and the VFS, which slices `&str` by byte index — so a
//! tree-sitter sub-node range edits correctly even on a line with multi-byte characters before it.
//! For the ASCII that code overwhelmingly is, a byte column and a character column coincide, so
//! nothing changes; the distinction only matters on a line with non-ASCII text before the column.
//!
//! (SCIP and LSP measure columns differently — SCIP in UTF-8 bytes, LSP in UTF-16 code units — but
//! their ranges are consumed at their own boundaries; the edit path's internal contract is bytes.)

/// Byte offset of a `(1-based line, 0-based UTF-8-byte column)` position in `content`, or `None`
/// if the line/column is out of range. A position one line past the last, at column 0, is allowed
/// (end-of-file), so an edit can target the very end of a file.
pub fn byte_offset(content: &str, line_1: u32, col_0: u32) -> Option<usize> {
    if line_1 == 0 {
        return None;
    }
    let mut off = 0usize;
    let mut line_no = 1u32;
    for l in content.split_inclusive('\n') {
        if line_no == line_1 {
            // `col_0` is a byte column; clamp to the line's own bytes (excluding the trailing
            // '\n' would over-restrict multi-line-agnostic callers, so allow up to the full line).
            let line_bytes = l.len();
            return Some(off + (col_0 as usize).min(line_bytes));
        }
        off += l.len();
        line_no += 1;
    }
    // Allow a position at EOF (one line past the last, column 0).
    if line_no == line_1 && col_0 == 0 {
        Some(off)
    } else {
        None
    }
}

/// Byte offset where each line begins (line 0 at 0, then just past each `\n`) — maps a
/// `(line, column)` span to an absolute content offset (movefix uses it to test spans against
/// string/comment masking extents). CRLF-safe: only line STARTS are recorded, and a column is
/// start-relative either way.
pub fn line_start_offsets(content: &str) -> Vec<usize> {
    std::iter::once(0).chain(content.match_indices('\n').map(|(i, _)| i + 1)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_offsets_are_plain_indices() {
        let s = "ab\ncde\n";
        assert_eq!(byte_offset(s, 1, 0), Some(0));
        assert_eq!(byte_offset(s, 1, 2), Some(2)); // end of "ab"
        assert_eq!(byte_offset(s, 2, 1), Some(4)); // 'd' in "cde"
        assert_eq!(byte_offset(s, 3, 0), Some(7)); // EOF (one past last line)
        assert_eq!(byte_offset(s, 0, 0), None);
    }

    #[test]
    fn column_is_a_byte_offset_not_a_char_count() {
        // "é" is 2 bytes (U+00E9 → 0xC3 0xA9). tree-sitter reports the column AFTER it as byte 2,
        // and the byte offset of that column must be 2 — a char-counting impl would return 1 and
        // slice mid-character. This is the non-ASCII bug the shared util fixes.
        let s = "é = 1\n"; // bytes: C3 A9 20 3D 20 31 0A
        assert_eq!(byte_offset(s, 1, 2), Some(2), "byte column past a 2-byte char is byte 2");
        assert_eq!(byte_offset(s, 1, 3), Some(3), "the space after 'é'");
        // Slicing at the returned offset lands on a valid char boundary (would panic otherwise).
        let at = byte_offset(s, 1, 2).unwrap();
        assert_eq!(&s[at..at + 1], " ");
    }

    #[test]
    fn column_past_line_end_clamps_to_line() {
        // A column beyond the line's bytes clamps to the line end (before '\n'), never past it.
        let s = "ab\ncd\n";
        assert_eq!(byte_offset(s, 1, 99), Some(3)); // end of "ab\n" run is byte 3 (the '\n')
    }
}
