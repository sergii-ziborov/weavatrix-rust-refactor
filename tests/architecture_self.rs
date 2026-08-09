//! Boundaries this crate has to keep, checked against its own source.
//!
//! These are the rules that stop the refactor surface drifting into the shape the JavaScript
//! host had to be corrected out of: a contract that quietly follows the implementation, engines
//! that crash instead of refusing, and a writer that appears somewhere other than the one place
//! the write gates guard.

use std::fs;
use std::path::{Path, PathBuf};

fn source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_sources() -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, found: &mut Vec<(PathBuf, String)>) {
        for entry in fs::read_dir(dir).expect("source tree is readable") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                walk(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = fs::read_to_string(&path).expect("source file is UTF-8");
                found.push((path, text));
            }
        }
    }
    let mut found = Vec::new();
    walk(&source_root(), &mut found);
    assert!(!found.is_empty(), "the crate must have source files");
    found
}

/// Strips `#[cfg(test)]` modules, which may use whatever they need to set a fixture up.
fn production_only(text: &str) -> String {
    let mut kept = String::new();
    let mut depth = 0_i32;
    let mut in_test = false;
    for line in text.lines() {
        if line.trim_start().starts_with("#[cfg(test)]") {
            in_test = true;
            depth = 0;
            continue;
        }
        if in_test {
            depth += i32::try_from(line.matches('{').count()).unwrap_or(0);
            depth -= i32::try_from(line.matches('}').count()).unwrap_or(0);
            if depth <= 0 && line.contains('}') {
                in_test = false;
            }
            continue;
        }
        kept.push_str(line);
        kept.push('\n');
    }
    kept
}

#[test]
fn the_contract_never_depends_on_the_implementation() {
    let contract = fs::read_to_string(source_root().join("contract.rs")).expect("contract module");
    let production = production_only(&contract);
    assert!(
        !production.contains("crate::operations"),
        "contract.rs must not reach into operations: the contract is what the implementation is \
         measured against, so it cannot follow it"
    );
}

#[test]
fn the_catalog_has_exactly_one_source() {
    let sources = rust_sources();
    let embedding = sources
        .iter()
        .filter(|(_, text)| text.contains("include_str!") && text.contains("contract/"))
        .count();
    assert_eq!(
        embedding, 1,
        "the frozen contract must be embedded in exactly one place, or two catalogs can drift"
    );
}

#[test]
fn no_engine_writes_to_the_filesystem() {
    for (path, text) in rust_sources() {
        if path.ends_with("test_support.rs") {
            continue;
        }
        let production = production_only(&text);
        for forbidden in ["fs::write", "fs::remove", "File::create", "OpenOptions"] {
            assert!(
                !production.contains(forbidden),
                "{} uses {forbidden}: every repository write belongs behind the three gates in \
                 weavatrix-worktree, not in an engine",
                path.display()
            );
        }
    }
}

#[test]
fn engines_refuse_with_a_status_instead_of_panicking() {
    for (path, text) in rust_sources() {
        if !path.to_string_lossy().contains("operations") {
            continue;
        }
        let production = production_only(&text);
        for forbidden in [
            "panic!",
            ".unwrap()",
            "unreachable!",
            "todo!",
            "unimplemented!",
        ] {
            assert!(
                !production.contains(forbidden),
                "{} uses {forbidden}: an agent branches on a status and cannot branch on a crash",
                path.display()
            );
        }
    }
}

#[test]
fn the_crate_forbids_unsafe_code() {
    let manifest = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("manifest");
    assert!(
        manifest.contains("unsafe_code = \"forbid\""),
        "a crate that edits source files may not opt out of the unsafe check"
    );
}
