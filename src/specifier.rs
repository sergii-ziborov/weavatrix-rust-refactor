//! Relative import specifiers, and what moving a file does to them.
//!
//! Only specifiers that denote a file take part. `./core.js` and `../lib/y` are paths; Rust's
//! `crate::core` and Java's `com.example.Core` are module names that a file move does not
//! rewrite, and treating them as paths would corrupt them. So everything here is gated on the
//! specifier starting with `./` or `../`.
//!
//! The arithmetic is done on normalised segments rather than by string surgery, because `..`
//! that escapes the repository root and `.` segments in the middle both produce specifiers that
//! look plausible and resolve somewhere else.

/// Whether a specifier names a file rather than a module.
#[must_use]
pub fn is_relative(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}

/// Splits a repository-relative path into its directory segments.
fn directory_of(path: &str) -> Vec<&str> {
    let mut segments = path.split('/').collect::<Vec<_>>();
    segments.pop();
    segments
}

/// Resolves a relative specifier against the directory of the importing file.
///
/// Returns `None` when the specifier climbs above the repository root — a specifier that cannot
/// be resolved must not be rewritten into one that can.
#[must_use]
pub fn resolve(importer: &str, specifier: &str) -> Option<String> {
    let mut segments = directory_of(importer);
    for part in specifier.split('/') {
        match part {
            "." | "" => {}
            ".." => {
                segments.pop()?;
            }
            other => segments.push(other),
        }
    }
    Some(segments.join("/"))
}

/// The specifier an importer in `importer` needs to reach `target`, preserving any extension
/// style the original used.
///
/// Always starts with `./` or `../`: a bare `core.js` is a package name in most ecosystems, so
/// emitting one would change what the import means.
#[must_use]
pub fn between(importer: &str, target: &str) -> String {
    let from = directory_of(importer);
    let to = target.split('/').collect::<Vec<_>>();
    let shared = from
        .iter()
        .zip(to.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let mut parts = vec![".."; from.len().saturating_sub(shared)];
    parts.extend(to[shared..].iter().copied());
    let joined = parts.join("/");
    if joined.starts_with("..") {
        joined
    } else {
        format!("./{joined}")
    }
}

/// The specifier for `target` as written from a file that has moved to `moved_to`.
///
/// Extensionless specifiers stay extensionless: a project that omits `.js` does so deliberately
/// and rewriting it to include one changes resolution.
#[must_use]
pub fn rewrite(moved_to: &str, original: &str, resolved: &str) -> String {
    let rewritten = between(moved_to, resolved);
    // "Did the author write an extension?" is answered on the last segment: a specifier like
    // `../v1.2/core` has a dot in it without naming a file type.
    let wrote_extension = original
        .rsplit('/')
        .next()
        .is_some_and(|last| last.contains('.') && !last.starts_with('.'));
    if wrote_extension || !resolved.contains('.') {
        return rewritten;
    }
    // The original omitted the extension; strip the one the resolved path carries.
    rewritten
        .rsplit_once('.')
        .map_or(rewritten.clone(), |(stem, _)| stem.to_owned())
}

#[cfg(test)]
mod tests {
    use super::{between, is_relative, resolve, rewrite};

    #[test]
    fn only_path_specifiers_are_relative() {
        assert!(is_relative("./core.js"));
        assert!(is_relative("../lib/y.js"));
        assert!(!is_relative("crate::core"));
        assert!(!is_relative("react"));
        assert!(!is_relative("com.example.Core"));
        assert!(!is_relative("/absolute/core.js"));
    }

    #[test]
    fn a_specifier_resolves_against_the_importing_directory() {
        assert_eq!(
            resolve("src/app/main.js", "./core.js"),
            Some("src/app/core.js".to_owned())
        );
        assert_eq!(
            resolve("src/app/main.js", "../lib/y.js"),
            Some("src/lib/y.js".to_owned())
        );
        assert_eq!(
            resolve("src/app/main.js", "../../top.js"),
            Some("top.js".to_owned())
        );
    }

    #[test]
    fn a_specifier_that_escapes_the_root_resolves_to_nothing() {
        assert_eq!(resolve("main.js", "../outside.js"), None);
    }

    #[test]
    fn the_specifier_between_two_files_is_always_explicitly_relative() {
        // A bare name is a package in most ecosystems, so a sibling must keep its `./`.
        assert_eq!(between("src/app/main.js", "src/app/core.js"), "./core.js");
        assert_eq!(between("src/app/main.js", "src/lib/y.js"), "../lib/y.js");
        assert_eq!(between("main.js", "src/deep/x.js"), "./src/deep/x.js");
    }

    #[test]
    fn a_round_trip_through_resolve_and_between_is_stable() {
        for (importer, specifier) in [
            ("src/app/main.js", "./core.js"),
            ("src/app/main.js", "../lib/y.js"),
            ("a/b/c/d.js", "../../e.js"),
        ] {
            let resolved = resolve(importer, specifier).expect("resolves");
            assert_eq!(
                between(importer, &resolved),
                specifier,
                "{specifier} from {importer} did not survive the round trip"
            );
        }
    }

    #[test]
    fn moving_a_file_deeper_adds_the_climb_its_imports_need() {
        // src/main.js imported ./core.js; moved to src/deep/main.js it needs ../core.js.
        let resolved = resolve("src/main.js", "./core.js").expect("resolves");
        assert_eq!(
            rewrite("src/deep/main.js", "./core.js", &resolved),
            "../core.js"
        );
    }

    #[test]
    fn moving_a_file_up_removes_the_climb() {
        let resolved = resolve("src/deep/main.js", "../core.js").expect("resolves");
        assert_eq!(rewrite("src/main.js", "../core.js", &resolved), "./core.js");
    }

    #[test]
    fn an_extensionless_specifier_stays_extensionless() {
        let resolved = "src/core.js";
        assert_eq!(rewrite("src/deep/main.js", "./core", resolved), "../core");
    }

    #[test]
    fn an_explicit_extension_is_kept() {
        let resolved = "src/core.js";
        assert_eq!(
            rewrite("src/deep/main.js", "./core.js", resolved),
            "../core.js"
        );
    }
}
