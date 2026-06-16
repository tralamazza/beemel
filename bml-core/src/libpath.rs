//! Library search-path assembly, shared by the `bml` CLI and `bml-lsp`.

use std::path::{Path, PathBuf};

/// Build the ordered library search path for resolving target `include`s and
/// source `import`s: caller-supplied dirs first (the CLI's `--lib`, the LSP's
/// `libs` option), then `$BML_PATH`, then the in-tree `lib/` for dev builds.
///
/// The importing file's own directory is always tried first by the resolver
/// itself ([`crate::target::resolve_include`] /
/// [`crate::imports::ImportResolver::resolve_module_path`]); these are the
/// global fallbacks, so a local file always wins. Nonexistent roots are dropped
/// so a stale `$BML_PATH` entry is harmless.
///
/// Note: after `cargo install` the in-tree `lib/` path no longer exists on the
/// target machine; installed users supply `--lib`/`$BML_PATH` until a real
/// install location is wired up.
#[must_use]
pub fn assemble_lib_roots(caller_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = caller_dirs.to_vec();
    if let Some(bml_path) = std::env::var_os("BML_PATH") {
        roots.extend(std::env::split_paths(&bml_path).filter(|p| !p.as_os_str().is_empty()));
    }
    // Dev fallback: the repo's `lib/` sits one level up from this crate. The
    // binary crates and bml-core are all siblings under the repo root, so this
    // resolves to the same `<repo>/lib` regardless of which crate is compiled.
    if let Some(repo_lib) = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("lib"))
        && repo_lib.is_dir()
    {
        roots.push(repo_lib);
    }
    roots.retain(|p| p.is_dir());
    roots
}
