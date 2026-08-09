//! A real repository state for engine tests.
//!
//! The engines read a graph, so they are tested against one the engine actually built rather
//! than a hand-written stub: a stub would let a wrong assumption about node ids or edge kinds
//! pass here and fail in the host.

use std::fs;
use std::path::PathBuf;
use weavatrix_rust::{RepositoryState, Weavatrix};

fn fixture_root() -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "wvxr-fixture-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("src")).expect("fixture root");
    fs::write(root.join("src/lib.rs"), "pub mod core;\npub mod caller;\n").expect("lib");
    fs::write(
        root.join("src/core.rs"),
        "pub fn used(value: u32) -> u32 {\n    value + 1\n}\n\npub fn orphan() -> u32 {\n    0\n}\n",
    )
    .expect("core");
    fs::write(
        root.join("src/caller.rs"),
        "use crate::core::used;\n\npub fn run() -> u32 {\n    used(1)\n}\n",
    )
    .expect("caller");
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    )
    .expect("manifest");
    root
}

/// Opens a built graph over a small fixture repository.
pub(crate) fn fixture_state() -> RepositoryState {
    let engine = Weavatrix::open(fixture_root()).expect("the fixture repository must open");
    engine.state().clone()
}
