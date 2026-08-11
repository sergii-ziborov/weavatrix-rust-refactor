//! Adding and removing a parameter, with the call sites that pass it.
//!
//! This is always `PARTIAL`. The call sites come from graph edges, so a call the graph did not
//! record is a call this does not touch — and unlike a rename, where a missed site merely keeps
//! an old name, a missed site here keeps an argument the function no longer takes. Every call
//! site the plan edits is listed, and everything the analysis could not settle is listed beside
//! it rather than folded into the count.
//!
//! Two shapes are refused rather than guessed. A call that spreads (`f(...args)`) has no
//! positional argument to remove, because the array decides at run time how many there are. And
//! adding a parameter with no default leaves every existing call one argument short; the value
//! to pass is a decision, not a derivation, so the declaration changes and the call sites are
//! reported.

use super::signature::{List, is_code, removal, split};
use crate::coordinates::utf16_offset;
use crate::evidence::{declaring_file, read_source};
use crate::plan::{PlanBuilder, sha256_of};
use crate::resolve::resolve_symbol;
use blazingly_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use weavatrix_parse::{Language, Token, TokenKind, tokenize};
use weavatrix_rust::RepositoryState;

/// What the caller asked for.
enum Change {
    Add {
        name: String,
        default: Option<String>,
    },
    Remove {
        index: usize,
    },
}

/// A file parsed once, so the tokens are not rebuilt for every lookup in it.
struct Source {
    text: String,
    tokens: Vec<Token>,
}

impl Source {
    fn open(root: &std::path::Path, path: &str) -> Option<Self> {
        let text = read_source(root, path)?;
        let language = path
            .rsplit_once('.')
            .and_then(|(_, extension)| Language::from_extension(extension))?;
        let tokens = tokenize(&text, language);
        Some(Self { text, tokens })
    }

    fn code(&self) -> Vec<&Token> {
        self.tokens.iter().filter(|token| is_code(token)).collect()
    }
}

/// The parameter or argument list belonging to `name`, called or declared on `line`.
///
/// The identifier is matched as a whole word so `run` never matches inside `runAll`, and the
/// list is the one opened by the very next token — a generic argument list in between is stepped
/// over, since `f<T>(a)` declares the same parameters as `f(a)`.
fn list_at(source: &Source, code: &[&Token], name: &str, line: u32) -> Option<List> {
    let position = code.iter().position(|token| {
        token.line == line
            && token.kind == TokenKind::Identifier
            && token.text(&source.text) == name
    })?;
    let mut at = position + 1;
    if code
        .get(at)
        .is_some_and(|token| token.text(&source.text) == "<")
        && let Some(list) = code[at..]
            .iter()
            .position(|token| token.text(&source.text) == ">")
    {
        at += list + 1;
    }
    split(&source.text, code, at)
}

/// Lines the graph proves call this symbol, by file.
///
/// Current graph edges carry the call expression's exact source line. The shared resolver keeps
/// a proven-body fallback for older evidence that only identifies the calling symbol.
fn call_lines(state: &RepositoryState, id: &str) -> BTreeMap<String, BTreeSet<u32>> {
    super::rename_symbol::referencing_lines(state, id)
}

/// The (line, byte column) of a byte offset, both 1-based.
fn position_of(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset.min(text.len())];
    let line = u32::try_from(before.matches('\n').count() + 1).unwrap_or(u32::MAX);
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    (
        line,
        u32::try_from(offset - line_start + 1).unwrap_or(u32::MAX),
    )
}

/// Adds one replacement to the plan, or nothing when its position cannot be expressed exactly.
fn replace(builder: PlanBuilder, text: &str, range: (usize, usize), after: &str) -> PlanBuilder {
    let (start_line, start_column) = position_of(text, range.0);
    let (end_line, end_column) = position_of(text, range.1);
    let (Ok(start_char), Ok(end_char)) = (
        utf16_offset(text, start_line, start_column),
        utf16_offset(text, end_line, end_column),
    ) else {
        return builder;
    };
    let Some(before) = text.get(range.0..range.1) else {
        return builder;
    };
    builder.edit(
        start_line, start_char, end_line, end_char, before, after, "RESOLVED",
    )
}

/// Reads the `operation` object, which the contract leaves untyped.
fn change(arguments: &Value) -> Result<Change, Value> {
    let Some(operation) = arguments.get("operation") else {
        return Err(super::invalid_args("change_signature", &["operation"]));
    };
    match operation.get("kind").and_then(Value::as_str) {
        Some("add_parameter") => {
            let Some(name) = operation.get("name").and_then(Value::as_str) else {
                return Err(super::invalid_args("change_signature", &["operation.name"]));
            };
            Ok(Change::Add {
                name: name.to_owned(),
                default: operation
                    .get("default")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        }
        Some("remove_parameter") => {
            let Some(index) = operation
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|index| usize::try_from(index).ok())
            else {
                return Err(super::invalid_args(
                    "change_signature",
                    &["operation.index"],
                ));
            };
            Ok(Change::Remove { index })
        }
        _ => Err(json!({
            "status": "INVALID_ARGS",
            "operation": "change_signature",
            "reason": "operation.kind must be \"add_parameter\" or \"remove_parameter\"",
        })),
    }
}

/// The text that adds a parameter to a declaration, and where it goes.
fn addition(list: &List, name: &str, default: Option<&str>) -> ((usize, usize), String) {
    let declared = default.map_or_else(|| name.to_owned(), |value| format!("{name} = {value}"));
    list.items.last().map_or_else(
        // An empty list has nothing to append to, so the parentheses themselves are rewritten.
        || ((list.open, list.close + 1), format!("({declared})")),
        |last| ((last.start, last.end), format!("{}, {declared}", last.text)),
    )
}

/// The symbol a change is being planned against, so the planners take one argument, not six.
struct Target<'a> {
    state: &'a RepositoryState,
    id: &'a str,
    name: &'a str,
    source: &'a Source,
    parameters: &'a List,
}

/// What one change resolved to.
struct Planned {
    builder: PlanBuilder,
    call_sites: Vec<Value>,
    uncertain: Vec<Value>,
}

/// Appends a parameter to the declaration.
///
/// Only the declaration changes. With a default the existing calls stay correct; without one
/// they are each short an argument, and the value to pass is a decision rather than something
/// that can be derived, so every proven call site is reported instead of edited.
fn plan_add(target: &Target, builder: PlanBuilder, name: &str, default: Option<&str>) -> Planned {
    let (range, text) = addition(target.parameters, name, default);
    let builder = replace(builder, &target.source.text, range, &text);
    let uncertain = if default.is_some() {
        Vec::new()
    } else {
        call_lines(target.state, target.id)
            .into_iter()
            .flat_map(|(file, lines)| {
                lines
                    .into_iter()
                    .map(|line| {
                        json!({
                            "file": file,
                            "line": line,
                            "kind": "VALUE_REQUIRED",
                            "reason": "the parameter has no default, so this call needs an \
                                       argument that only you can choose",
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    Planned {
        builder,
        call_sites: Vec::new(),
        uncertain,
    }
}

/// Removes a parameter from the declaration and its argument from each proven call.
fn plan_remove(target: &Target, builder: PlanBuilder, index: usize) -> Result<Planned, Value> {
    let items = &target.parameters.items;
    if index >= items.len() {
        return Err(json!({
            "status": "NO_CHANGE",
            "reason": format!(
                "{} declares {} parameter(s); index {index} is not one of them",
                target.name,
                items.len()
            ),
        }));
    }
    let Some(range) = removal(items, index) else {
        return Err(json!({"status": "NO_CHANGE", "reason": "nothing to remove"}));
    };
    let builder = replace(builder, &target.source.text, range, "");
    let mut uncertain = vec![json!({
        "kind": "REMOVED_PARAMETER",
        "parameter": items[index].text.trim(),
    })];
    let (builder, call_sites) = edit_call_sites(target, builder, index, &mut uncertain);
    Ok(Planned {
        builder,
        call_sites,
        uncertain,
    })
}

/// Removes one argument at each call site the graph proved, reporting the rest.
fn edit_call_sites(
    target: &Target,
    mut builder: PlanBuilder,
    index: usize,
    uncertain: &mut Vec<Value>,
) -> (PlanBuilder, Vec<Value>) {
    let state = target.state;
    let name = target.name;
    let mut edited = Vec::new();
    for (path, lines) in call_lines(state, target.id) {
        let Some(source) = Source::open(state.root(), &path) else {
            uncertain.push(json!({"file": path, "kind": "SOURCE_UNAVAILABLE"}));
            continue;
        };
        let code = source.code();
        let mut opened = false;
        for line in lines {
            let Some(list) = list_at(&source, &code, name, line) else {
                continue;
            };
            if list.items.iter().any(|item| item.spread) {
                uncertain.push(json!({
                    "file": path,
                    "line": line,
                    "kind": "SPREAD_ARGUMENT",
                    "reason": "the call spreads an array, so which argument sits at this index is \
                               only known at run time",
                }));
                continue;
            }
            // A call that already passes fewer arguments than this index relies on the parameter
            // being optional; removing the parameter leaves it correct and needs no edit.
            if index >= list.items.len() {
                continue;
            }
            let Some(range) = removal(&list.items, index) else {
                continue;
            };
            if !opened {
                builder = builder.file(&path, &sha256_of(&source.text));
                opened = true;
            }
            builder = replace(builder, &source.text, range, "");
            edited.push(json!({
                "file": path,
                "line": line,
                "removed": list.items[index].text.trim(),
            }));
        }
    }
    (builder, edited)
}

pub(super) fn change_signature(state: &RepositoryState, arguments: &Value) -> Value {
    let Some(symbol) = arguments.get("symbol").and_then(Value::as_str) else {
        return super::invalid_args("change_signature", &["symbol", "operation"]);
    };
    let change = match change(arguments) {
        Ok(change) => change,
        Err(refusal) => return refusal,
    };
    let Some(index) = resolve_symbol(state.graph(), symbol) else {
        return super::not_found(state.graph(), symbol);
    };
    let Some(node) = state.graph().node_at(index) else {
        return super::not_found(state.graph(), symbol);
    };
    let name = node.label.trim_end_matches("()").to_owned();
    let (Some(path), Some(span)) = (declaring_file(node), node.span.as_ref()) else {
        return super::not_found(state.graph(), symbol);
    };
    let Some(source) = Source::open(state.root(), &path) else {
        return json!({
            "status": "SOURCE_UNAVAILABLE",
            "reason": format!("{path} could not be read, or its language is not recognised"),
        });
    };
    let code = source.code();
    let Some(parameters) = list_at(&source, &code, &name, span.start.line) else {
        return json!({
            "status": "NOT_A_SYMBOL",
            "reason": format!(
                "no parameter list was found for {name} at {path}:{}; a signature can only be \
                 changed on something that declares one",
                span.start.line
            ),
        });
    };

    let id = node.id.as_str().to_owned();
    let builder = PlanBuilder::new("change_signature").file(&path, &sha256_of(&source.text));
    let target = Target {
        state,
        id: &id,
        name: &name,
        source: &source,
        parameters: &parameters,
    };
    let Planned {
        builder: plan,
        call_sites,
        uncertain,
    } = match match &change {
        Change::Add {
            name: added,
            default,
        } => Ok(plan_add(&target, builder, added, default.as_deref())),
        Change::Remove { index } => plan_remove(&target, builder, *index),
    } {
        Ok(planned) => planned,
        Err(refusal) => return refusal,
    };

    if plan.is_empty() {
        return json!({
            "status": "NO_CHANGE",
            "reason": "the requested change produced no edit",
        });
    }
    json!({
        "status": "PLANNED",
        // The call sites come from graph edges, so this proves what it edits and not what it missed.
        "completeness": "PARTIAL",
        "backend": "graph+lexical",
        "symbol": name,
        "declaration": path,
        "callSites": call_sites,
        "uncertain": uncertain,
        "plan": plan.build(),
        "warnings": ["GRAPH_PROVEN_CALL_SITES_ONLY"],
        "next": "apply with apply_edit_plan (preview -> confirm). Check every uncertain entry \
                 first: those are the calls this could not settle on its own.",
    })
}

/// Exposed so the declaration parser can be exercised without a repository.
#[cfg(test)]
pub(crate) fn parameters_of(
    source: &str,
    language: Language,
    name: &str,
    line: u32,
) -> Vec<super::signature::Item> {
    let parsed = Source {
        text: source.to_owned(),
        tokens: tokenize(source, language),
    };
    let code = parsed.code();
    list_at(&parsed, &code, name, line)
        .map(|list| list.items)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::parameters_of;
    use weavatrix_parse::Language;

    #[test]
    fn a_declaration_yields_its_parameters() {
        let items = parameters_of(
            "export function run(one, two) {\n  return one;\n}\n",
            Language::JavaScript,
            "run",
            1,
        );
        let texts: Vec<&str> = items.iter().map(|item| item.text.as_str()).collect();
        assert_eq!(texts, ["one", "two"]);
    }

    #[test]
    fn a_generic_declaration_yields_the_parameters_not_the_type_arguments() {
        let items = parameters_of(
            "export function run<T, U>(one: T, two: U) {}\n",
            Language::TypeScript,
            "run",
            1,
        );
        assert_eq!(items.len(), 2, "{items:?}");
        assert_eq!(items[0].text, "one: T");
    }

    #[test]
    fn a_similarly_named_declaration_on_the_line_is_not_matched() {
        let items = parameters_of(
            "export function runAll(a, b, c) {}\n",
            Language::JavaScript,
            "run",
            1,
        );
        assert!(items.is_empty(), "{items:?}");
    }
}
