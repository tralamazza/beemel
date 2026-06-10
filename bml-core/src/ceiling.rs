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
//! - `Any`-context functions contribute the contexts of their known callers
//!   (`propagate_contexts` below); with no known concrete caller they
//!   contribute nothing -- their own accesses always take the conservative
//!   critical section (`Any` in `needs_critical_section`).
//! - A bare `@shared` static mentioned by no concrete-context function gets
//!   the thread level (255): only thread/`Any` code touches it, and both
//!   already take the conservative critical section.

use crate::ast::{Block, Expr, Item, LValue, Program, Stmt, StorageAnnotation};
use crate::context::Context;
use crate::resolver::FnSymbol;
use std::collections::{HashMap, HashSet};

/// Propagate caller contexts through the call graph into `Any`-context
/// functions: an `Any` function runs in whatever context its callers run in,
/// so its possible-context set is the union of theirs (computed to fixpoint;
/// `Any`-through-`Any` chains converge). Concrete functions keep exactly
/// their declared context -- E403 already forbids cross-context calls, so a
/// `@context(thread)` body cannot run in ISR context.
///
/// This closes the context-laundering hole: an unannotated helper called from
/// an ISR used to look context-free at its access sites, hiding the ISR from
/// E404/E402 and from the derived-ceiling computation.
///
/// Call edges are collected here from the AST (the resolver's
/// `FnSymbol.callees` is filled by a later pass), reusing the same exhaustive
/// mention scan as the ceiling derivation: any mention of a function name in
/// a body counts as an edge. That over-approximates address-taking
/// (`&helper`) as a call -- the safe direction, since taking an fn's address
/// in an ISR strongly implies invoking it there. The remaining blind spot,
/// deliberate: a pointer CALL whose pointee was taken elsewhere is not
/// connected to the calling site's context. Same acceptance as today;
/// recorded in the plan doc.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn propagate_contexts(
    program: &Program,
    functions: &HashMap<String, FnSymbol>,
) -> HashMap<String, Vec<Context>> {
    let fn_names: HashSet<&str> = functions.keys().map(String::as_str).collect();
    let mut edges: HashMap<&str, HashSet<&str>> = HashMap::new();
    for item in &program.items {
        if let Item::FnDef(f) = item {
            let mut mentioned = HashSet::new();
            scan_block(&f.body, &fn_names, &mut mentioned);
            edges.insert(f.name.0.as_str(), mentioned);
        }
    }

    let mut possible: HashMap<String, HashSet<Context>> = functions
        .iter()
        .map(|(name, f)| {
            let mut set = HashSet::new();
            if f.context != Context::Any {
                set.insert(f.context);
            }
            (name.clone(), set)
        })
        .collect();

    loop {
        let mut changed = false;
        for (caller, callees) in &edges {
            let Some(caller_set) = possible.get(*caller) else {
                continue;
            };
            let caller_set: Vec<Context> = caller_set.iter().copied().collect();
            for callee in callees {
                let Some(callee_sym) = functions.get(*callee) else {
                    continue;
                };
                if callee_sym.context != Context::Any {
                    continue; // concrete callee runs in its own context
                }
                if let Some(set) = possible.get_mut(*callee) {
                    for c in &caller_set {
                        changed |= set.insert(*c);
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    possible
        .into_iter()
        .map(|(name, set)| {
            let mut v: Vec<Context> = set.into_iter().collect();
            v.sort_by_key(|c| c.level());
            (name, v)
        })
        .collect()
}

/// Compute the derived ceiling for every `@shared` static (bare or pinned).
/// The resolver consults this for bare `@shared`; pinned ones keep their
/// declared value. `possible` is the propagated context map: an `Any`
/// function contributes the contexts it can actually run in (its known
/// callers), not nothing.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn derive_shared_ceilings(
    program: &Program,
    possible: &HashMap<String, Vec<Context>>,
) -> HashMap<String, u8> {
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
            // The contexts this body can run in: its declared context, or --
            // for an `Any` fn -- its propagated caller contexts. An `Any` fn
            // with no known concrete caller contributes nothing (its accesses
            // always take the conservative critical section anyway).
            let Some(contexts) = possible.get(&f.name.0) else {
                continue;
            };
            let Some(level) = contexts
                .iter()
                .filter(|c| **c != Context::Any)
                .map(|c| c.level())
                .min()
            else {
                continue;
            };
            let mut mentioned = HashSet::new();
            scan_block(&f.body, &shared, &mut mentioned);
            for name in mentioned {
                // Mentioned names are pre-filtered to the shared set, so the
                // entry always exists; a quiet skip keeps this panic-free.
                if let Some(entry) = ceilings.get_mut(name) {
                    *entry = (*entry).min(level);
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
        Stmt::Claim(c) => {
            if shared.contains(c.name.0.as_str()) {
                out.insert(c.name.0.as_str());
            }
            scan_block(&c.body, shared, out);
        }
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
