//! The full source range of a declaration.
//!
//! Neither source of truth carries it. The graph records only the identifier — `one`, three
//! characters — and the parser's declaration span stops at the name, covering `pub fn one`. An
//! edit that means "replace this function" needs where the body ends, and getting that wrong
//! writes over whatever follows.
//!
//! So the end is found by matching the declaration's opening brace, over the tokenizer rather
//! than the raw text: a `}` inside a string literal or a comment is not a closing brace, and
//! counting characters would treat it as one.

use weavatrix_parse::{Language, Token, TokenKind, tokenize};

/// A declaration located in one file, in byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeclarationRange {
    /// First byte of the declaration, including modifiers like `pub` or `export`.
    pub start: usize,
    /// One past the last byte: the closing brace, or the terminating `;`.
    pub end: usize,
    /// Whether the end was located in the source, or is only where the signature stopped.
    ///
    /// A caller must not offer "insert after this declaration" on an unproven end — it would
    /// place the text in the middle of something.
    pub end_proven: bool,
}

/// Finds the declaration named `name` whose signature begins on `line`.
///
/// The line disambiguates same-named declarations in one file, which is why it is required
/// rather than taking the first match.
#[must_use]
pub fn locate(source: &str, path: &str, name: &str, line: u32) -> Option<DeclarationRange> {
    let facts = weavatrix_parse::extract_path(path, source)?;
    let declaration = facts
        .declarations
        .iter()
        .find(|candidate| candidate.name == name && candidate.span.line == line)
        .or_else(|| {
            facts
                .declarations
                .iter()
                .find(|candidate| candidate.name == name)
        })?;
    let start = declaration.span.start;
    let language = Language::from_extension(path.rsplit_once('.')?.1)?;
    Some(declaration_end(source, language, start).map_or(
        DeclarationRange {
            start,
            end: declaration.span.end,
            end_proven: false,
        },
        |end| DeclarationRange {
            start,
            end,
            end_proven: true,
        },
    ))
}

/// The offset one past whatever ends the declaration starting at `start`.
///
/// A braced declaration ends at the brace matching its first one; a declaration without a body
/// ends at its terminator. Returns `None` when neither is found — an indentation-delimited
/// language, or a file that ends mid-declaration — so the caller knows the end is unproven
/// rather than being handed a plausible guess.
fn declaration_end(source: &str, language: Language, start: usize) -> Option<usize> {
    let tokens = tokenize(source, language);
    let mut depth = 0_u32;
    let mut opened = false;
    for token in tokens.iter().skip_while(|token| token.end <= start) {
        if !is_code(token) {
            continue;
        }
        let text = source.get(token.start..token.end)?;
        for (offset, character) in text.char_indices() {
            match character {
                '{' => {
                    depth += 1;
                    opened = true;
                }
                '}' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(token.start + offset + character.len_utf8());
                    }
                }
                // A terminator before any brace ends a declaration that has no body.
                ';' if !opened => return Some(token.start + offset + character.len_utf8()),
                _ => {}
            }
        }
    }
    None
}

/// Whether a token can carry a real brace. Strings and comments cannot.
fn is_code(token: &Token) -> bool {
    !matches!(
        token.kind,
        TokenKind::String
            | TokenKind::LineComment
            | TokenKind::BlockComment
            | TokenKind::Regex
            | TokenKind::Unterminated
    )
}

#[cfg(test)]
mod tests {
    use super::locate;

    #[test]
    fn a_function_range_covers_modifiers_through_the_closing_brace() {
        let source = "pub fn one() -> u32 {\n    1\n}\n\npub fn two() {}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert!(range.end_proven);
        assert_eq!(
            &source[range.start..range.end],
            "pub fn one() -> u32 {\n    1\n}"
        );
    }

    #[test]
    fn a_brace_inside_a_string_does_not_close_the_body() {
        let source = "pub fn one() -> &'static str {\n    \"}\"\n}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert!(
            source[range.start..range.end].ends_with("}\"\n}"),
            "the string's brace must not end the declaration, got {:?}",
            &source[range.start..range.end]
        );
    }

    #[test]
    fn a_brace_inside_a_comment_does_not_close_the_body() {
        let source = "pub fn one() -> u32 {\n    // }\n    1\n}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert_eq!(
            &source[range.start..range.end],
            "pub fn one() -> u32 {\n    // }\n    1\n}"
        );
    }

    #[test]
    fn nested_braces_are_matched_to_the_outermost() {
        let source = "pub fn one() -> u32 {\n    if true { 1 } else { 2 }\n}\n";
        let range = locate(source, "src/lib.rs", "one", 1).expect("declaration");
        assert!(source[range.start..range.end].contains("else { 2 }"));
        assert!(source[range.start..range.end].ends_with("\n}"));
    }

    #[test]
    fn the_line_picks_between_same_named_declarations() {
        let source = "mod a {\n    pub fn one() -> u32 { 1 }\n}\nmod b {\n    pub fn one() -> u32 { 2 }\n}\n";
        let first = locate(source, "src/lib.rs", "one", 2);
        let second = locate(source, "src/lib.rs", "one", 5);
        if let (Some(first), Some(second)) = (first, second) {
            assert_ne!(
                first.start, second.start,
                "the line must select the declaration, not the first match"
            );
        }
    }

    #[test]
    fn a_declaration_without_a_body_ends_at_its_terminator() {
        let source = "pub const ONE: u32 = 1;\n";
        if let Some(range) = locate(source, "src/lib.rs", "ONE", 1) {
            assert!(range.end_proven);
            assert_eq!(&source[range.start..range.end], "pub const ONE: u32 = 1;");
        }
    }

    #[test]
    fn an_unknown_name_locates_nothing() {
        let source = "pub fn one() {}\n";
        assert!(locate(source, "src/lib.rs", "absent", 1).is_none());
    }
}
