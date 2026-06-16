use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::ast::{self, Item, Program};
use crate::errors::DiagnosticBag;
use crate::parser::Parser;
use crate::source::{SourceFile, SourceMap, Span};

/// Cross-analysis parse cache for imported modules. A long-lived process (the
/// LSP) holds one of these and threads it through each `ImportResolver` so an
/// unchanged imported file is read from disk and parsed only once, not on every
/// keystroke. Only the raw per-file parse is cached; the flatten/alias logic in
/// `resolve_imports` still runs fresh each time.
#[derive(Default)]
pub struct ModuleCache {
    entries: HashMap<PathBuf, CachedModule>,
}

/// One cached file parse. `file` carries the original `SourceFile` (and its
/// `FileId`) so the cached AST's spans can be made valid in a fresh
/// `SourceMap`; `mtime` is the on-disk modified time used for invalidation.
struct CachedModule {
    mtime: SystemTime,
    file: SourceFile,
    program: Program,
}

pub struct ImportResolver {
    pub source_map: SourceMap,
    pub diags: DiagnosticBag,
    /// Persistent across analyses when the caller swaps in its own cache;
    /// otherwise an empty per-run cache. See [`ModuleCache`].
    pub cache: ModuleCache,
    /// Global library search roots, tried *after* the importing file's own
    /// directory (so a local module always shadows a library one). Empty by
    /// default; the CLI fills it from `--lib`/`$BML_PATH`/the dev fallback. See
    /// [`resolve_module_path`](ImportResolver::resolve_module_path).
    pub lib_roots: Vec<PathBuf>,
    visiting: Vec<PathBuf>,
}

impl Default for ImportResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ImportResolver {
    #[must_use]
    pub fn new() -> Self {
        ImportResolver {
            source_map: SourceMap::new(),
            diags: DiagnosticBag::new(),
            cache: ModuleCache::default(),
            lib_roots: Vec::new(),
            visiting: Vec::new(),
        }
    }

    pub fn resolve(&mut self, root_program: Program, root_path: &Path) -> Program {
        let parent_dir = root_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        // Track root in visiting so circular imports involving the root are detected
        self.visiting.push(canonicalize(root_path));
        // The root program's own items keep their bare names (empty prefix);
        // every imported module is qualified by its import name / alias.
        let mut program = self.resolve_imports(root_program, &parent_dir, "");
        // With all modules flattened in, fold const-valued array lengths
        // (e.g. `[u8; N]`) into literals so type resolution sees a concrete size.
        crate::constfold::fold_array_lengths(&mut program);
        program
    }

    /// Flatten `program`'s imports into one item list. `prefix` is the qualifier
    /// for this program's *own* items (`""` for the root): every imported module
    /// is recursively flattened under its own qualifier (alias or last path
    /// segment), and this program's own items are renamed by `prefix` and have
    /// their references to imports collapsed to flat qualified names. See
    /// [`crate::qualify`].
    fn resolve_imports(&mut self, program: Program, parent_dir: &Path, prefix: &str) -> Program {
        let own_names = crate::qualify::top_level_names(&program.items);
        // Per import qualifier -> the set of names that module `export`s. Drives
        // qualified-access resolution and the E503 export check.
        let mut import_exports: HashMap<String, HashSet<String>> = HashMap::new();
        let mut items = Vec::new();
        let mut seen_spans: HashSet<Span> = HashSet::new();
        let mut own_items: Vec<Item> = Vec::new();
        // Wrap-intent spans from this module plus every resolved child.
        let mut wrap_spans = program.wrap_spans;

        for item in program.items {
            match item {
                Item::Import(import) => {
                    let module_name = import
                        .module
                        .iter()
                        .map(|(name, _)| name.as_str())
                        .collect::<Vec<_>>()
                        .join(".");
                    let span = import.module[0].1;
                    // Qualifier: explicit alias, else the module's last path
                    // segment. Stable across importers for plain imports, so a
                    // diamond import dedups to one copy.
                    let qualifier = import.alias.as_ref().map_or_else(
                        || {
                            import
                                .module
                                .last()
                                .map_or(String::new(), |(n, _)| n.clone())
                        },
                        |a| a.0.clone(),
                    );

                    let Some(path) = self.resolve_module_path(&import.module, parent_dir) else {
                        // List every candidate tried (relative dir, then each lib
                        // root) so a lib-path miss is diagnosable, like target
                        // `include`. Mirrors `target::resolve_include`.
                        let searched = std::iter::once(parent_dir)
                            .chain(self.lib_roots.iter().map(PathBuf::as_path))
                            .map(|root| {
                                module_candidate(root, &import.module).display().to_string()
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        self.diags.error(
                            format!("module not found: `{module_name}`; searched: {searched}"),
                            "E501",
                            span,
                        );
                        continue;
                    };

                    let canon = canonicalize(&path);
                    match self.check_cycle(&canon, span) {
                        CycleState::CycleDetected => continue,
                        CycleState::Pushed => {}
                    }

                    let Ok(parsed) = self.load_and_parse(&path, &canon) else {
                        self.visiting.pop();
                        continue;
                    };
                    // Record what this module exports (from its own parsed items,
                    // bare names) so references to `qualifier.x` can be checked.
                    import_exports.insert(
                        qualifier.clone(),
                        crate::qualify::exported_names(&parsed.items),
                    );
                    let module_dir = path
                        .parent()
                        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
                    let child = self.resolve_imports(parsed, &module_dir, &qualifier);
                    self.visiting.pop();

                    wrap_spans.extend_from_slice(&child.wrap_spans);
                    // Child items are already fully qualified; inline (deduped:
                    // a diamond yields identical qualified names).
                    for sub in child.items {
                        push_unique(&mut items, &mut seen_spans, sub);
                    }
                }
                other => own_items.push(other),
            }
        }

        // Rename this module's own items: own top-level names -> `prefix.name`,
        // and `q.x` references to imports -> the flat name `"q.x"`. The renamer
        // also collects E503 violations (qualified access to a non-exported item).
        let renamer = crate::qualify::Renamer {
            local: own_names,
            prefix: prefix.to_string(),
            exports: import_exports,
            errors: std::cell::RefCell::new(Vec::new()),
        };
        renamer.rewrite_items(&mut own_items);
        for (msg, span) in renamer.errors.into_inner() {
            self.diags.error(msg, "E503", span);
        }
        for it in own_items {
            push_unique(&mut items, &mut seen_spans, it);
        }

        Program { items, wrap_spans }
    }

    /// Resolve `import a.b.c;` to a file: the importing file's own directory
    /// first, then each library root in order (intermediate segments are
    /// directories, the last is `<name>.bml`). The first existing file wins, so
    /// a local module always shadows a library one. Returns `None` if no
    /// candidate exists.
    fn resolve_module_path(&self, segments: &[ast::Ident], parent_dir: &Path) -> Option<PathBuf> {
        std::iter::once(parent_dir)
            .chain(self.lib_roots.iter().map(PathBuf::as_path))
            .map(|root| module_candidate(root, segments))
            .find(|candidate| candidate.exists())
    }

    fn load_and_parse(&mut self, path: &Path, canon: &Path) -> Result<Program, ()> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();

        // Reuse a cached parse when the file is unchanged on disk. Re-inject its
        // `SourceFile` (preserving the original `FileId`) so the cached AST's
        // spans stay valid in this analysis's fresh `SourceMap`.
        if let Some(mtime) = mtime
            && let Some(entry) = self.cache.entries.get(canon)
            && entry.mtime == mtime
        {
            self.source_map.insert_file(entry.file.clone());
            return Ok(entry.program.clone());
        }

        let file_id = match self.source_map.add_file(path.to_path_buf()) {
            Ok(id) => id,
            Err(e) => {
                self.diags.error(
                    format!("error reading `{}`: {e}", path.display()),
                    "E501",
                    crate::source::Span::empty(crate::source::FileId::new(), 0),
                );
                return Err(());
            }
        };

        let source = self.source_map.source(file_id);
        let mut parser = Parser::new(source, file_id, &mut self.diags);
        let program = parser.parse_program();

        if self.diags.has_errors() {
            return Err(());
        }

        // Cache the clean parse keyed by canonical path + mtime. Skipped when
        // the mtime is unavailable, so we never serve a stale entry we can't
        // invalidate.
        if let Some(mtime) = mtime {
            let file = self.source_map.get_file(file_id).clone();
            self.cache.entries.insert(
                canon.to_path_buf(),
                CachedModule {
                    mtime,
                    file,
                    program: program.clone(),
                },
            );
        }

        Ok(program)
    }

    fn check_cycle(&mut self, canon: &PathBuf, import_span: crate::source::Span) -> CycleState {
        if self.visiting.contains(canon) {
            let cycle: Vec<String> = self
                .visiting
                .iter()
                .skip_while(|p| *p != canon)
                .chain(std::iter::once(canon))
                .map(|p| {
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .collect();
            self.diags.error(
                format!("circular import: {}", cycle.join(" → ")),
                "E500",
                import_span,
            );
            return CycleState::CycleDetected;
        }
        self.visiting.push(canon.clone());
        CycleState::Pushed
    }
}

#[derive(Clone, Copy)]
enum CycleState {
    CycleDetected,
    Pushed,
}

/// Push an item unless its defining-name span was already inlined. Identity is
/// the span (preserved across `Item::clone()` since `Span` is `Copy`): a diamond
/// import reaches the same definition via the shared cached parse, so the spans
/// match and it dedups; two genuinely distinct definitions that share a name
/// have different spans, both pass through, and the resolver emits E200.
/// (Limitation: importing the *same* file under two different aliases shares the
/// spans, so only the first qualification survives -- vanishingly rare.)
fn push_unique(items: &mut Vec<Item>, seen: &mut HashSet<Span>, item: Item) {
    match item_def_span(&item) {
        Some(span) => {
            if seen.insert(span) {
                items.push(item);
            }
        }
        None => items.push(item),
    }
}

/// Build the candidate path for import `segments` under `root`: intermediate
/// segments are directories, the last is `<name>.bml` (`a.b.c` -> `root/a/b/c.bml`).
fn module_candidate(root: &Path, segments: &[ast::Ident]) -> PathBuf {
    let mut candidate = root.to_path_buf();
    for (i, (seg, _)) in segments.iter().enumerate() {
        if i == segments.len() - 1 {
            candidate.push(format!("{seg}.bml"));
        } else {
            candidate.push(seg);
        }
    }
    candidate
}

fn canonicalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn item_def_span(item: &Item) -> Option<Span> {
    match item {
        Item::FnDef(f) => Some(f.name.1),
        Item::ExternFnDef(e) => Some(e.name.1),
        Item::StaticDef(s) => Some(s.name.1),
        Item::ConstDef(c) => Some(c.name.1),
        Item::PeripheralDef(p) => Some(p.name.1),
        Item::StructDef(s) => Some(s.name.1),
        Item::EnumDef(e) => Some(e.name.1),
        // An `owns` clause has no name; key dedup on its first path's span so a
        // diamond-imported module's claim is not duplicated, while two distinct
        // claims in one file (different spans) are both kept.
        Item::Owns(o) => o.paths.first().map(|p| p.span),
        Item::Import(_) | Item::ComptimeAssert(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{FileId, Span};

    fn seg(name: &str) -> ast::Ident {
        (name.to_string(), Span::new(FileId::new(), 0, 0))
    }

    // `import foo.svd.bar;` resolves from a library root when no local file
    // exists -- the import-side analogue of the target `include` lib fallback.
    #[test]
    fn import_resolves_from_lib_root() {
        let base = std::env::temp_dir().join(format!("bml_imp_lib_{}", std::process::id()));
        let proj = base.join("proj");
        let lib = base.join("lib");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(lib.join("foo/svd")).unwrap();
        std::fs::write(lib.join("foo/svd/bar.bml"), "// lib module\n").unwrap();

        let mut r = ImportResolver::new();
        r.lib_roots = vec![lib.clone()];
        let segs = [seg("foo"), seg("svd"), seg("bar")];
        assert_eq!(
            r.resolve_module_path(&segs, &proj),
            Some(lib.join("foo/svd/bar.bml"))
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // A local module shadows a library one reachable by the same import path.
    #[test]
    fn import_local_shadows_lib() {
        let base = std::env::temp_dir().join(format!("bml_imp_shadow_{}", std::process::id()));
        let proj = base.join("proj");
        let lib = base.join("lib");
        std::fs::create_dir_all(proj.join("foo/svd")).unwrap();
        std::fs::create_dir_all(lib.join("foo/svd")).unwrap();
        std::fs::write(proj.join("foo/svd/bar.bml"), "// local\n").unwrap();
        std::fs::write(lib.join("foo/svd/bar.bml"), "// lib\n").unwrap();

        let mut r = ImportResolver::new();
        r.lib_roots = vec![lib];
        let segs = [seg("foo"), seg("svd"), seg("bar")];
        assert_eq!(
            r.resolve_module_path(&segs, &proj),
            Some(proj.join("foo/svd/bar.bml")),
            "local module must shadow the lib one"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    // Neither relative nor any lib root has the module -> None (caller emits E501).
    #[test]
    fn import_unresolved_is_none() {
        let base = std::env::temp_dir().join(format!("bml_imp_none_{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let r = ImportResolver::new(); // empty lib_roots
        assert_eq!(r.resolve_module_path(&[seg("nope")], &base), None);
        let _ = std::fs::remove_dir_all(&base);
    }
}
