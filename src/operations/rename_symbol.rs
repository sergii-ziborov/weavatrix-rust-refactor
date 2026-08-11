//! Renaming a symbol from graph evidence.
//!
//! Every edit here is anchored to a reference the graph proved: the declaration, and each site
//! that calls or references it. Within those lines the identifier is matched on word boundaries,
//! so `resolveTarget` never matches inside `resolveTargetPath`.
//!
//! What this backend cannot do is prove the *absence* of other references, so it never claims
//! `COMPLETE`. Occurrences of the same name that the graph does not vouch for are reported as
//! uncertain rather than renamed — that is the difference between renaming a symbol and
//! find-replacing a string, and the benchmark's shadow trap is exactly the case where they
//! diverge.

use crate::coordinates::utf16_offset;
use crate::evidence::{declaring_file, read_source};
use crate::plan::{PlanBuilder, sha256_of};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_parse::{Language, Token, TokenKind, tokenize};
use weavatrix_rust::{EdgeKind, NodeKind, RepositoryState};

/// Relations that mean "this site uses that symbol", as opposed to containing it.
fn is_use(kind: &EdgeKind) -> bool {
    matches!(kind, EdgeKind::Calls | EdgeKind::References)
}

/// A valid identifier in the languages this backend serves.
fn is_identifier(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_')
        && name
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
}

/// Word-boundary occurrences of `name` on a 1-based line, as (start, end) byte columns.
fn occurrences_on_line(source: &str, line: u32, name: &str) -> Vec<(u32, u32)> {
    let Some(text) = source
        .split('\n')
        .nth(usize::try_from(line.saturating_sub(1)).unwrap_or(usize::MAX))
    else {
        return Vec::new();
    };
    let text = text.strip_suffix('\r').unwrap_or(text);
    let boundary = |character: Option<char>| {
        character.is_none_or(|character| !character.is_alphanumeric() && character != '_')
    };
    let mut hits = Vec::new();
    let mut from = 0_usize;
    while let Some(found) = text[from..].find(name) {
        let start = from + found;
        let end = start + name.len();
        let before = text[..start].chars().next_back();
        let after = text[end..].chars().next();
        if boundary(before)
            && boundary(after)
            && let (Ok(start), Ok(end)) = (u32::try_from(start + 1), u32::try_from(end + 1))
        {
            hits.push((start, end));
        }
        from = start + 1;
    }
    hits
}

/// Exact lines the graph proves reference this symbol, grouped by file.
///
/// Current edges carry the reference expression's own span, including module-level calls that
/// have no enclosing symbol. Older/fallback edges may carry only a source symbol; those are
/// widened to that declaration's proven body without turning an unbounded guess into evidence.
pub(super) fn referencing_lines(
    state: &RepositoryState,
    id: &str,
) -> BTreeMap<String, BTreeSet<u32>> {
    let mut by_file: BTreeMap<String, BTreeSet<u32>> = BTreeMap::new();
    for edge in state.graph().edges() {
        if !is_use(&edge.kind) || edge.target.as_str() != id {
            continue;
        }
        if let Some(span) = edge.provenance.span.as_ref() {
            by_file
                .entry(span.file.clone())
                .or_default()
                .insert(span.start.line);
            continue;
        }
        let Some(node) = state
            .graph()
            .nodes()
            .iter()
            .find(|node| node.id.as_str() == edge.source.as_str())
        else {
            continue;
        };
        let Some(span) = node.span.as_ref() else {
            continue;
        };
        let Some(source) = read_source(state.root(), &span.file) else {
            continue;
        };
        let (from, to) = crate::declaration::body_lines(
            &source,
            &span.file,
            node.label.trim_end_matches("()"),
            span.start.line,
        );
        by_file
            .entry(span.file.clone())
            .or_default()
            .extend(from..=to);
    }
    by_file
}

/// Files the graph proves import the one that declares the symbol.
fn importing_files(state: &RepositoryState, declaring: &str) -> BTreeSet<String> {
    let Some(target) = state
        .graph()
        .nodes()
        .iter()
        .find(|node| node.kind == NodeKind::File && node.label == declaring)
    else {
        return BTreeSet::new();
    };
    state
        .graph()
        .edges()
        .iter()
        .filter(|edge| {
            matches!(edge.kind, EdgeKind::Imports) && edge.target.as_str() == target.id.as_str()
        })
        .filter_map(|edge| {
            state
                .graph()
                .nodes()
                .iter()
                .find(|node| node.id.as_str() == edge.source.as_str())
                .map(|node| node.label.clone())
        })
        .collect()
}

/// Lines of an import statement, which in an importing file name this symbol and no other.
///
/// Without these a rename leaves the file importing a name that no longer exists — the call was
/// updated and the import was not. The proof is the graph's own import edge: this file brings in
/// the file that declares the symbol, so the name on its import line is that symbol.
fn import_lines(source: &str, name: &str) -> BTreeSet<u32> {
    source
        .split('\n')
        .enumerate()
        .filter(|(_, text)| {
            let head = text.trim_start();
            ["import ", "use ", "pub use ", "from ", "export "]
                .iter()
                .any(|keyword| head.starts_with(keyword))
        })
        .filter(|(number, _)| {
            let line = u32::try_from(number + 1).unwrap_or(u32::MAX);
            !occurrences_on_line(source, line, name).is_empty()
        })
        .map(|(number, _)| u32::try_from(number + 1).unwrap_or(u32::MAX))
        .collect()
}

/// Byte offsets of `name` wherever the tokenizer proves it is an identifier.
///
/// The expression inside a template interpolation is code — `` `/${resolveTarget(x)}` `` calls
/// the function as surely as a bare call does — but the tokenizer emits the whole template,
/// interpolations included, as one `String` token. So template tokens are searched for balanced
/// `${...}` sections and each interior is re-tokenized recursively, while every other string
/// stays prose. Depth-limited because templates can nest.
fn identifier_offsets(
    slice: &str,
    base: usize,
    language: Language,
    name: &str,
    out: &mut Vec<(usize, usize)>,
    depth: u8,
) {
    if depth > 4 {
        return;
    }
    let tokens = tokenize(slice, language);
    for (index, token) in tokens.iter().enumerate() {
        match token.kind {
            TokenKind::Identifier if token.text(slice) == name => {
                out.push((base + token.start, base + token.end));
            }
            TokenKind::String | TokenKind::Interpolation => {
                let text = token.text(slice);
                let sections = if token.kind == TokenKind::Interpolation || text.starts_with('`') {
                    interpolation_sections(text)
                } else if language == Language::Python
                    && python_f_string_prefix(&tokens, index, slice)
                {
                    python_f_string_sections(text)
                } else {
                    Vec::new()
                };
                for (inner_start, inner_end) in sections {
                    if let Some(inner) = text.get(inner_start..inner_end) {
                        identifier_offsets(
                            inner,
                            base + token.start + inner_start,
                            language,
                            name,
                            out,
                            depth + 1,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn python_f_string_prefix(tokens: &[Token], index: usize, source: &str) -> bool {
    index
        .checked_sub(1)
        .filter(|previous| tokens[*previous].end == tokens[index].start)
        .filter(|previous| tokens[*previous].kind == TokenKind::Identifier)
        .map(|previous| tokens[previous].text(source).to_ascii_lowercase())
        .is_some_and(|prefix| {
            prefix.contains('f')
                && prefix
                    .chars()
                    .all(|character| matches!(character, 'f' | 'r' | 'b' | 'u'))
        })
}

/// Executable `{...}` sections of a Python f-string; doubled braces stay literal text.
fn python_f_string_sections(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut sections = Vec::new();
    let mut at = 0_usize;
    while at < bytes.len() {
        if bytes[at] != b'{' {
            at += 1;
            continue;
        }
        if bytes.get(at + 1) == Some(&b'{') {
            at += 2;
            continue;
        }
        let start = at + 1;
        let mut cursor = start;
        let mut depth = 1_u32;
        let mut quote = None;
        let mut escaped = false;
        while cursor < bytes.len() {
            let byte = bytes[cursor];
            if let Some(delimiter) = quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == delimiter {
                    quote = None;
                }
                cursor += 1;
                continue;
            }
            match byte {
                b'\'' | b'"' => quote = Some(byte),
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        sections.push((start, cursor));
                        cursor += 1;
                        break;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        at = cursor.max(at + 1);
    }
    sections
}

/// The interiors of every balanced `${...}` in a template literal's text.
fn interpolation_sections(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut sections = Vec::new();
    let mut at = 0_usize;
    while at + 1 < bytes.len() {
        if bytes[at] == b'\\' {
            at += 2;
            continue;
        }
        if bytes[at] == b'$' && bytes[at + 1] == b'{' {
            let mut braces = 1_i32;
            let mut close = at + 2;
            while close < bytes.len() && braces > 0 {
                match bytes[close] {
                    b'{' => braces += 1,
                    b'}' => braces -= 1,
                    b'\\' => close += 1,
                    _ => {}
                }
                close += 1;
            }
            if braces == 0 {
                sections.push((at + 2, close - 1));
                at = close;
                continue;
            }
        }
        at += 1;
    }
    sections
}

/// Occurrences of `name` the tokenizer proves are identifiers, as 1-based byte columns by line.
///
/// A textual scan cannot tell `resolveTarget` the identifier from `resolveTarget` inside
/// `'call resolveTarget'` — the word boundaries are identical. The tokenizer can, and the
/// benchmark's string trap is the case where the difference is an edit that corrupts a literal.
/// `None` when the file's language is unknown; the caller then falls back to the textual scan,
/// which is the pre-existing behaviour for such files.
fn identifier_occurrences(
    source: &str,
    path: &str,
    name: &str,
) -> Option<BTreeMap<u32, Vec<(u32, u32)>>> {
    let language = Language::from_extension(path.rsplit_once('.')?.1)?;
    let mut offsets = Vec::new();
    identifier_offsets(source, 0, language, name, &mut offsets, 0);

    let mut line_starts = vec![0_usize];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            line_starts.push(index + 1);
        }
    }
    let mut by_line: BTreeMap<u32, Vec<(u32, u32)>> = BTreeMap::new();
    for (start, end) in offsets {
        let line_index = line_starts.partition_point(|&at| at <= start) - 1;
        let line_start = line_starts[line_index];
        let (Ok(line), Ok(start), Ok(end)) = (
            u32::try_from(line_index + 1),
            u32::try_from(start - line_start + 1),
            u32::try_from(end - line_start + 1),
        ) else {
            continue;
        };
        by_line.entry(line).or_default().push((start, end));
    }
    Some(by_line)
}

/// One identifier to rewrite: a line and the UTF-16 range holding the name on it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Site {
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

/// Everything one rename resolved to, before any of it becomes an envelope.
///
/// The sites are kept apart from the plan so a coordinated rename can merge several of these and
/// find the places where two renames collide — which is impossible once each has been flattened
/// into its own envelope.
pub(super) struct Rename {
    pub old_name: String,
    pub new_name: String,
    /// Path to its content hash and the sites within it, in file order.
    pub files: BTreeMap<String, (String, Vec<Site>)>,
    pub uncertain: Vec<Value>,
}

impl Rename {
    /// How many identifiers this rename rewrites.
    pub fn edits(&self) -> usize {
        self.files.values().map(|(_, sites)| sites.len()).sum()
    }
}

/// Finds every site one rename touches, or the refusal that stops it.
///
/// Both entry points come through here, so a symbol that `rename_symbol` refuses is refused
/// identically inside a batch — the alternative is a batch that quietly renames what the single
/// operation would not.
pub(super) fn sites(
    state: &RepositoryState,
    symbol: &str,
    new_name: &str,
) -> Result<Rename, Value> {
    if !is_identifier(new_name) {
        return Err(json!({
            "status": "INVALID_NEW_NAME",
            "reason": format!("{new_name:?} is not a valid identifier"),
        }));
    }
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    let Some(node) = state.graph().node_at(index) else {
        return Err(super::not_found(state.graph(), symbol));
    };
    let old_name = node.label.trim_end_matches("()").to_owned();
    if !is_identifier(&old_name) {
        return Err(json!({
            "status": "NOT_SUPPORTED",
            "reason": format!(
                "the graph records this symbol as {:?}, which is not a plain identifier; rename \
                 needs one to anchor its edits",
                node.label
            ),
        }));
    }
    if old_name == new_name {
        return Err(json!({"status": "NO_CHANGE", "reason": "the symbol already has that name"}));
    }
    let (Some(file), Some(span)) = (declaring_file(node), node.span.as_ref()) else {
        return Err(super::not_found(state.graph(), symbol));
    };

    let id = node.id.as_str().to_owned();
    let mut referencing = referencing_lines(state, &id);
    // The declaration itself is always a site. Recursive calls arrive as their own exact edges.
    referencing
        .entry(file.clone())
        .or_default()
        .insert(span.start.line);
    let importing = importing_files(state, &file);
    for path in &importing {
        referencing.entry(path.clone()).or_default();
    }
    let mut found = Rename {
        old_name,
        new_name: new_name.to_owned(),
        files: BTreeMap::new(),
        uncertain: Vec::new(),
    };
    for (path, proven) in &referencing {
        let Some(source) = read_source(state.root(), path) else {
            found.uncertain.push(json!({
                "file": path,
                "reason": "the file is unreadable, so its references were not planned",
            }));
            continue;
        };
        let mut proven = proven.clone();
        if importing.contains(path) {
            proven.extend(import_lines(&source, &found.old_name));
        }
        collect_file(&mut found, path, &source, &proven);
    }
    if found.edits() == 0 {
        return Err(json!({
            "status": "NO_EDITS",
            "reason": format!(
                "{} was not found on any line the graph attributes to this symbol; rebuild the \
                 graph if the file changed",
                found.old_name
            ),
        }));
    }
    Ok(found)
}

/// Splits one file's occurrences into the proven sites and the ones this must not touch.
///
/// A site needs two proofs: the graph attributes the line to this symbol, and the tokenizer
/// says the occurrence is an identifier rather than the same characters inside a string. Every
/// textual occurrence that did not become a site is reported, so nothing is silently dropped —
/// which is what makes the uncertain list trustworthy.
fn collect_file(found: &mut Rename, path: &str, source: &str, proven: &BTreeSet<u32>) {
    let identifiers = identifier_occurrences(source, path, &found.old_name);
    let mut sites = Vec::new();
    let mut accepted: BTreeSet<(u32, u32, u32)> = BTreeSet::new();
    for line in proven {
        let ranges = match &identifiers {
            Some(by_line) => by_line.get(line).cloned().unwrap_or_default(),
            None => occurrences_on_line(source, *line, &found.old_name),
        };
        for (start_column, end_column) in ranges {
            let (Ok(start), Ok(end)) = (
                utf16_offset(source, *line, start_column),
                utf16_offset(source, *line, end_column),
            ) else {
                continue;
            };
            accepted.insert((*line, start_column, end_column));
            sites.push(Site {
                line: *line,
                start,
                end,
            });
        }
    }
    if !sites.is_empty() {
        found
            .files
            .insert(path.to_owned(), (sha256_of(source), sites));
    }
    // Everything textual that was not planned: occurrences on unproven lines may belong to a
    // different symbol sharing the name, and occurrences inside strings or comments on proven
    // lines are prose. Both would be renamed by a find-replace, and neither is here.
    for (number, text) in source.split('\n').enumerate() {
        let line = u32::try_from(number + 1).unwrap_or(u32::MAX);
        let unplanned = occurrences_on_line(source, line, &found.old_name)
            .into_iter()
            .any(|(start, end)| !accepted.contains(&(line, start, end)));
        if unplanned {
            found.uncertain.push(json!({
                "file": path,
                "line": line,
                "kind": "UNPROVEN_OCCURRENCE",
                "excerpt": text.trim(),
            }));
        }
    }
}

/// One site with the names it rewrites, kept together while several renames are merged.
type PlacedEdit<'a> = (Site, &'a str, &'a str);

/// A file's content hash and every edit landing in it, from however many renames.
type FileEdits<'a> = (&'a str, Vec<PlacedEdit<'a>>);

/// Folds one or more renames into a single envelope, ordered by file and then by position.
pub(super) fn build_plan(operation: &str, renames: &[Rename]) -> Value {
    let mut by_file: BTreeMap<&str, FileEdits<'_>> = BTreeMap::new();
    for rename in renames {
        for (path, (hash, sites)) in &rename.files {
            let entry = by_file.entry(path).or_insert((hash, Vec::new()));
            for site in sites {
                entry
                    .1
                    .push((*site, rename.old_name.as_str(), rename.new_name.as_str()));
            }
        }
    }
    let mut builder = PlanBuilder::new(operation);
    for (path, (hash, mut sites)) in by_file {
        sites.sort_by_key(|(site, _, _)| *site);
        builder = builder.file(path, hash);
        for (site, before, after) in sites {
            builder = builder.edit(
                site.line, site.start, site.line, site.end, before, after, "RESOLVED",
            );
        }
    }
    builder.build()
}

pub(super) fn rename_symbol(
    state: &RepositoryState,
    tokens: &crate::token::TokenStore,
    arguments: &Value,
    write_allowed: bool,
) -> Value {
    let symbol = arguments.get("symbol").and_then(Value::as_str);
    let new_name = arguments.get("new_name").and_then(Value::as_str);
    let (Some(symbol), Some(new_name)) = (symbol, new_name) else {
        return super::invalid_args("rename_symbol", &["symbol", "new_name"]);
    };
    let found = match sites(state, symbol, new_name) {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    let plan = build_plan("rename_symbol", std::slice::from_ref(&found));
    if arguments.get("mode").and_then(Value::as_str) == Some("apply") {
        return super::apply::apply_generated_plan(
            state.root(),
            tokens,
            &plan,
            arguments.get("confirm_token").and_then(Value::as_str),
            write_allowed,
        );
    }
    let mut answer = json!({
        "status": "PLANNED",
        // Never COMPLETE: this backend proves the sites it renames, not the absence of others.
        "completeness": "PARTIAL",
        "backend": "graph+lexical",
        "oldName": found.old_name,
        "newName": found.new_name,
        "renamedEdits": found.edits(),
        "plan": plan,
        "uncertainReferences": found.uncertain,
        "warnings": ["GRAPH_PROVEN_SITES_ONLY"],
        "next": "call rename_symbol again with the identical symbol and new_name, \
                 mode=\"apply\", and this confirm_token. Every uncertainReference is a \
                 same-named occurrence this backend refused to guess at; review them yourself.",
    });
    // The plan is previewed here rather than by a second call, and the confirmation rides in
    // this answer. The agent then applies with the token alone — it never has to echo back the
    // plan bytes it just received, which the benchmark measured as the largest single cost of
    // the whole flow.
    if let Some(preview) = preview_plan(state, tokens, answer.get("plan"))
        && let (Some(object), Some(extra)) = (answer.as_object_mut(), preview.as_object())
    {
        for (key, value) in extra {
            object.insert(key.clone(), value.clone());
        }
    }
    answer
}

/// Dry-runs a freshly built plan and issues its confirmation, or reports why it could not.
///
/// A failure here is not a failure of the rename: the plan is still returned and can be
/// previewed explicitly. What must never happen is a token for a plan the working tree would
/// reject — so the token only exists when the dry run passed.
fn preview_plan(
    state: &RepositoryState,
    tokens: &crate::token::TokenStore,
    plan: Option<&Value>,
) -> Option<Value> {
    let envelope = crate::envelope::read_envelope(plan?).ok()?;
    let tree = weavatrix_worktree::Worktree::open(state.root()).ok()?;
    match tree.dry_run(&envelope) {
        Ok(_) => {
            let token = tokens.issue(&envelope, state.root());
            Some(json!({
                "previewed": true,
                "confirmToken": token.value,
                "expiresAt": token.expires_at,
            }))
        }
        Err(error) => Some(json!({
            "previewed": false,
            "previewBlocked": format!("{error}"),
        })),
    }
}
