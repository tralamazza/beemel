use std::collections::{HashMap, HashSet};

use crate::ast::{self, BitSpec, Program, StorageAnnotation};
use crate::context::Context;
use crate::errors::DiagnosticBag;
use crate::types::{StructInfo, Type};

#[derive(Debug, Default, Clone)]
pub struct SymbolTable {
    pub functions: HashMap<String, FnSymbol>,
    pub statics: HashMap<String, StaticSymbol>,
    pub consts: HashMap<String, ConstSymbol>,
    pub peripherals: HashMap<String, PeripheralSymbol>,
    /// `peripheral_type` register layouts (templates), keyed by type name. Unlike
    /// `peripherals` these have no address -- they are types a function parameter
    /// can name (`fn f(u: Usart)`), checked against an instance whose
    /// `PeripheralSymbol::type_name` matches (slice 2).
    pub peripheral_types: HashMap<String, PeripheralTypeSymbol>,
    pub structs: HashMap<String, StructInfo>,
    pub enums: HashMap<String, (crate::types::Type, Vec<(String, i64)>)>,
    /// Possible run contexts per function (`ceiling.rs::propagate_contexts`):
    /// a concrete fn maps to its declared context, an `Any` fn to the union of
    /// its known callers' contexts (empty = no known concrete caller). Closes
    /// the context-laundering hole in E404/E402 and feeds the derived ceiling.
    pub fn_possible_contexts: HashMap<String, Vec<Context>>,
    /// Functions declared as core entry points in the target
    /// (`[agent.X] entry = <fn>`). The launch handshake takes their address
    /// and hands it to HARDWARE (another core's boot), not to a bml pointer
    /// call, so E408's address-of rejection is waived for them even when
    /// they carry a concrete `@context`. Empty without a target.
    pub entry_fns: std::collections::HashSet<String>,
    /// Native byte order of the build target. Resolution is target-agnostic, so
    /// this is the default (little-endian) until a caller with a target sets it
    /// (e.g. `bml build`/`verify`); `bml check` runs without a target and keeps
    /// the default. Consumed by byte-order field diagnostics (E360).
    pub target_endianness: crate::arch::Endianness,
}

#[derive(Debug, Clone)]
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

impl SymbolTable {
    /// A copy with `name`'s `Shared` type wrapper stripped -- the view of the
    /// world inside a `claim name { ... }` window. The checker and the IR
    /// emitter recurse into the claim body with this table, so the static is
    /// its inner type there (views and index-reads allowed); everything else
    /// (including the storage annotation, which the emitter's per-access
    /// critical-section logic reads) is untouched -- the claim's own masked
    /// window covers those accesses.
    #[must_use]
    pub fn with_claimed(&self, name: &str) -> SymbolTable {
        let mut t = self.clone();
        if let Some(sym) = t.statics.get_mut(name)
            && let Type::Shared(inner, _) = &sym.ty
        {
            sym.ty = (**inner).clone();
        }
        t
    }
}

impl FnSymbol {
    /// The function-pointer type produced by reading this function as a value
    /// (a bare function name or `&fn`): parameter types in order, with `void`
    /// for an absent return. The type checker and the IR emitter must agree on
    /// this, so both go through here.
    #[must_use]
    pub fn fn_pointer_type(&self) -> crate::types::Type {
        let params = self.params.iter().map(|(_, t)| t.clone()).collect();
        let ret = self.ret.clone().unwrap_or(crate::types::Type::Void);
        crate::types::Type::Fn(params, Box::new(ret))
    }
}

#[derive(Debug, Clone)]
pub struct StaticSymbol {
    pub ty: crate::types::Type,
    pub storage: Vec<StorageAnnotation>,
}

#[derive(Debug, Clone)]
pub struct ConstSymbol {
    pub ty: crate::types::Type,
}

#[derive(Debug, Clone)]
pub struct PeripheralSymbol {
    pub base_addr: u64,
    pub regs: HashMap<String, RegSymbol>,
    /// The `peripheral_type` this instance was materialized from, or `None` for
    /// an anonymous peripheral. Used to match a `peripheral_type` parameter to a
    /// concrete instance argument (slice 2).
    pub type_name: Option<String>,
}

/// A `peripheral_type` register layout (no address). The type-level counterpart
/// of `PeripheralSymbol`, named by a function parameter.
#[derive(Debug, Clone)]
pub struct PeripheralTypeSymbol {
    pub regs: HashMap<String, RegSymbol>,
}

#[derive(Debug, Clone)]
pub struct RegSymbol {
    pub offset: u64,
    pub access: crate::ast::Access,
    pub fields: HashMap<String, FieldSymbol>,
}

#[derive(Debug, Clone)]
pub struct FieldSymbol {
    pub bit_spec: BitSpec,
    pub ty: Type,
    pub access: crate::ast::Access,
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
                peripheral_types: HashMap::new(),
                structs: HashMap::new(),
                enums: HashMap::new(),
                fn_possible_contexts: HashMap::new(),
                entry_fns: std::collections::HashSet::new(),
                target_endianness: crate::arch::Endianness::default(),
            },
        }
    }

    pub fn resolve(mut self, program: &Program, diags: &mut DiagnosticBag) -> SymbolTable {
        for item in &program.items {
            match item {
                ast::Item::FnDef(f) => self.collect_fn(f, diags),
                ast::Item::ExternFnDef(e) => self.collect_extern_fn(e, diags),
                ast::Item::StaticDef(s) => self.collect_static(s, diags),
                ast::Item::ConstDef(c) => self.collect_const(c, diags),
                ast::Item::PeripheralDef(p) => self.collect_peripheral(p, diags),
                ast::Item::StructDef(s) => self.collect_struct(s, diags),
                ast::Item::EnumDef(e) => self.collect_enum(e, diags),
                ast::Item::Import(_) => {}
                // The template's layout is collected as a type (a function
                // parameter may name it). Its instances were materialized into
                // PeripheralDefs before the resolver; a raw PeripheralInstance
                // only reaches here via the fuzzer (skips import resolution).
                ast::Item::PeripheralType(t) => self.collect_peripheral_type(t, diags),
                ast::Item::PeripheralInstance(_) => {}
                // Defines no symbol; checked separately (owns by the region
                // pass, comptime_assert during type checking).
                ast::Item::Owns(_) | ast::Item::ComptimeAssert(_) => {}
            }
        }

        // Context propagation + derived ceilings. Runs after the items loop
        // (it needs every function's declared context in the table), before
        // the pass-2 type resolution (pass 2c re-wraps static types from the
        // storage patched here, replacing the provisional ceiling
        // collect_static used).
        self.table.fn_possible_contexts =
            crate::ceiling::propagate_contexts(program, &self.table.functions);
        let derived =
            crate::ceiling::derive_shared_ceilings(program, &self.table.fn_possible_contexts);
        for (name, sym) in &mut self.table.statics {
            for ann in &mut sym.storage {
                if let StorageAnnotation::Shared(ceiling @ None) = ann {
                    *ceiling = Some(
                        derived
                            .get(name)
                            .copied()
                            .unwrap_or_else(|| Context::Thread.level()),
                    );
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
        if self.top_level_name_exists(&name) {
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
        if self.top_level_name_exists(&name) {
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
        if self.top_level_name_exists(&name) {
            diags.error(format!("duplicate name: `{name}`"), "E200", s.name.1);
            return;
        }

        // Bare `@shared` keeps ceiling None here; resolve() patches it to the
        // derived value after the items loop (the derivation needs every
        // function's callees), and pass 2c re-wraps the type from the patched
        // storage. The wrap below is therefore provisional for bare `@shared`.
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
        if self.top_level_name_exists(&name) {
            diags.error(format!("duplicate name: `{name}`"), "E200", c.name.1);
            return;
        }

        let ty = crate::types::resolve_type_expr(&c.ty, &self.table.structs, &self.table.enums);
        self.table.consts.insert(name, ConstSymbol { ty });
    }

    fn collect_peripheral(&mut self, p: &ast::PeripheralDef, diags: &mut DiagnosticBag) {
        let name = p.name.0.clone();
        if self.top_level_name_exists(&name) {
            diags.error(format!("duplicate name: `{name}`"), "E200", p.name.1);
            return;
        }

        let regs = self.resolve_reg_map(&name, &p.regs, diags);
        self.table.peripherals.insert(
            name,
            PeripheralSymbol {
                base_addr: p.base_addr,
                regs,
                type_name: p.of_type.as_ref().map(|t| t.0.clone()),
            },
        );
    }

    /// Collect a `peripheral_type` template's register layout as a type. The
    /// E115/E200 uniqueness checks ran in `elaborate_peripheral_types`; here we
    /// just record the layout (last write wins on an already-reported dup).
    fn collect_peripheral_type(&mut self, t: &ast::PeripheralTypeDef, diags: &mut DiagnosticBag) {
        let regs = self.resolve_reg_map(&t.name.0, &t.regs, diags);
        self.table
            .peripheral_types
            .insert(t.name.0.clone(), PeripheralTypeSymbol { regs });
    }

    /// Resolve a register list into the symbol-table layout. Shared by
    /// `collect_peripheral` (instances) and `collect_peripheral_type`
    /// (templates). Field enum types are re-resolved in pass 2e once every enum
    /// is collected.
    fn resolve_reg_map(
        &self,
        name: &str,
        reg_defs: &[ast::RegDef],
        diags: &mut DiagnosticBag,
    ) -> HashMap<String, RegSymbol> {
        let mut regs = HashMap::new();
        for reg in reg_defs {
            if regs.contains_key(&reg.name.0) {
                diags.error(
                    format!("duplicate register `{}` in peripheral `{name}`", reg.name.0),
                    "E200",
                    reg.name.1,
                );
                continue;
            }

            let mut fields = HashMap::new();
            for field in &reg.fields {
                if fields.contains_key(&field.name.0) {
                    diags.error(
                        format!(
                            "duplicate field `{}` in register `{}.{}`",
                            field.name.0, name, reg.name.0
                        ),
                        "E319",
                        field.name.1,
                    );
                    continue;
                }

                let ty = crate::types::resolve_type_expr(
                    &field.ty,
                    &self.table.structs,
                    &self.table.enums,
                );
                let field_access = field.access.unwrap_or(crate::ast::Access::ReadWrite);
                // Validate bit spec
                match &field.bit_spec {
                    crate::ast::BitSpec::Single(n) => {
                        if *n >= 32 {
                            diags.error(
                                format!("bit index {n} out of range (must be 0..32)"),
                                "E114",
                                field.name.1,
                            );
                        }
                    }
                    crate::ast::BitSpec::Range(lo, hi) => {
                        if *lo >= 32 || *hi >= 32 {
                            diags.error(
                                format!("bit range [{lo}..{hi}] out of range (must be 0..32)"),
                                "E114",
                                field.name.1,
                            );
                        } else if lo > hi {
                            diags.error(
                                format!("invalid bit range [{lo}..{hi}] (low must be <= high)"),
                                "E114",
                                field.name.1,
                            );
                        }
                    }
                }
                fields.insert(
                    field.name.0.clone(),
                    FieldSymbol {
                        bit_spec: field.bit_spec.clone(),
                        ty,
                        access: field_access,
                    },
                );
            }
            let reg_access = derive_reg_access(&fields);
            regs.insert(
                reg.name.0.clone(),
                RegSymbol {
                    offset: reg.offset,
                    access: reg_access,
                    fields,
                },
            );
        }
        regs
    }

    fn collect_struct(&mut self, s: &ast::StructDef, diags: &mut DiagnosticBag) {
        let name = s.name.0.clone();
        if self.top_level_name_exists(&name) {
            diags.error(format!("duplicate name: `{name}`"), "E200", s.name.1);
            return;
        }

        // Check for duplicate field names. `_` is explicit padding and may be repeated.
        let mut seen: HashSet<String> = HashSet::new();
        for field in &s.fields {
            if field.name.0 == "_" {
                continue;
            }
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
        self.table.structs.insert(
            name,
            StructInfo {
                repr: s.repr,
                fields,
                field_endian: s.fields.iter().map(|f| f.endian).collect(),
            },
        );
    }

    fn collect_enum(&mut self, e: &ast::EnumDef, diags: &mut DiagnosticBag) {
        let name = e.name.0.clone();
        if self.top_level_name_exists(&name) {
            diags.error(format!("duplicate name: `{name}`"), "E200", e.name.1);
            return;
        }

        // Resolve the underlying type (must be u8, u16, or u32)
        let inner_ty = crate::types::resolve_type_expr(&e.ty, &self.table.structs, &HashMap::new());
        let (max_val, ll_ty) = match &inner_ty {
            crate::types::Type::U8 => (i128::from(u8::MAX), crate::types::Type::U8),
            crate::types::Type::U16 => (i128::from(u16::MAX), crate::types::Type::U16),
            crate::types::Type::U32 => (i128::from(u32::MAX), crate::types::Type::U32),
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
        // Use i128 to avoid u64→i64 wrap for large values
        let mut variants: Vec<(String, i64)> = Vec::new();
        let mut next_val: i128 = 0;
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

            let disc: i128 = if let Some(val) = v.value {
                let val = i128::from(val);
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

            #[allow(clippy::cast_possible_truncation)]
            variants.push((v.name.0.clone(), disc as i64));
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
                self.table.structs.insert(
                    name,
                    StructInfo {
                        repr: s.repr,
                        fields: resolved_fields,
                        field_endian: s.fields.iter().map(|f| f.endian).collect(),
                    },
                );
            }
        }

        // Pass 2b: re-resolve function parameter and return types
        // (struct names in params/ret that were Unresolved in pass 1). A param
        // naming a `peripheral_type` is upgraded to a `PeripheralHandle` here
        // (slice 2) -- only valid on a `fn`, not an `extern fn`.
        let periph_type_names: std::collections::HashSet<String> =
            self.table.peripheral_types.keys().cloned().collect();
        for item in &program.items {
            match item {
                ast::Item::FnDef(f) => {
                    if let Some(fn_sym) = self.table.functions.get_mut(&f.name.0) {
                        for (i, param) in f.params.iter().enumerate() {
                            fn_sym.params[i].1 = crate::types::upgrade_peripheral_handle(
                                crate::types::resolve_type_expr(
                                    &param.ty,
                                    &self.table.structs,
                                    &self.table.enums,
                                ),
                                &periph_type_names,
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

        // TODO: passes 2c/2d/2e re-resolve types after the symbol table is built
        // because pass 1 ran before all struct/enum names were registered.
        // Collapse into a single post-symbol resolution pass; adding a new top-level
        // item kind currently requires remembering to add another 2x pass here.

        // Pass 2c: re-resolve static types
        // (struct/enum names that were Unresolved in pass 1)
        for item in &program.items {
            if let ast::Item::StaticDef(s) = item {
                // Borrow-split: resolve against structs/enums first, then
                // re-wrap with the symbol's storage -- the MATERIALIZED copy
                // from collect_static (bare `@shared` already carries its
                // derived ceiling), not the raw AST annotations.
                let base_ty =
                    crate::types::resolve_type_expr(&s.ty, &self.table.structs, &self.table.enums);
                if let Some(sym) = self.table.statics.get_mut(&s.name.0) {
                    let wrapped_ty = wrap_with_storage(base_ty, &sym.storage);
                    sym.ty = wrapped_ty;
                }
            }
        }

        // Pass 2d: re-resolve const types
        for item in &program.items {
            if let ast::Item::ConstDef(c) = item
                && let Some(sym) = self.table.consts.get_mut(&c.name.0)
            {
                sym.ty =
                    crate::types::resolve_type_expr(&c.ty, &self.table.structs, &self.table.enums);
            }
        }

        // Pass 2e: re-resolve peripheral field types now that every enum/struct
        // is collected (collection order has peripherals before enums). Covers
        // both instances (peripherals) and `peripheral_type` templates.
        for item in &program.items {
            match item {
                ast::Item::PeripheralDef(p) => {
                    if let Some(periph_sym) = self.table.peripherals.get_mut(&p.name.0) {
                        Self::reresolve_field_types(
                            &p.regs,
                            &mut periph_sym.regs,
                            &self.table.structs,
                            &self.table.enums,
                        );
                    }
                }
                ast::Item::PeripheralType(t) => {
                    if let Some(ty_sym) = self.table.peripheral_types.get_mut(&t.name.0) {
                        Self::reresolve_field_types(
                            &t.regs,
                            &mut ty_sym.regs,
                            &self.table.structs,
                            &self.table.enums,
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Re-resolve each field's type against the now-complete struct/enum tables
    /// (pass 2e). Shared by peripheral instances and `peripheral_type` templates.
    fn reresolve_field_types(
        reg_defs: &[ast::RegDef],
        regs: &mut HashMap<String, RegSymbol>,
        structs: &HashMap<String, StructInfo>,
        enums: &crate::types::EnumDefs,
    ) {
        for reg in reg_defs {
            if let Some(reg_sym) = regs.get_mut(&reg.name.0) {
                for field in &reg.fields {
                    if let Some(field_sym) = reg_sym.fields.get_mut(&field.name.0) {
                        field_sym.ty = crate::types::resolve_type_expr(&field.ty, structs, enums);
                    }
                }
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

    fn top_level_name_exists(&self, name: &str) -> bool {
        self.table.functions.contains_key(name)
            || self.table.statics.contains_key(name)
            || self.table.consts.contains_key(name)
            || self.table.peripherals.contains_key(name)
            || self.table.structs.contains_key(name)
            || self.table.enums.contains_key(name)
    }
}

fn context_from_ast(ctx: &ast::ContextExpr) -> Context {
    match ctx {
        ast::ContextExpr::Thread => Context::Thread,
        ast::ContextExpr::Any => Context::Any,
    }
}

fn derive_reg_access(fields: &HashMap<String, FieldSymbol>) -> crate::ast::Access {
    if fields.is_empty() {
        return crate::ast::Access::ReadWrite;
    }
    let first = fields.values().next().unwrap().access;
    if first == crate::ast::Access::ReadWrite {
        return crate::ast::Access::ReadWrite;
    }
    if fields.values().all(|f| f.access == first) {
        first
    } else {
        crate::ast::Access::ReadWrite
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
                // None only during the provisional wrap in collect_static
                // (bare `@shared` before derivation); resolve() patches the
                // storage and pass 2c re-wraps with the real value. The
                // thread level stands in until then -- nothing reads the
                // number from the type in between (decisions read storage).
                ty = Type::Shared(
                    Box::new(ty),
                    ceiling.unwrap_or_else(|| Context::Thread.level()),
                );
            }
            // `@dma` and `@external` are distinct keywords (different intent)
            // but the same type: memory an autonomous agent concurrently
            // accesses. The agent kind is not part of type identity.
            StorageAnnotation::Dma | StorageAnnotation::External => {
                ty = Type::AgentShared(Box::new(ty));
            }
            StorageAnnotation::Section(_) | StorageAnnotation::Align(_) => {
                // Section/align don't change the type, only placement/layout
            }
        }
    }
    ty
}
