use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::ast::{self, Item, Program};
use crate::errors::DiagnosticBag;
use crate::parser::Parser;
use crate::source::SourceMap;

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
    resolved: HashSet<PathBuf>,
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
            resolved: HashSet::new(),
        }
    }

    pub fn resolve(&mut self, root_program: Program, root_path: &Path) -> (Program, AliasMap) {
        let parent_dir = root_path
            .parent()
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        // Track root in visiting so circular imports involving the root are detected
        self.visiting.push(canonicalize(root_path));
        let program = self.resolve_imports(root_program, &parent_dir);
        let aliases = std::mem::take(&mut self.aliases);
        (program, aliases)
    }

    fn resolve_imports(&mut self, program: Program, parent_dir: &Path) -> Program {
        let mut items = Vec::new();

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

                    let Ok(module_program) = self.load_and_parse(&path) else {
                        if matches!(cycle_state, CycleState::Pushed) {
                            self.visiting.pop();
                        }
                        continue;
                    };

                    // Collect export names BEFORE resolving nested imports
                    // (resolve_imports strips Export items)
                    let export_names = collect_export_names(&module_program);

                    let module_dir = path
                        .parent()
                        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
                    let module_program = self.resolve_imports(module_program, &module_dir);

                    let exports = filter_exports(&module_program, &export_names);

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
                        match &import.imports {
                            ast::ImportKind::All => {
                                for item in exports.values() {
                                    items.push(item.clone());
                                }
                            }
                            ast::ImportKind::Selective(names) => {
                                for (ident_name, ident_span) in names {
                                    if let Some(item) = exports.get(ident_name) {
                                        items.push(item.clone());
                                    } else {
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
                        }
                    }

                    self.resolved.insert(canon);
                    if matches!(cycle_state, CycleState::Pushed) {
                        self.visiting.pop();
                    }
                }
                Item::Export(_) => {}
                other => items.push(other),
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
        if self.resolved.contains(canon) {
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
