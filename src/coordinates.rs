//! Graph positions to edit-plan positions.
//!
//! These are two different coordinate systems and nothing warns you when they are confused:
//! the graph records a **1-based byte column**, the edit plan carries a **0-based UTF-16
//! character offset**. They agree on every ASCII line, which is exactly why a mix-up survives
//! testing and then corrupts the first file with an accented name or an emoji in a comment.
//!
//! Every conversion here refuses rather than clamps. A column past the end of its line means the
//! graph no longer describes the file, and silently landing an edit at the nearest valid offset
//! would write bytes nobody planned.

/// A position that could not be converted, with the reason an agent can act on.
#[derive(Debug, PartialEq, Eq)]
pub enum PositionError {
    /// The file has no such line.
    LineOutOfRange { line: u32, lines: usize },
    /// The line is shorter than the column claims.
    ColumnOutOfRange {
        line: u32,
        column: u32,
        bytes: usize,
    },
    /// The byte column falls inside a multi-byte character.
    NotACharBoundary { line: u32, column: u32 },
}

impl std::fmt::Display for PositionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LineOutOfRange { line, lines } => write!(
                formatter,
                "the graph points at line {line} but the file has {lines}; rebuild the graph"
            ),
            Self::ColumnOutOfRange {
                line,
                column,
                bytes,
            } => write!(
                formatter,
                "the graph points at byte column {column} on line {line}, which is {bytes} bytes \
                 long; rebuild the graph"
            ),
            Self::NotACharBoundary { line, column } => write!(
                formatter,
                "byte column {column} on line {line} falls inside a character; the graph and the \
                 file disagree"
            ),
        }
    }
}

/// The 0-based UTF-16 offset of a 1-based byte column on a 1-based line.
///
/// # Errors
///
/// Returns the position error describing how the graph and the file disagree.
pub fn utf16_offset(text: &str, line: u32, byte_column: u32) -> Result<u32, PositionError> {
    let lines = text.split('\n').collect::<Vec<_>>();
    let index = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
    let Some(source) = lines.get(index) else {
        return Err(PositionError::LineOutOfRange {
            line,
            lines: lines.len(),
        });
    };
    // A trailing \r belongs to the line separator, not to the text a column can point into.
    let source = source.strip_suffix('\r').unwrap_or(source);
    let offset = usize::try_from(byte_column.saturating_sub(1)).unwrap_or(usize::MAX);
    if offset > source.len() {
        return Err(PositionError::ColumnOutOfRange {
            line,
            column: byte_column,
            bytes: source.len(),
        });
    }
    if !source.is_char_boundary(offset) {
        return Err(PositionError::NotACharBoundary {
            line,
            column: byte_column,
        });
    }
    let units = source[..offset]
        .chars()
        .map(|character| u32::try_from(character.len_utf16()).unwrap_or(1))
        .sum();
    Ok(units)
}

/// The UTF-16 length of a whole line, for an edit that ends where the line does.
///
/// # Errors
///
/// Returns `LineOutOfRange` when the file has no such line.
pub fn utf16_line_length(text: &str, line: u32) -> Result<u32, PositionError> {
    let lines = text.split('\n').collect::<Vec<_>>();
    let index = usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX);
    let Some(source) = lines.get(index) else {
        return Err(PositionError::LineOutOfRange {
            line,
            lines: lines.len(),
        });
    };
    let source = source.strip_suffix('\r').unwrap_or(source);
    Ok(source
        .chars()
        .map(|character| u32::try_from(character.len_utf16()).unwrap_or(1))
        .sum())
}

/// The exact text between two graph positions, or `None` when either does not convert.
#[must_use]
pub fn slice_between(text: &str, start: (u32, u32), end: (u32, u32)) -> Option<String> {
    let start_offset = byte_offset(text, start.0, start.1)?;
    let end_offset = byte_offset(text, end.0, end.1)?;
    if start_offset > end_offset {
        return None;
    }
    text.get(start_offset..end_offset).map(ToOwned::to_owned)
}

/// Absolute byte offset of a 1-based line and 1-based byte column.
fn byte_offset(text: &str, line: u32, byte_column: u32) -> Option<usize> {
    let mut consumed = 0_usize;
    for (number, source) in text.split('\n').enumerate() {
        let current = u32::try_from(number + 1).unwrap_or(u32::MAX);
        if current == line {
            let offset = usize::try_from(byte_column.saturating_sub(1)).ok()?;
            let trimmed = source.strip_suffix('\r').unwrap_or(source);
            if offset > trimmed.len() || !trimmed.is_char_boundary(offset) {
                return None;
            }
            return Some(consumed + offset);
        }
        consumed += source.len() + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{PositionError, slice_between, utf16_line_length, utf16_offset};

    #[test]
    fn ascii_columns_convert_to_the_offset_one_less() {
        let text = "export function resolveTarget(input) {\n";
        // "export function " is 16 bytes, so the identifier starts at byte column 17.
        assert_eq!(utf16_offset(text, 1, 17), Ok(16));
        assert_eq!(utf16_offset(text, 1, 1), Ok(0));
    }

    #[test]
    fn a_multi_byte_prefix_shortens_the_utf16_offset() {
        // "héllo " is 7 bytes but 6 UTF-16 units: the accented character is two bytes, one unit.
        let text = "héllo world\n";
        assert_eq!(utf16_offset(text, 1, 8), Ok(6));
    }

    #[test]
    fn a_surrogate_pair_counts_as_two_utf16_units() {
        // `let x = "` is 9 bytes, so the emoji occupies bytes 10..13 and the quote after it
        // starts at byte column 14. Four bytes in, two UTF-16 units out.
        let text = "let x = \"🎯\" // done\n";
        let before_emoji = utf16_offset(text, 1, 10).expect("byte column before the emoji");
        let after_emoji = utf16_offset(text, 1, 14).expect("byte column after the emoji");
        assert_eq!(before_emoji, 9);
        assert_eq!(after_emoji - before_emoji, 2);
    }

    #[test]
    fn a_column_inside_a_character_is_refused_not_rounded() {
        let text = "héllo\n";
        assert_eq!(
            utf16_offset(text, 1, 3),
            Err(PositionError::NotACharBoundary { line: 1, column: 3 })
        );
    }

    #[test]
    fn a_column_past_the_line_is_refused() {
        let text = "one\ntwo\n";
        assert!(matches!(
            utf16_offset(text, 1, 99),
            Err(PositionError::ColumnOutOfRange { .. })
        ));
    }

    #[test]
    fn a_line_past_the_file_is_refused() {
        let text = "one\n";
        assert!(matches!(
            utf16_offset(text, 9, 1),
            Err(PositionError::LineOutOfRange { .. })
        ));
    }

    #[test]
    fn carriage_returns_belong_to_the_separator_not_the_line() {
        let text = "one\r\ntwo\r\n";
        assert_eq!(utf16_line_length(text, 1), Ok(3));
        assert_eq!(utf16_offset(text, 1, 4), Ok(3));
    }

    #[test]
    fn slicing_returns_the_exact_source_between_two_positions() {
        let text = "pub fn one() -> u32 {\n    1\n}\n";
        assert_eq!(slice_between(text, (1, 8), (1, 11)), Some("one".to_owned()));
        assert_eq!(
            slice_between(text, (1, 1), (3, 2)),
            Some("pub fn one() -> u32 {\n    1\n}".to_owned())
        );
    }

    #[test]
    fn slicing_a_multi_byte_line_returns_characters_not_bytes() {
        let text = "let café = 1\n";
        assert_eq!(
            slice_between(text, (1, 5), (1, 10)),
            Some("café".to_owned())
        );
    }

    #[test]
    fn an_inverted_range_slices_to_nothing() {
        let text = "one two\n";
        assert_eq!(slice_between(text, (1, 5), (1, 2)), None);
    }
}
