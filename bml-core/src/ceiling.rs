//! Derived `@shared` ceilings -- the ceiling half of the ownership
//! unification (see doc/regions-agents-plan.md, "Toward unifying with the
//! ceiling protocol").
//!
//! `@shared(ceiling=N)` declares a number the compiler can already compute:
//! every accessor's context is known statically, the emitted critical section
//! is a global mask (`cpsid i`), so N only feeds the access check (E402) and
//! the skip-CS optimization at the top accessor -- both functions of the
//! accessor set. Bare `@shared` therefore asks the compiler to derive it:
//!
//!   ceiling(S) = the highest priority (lowest ARM number) among the contexts
//!   of functions whose bodies mention S.
//!
//! An explicit `ceiling=N` remains valid as a pin: it is used as declared,
//! and an accessor that outranks it still fails E402 at the access site
//! (which is exactly "the pin disagrees with usage"). This mirrors how the
//! agent side derives protection from the sharer set (derived-Move from a
//! region's agents): the annotation acknowledges *that* the data is shared;
//! the mechanism and its parameters come from usage.
//!
//! Conservative approximations, both safe (they can only lower the derived
//! ceiling, i.e. protect more, or leave a conservative critical section in
//! place):
//! - Name-based: a body mentioning identifier S counts as an access even if
//!   a local shadows the static.
//! - Direct contexts only: an `Any`-context function contributes nothing.
//!   Its own accesses are always wrapped in a critical section (`Any` is
//!   conservative in `needs_critical_section`), but the contexts of its
//!   callers are not propagated -- the same blind spot the declared form has
//!   today. Call-graph context propagation is the recorded follow-up.
//! - A bare `@shared` static mentioned by no concrete-context function gets
//!   the thread level (255): only thread/`Any` code touches it, and both
//!   already take the conservative critical section.

use crate::ast::{Block, Expr, Item, LValue, Program, Stmt, StorageAnnotation};
use crate::context::Context;
use std::collections::{HashMap, HashSet};

/// Compute the derived ceiling for every `@shared` static (bare or pinned).
/// The resolver consults this for bare `@shared`; pinned ones keep their
/// declared value.
#[must_use]
pub fn derive_shared_ceilings(program: &Program) -> HashMap<String, u8> {
    let mut shared: HashSet<&str> = HashSet::new();
    for item in &program.items {
        if let Item::StaticDef(s) = item
            && s.storage
                .iter()
                .any(|a| matches!(a, StorageAnnotation::Shared(_)))
        {
            shared.insert(s.name.0.as_str());
        }
    }
    let mut ceilings: HashMap<String, u8> = shared
        .iter()
        .map(|n| ((*n).to_string(), Context::Thread.level()))
        .collect();
    if shared.is_empty() {
        return ceilings;
    }

    for item in &program.items {
        if let Item::FnDef(f) = item {
            let ctx = if let Some(isr) = &f.isr {
                Context::Isr(isr.priority)
            } else {
                match f.context {
                    crate::ast::ContextExpr::Thread => Context::Thread,
                    crate::ast::ContextExpr::Any => Context::Any,
                }
            };
            if ctx == Context::Any {
                continue; // conservative CS regardless; no exclusion info
            }
            let mut mentioned = HashSet::new();
            scan_block(&f.body, &shared, &mut mentioned);
            for name in mentioned {
                // Mentioned names are pre-filtered to the shared set, so the
                // entry always exists; a quiet skip keeps this panic-free.
                if let Some(entry) = ceilings.get_mut(name) {
                    *entry = (*entry).min(ctx.level());
                }
            }
        }
    }
    ceilings
}

// Exhaustive walkers (no catch-all), mirroring region.rs::walk_*: collect
// every identifier mention of a shared static, in expression or lvalue
// position, including inside block/if/match expressions.

fn scan_block<'p>(block: &'p Block, shared: &HashSet<&str>, out: &mut HashSet<&'p str>) {
    for stmt in &block.stmts {
        scan_stmt(stmt, shared, out);
    }
    if let Some(trailing) = &block.trailing {
        scan_expr(trailing, shared, out);
    }
}

fn scan_stmt<'p>(stmt: &'p Stmt, shared: &HashSet<&str>, out: &mut HashSet<&'p str>) {
    match stmt {
        Stmt::VarDecl(vd) => scan_expr(&vd.init, shared, out),
        Stmt::Assign(a) => {
            scan_lvalue(&a.target, shared, out);
            scan_expr(&a.value, shared, out);
        }
        Stmt::CompoundAssign(ca) => {
            scan_lvalue(&ca.target, shared, out);
            scan_expr(&ca.value, shared, out);
        }
        Stmt::Expr(e) => scan_expr(e, shared, out),
        Stmt::If(i) => {
            scan_expr(&i.cond, shared, out);
            scan_block(&i.then_block, shared, out);
            if let Some(eb) = &i.else_branch {
                scan_stmt(eb, shared, out);
            }
        }
        Stmt::Loop(l) => scan_block(&l.body, shared, out),
        Stmt::While(w) => {
            scan_expr(&w.cond, shared, out);
            scan_block(&w.body, shared, out);
        }
        Stmt::For(f) => {
            scan_expr(&f.start, shared, out);
            scan_expr(&f.end, shared, out);
            if let Some(step) = &f.step {
                scan_expr(step, shared, out);
            }
            scan_block(&f.body, shared, out);
        }
        Stmt::Match(m) => {
            scan_expr(&m.scrutinee, shared, out);
            for arm in &m.arms {
                scan_block(&arm.body, shared, out);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                scan_expr(v, shared, out);
            }
        }
        Stmt::Asm(a) => {
            for (_, target) in &a.outputs {
                scan_expr(target, shared, out);
            }
            for (_, value) in &a.inputs {
                scan_expr(value, shared, out);
            }
        }
        Stmt::Assume(a) => scan_expr(&a.cond, shared, out),
        Stmt::Assert(a) => scan_expr(&a.cond, shared, out),
        Stmt::Block(b) => scan_block(b, shared, out),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn scan_lvalue<'p>(lv: &'p LValue, shared: &HashSet<&str>, out: &mut HashSet<&'p str>) {
    match lv {
        LValue::Name((name, _)) => {
            if shared.contains(name.as_str()) {
                out.insert(name.as_str());
            }
        }
        LValue::Field(base, _) => scan_lvalue(base, shared, out),
        LValue::Index(base, idx) => {
            scan_lvalue(base, shared, out);
            scan_expr(idx, shared, out);
        }
        LValue::Deref(e) => scan_expr(e, shared, out),
    }
}

fn scan_expr<'p>(expr: &'p Expr, shared: &HashSet<&str>, out: &mut HashSet<&'p str>) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        Expr::Ident((name, _)) => {
            if shared.contains(name.as_str()) {
                out.insert(name.as_str());
            }
        }
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            scan_expr(e, shared, out);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            scan_expr(l, shared, out);
            scan_expr(r, shared, out);
        }
        Expr::Call(callee, args) => {
            scan_expr(callee, shared, out);
            for a in args {
                scan_expr(a, shared, out);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            scan_expr(base, shared, out);
            if let Some(l) = len {
                scan_expr(l, shared, out);
            }
            if let Some(s) = stride {
                scan_expr(s, shared, out);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            scan_expr(base, shared, out);
            if let Some(c) = capacity {
                scan_expr(c, shared, out);
            }
            scan_expr(head, shared, out);
            scan_expr(len, shared, out);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            scan_expr(base, shared, out);
            if let Some(o) = bit_offset {
                scan_expr(o, shared, out);
            }
            if let Some(l) = len_bits {
                scan_expr(l, shared, out);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                scan_expr(e, shared, out);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                scan_expr(e, shared, out);
            }
        }
        Expr::Match(m) => {
            scan_expr(&m.scrutinee, shared, out);
            for arm in &m.arms {
                scan_block(&arm.body, shared, out);
            }
        }
        Expr::Block(b) => scan_block(&b.block, shared, out),
        Expr::If(i) => {
            scan_expr(&i.cond, shared, out);
            scan_block(&i.then_block, shared, out);
            scan_expr(&i.else_branch, shared, out);
        }
    }
}
