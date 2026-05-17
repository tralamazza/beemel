use std::collections::{HashMap, HashSet};

use crate::ast::{self, BitSpec, Item, Program, StorageAnnotation};
use crate::context::Context;
use crate::errors::DiagnosticBag;
use crate::types::Type;

#[derive(Debug, Default)]
pub struct SymbolTable {
    pub functions: HashMap<String, FnSymbol>,
    pub statics: HashMap<String, StaticSymbol>,
    pub consts: HashMap<String, ConstSymbol>,
    pub peripherals: HashMap<String, PeripheralSymbol>,
    pub structs: HashMap<String, Vec<(String, crate::types::Type)>>,
    pub enums: HashMap<String, (crate::types::Type, Vec<(String, i64)>)>,
    pub import_aliases: HashMap<String, HashMap<String, Item>>,
}

#[derive(Debug)]
pub struct FnSymbol {
    pub context: Context,
    pub params: Vec<(String, crate::types::Type)>,
    pub ret: Option<crate::types::Type>,
    pub isr_label: Option<String>,
    pub naked: bool,
    pub section: Option<String>,
    pub tailchain: bool,
    pub has_calls: bool,
    /// Stack frame size in bytes (locals + params + temps).
    pub local_frame: u32,
    /// Names of directly-called non-extern functions.
    pub callees: Vec<String>,
    /// Total max stack depth (`local_frame` + deepest callee chain).
    pub max_depth: u32,
}

#[derive(Debug)]
pub struct StaticSymbol {
    pub ty: crate::types::Type,
    pub storage: Vec<StorageAnnotation>,
}

#[derive(Debug)]
pub struct ConstSymbol {
    pub ty: crate::types::Type,
}

#[derive(Debug)]
pub struct PeripheralSymbol {
    pub base_addr: u64,
    pub regs: HashMap<String, RegSymbol>,
}

#[derive(Debug)]
pub struct RegSymbol {
    pub offset: u64,
    pub fields: HashMap<String, FieldSymbol>,
}

#[derive(Debug)]
pub struct FieldSymbol {
    pub bit_spec: BitSpec,
    pub ty: Type,
}

pub struct Resolver {
    table: SymbolTable,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver {
    #[must_use]
    pub fn new() -> Self {
        Resolver {
            table: SymbolTable {
                functions: HashMap::new(),
                statics: HashMap::new(),
                consts: HashMap::new(),
                peripherals: HashMap::new(),
                structs: HashMap::new(),
                enums: HashMap::new(),
                import_aliases: HashMap::new(),
            },
        }
    }

    pub fn resolve(
        mut self,
        program: &Program,
        diags: &mut DiagnosticBag,
        aliases: HashMap<String, HashMap<String, Item>>,
    ) -> SymbolTable {
        self.table.import_aliases = aliases;

        for item in &program.items {
            match item {
                ast::Item::FnDef(f) => self.collect_fn(f, diags),
                ast::Item::ExternFnDef(e) => self.collect_extern_fn(e, diags),
                ast::Item::StaticDef(s) => self.collect_static(s, diags),
                ast::Item::ConstDef(c) => self.collect_const(c, diags),
                ast::Item::PeripheralDef(p) => self.collect_peripheral(p, diags),
                ast::Item::StructDef(s) => self.collect_struct(s, diags),
                ast::Item::EnumDef(e) => self.collect_enum(e, diags),
                ast::Item::Import(_) => {
                    let alias_items: Vec<Item> = self
                        .table
                        .import_aliases
                        .values()
                        .flat_map(|e| e.values().cloned())
                        .collect();
                    // Pass 1a: register structs/enums first (other items may reference them)
                    for item in &alias_items {
                        match item {
                            ast::Item::StructDef(s) => self.collect_struct(s, diags),
                            ast::Item::EnumDef(e) => self.collect_enum(e, diags),
                            _ => {}
                        }
                    }
                    // Pass 1b: register everything else
                    for item in &alias_items {
                        match item {
                            ast::Item::FnDef(f) => self.collect_fn(f, diags),
                            ast::Item::ExternFnDef(e) => self.collect_extern_fn(e, diags),
                            ast::Item::StaticDef(s) => self.collect_static(s, diags),
                            ast::Item::ConstDef(c) => self.collect_const(c, diags),
                            ast::Item::PeripheralDef(p) => self.collect_peripheral(p, diags),
                            ast::Item::StructDef(_) | ast::Item::EnumDef(_) => {}
                            _ => {}
                        }
                    }
                }
                ast::Item::Export(_) => {
                    // Export statements are consumed by import resolver
                }
            }
        }

        // Pass 2: resolve types for functions, statics, consts
        self.resolve_types(program, diags);
        self.resolve_storage_annotations(program, diags);

        self.table
    }

    fn collect_fn(&mut self, f: &ast::FnDef, diags: &mut DiagnosticBag) {
        let name = f.name.0.clone();
        if self.table.functions.contains_key(&name)
            || self.table.statics.contains_key(&name)
            || self.table.consts.contains_key(&name)
            || self.table.structs.contains_key(&name)
            || self.table.enums.contains_key(&name)
        {
            diags.error(format!("duplicate name: `{name}`"), "E200", f.name.1);
            return;
        }

        let context = if let Some(isr) = &f.isr {
            Context::Isr(isr.priority)
        } else {
            context_from_ast(&f.context)
        };
        let params: Vec<(String, crate::types::Type)> = f
            .params
            .iter()
            .map(|p| {
                (
                    p.name.0.clone(),
                    crate::types::resolve_type_expr(&p.ty, &self.table.structs, &self.table.enums),
                )
            })
            .collect();
        let ret = f
            .ret
            .as_ref()
            .map(|ty| crate::types::resolve_type_expr(ty, &self.table.structs, &self.table.enums));
        let isr_label = f.isr.as_ref().and_then(|i| i.label.clone());
        let tailchain = f.isr.as_ref().is_some_and(|i| i.tailchain);
        self.table.functions.insert(
            name,
            FnSymbol {
                context,
                params,
                ret,
                isr_label,
                naked: f.naked,
                section: f.section.clone(),
                tailchain,
                has_calls: false,
                local_frame: 0,
                callees: Vec::new(),
                max_depth: 0,
            },
        );
    }

    fn collect_extern_fn(&mut self, e: &ast::ExternFnDef, diags: &mut DiagnosticBag) {
        let name = e.name.0.clone();
        if self.table.functions.contains_key(&name)
            || self.table.statics.contains_key(&name)
            || self.table.consts.contains_key(&name)
            || self.table.structs.contains_key(&name)
            || self.table.enums.contains_key(&name)
        {
            diags.error(format!("duplicate name: `{name}`"), "E200", e.name.1);
            return;
        }

        let context = if let Some(isr) = &e.isr {
            Context::Isr(isr.priority)
        } else if let Some(ctx) = &e.context {
            context_from_ast(ctx)
        } else {
            Context::Any
        };
        let params: Vec<(String, crate::types::Type)> = e
            .params
            .iter()
            .map(|p| {
                (
                    p.name.0.clone(),
                    crate::types::resolve_type_expr(&p.ty, &self.table.structs, &self.table.enums),
                )
            })
            .collect();
        let ret = e
            .ret
            .as_ref()
            .map(|ty| crate::types::resolve_type_expr(ty, &self.table.structs, &self.table.enums));
        let isr_label = e.isr.as_ref().and_then(|i| i.label.clone());
        self.table.functions.insert(
            name,
            FnSymbol {
                context,
                params,
                ret,
                isr_label,
                naked: false,
                section: None,
                tailchain: false,
                has_calls: false,
                local_frame: 0,
                callees: Vec::new(),
                max_depth: 0,
            },
        );
    }

    fn collect_static(&mut self, s: &ast::StaticDef, diags: &mut DiagnosticBag) {
        let name = s.name.0.clone();
        if self.table.functions.contains_key(&name)
            || self.table.statics.contains_key(&name)
            || self.table.consts.contains_key(&name)
            || self.table.structs.contains_key(&name)
            || self.table.enums.contains_key(&name)
        {
            diags.error(format!("duplicate name: `{name}`"), "E200", s.name.1);
            return;
        }

        let base_ty =
            crate::types::resolve_type_expr(&s.ty, &self.table.structs, &self.table.enums);
        let wrapped_ty = wrap_with_storage(base_ty, &s.storage);

        self.table.statics.insert(
            name,
            StaticSymbol {
                ty: wrapped_ty,
                storage: s.storage.clone(),
            },
        );
    }

    fn collect_const(&mut self, c: &ast::ConstDef, diags: &mut DiagnosticBag) {
        let name = c.name.0.clone();
        if self.table.functions.contains_key(&name)
            || self.table.statics.contains_key(&name)
            || self.table.consts.contains_key(&name)
            || self.table.structs.contains_key(&name)
            || self.table.enums.contains_key(&name)
        {
            diags.error(format!("duplicate name: `{name}`"), "E200", c.name.1);
            return;
        }

        let ty = crate::types::resolve_type_expr(&c.ty, &self.table.structs, &self.table.enums);
        self.table.consts.insert(name, ConstSymbol { ty });
    }

    fn collect_peripheral(&mut self, p: &ast::PeripheralDef, diags: &mut DiagnosticBag) {
        let name = p.name.0.clone();
        if self.table.peripherals.contains_key(&name) {
            diags.error(format!("duplicate peripheral: `{name}`"), "E200", p.name.1);
            return;
        }

        let mut regs = HashMap::new();
        for reg in &p.regs {
            let mut fields = HashMap::new();
            for field in &reg.fields {
                let ty = crate::types::resolve_type_expr(
                    &field.ty,
                    &self.table.structs,
                    &self.table.enums,
                );
                fields.insert(
                    field.name.0.clone(),
                    FieldSymbol {
                        bit_spec: field.bit_spec.clone(),
                        ty,
                    },
                );
            }
            regs.insert(
                reg.name.0.clone(),
                RegSymbol {
                    offset: reg.offset,
                    fields,
                },
            );
        }

        self.table.peripherals.insert(
            name,
            PeripheralSymbol {
                base_addr: p.base_addr,
                regs,
            },
        );
    }

    fn collect_struct(&mut self, s: &ast::StructDef, diags: &mut DiagnosticBag) {
        let name = s.name.0.clone();
        if self.table.structs.contains_key(&name)
            || self.table.enums.contains_key(&name)
            || self.table.functions.contains_key(&name)
            || self.table.statics.contains_key(&name)
            || self.table.consts.contains_key(&name)
            || self.table.structs.contains_key(&name)
            || self.table.enums.contains_key(&name)
        {
            diags.error(format!("duplicate name: `{name}`"), "E200", s.name.1);
            return;
        }

        // Check for duplicate field names
        let mut seen: HashSet<String> = HashSet::new();
        for field in &s.fields {
            if seen.contains(&field.name.0) {
                diags.error(
                    format!("duplicate field `{}` in struct `{name}`", field.name.0),
                    "E319",
                    field.name.1,
                );
            }
            seen.insert(field.name.0.clone());
        }

        // Register with placeholder field types (resolved in pass 2)
        let fields: Vec<(String, crate::types::Type)> = s
            .fields
            .iter()
            .map(|f| {
                (
                    f.name.0.clone(),
                    crate::types::Type::Unresolved(f.name.0.clone()),
                )
            })
            .collect();
        self.table.structs.insert(name, fields);
    }

    fn collect_enum(&mut self, e: &ast::EnumDef, diags: &mut DiagnosticBag) {
        let name = e.name.0.clone();
        if self.table.enums.contains_key(&name)
            || self.table.functions.contains_key(&name)
            || self.table.statics.contains_key(&name)
            || self.table.consts.contains_key(&name)
            || self.table.structs.contains_key(&name)
            || self.table.enums.contains_key(&name)
            || self.table.structs.contains_key(&name)
        {
            diags.error(format!("duplicate name: `{name}`"), "E200", e.name.1);
            return;
        }

        // Resolve the underlying type (must be u8, u16, or u32)
        let inner_ty = crate::types::resolve_type_expr(&e.ty, &self.table.structs, &HashMap::new());
        let (max_val, ll_ty) = match &inner_ty {
            crate::types::Type::U8 => (255i64, crate::types::Type::U8),
            crate::types::Type::U16 => (65535i64, crate::types::Type::U16),
            crate::types::Type::U32 => (4_294_967_295i64, crate::types::Type::U32),
            _ => {
                diags.error(
                    format!("enum underlying type must be u8, u16, or u32, got `{inner_ty:?}`"),
                    "E323",
                    e.ty.span(),
                );
                return;
            }
        };

        // Compute discriminants with auto-increment
        let mut variants: Vec<(String, i64)> = Vec::new();
        let mut next_val: i64 = 0;
        let mut seen: HashSet<String> = HashSet::new();
        for v in &e.variants {
            if seen.contains(&v.name.0) {
                diags.error(
                    format!("duplicate variant `{}` in enum `{name}`", v.name.0),
                    "E319",
                    v.name.1,
                );
            }
            seen.insert(v.name.0.clone());

            let disc = if let Some(val) = v.value {
                #[allow(clippy::cast_possible_wrap)]
                let val = val as i64;
                next_val = val + 1;
                val
            } else {
                let val = next_val;
                next_val += 1;
                val
            };

            if disc > max_val {
                diags.error(
                    format!(
                        "discriminant {disc} for variant `{}` exceeds underlying type range (max {max_val})",
                        v.name.0
                    ),
                    "E323",
                    v.name.1,
                );
            }

            variants.push((v.name.0.clone(), disc));
        }

        self.table.enums.insert(name, (ll_ty, variants));
    }

    fn resolve_types(&mut self, program: &Program, _diags: &mut DiagnosticBag) {
        // Pass 2a: resolve struct field types
        // Now that all struct names are registered, resolve field types properly
        for item in &program.items {
            if let ast::Item::StructDef(s) = item {
                let name = s.name.0.clone();
                let resolved_fields: Vec<(String, crate::types::Type)> = s
                    .fields
                    .iter()
                    .map(|f| {
                        let ty = crate::types::resolve_type_expr(
                            &f.ty,
                            &self.table.structs,
                            &self.table.enums,
                        );
                        (f.name.0.clone(), ty)
                    })
                    .collect();
                self.table.structs.insert(name, resolved_fields);
            }
        }

        // Pass 2b: re-resolve function parameter and return types
        // (struct names in params/ret that were Unresolved in pass 1)
        for item in &program.items {
            match item {
                ast::Item::FnDef(f) => {
                    if let Some(fn_sym) = self.table.functions.get_mut(&f.name.0) {
                        for (i, param) in f.params.iter().enumerate() {
                            fn_sym.params[i].1 = crate::types::resolve_type_expr(
                                &param.ty,
                                &self.table.structs,
                                &self.table.enums,
                            );
                        }
                        if let Some(ret_ty) = &f.ret {
                            fn_sym.ret = Some(crate::types::resolve_type_expr(
                                ret_ty,
                                &self.table.structs,
                                &self.table.enums,
                            ));
                        }
                    }
                }
                ast::Item::ExternFnDef(e) => {
                    if let Some(fn_sym) = self.table.functions.get_mut(&e.name.0) {
                        for (i, param) in e.params.iter().enumerate() {
                            fn_sym.params[i].1 = crate::types::resolve_type_expr(
                                &param.ty,
                                &self.table.structs,
                                &self.table.enums,
                            );
                        }
                        if let Some(ret_ty) = &e.ret {
                            fn_sym.ret = Some(crate::types::resolve_type_expr(
                                ret_ty,
                                &self.table.structs,
                                &self.table.enums,
                            ));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn resolve_storage_annotations(&mut self, _program: &Program, diags: &mut DiagnosticBag) {
        // Validate that @exclusive(owner) references a real function name
        for (name, sym) in &self.table.statics {
            for ann in &sym.storage {
                if let StorageAnnotation::Exclusive((owner, span)) = ann
                    && !self.table.functions.contains_key(owner)
                {
                    diags.error(
                        format!("@exclusive(`{owner}`) on `{name}` references unknown function"),
                        "E201",
                        *span,
                    );
                }
            }
        }
    }
}

fn context_from_ast(ctx: &ast::ContextExpr) -> Context {
    match ctx {
        ast::ContextExpr::Thread => Context::Thread,
        ast::ContextExpr::Any => Context::Any,
    }
}

/// Wrap a base type with storage annotation wrappers.
/// For statics with @exclusive or @shared, the type gets the wrapper.
fn wrap_with_storage(
    base: crate::types::Type,
    storage: &[StorageAnnotation],
) -> crate::types::Type {
    use crate::types::Type;
    let mut ty = base;
    for ann in storage {
        match ann {
            StorageAnnotation::Exclusive(_) => {
                ty = Type::Exclusive(Box::new(ty));
            }
            StorageAnnotation::Shared(ceiling) => {
                ty = Type::Shared(Box::new(ty), *ceiling);
            }
            StorageAnnotation::Dma => {
                ty = Type::Dma(Box::new(ty));
            }
            StorageAnnotation::External => {
                ty = Type::External(Box::new(ty));
            }
            StorageAnnotation::Section(_) => {
                // Section doesn't change the type, only placement
            }
        }
    }
    ty
}
