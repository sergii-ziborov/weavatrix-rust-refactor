//! Splitting a parenthesised list into its top-level items.
//!
//! Both halves of `change_signature` need this: the declaration's parameters and each call site's
//! arguments. Splitting on commas is only correct at depth zero, and the depth that matters is
//! not just `()[]{}` — `Map<string, number>` holds a comma that separates nothing.
//!
//! Angle brackets are the hard case, because `<` is also less-than. A `<` is treated as opening a
//! generic only when a plausible `>` closes it with nothing statement-like in between. Real
//! comparisons inside a default value do not satisfy that; contrived ones (`a < b, c > d`) would,
//! and are accepted as the price of reading ordinary TypeScript.

use weavatrix_parse::{Token, TokenKind};

/// One item of a list, and the exact bytes it occupies.
#[derive(Debug, Clone)]
pub struct Item {
    pub start: usize,
    pub end: usize,
    pub text: String,
    /// Whether the item spreads a value: `...rest`, which is not a positional item at all.
    pub spread: bool,
}

/// A parsed `( ... )`, with the byte range of the parentheses themselves.
#[derive(Debug, Clone)]
pub struct List {
    pub items: Vec<Item>,
    pub open: usize,
    pub close: usize,
}

/// Tokens that carry program meaning; a comma inside a comment separates nothing.
pub fn is_code(token: &Token) -> bool {
    !matches!(
        token.kind,
        TokenKind::Whitespace
            | TokenKind::Newline
            | TokenKind::Indent
            | TokenKind::LineComment
            | TokenKind::BlockComment
            | TokenKind::Unterminated
    )
}

/// How far a generic argument list starting at `at` reaches, if it is one.
///
/// Bounded deliberately: an unclosed `<` in a long file would otherwise scan to the end and read
/// a comparison as a type.
fn generic_end(source: &str, code: &[&Token], at: usize) -> Option<usize> {
    const LOOKAHEAD: usize = 200;
    let mut angle = 0_i32;
    let mut nested = 0_i32;
    let end = code.len().min(at + LOOKAHEAD);
    for (offset, token) in code[at..end].iter().enumerate() {
        match token.text(source) {
            "<" => angle += 1,
            ">" => {
                angle -= 1;
                if angle == 0 && nested == 0 {
                    return Some(at + offset);
                }
            }
            "(" | "[" | "{" => nested += 1,
            ")" | "]" | "}" => {
                nested -= 1;
                // The enclosing list closed before the angle bracket did, so it was not one.
                if nested < 0 {
                    return None;
                }
            }
            ";" | "=>" => return None,
            _ => {}
        }
    }
    None
}

/// Builds one item from the bytes it spans.
///
/// Spread is read off the text rather than off a token, because a tokenizer is free to emit
/// `...` as three punctuation tokens and this must not depend on which it chose.
fn item(source: &str, start: usize, end: usize) -> Item {
    let text = source.get(start..end).unwrap_or_default().to_owned();
    let head = text.trim_start();
    Item {
        spread: head.starts_with("...") || head.starts_with('*'),
        start,
        end,
        text,
    }
}

/// Splits the list opened by `code[open_index]`, which must be a `(`.
///
/// Returns `None` when the parentheses do not close, because every caller's next step is to
/// delete or insert bytes and a guessed boundary would land them in the wrong place.
pub fn split(source: &str, code: &[&Token], open_index: usize) -> Option<List> {
    if code.get(open_index)?.text(source) != "(" {
        return None;
    }
    let open = code[open_index].start;
    let mut items: Vec<Item> = Vec::new();
    let mut depth = 0_i32;
    let mut start: Option<usize> = None;
    let mut end = open;
    let mut at = open_index + 1;

    while at < code.len() {
        let token = code[at];
        let text = token.text(source);
        match text {
            "(" | "[" | "{" => depth += 1,
            ")" | "]" | "}" if depth > 0 => depth -= 1,
            ")" => {
                if let Some(from) = start {
                    items.push(item(source, from, end));
                }
                return Some(List {
                    items,
                    open,
                    close: token.start,
                });
            }
            "," if depth == 0 => {
                if let Some(from) = start {
                    items.push(item(source, from, end));
                }
                start = None;
                at += 1;
                continue;
            }
            "<" => {
                if let Some(close) = generic_end(source, code, at) {
                    if start.is_none() {
                        start = Some(token.start);
                    }
                    end = code[close].end;
                    at = close + 1;
                    continue;
                }
            }
            _ => {}
        }
        if start.is_none() {
            start = Some(token.start);
        }
        end = token.end;
        at += 1;
    }
    None
}

/// The bytes to delete so that item `index` disappears, comma included.
///
/// The comma belongs to whichever neighbour survives, which is why this cannot be the item's own
/// range: deleting `b` from `(a, b)` has to take the comma before it, and from `(a, b, c)` the
/// comma after.
pub fn removal(items: &[Item], index: usize) -> Option<(usize, usize)> {
    let item = items.get(index)?;
    if let Some(next) = items.get(index + 1) {
        Some((item.start, next.start))
    } else if index > 0 {
        Some((items[index - 1].end, item.end))
    } else {
        Some((item.start, item.end))
    }
}

#[cfg(test)]
mod tests {
    use super::{is_code, removal, split};
    use weavatrix_parse::{Language, Token, tokenize};

    fn parse(source: &str) -> super::List {
        let tokens: Vec<Token> = tokenize(source, Language::TypeScript);
        let code: Vec<&Token> = tokens.iter().filter(|token| is_code(token)).collect();
        let open = code
            .iter()
            .position(|token| token.text(source) == "(")
            .expect("an open parenthesis");
        split(source, &code, open).expect("the list closes")
    }

    #[test]
    fn an_empty_list_has_no_items() {
        assert!(parse("f()").items.is_empty());
    }

    #[test]
    fn items_carry_their_exact_bytes() {
        let list = parse("f(alpha, beta)");
        let texts: Vec<&str> = list.items.iter().map(|item| item.text.as_str()).collect();
        assert_eq!(texts, ["alpha", "beta"]);
    }

    #[test]
    fn a_comma_inside_a_generic_separates_nothing() {
        let list = parse("f(map: Map<string, number>, flag: boolean)");
        assert_eq!(list.items.len(), 2, "{:?}", list.items);
        assert_eq!(list.items[0].text, "map: Map<string, number>");
    }

    #[test]
    fn a_comma_inside_a_nested_call_separates_nothing() {
        let list = parse("f(a = make(1, 2), b)");
        assert_eq!(list.items.len(), 2, "{:?}", list.items);
    }

    #[test]
    fn a_comma_inside_an_object_pattern_separates_nothing() {
        let list = parse("f({ one, two }, rest)");
        assert_eq!(list.items.len(), 2, "{:?}", list.items);
    }

    #[test]
    fn a_comma_inside_a_string_separates_nothing() {
        let list = parse("f('a, b', c)");
        assert_eq!(list.items.len(), 2, "{:?}", list.items);
    }

    #[test]
    fn a_spread_is_marked_rather_than_counted_as_ordinary() {
        let list = parse("f(a, ...rest)");
        assert!(!list.items[0].spread && list.items[1].spread);
    }

    #[test]
    fn a_less_than_in_a_default_is_not_read_as_a_generic() {
        let list = parse("f(a = x < 1 ? 2 : 3, b)");
        assert_eq!(list.items.len(), 2, "{:?}", list.items);
    }

    #[test]
    fn an_unclosed_list_is_refused_rather_than_guessed() {
        let source = "f(a, b";
        let tokens: Vec<Token> = tokenize(source, Language::TypeScript);
        let code: Vec<&Token> = tokens.iter().filter(|token| is_code(token)).collect();
        let open = code
            .iter()
            .position(|token| token.text(source) == "(")
            .expect("open");
        assert!(split(source, &code, open).is_none());
    }

    #[test]
    fn removing_a_middle_item_takes_the_comma_after_it() {
        let list = parse("f(a, b, c)");
        let (start, end) = removal(&list.items, 1).expect("range");
        assert_eq!(&"f(a, b, c)"[start..end], "b, ");
    }

    #[test]
    fn removing_the_last_item_takes_the_comma_before_it() {
        let list = parse("f(a, b)");
        let (start, end) = removal(&list.items, 1).expect("range");
        assert_eq!(&"f(a, b)"[start..end], ", b");
    }

    #[test]
    fn removing_the_only_item_leaves_the_parentheses() {
        let list = parse("f(a)");
        let (start, end) = removal(&list.items, 0).expect("range");
        assert_eq!(&"f(a)"[start..end], "a");
    }
}
