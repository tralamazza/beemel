use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::{self, Item, Program};
use crate::errors::DiagnosticBag;
use crate::parser::Parser;
use crate::source::{SourceMap, Span};

pub type Exports = HashMap<String, Item>;
pub type AliasMap = HashMap<String, AliasInfo>;

#[derive(Debug, Clone)]
pub struct AliasInfo {
    pub exports: Exports,
    pub items: Vec<Item>,
}

pub struct ImportResolver {
    pub source_map: SourceMap,
    pub diags: DiagnosticBag,
    pub aliases: AliasMap,
    visiting: Vec<PathBuf>,
    /// Resolved modules, keyed by canonical path. Cached so a module reached
    /// via two import paths (diamond) shares one parse / one set of `FileId`s,
    /// keeping span-based dedup stable.
    resolved: HashMap<PathBuf, ResolvedModule>,
}

#[derive(Clone)]
struct ResolvedModule {
    program: Program,
    export_names: HashSet<String>,
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
            aliases: HashMap::new(),
            visiting: Vec::new(),
            resolved: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, root_program: Program, root_path: &Path) -> (Program, AliasMap) {
        let parent_dir = root_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        // Track root in visiting so circular imports involving the root are detected
        self.visiting.push(canonicalize(root_path));
        let mut program = self.resolve_imports(root_program, &parent_dir);
        // With all modules flattened in, fold const-valued array lengths
        // (e.g. `[u8; N]`) into literals so type resolution sees a concrete size.
        crate::constfold::fold_array_lengths(&mut program);
        let aliases = std::mem::take(&mut self.aliases);
        (program, aliases)
    }

    fn resolve_imports(&mut self, program: Program, parent_dir: &Path) -> Program {
        let mut items = Vec::new();
        let mut seen_defs: HashSet<Span> = HashSet::new();

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

                    let module_path = self.resolve_module_path(&import.module, parent_dir);
                    let Some(path) = module_path else {
                        self.diags.error(
                            format!(
                                "module not found: `{module_name}` (expected `{}.bml`)",
                                import
                                    .module
                                    .iter()
                                    .map(|(name, _)| name.as_str())
                                    .collect::<Vec<_>>()
                                    .join("/"),
                            ),
                            "E501",
                            span,
                        );
                        continue;
                    };

                    let canon = canonicalize(&path);
                    let cycle_state = self.check_cycle(&canon, span);
                    match cycle_state {
                        CycleState::CycleDetected => continue,
                        CycleState::AlreadyResolved | CycleState::Pushed => {}
                    }

                    // Use the cached resolved Program when this module has
                    // already been resolved via another import path. Sharing
                    // the parse means identical `FileId`s / `Span`s, which is
                    // what span-based dedup keys on.
                    let resolved = if let CycleState::AlreadyResolved = cycle_state {
                        self.resolved
                            .get(&canon)
                            .expect("AlreadyResolved implies cached")
                            .clone()
                    } else {
                        let Ok(parsed) = self.load_and_parse(&path) else {
                            if matches!(cycle_state, CycleState::Pushed) {
                                self.visiting.pop();
                            }
                            continue;
                        };
                        // Collect export names BEFORE resolving nested imports
                        // (`resolve_imports` drops Export items).
                        let export_names = collect_export_names(&parsed);
                        let module_dir = path
                            .parent()
                            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
                        let program = self.resolve_imports(parsed, &module_dir);
                        ResolvedModule {
                            program,
                            export_names,
                        }
                    };

                    let module_program = &resolved.program;
                    let exports = filter_exports(module_program, &resolved.export_names);

                    if let Some(alias) = &import.alias {
                        self.aliases.insert(
                            alias.0.clone(),
                            AliasInfo {
                                exports,
                                items: module_program.items.clone(),
                            },
                        );
                        items.push(Item::Import(import));
                    } else {
                        // Validate selective imports name only exported items.
                        if let ast::ImportKind::Selective(names) = &import.imports {
                            for (ident_name, ident_span) in names {
                                if !exports.contains_key(ident_name) {
                                    self.diags.error(
                                        format!(
                                            "item `{ident_name}` is not exported from module `{module_name}`"
                                        ),
                                        "E503",
                                        *ident_span,
                                    );
                                }
                            }
                        }
                        // Inline every item from the resolved child module --
                        // both exported and non-exported -- so the type checker
                        // and IR emitter can resolve calls into private helpers
                        // (e.g. lib_b/bar calling lib_c/quux through a wildcard
                        // import in lib_b). Span-based dedup handles diamond
                        // imports where a transitively-imported module reaches
                        // the parent under multiple paths.
                        for sub_item in &module_program.items {
                            push_unique_def(&mut items, &mut seen_defs, sub_item.clone());
                        }
                    }

                    if let CycleState::Pushed = cycle_state {
                        self.resolved.insert(canon, resolved);
                        self.visiting.pop();
                    }
                }
                Item::Export(_) => {}
                other => push_unique_def(&mut items, &mut seen_defs, other),
            }
        }

        Program { items }
    }

    fn resolve_module_path(&self, segments: &[ast::Ident], parent_dir: &Path) -> Option<PathBuf> {
        let _ = self;
        let mut candidate = parent_dir.to_path_buf();
        for (i, (seg, _)) in segments.iter().enumerate() {
            if i == segments.len() - 1 {
                candidate.push(format!("{seg}.bml"));
            } else {
                candidate.push(seg);
            }
        }
        candidate.exists().then_some(candidate)
    }

    fn load_and_parse(&mut self, path: &Path) -> Result<Program, ()> {
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
        if self.resolved.contains_key(canon) {
            return CycleState::AlreadyResolved;
        }
        self.visiting.push(canon.clone());
        CycleState::Pushed
    }
}

#[derive(Clone, Copy)]
enum CycleState {
    CycleDetected,
    AlreadyResolved,
    Pushed,
}

fn collect_export_names(program: &Program) -> HashSet<String> {
    let mut exported_names: HashSet<String> = HashSet::new();
    for item in &program.items {
        if let Item::Export(export) = item {
            for export_item in &export.names {
                let (name, _span) = match export_item {
                    ast::ExportItem::Fn((n, s)) => (n, s),
                    ast::ExportItem::Static((n, s)) => (n, s),
                    ast::ExportItem::Const((n, s)) => (n, s),
                    ast::ExportItem::Peripheral((n, s)) => (n, s),
                    ast::ExportItem::Struct((n, s)) => (n, s),
                    ast::ExportItem::Enum((n, s)) => (n, s),
                };
                exported_names.insert(name.clone());
            }
        }
    }
    exported_names
}

fn filter_exports(program: &Program, exported_names: &HashSet<String>) -> Exports {
    let mut exports: Exports = HashMap::new();
    for item in &program.items {
        let (name, should_export) = match item {
            Item::FnDef(f) => (f.name.0.clone(), exported_names.contains(&f.name.0)),
            Item::ExternFnDef(e) => (e.name.0.clone(), exported_names.contains(&e.name.0)),
            Item::StaticDef(s) => (s.name.0.clone(), exported_names.contains(&s.name.0)),
            Item::ConstDef(c) => (c.name.0.clone(), exported_names.contains(&c.name.0)),
            Item::PeripheralDef(p) => (p.name.0.clone(), exported_names.contains(&p.name.0)),
            Item::StructDef(s) => (s.name.0.clone(), exported_names.contains(&s.name.0)),
            Item::EnumDef(e) => (e.name.0.clone(), exported_names.contains(&e.name.0)),
            _ => continue,
        };
        if should_export {
            exports.insert(name, item.clone());
        }
    }
    exports
}

fn canonicalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Push an item if its defining span hasn't been inlined yet in this scope.
/// Items without a name span (Import/Export) are always pushed.
///
/// Identity is the span of the item's defining name, which is preserved across
/// `Item::clone()` because Span is Copy. Diamond imports where the same
/// definition reaches a parent under multiple paths share an identity and
/// dedup silently. Two distinct definitions that happen to share a name have
/// different spans, both pass through, and the resolver emits E200.
fn push_unique_def(items: &mut Vec<Item>, seen: &mut HashSet<Span>, item: Item) {
    match item_def_span(&item) {
        Some(span) => {
            if seen.insert(span) {
                items.push(item);
            }
        }
        None => items.push(item),
    }
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
        Item::Import(_) | Item::Export(_) | Item::ComptimeAssert(_) => None,
    }
}
