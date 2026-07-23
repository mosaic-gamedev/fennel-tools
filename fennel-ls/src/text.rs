/// LSP position ↔ byte-offset conversion utilities.
///
/// LSP positions are (0-based line, 0-based UTF-16 character index).
/// Internally we work with byte offsets into the file's UTF-8 text.

use tower_lsp::lsp_types::{Position, Range};
use crate::lexer::Span;

/// Convert a byte offset in `text` to an LSP Position (UTF-16 character units).
pub fn byte_to_position(text: &str, byte: usize) -> Position {
    let byte = byte.min(text.len());
    let prefix = &text[..byte];
    let mut line = 0u32;
    let mut last_nl = 0usize;

    for (i, ch) in prefix.char_indices() {
        if ch == '\n' {
            line += 1;
            last_nl = i + 1;
        }
    }

    // Count UTF-16 code units on the current line
    let line_text = &prefix[last_nl..];
    let character = utf16_len(line_text) as u32;

    Position { line, character }
}

/// Convert an LSP Position to a byte offset in `text`.
/// Returns the byte offset of the start of the character, or None if out of bounds.
pub fn position_to_byte(text: &str, pos: Position) -> Option<usize> {
    let mut current_line = 0u32;
    let mut line_start = 0usize;

    for (i, ch) in text.char_indices() {
        if current_line == pos.line {
            break;
        }
        if ch == '\n' {
            current_line += 1;
            line_start = i + ch.len_utf8();
        }
    }

    if current_line != pos.line {
        if pos.line == 0 {
            line_start = 0;
        } else {
            return None;
        }
    }

    // Now walk `pos.character` UTF-16 code units along the line
    let line_text = &text[line_start..];
    let mut u16_count = 0u32;
    let mut byte_off = 0usize;

    for ch in line_text.chars() {
        if u16_count >= pos.character {
            break;
        }
        u16_count += ch.len_utf16() as u32;
        byte_off += ch.len_utf8();
    }

    Some(line_start + byte_off)
}

/// Convert a Span (byte offsets) to an LSP Range.
pub fn span_to_range(text: &str, span: &Span) -> Range {
    Range {
        start: byte_to_position(text, span.start as usize),
        end: byte_to_position(text, span.end as usize),
    }
}

/// Number of UTF-16 code units needed to encode `s`.
fn utf16_len(s: &str) -> usize {
    s.chars().map(|c| c.len_utf16()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower_lsp::lsp_types::Position;

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    // ── byte_to_position ─────────────────────────────────────────────────────

    #[test]
    fn byte_to_position_single_line_ascii() {
        let text = "hello";
        assert_eq!(byte_to_position(text, 0), pos(0, 0));
        assert_eq!(byte_to_position(text, 3), pos(0, 3));
        assert_eq!(byte_to_position(text, 5), pos(0, 5));
    }

    #[test]
    fn byte_to_position_multiline() {
        // "hello\nworld"
        //  01234 5 6789...
        let text = "hello\nworld";
        assert_eq!(byte_to_position(text, 0),  pos(0, 0)); // h
        assert_eq!(byte_to_position(text, 5),  pos(0, 5)); // \n
        assert_eq!(byte_to_position(text, 6),  pos(1, 0)); // w
        assert_eq!(byte_to_position(text, 10), pos(1, 4)); // d
    }

    #[test]
    fn byte_to_position_three_lines() {
        let text = "a\nb\nc";
        assert_eq!(byte_to_position(text, 0), pos(0, 0)); // a
        assert_eq!(byte_to_position(text, 2), pos(1, 0)); // b
        assert_eq!(byte_to_position(text, 4), pos(2, 0)); // c
    }

    #[test]
    fn byte_to_position_past_end_clamped() {
        let text = "hi";
        // Byte past end is clamped to text.len()
        assert_eq!(byte_to_position(text, 999), pos(0, 2));
    }

    #[test]
    fn byte_to_position_empty_text() {
        assert_eq!(byte_to_position("", 0), pos(0, 0));
    }

    #[test]
    fn byte_to_position_wide_utf16_char() {
        // "a🎉b" — 🎉 is U+1F389, 4 UTF-8 bytes, 2 UTF-16 code units
        // bytes: a=0, 🎉=1..4, b=5
        let text = "a🎉b";
        assert_eq!(byte_to_position(text, 0), pos(0, 0)); // a  → char 0
        assert_eq!(byte_to_position(text, 1), pos(0, 1)); // 🎉 → char 1 (start)
        assert_eq!(byte_to_position(text, 5), pos(0, 3)); // b  → char 3 (🎉 takes 2 units)
    }

    #[test]
    fn byte_to_position_bmp_multibyte() {
        // "é" is U+00E9, 2 UTF-8 bytes but only 1 UTF-16 unit (BMP)
        // "aéb": a=0, é=1..2, b=3
        let text = "aéb";
        assert_eq!(byte_to_position(text, 0), pos(0, 0)); // a
        assert_eq!(byte_to_position(text, 1), pos(0, 1)); // é → char 1
        assert_eq!(byte_to_position(text, 3), pos(0, 2)); // b → char 2 (é = 1 UTF-16 unit)
    }

    // ── position_to_byte ─────────────────────────────────────────────────────

    #[test]
    fn position_to_byte_single_line_ascii() {
        let text = "hello";
        assert_eq!(position_to_byte(text, pos(0, 0)), Some(0));
        assert_eq!(position_to_byte(text, pos(0, 3)), Some(3));
        assert_eq!(position_to_byte(text, pos(0, 5)), Some(5));
    }

    #[test]
    fn position_to_byte_multiline() {
        let text = "hello\nworld";
        assert_eq!(position_to_byte(text, pos(0, 0)), Some(0));
        assert_eq!(position_to_byte(text, pos(1, 0)), Some(6));
        assert_eq!(position_to_byte(text, pos(1, 4)), Some(10));
    }

    #[test]
    fn position_to_byte_out_of_range_line() {
        let text = "hello";
        assert_eq!(position_to_byte(text, pos(5, 0)), None);
    }

    #[test]
    fn position_to_byte_wide_utf16_char() {
        // "a🎉b" — 🎉 takes 2 UTF-16 code units
        let text = "a🎉b";
        assert_eq!(position_to_byte(text, pos(0, 0)), Some(0)); // a
        assert_eq!(position_to_byte(text, pos(0, 1)), Some(1)); // start of 🎉
        assert_eq!(position_to_byte(text, pos(0, 3)), Some(5)); // b
    }

    #[test]
    fn position_to_byte_bmp_multibyte() {
        let text = "aéb";
        assert_eq!(position_to_byte(text, pos(0, 0)), Some(0));
        assert_eq!(position_to_byte(text, pos(0, 1)), Some(1)); // é at byte 1
        assert_eq!(position_to_byte(text, pos(0, 2)), Some(3)); // b at byte 3
    }

    // ── Round-trip ────────────────────────────────────────────────────────────

    #[test]
    fn round_trip_byte_position_byte() {
        let text = "hello\nwörld\n🎉";
        // Every char boundary should survive a byte→position→byte round-trip.
        let mut byte = 0;
        for ch in text.chars() {
            let p = byte_to_position(text, byte);
            let back = position_to_byte(text, p).expect("round-trip must succeed");
            assert_eq!(back, byte, "round-trip failed at byte {byte} (char {ch:?})");
            byte += ch.len_utf8();
        }
        // Also check byte == text.len() (past last char)
        let p = byte_to_position(text, text.len());
        let back = position_to_byte(text, p).expect("round-trip at end must succeed");
        assert_eq!(back, text.len());
    }

    // ── span_to_range ────────────────────────────────────────────────────────

    #[test]
    fn span_to_range_basic() {
        use crate::lexer::Span;
        let text = "hello world";
        let span = Span { start: 6, end: 11, line: 0, col: 6, end_line: 0, end_col: 11 };
        let range = span_to_range(text, &span);
        assert_eq!(range.start, pos(0, 6));
        assert_eq!(range.end,   pos(0, 11));
    }

    #[test]
    fn span_to_range_multiline() {
        use crate::lexer::Span;
        // "line1\nline2" — span from byte 0 to byte 11
        let text = "line1\nline2";
        let span = Span { start: 0, end: 11, line: 0, col: 0, end_line: 1, end_col: 5 };
        let range = span_to_range(text, &span);
        assert_eq!(range.start, pos(0, 0));
        assert_eq!(range.end,   pos(1, 5));
    }

    // ── CRLF ─────────────────────────────────────────────────────────────────

    #[test]
    fn byte_to_position_crlf_first_line() {
        // "hello\r\nworld" — \r is part of line 0, \n is the line separator.
        // byte_to_position splits on \n only, so \r counts as a column.
        let text = "hello\r\nworld";
        assert_eq!(byte_to_position(text, 0), pos(0, 0)); // h
        assert_eq!(byte_to_position(text, 4), pos(0, 4)); // o
        assert_eq!(byte_to_position(text, 5), pos(0, 5)); // \r (part of line 0)
        assert_eq!(byte_to_position(text, 6), pos(0, 6)); // \n (still line 0 in prefix)
        assert_eq!(byte_to_position(text, 7), pos(1, 0)); // w (first char of line 1)
    }

    #[test]
    fn position_to_byte_crlf() {
        let text = "hello\r\nworld";
        assert_eq!(position_to_byte(text, pos(0, 0)), Some(0)); // h
        assert_eq!(position_to_byte(text, pos(0, 5)), Some(5)); // \r
        assert_eq!(position_to_byte(text, pos(1, 0)), Some(7)); // w
        assert_eq!(position_to_byte(text, pos(1, 4)), Some(11)); // d
    }

    #[test]
    fn round_trip_crlf_file() {
        // Every char boundary in a CRLF file must survive byte→position→byte.
        let text = "fn greet\r\n  \"hello\"\r\nend\r\n";
        let mut byte = 0;
        for ch in text.chars() {
            let p = byte_to_position(text, byte);
            let back = position_to_byte(text, p).expect("CRLF round-trip must succeed");
            assert_eq!(back, byte, "CRLF round-trip failed at byte {byte} (char {ch:?})");
            byte += ch.len_utf8();
        }
    }

    // ── Non-ASCII in span_to_range (diagnostic positions) ────────────────────

    #[test]
    fn span_to_range_after_bmp_non_ascii() {
        use crate::lexer::Span;
        // "aéfoo" — é is U+00E9 (2 UTF-8 bytes, 1 UTF-16 unit)
        // `foo` starts at byte 3 (a=0, é=1..2, f=3)
        // In UTF-16: col of f = 2 (a=0→1, é=1→2, f=2→col 2)
        let text = "a\u{00E9}foo";
        let span = Span { start: 3, end: 6, line: 0, col: 3, end_line: 0, end_col: 6 };
        let range = span_to_range(text, &span);
        // byte_to_position uses utf16_len, so start.character must be 2 (not 3)
        assert_eq!(range.start, pos(0, 2),
            "span_to_range start must use UTF-16 offset: got {:?}", range.start);
        assert_eq!(range.end, pos(0, 5),
            "span_to_range end must use UTF-16 offset: got {:?}", range.end);
    }

    #[test]
    fn span_to_range_after_supplementary_plane_char() {
        use crate::lexer::Span;
        // "🎉foo" — 🎉 is U+1F389 (4 UTF-8 bytes, 2 UTF-16 units)
        // `foo` starts at byte 4
        // UTF-16 col of f = 2 (🎉 takes 2 UTF-16 units)
        let text = "\u{1F389}foo";
        let span = Span { start: 4, end: 7, line: 0, col: 4, end_line: 0, end_col: 7 };
        let range = span_to_range(text, &span);
        assert_eq!(range.start, pos(0, 2),
            "span_to_range must count supplementary-plane char as 2 UTF-16 units: got {:?}", range.start);
        assert_eq!(range.end, pos(0, 5), "got {:?}", range.end);
    }
}
