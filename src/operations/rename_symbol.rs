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
use weavatrix_parse::{Language, TokenKind, tokenize};
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

/// The symbols the graph proves reference this one, by file: each one's name and declaring line.
///
/// A use edge points at the *referencing symbol*, and the graph records that symbol's span as its
/// declaration line alone. The reference itself sits somewhere in the body, so the caller widens
/// each of these to its body range once the file has been read.
pub(super) fn referencing_symbols(
    state: &RepositoryState,
    id: &str,
) -> BTreeMap<String, Vec<(String, u32)>> {
    let mut by_file: BTreeMap<String, Vec<(String, u32)>> = BTreeMap::new();
    for edge in state.graph().edges() {
        if !is_use(&edge.kind) || edge.target.as_str() != id {
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
        by_file.entry(span.file.clone()).or_default().push((
            node.label.trim_end_matches("()").to_owned(),
            span.start.line,
        ));
    }
    by_file
}

/// Every line of a file the graph attributes to this symbol.
fn proven_lines(source: &str, path: &str, referencing: &[(String, u32)]) -> BTreeSet<u32> {
    let mut lines = BTreeSet::new();
    for (name, line) in referencing {
        let (from, to) = crate::declaration::body_lines(source, path, name, *line);
        lines.extend(from..=to);
    }
    lines
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
    let mut line_starts = vec![0_usize];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            line_starts.push(index + 1);
        }
    }
    let mut by_line: BTreeMap<u32, Vec<(u32, u32)>> = BTreeMap::new();
    for token in tokenize(source, language) {
        if token.kind != TokenKind::Identifier || token.text(source) != name {
            continue;
        }
        let Some(&line_start) =
            line_starts.get(usize::try_from(token.line).ok()?.saturating_sub(1))
        else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            u32::try_from(token.start - line_start + 1),
            u32::try_from(token.end - line_start + 1),
        ) else {
            continue;
        };
        by_line.entry(token.line).or_default().push((start, end));
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
        return Err(super::not_found(symbol));
    };
    let Some(node) = state.graph().node_at(index) else {
        return Err(super::not_found(symbol));
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
        return Err(super::not_found(symbol));
    };

    let id = node.id.as_str().to_owned();
    let mut referencing = referencing_symbols(state, &id);
    // The declaration itself is a site, and its own body is where a recursive call would be.
    referencing
        .entry(file.clone())
        .or_default()
        .push((old_name.clone(), span.start.line));
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
    for (path, symbols) in &referencing {
        let Some(source) = read_source(state.root(), path) else {
            found.uncertain.push(json!({
                "file": path,
                "reason": "the file is unreadable, so its references were not planned",
            }));
            continue;
        };
        let mut proven = proven_lines(&source, path, symbols);
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

pub(super) fn rename_symbol(state: &RepositoryState, arguments: &Value) -> Value {
    let symbol = arguments.get("symbol").and_then(Value::as_str);
    let new_name = arguments.get("new_name").and_then(Value::as_str);
    let (Some(symbol), Some(new_name)) = (symbol, new_name) else {
        return super::invalid_args("rename_symbol", &["symbol", "new_name"]);
    };
    let found = match sites(state, symbol, new_name) {
        Ok(found) => found,
        Err(refusal) => return refusal,
    };
    json!({
        "status": "PLANNED",
        // Never COMPLETE: this backend proves the sites it renames, not the absence of others.
        "completeness": "PARTIAL",
        "backend": "graph+lexical",
        "oldName": found.old_name,
        "newName": found.new_name,
        "renamedEdits": found.edits(),
        "plan": build_plan("rename_symbol", std::slice::from_ref(&found)),
        "uncertainReferences": found.uncertain,
        "warnings": ["GRAPH_PROVEN_SITES_ONLY"],
        "next": "apply with apply_edit_plan (preview -> confirm). Every uncertainReference is a \
                 same-named occurrence this backend refused to guess at; review them yourself.",
    })
}
