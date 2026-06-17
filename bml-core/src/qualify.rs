//! Module-qualified name rewriting for the import resolver.
//!
//! BML imports are fully qualified: after `import sensors;` you reach the
//! module's items as `sensors.foo()`, `sensors.Color { ... }`, `sensors.MAX`,
//! `sensors.State@Idle` -- never by a bare name. The import resolver flattens
//! every module into one program, and this pass renames each module's
//! contribution so that:
//!
//! - a module's own top-level items (`foo`, `Color`, ...) become `prefix.foo`,
//!   `prefix.Color`, ... where `prefix` is the qualifier the importer used
//!   (an explicit alias, or the module's last path segment);
//! - references *within* that module to its own items are renamed to match;
//! - references to the module's own imports (`q.x`, parsed as a field access or
//!   a dotted name) collapse to the flat qualified name `"q.x"`.
//!
//! The root program is rewritten with an empty prefix: its own items keep their
//! bare names, but its `q.x` references to imports still collapse to `"q.x"`.
//!
//! Soundness rests on the no-shadowing rule (E347): inside a module a local or
//! parameter can never share a name with a top-level item, so a bare reference
//! that matches a top-level name is unambiguously that item -- no scope tracking
//! is needed to rewrite it.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use crate::ast::{
    Block, Expr, Item, LValue, MatchArm, MatchPattern, RegDef, Stmt, StorageAnnotation, TypeExpr,
};
use crate::source::Span;

/// Renames names in one module's items per the rules in the module docs, and
/// checks that every reference to an imported item names something the imported
/// module actually `export`ed (collected as `errors`, drained by the caller).
pub struct Renamer {
    /// This module's own top-level definition names (bare).
    pub local: HashSet<String>,
    /// Qualifier prefix for this module's own items (`""` for the root).
    pub prefix: String,
    /// The qualifiers this module imports, each mapped to the set of bare names
    /// that module `export`s. A `q.x` reference is valid only if `x` is in
    /// `exports[q]`.
    pub exports: HashMap<String, HashSet<String>>,
    /// `E503` violations collected during the walk (qualified access to a
    /// non-exported item): `(message, span)`.
    pub errors: RefCell<Vec<(String, Span)>>,
}

impl Renamer {
    /// The qualified form of a bare local top-level name, or `None` to leave a
    /// name untouched (not a local item, or the root's bare-kept items).
    fn map(&self, name: &str) -> Option<String> {
        if self.prefix.is_empty() {
            return None;
        }
        self.local
            .contains(name)
            .then(|| format!("{}.{}", self.prefix, name))
    }

    fn rename_in_place(&self, name: &mut String) {
        if let Some(q) = self.map(name) {
            *name = q;
        }
    }

    /// Qualify the field types of a register list -- shared by `peripheral` and
    /// `peripheral_type` (e.g. an enum-typed field naming a module-local enum).
    fn rewrite_reg_field_types(&self, regs: &mut [RegDef]) {
        for reg in regs {
            for field in &mut reg.fields {
                self.rewrite_type(&mut field.ty);
            }
        }
    }

    /// Whether `q` is one of this module's import qualifiers.
    fn is_import(&self, q: &str) -> bool {
        self.exports.contains_key(q)
    }

    /// Record an `E503` if `q` is an import qualifier and `name` is not in its
    /// export set. `q`/`name` are the two halves of a `q.name` reference.
    fn check_export(&self, q: &str, name: &str, span: Span) {
        if let Some(set) = self.exports.get(q)
            && !set.contains(name)
        {
            self.errors.borrow_mut().push((
                format!("`{name}` is not exported from module `{q}` (mark it `export`)"),
                span,
            ));
        }
    }

    /// Export-check an already-dotted `"q.name"` reference (a qualified type,
    /// struct-init, or enum name produced by the parser).
    fn check_dotted(&self, dotted: &str, span: Span) {
        if let Some((q, name)) = dotted.split_once('.') {
            self.check_export(q, name, span);
        }
    }

    pub fn rewrite_items(&self, items: &mut [Item]) {
        for item in items {
            self.rewrite_item(item);
        }
    }

    fn rewrite_item(&self, item: &mut Item) {
        match item {
            Item::FnDef(f) => {
                self.rename_in_place(&mut f.name.0);
                for p in &mut f.params {
                    self.rewrite_type(&mut p.ty);
                }
                if let Some(ret) = &mut f.ret {
                    self.rewrite_type(ret);
                }
                self.rewrite_block(&mut f.body);
            }
            Item::ExternFnDef(f) => {
                self.rename_in_place(&mut f.name.0);
                for p in &mut f.params {
                    self.rewrite_type(&mut p.ty);
                }
                if let Some(ret) = &mut f.ret {
                    self.rewrite_type(ret);
                }
            }
            Item::StaticDef(s) => {
                self.rename_in_place(&mut s.name.0);
                self.rewrite_type(&mut s.ty);
                // `@exclusive(owner)` names a function; qualify it like any
                // other reference to a module-local item.
                for ann in &mut s.storage {
                    if let StorageAnnotation::Exclusive(owner) = ann {
                        self.rename_in_place(&mut owner.0);
                    }
                }
                if let Some(init) = &mut s.init {
                    self.rewrite_expr(init);
                }
            }
            Item::ConstDef(c) => {
                self.rename_in_place(&mut c.name.0);
                self.rewrite_type(&mut c.ty);
                self.rewrite_expr(&mut c.value);
            }
            Item::StructDef(s) => {
                self.rename_in_place(&mut s.name.0);
                for field in &mut s.fields {
                    self.rewrite_type(&mut field.ty);
                }
            }
            Item::EnumDef(e) => {
                self.rename_in_place(&mut e.name.0);
                self.rewrite_type(&mut e.ty);
            }
            Item::PeripheralDef(p) => {
                self.rename_in_place(&mut p.name.0);
                self.rewrite_reg_field_types(&mut p.regs);
            }
            // A `peripheral_type` is global like a peripheral (its name is not
            // qualified), but its field types must be rewritten so an inline
            // enum synthesized in this module still resolves after the template
            // is cloned into instances in another module.
            Item::PeripheralType(t) => self.rewrite_reg_field_types(&mut t.regs),
            // An instance's own name and its `peripheral_type` reference are both
            // global (bare); nothing to qualify.
            Item::PeripheralInstance(_) => {}
            Item::ComptimeAssert(a) => self.rewrite_expr(&mut a.cond),
            // `owns` references a peripheral by name. The grammar parses `a.b`
            // as peripheral `a` + register `b`; disambiguate with import
            // knowledge: if `a` is an import qualifier, `a.b` is the qualified
            // peripheral `"a.b"` (no register). Otherwise `a` is a local
            // peripheral, qualified by this module's prefix.
            Item::Owns(o) => {
                for path in &mut o.paths {
                    if self.is_import(&path.peripheral.0) {
                        if let Some(reg) = path.register.take() {
                            path.peripheral.0 = format!("{}.{}", path.peripheral.0, reg.0);
                        }
                    } else {
                        self.rename_in_place(&mut path.peripheral.0);
                    }
                }
            }
            // Imports are consumed by the resolver before rewriting. Nothing to
            // rename.
            Item::Import(_) => {}
        }
    }

    fn rewrite_block(&self, block: &mut Block) {
        for stmt in &mut block.stmts {
            self.rewrite_stmt(stmt);
        }
        if let Some(trailing) = &mut block.trailing {
            self.rewrite_expr(trailing);
        }
    }

    fn rewrite_stmt(&self, stmt: &mut Stmt) {
        match stmt {
            Stmt::VarDecl(v) => {
                if let Some(ty) = &mut v.ty_ann {
                    self.rewrite_type(ty);
                }
                self.rewrite_expr(&mut v.init);
            }
            Stmt::Assign(a) => {
                self.rewrite_lvalue(&mut a.target);
                self.rewrite_expr(&mut a.value);
            }
            Stmt::CompoundAssign(a) => {
                self.rewrite_lvalue(&mut a.target);
                self.rewrite_expr(&mut a.value);
            }
            Stmt::Expr(e) => self.rewrite_expr(e),
            Stmt::If(i) => {
                self.rewrite_expr(&mut i.cond);
                self.rewrite_block(&mut i.then_block);
                if let Some(eb) = &mut i.else_branch {
                    self.rewrite_stmt(eb);
                }
            }
            Stmt::Loop(l) => self.rewrite_block(&mut l.body),
            Stmt::While(w) => {
                self.rewrite_expr(&mut w.cond);
                self.rewrite_block(&mut w.body);
            }
            Stmt::For(f) => {
                self.rewrite_type(&mut f.ty);
                self.rewrite_expr(&mut f.start);
                self.rewrite_expr(&mut f.end);
                if let Some(step) = &mut f.step {
                    self.rewrite_expr(step);
                }
                self.rewrite_block(&mut f.body);
            }
            Stmt::Return(r) => {
                if let Some(v) = &mut r.value {
                    self.rewrite_expr(v);
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
            Stmt::Block(b) => self.rewrite_block(b),
            Stmt::Match(m) => {
                self.rewrite_expr(&mut m.scrutinee);
                for arm in &mut m.arms {
                    self.rewrite_arm(arm);
                }
            }
            Stmt::Asm(a) => {
                for (_, e) in &mut a.outputs {
                    self.rewrite_expr(e);
                }
                for (_, e) in &mut a.inputs {
                    self.rewrite_expr(e);
                }
            }
            Stmt::Assume(a) => self.rewrite_expr(&mut a.cond),
            Stmt::Assert(a) => self.rewrite_expr(&mut a.cond),
            Stmt::Claim(c) => {
                self.rename_in_place(&mut c.name.0);
                self.rewrite_block(&mut c.body);
            }
        }
    }

    fn rewrite_arm(&self, arm: &mut MatchArm) {
        for pat in &mut arm.patterns {
            if let MatchPattern::Variant(enum_name, _) = pat {
                self.rename_in_place(&mut enum_name.0);
                self.check_dotted(&enum_name.0, enum_name.1);
            }
        }
        self.rewrite_block(&mut arm.body);
    }

    fn rewrite_expr(&self, expr: &mut Expr) {
        match expr {
            Expr::IntLiteral(..)
            | Expr::FloatLiteral(..)
            | Expr::BoolLiteral(..)
            | Expr::StringLiteral(..)
            | Expr::NullLiteral(..) => {}
            Expr::Ident(n) => self.rename_in_place(&mut n.0),
            Expr::Unary(_, e) => self.rewrite_expr(e),
            Expr::Binary(l, _, r) => {
                self.rewrite_expr(l);
                self.rewrite_expr(r);
            }
            Expr::Call(callee, args) => {
                self.rewrite_expr(callee);
                for a in args {
                    self.rewrite_expr(a);
                }
            }
            // `q.x` where `q` is an import qualifier collapses to the flat
            // qualified name `"q.x"`. Otherwise it is an ordinary field access
            // on a value -- recurse into the base.
            Expr::FieldAccess(base, field) => {
                if let Expr::Ident(q) = base.as_ref()
                    && self.is_import(&q.0)
                {
                    self.check_export(&q.0, &field.0, field.1);
                    let span = q.1.merge(field.1);
                    *expr = Expr::Ident((format!("{}.{}", q.0, field.0), span));
                } else {
                    self.rewrite_expr(base);
                }
            }
            Expr::Index(base, idx) => {
                self.rewrite_expr(base);
                self.rewrite_expr(idx);
            }
            Expr::Group(e) => self.rewrite_expr(e),
            Expr::Cast(e, ty) => {
                self.rewrite_expr(e);
                self.rewrite_type(ty);
            }
            Expr::SizeOf(ty, _) => self.rewrite_type(ty),
            Expr::ViewNew {
                base, len, stride, ..
            } => {
                self.rewrite_expr(base);
                if let Some(l) = len {
                    self.rewrite_expr(l);
                }
                if let Some(s) = stride {
                    self.rewrite_expr(s);
                }
            }
            Expr::RingNew {
                base,
                capacity,
                head,
                len,
                ..
            } => {
                self.rewrite_expr(base);
                if let Some(c) = capacity {
                    self.rewrite_expr(c);
                }
                self.rewrite_expr(head);
                self.rewrite_expr(len);
            }
            Expr::BitNew {
                base,
                bit_offset,
                len_bits,
                ..
            } => {
                self.rewrite_expr(base);
                if let Some(o) = bit_offset {
                    self.rewrite_expr(o);
                }
                if let Some(l) = len_bits {
                    self.rewrite_expr(l);
                }
            }
            Expr::EnumVariant { enum_name, .. } => {
                self.rename_in_place(&mut enum_name.0);
                self.check_dotted(&enum_name.0, enum_name.1);
            }
            Expr::ArrayInit(elems, _) => {
                for e in elems {
                    self.rewrite_expr(e);
                }
            }
            Expr::StructInit { name, fields, .. } => {
                self.rename_in_place(&mut name.0);
                self.check_dotted(&name.0, name.1);
                for (_, e) in fields {
                    self.rewrite_expr(e);
                }
            }
            Expr::Match(m) => {
                self.rewrite_expr(&mut m.scrutinee);
                for arm in &mut m.arms {
                    self.rewrite_arm(arm);
                }
            }
            Expr::Block(b) => self.rewrite_block(&mut b.block),
            Expr::If(i) => {
                self.rewrite_expr(&mut i.cond);
                self.rewrite_block(&mut i.then_block);
                self.rewrite_expr(&mut i.else_branch);
            }
        }
    }

    fn rewrite_lvalue(&self, lv: &mut LValue) {
        match lv {
            LValue::Name(n) => self.rename_in_place(&mut n.0),
            LValue::Field(inner, field) => {
                self.rewrite_lvalue(inner);
                // `q.x` (an import qualifier followed by a name, e.g. an imported
                // peripheral `mod.GPIO`) collapses to the flat name `"q.x"`, the
                // lvalue analogue of the expression rule.
                if let LValue::Name(q) = inner.as_ref()
                    && self.is_import(&q.0)
                {
                    self.check_export(&q.0, &field.0, field.1);
                    let span = q.1.merge(field.1);
                    *lv = LValue::Name((format!("{}.{}", q.0, field.0), span));
                }
            }
            LValue::Index(inner, idx) => {
                self.rewrite_lvalue(inner);
                self.rewrite_expr(idx);
            }
            LValue::Deref(e) => self.rewrite_expr(e),
        }
    }

    fn rewrite_type(&self, ty: &mut TypeExpr) {
        match ty {
            // A dotted `"q.Type"` (already-qualified, from the parser) is left
            // as-is; a bare local type name is qualified. Either way, a qualified
            // name is export-checked.
            TypeExpr::Named(n) => {
                self.rename_in_place(&mut n.0);
                self.check_dotted(&n.0, n.1);
            }
            TypeExpr::Ptr(inner) | TypeExpr::ConstPtr(inner) => self.rewrite_type(inner),
            TypeExpr::View(inner, _) | TypeExpr::Ring(inner, _) => self.rewrite_type(inner),
            TypeExpr::StridedView(inner, _, stride) => {
                self.rewrite_type(inner);
                self.rewrite_expr(stride);
            }
            TypeExpr::Bits(_) => {}
            TypeExpr::Array(inner, size) => {
                self.rewrite_type(inner);
                self.rewrite_expr(size);
            }
            TypeExpr::Fn(params, ret) => {
                for p in params {
                    self.rewrite_type(p);
                }
                if let Some(r) = ret {
                    self.rewrite_type(r);
                }
            }
            // A region name, not a type name; resolved against the target.
            TypeExpr::Addr(_) => {}
            TypeExpr::Void(_) => {}
        }
    }
}

/// The bare top-level definition names a module program introduces -- the names
/// this pass qualifies. (`import`/`export` define no symbol; `comptime_assert`/
/// `owns` are nameless.)
///
/// Peripherals are deliberately EXCLUDED: a peripheral is global hardware (one
/// `RCC` per chip), addressed at a fixed address and referenced by name in
/// target files (`handoff`, `enabled_by`, `owns`). It stays in a flat shared
/// namespace -- `RCC.APB2ENR` even when imported -- so it is never prefixed and
/// references to it are left bare.
#[must_use]
pub fn top_level_names(items: &[Item]) -> HashSet<String> {
    let mut names = HashSet::new();
    for item in items {
        let name = match item {
            Item::FnDef(f) => Some(&f.name.0),
            Item::ExternFnDef(f) => Some(&f.name.0),
            Item::StaticDef(s) => Some(&s.name.0),
            Item::ConstDef(c) => Some(&c.name.0),
            Item::StructDef(s) => Some(&s.name.0),
            Item::EnumDef(e) => Some(&e.name.0),
            // Peripherals stay bare (global hardware) -- never qualified. A
            // `peripheral_type` and its instances are global the same way.
            Item::PeripheralDef(_)
            | Item::PeripheralType(_)
            | Item::PeripheralInstance(_)
            | Item::Import(_)
            | Item::Owns(_)
            | Item::ComptimeAssert(_) => None,
        };
        if let Some(n) = name {
            names.insert(n.clone());
        }
    }
    names
}

/// The bare names a module marks `export` -- its public API. Peripherals are
/// global (always reachable bare), so they are not part of the qualified-export
/// surface and are excluded here.
#[must_use]
pub fn exported_names(items: &[Item]) -> HashSet<String> {
    let mut names = HashSet::new();
    for item in items {
        let name = match item {
            Item::FnDef(f) if f.exported => Some(&f.name.0),
            Item::ExternFnDef(f) if f.exported => Some(&f.name.0),
            Item::StaticDef(s) if s.exported => Some(&s.name.0),
            Item::ConstDef(c) if c.exported => Some(&c.name.0),
            Item::StructDef(s) if s.exported => Some(&s.name.0),
            Item::EnumDef(e) if e.exported => Some(&e.name.0),
            _ => None,
        };
        if let Some(n) = name {
            names.insert(n.clone());
        }
    }
    names
}
