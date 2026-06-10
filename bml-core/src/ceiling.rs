//! Derived `@shared` ceilings -- the ceiling half of the ownership
//! unification (see doc/regions-agents.md, "Toward unifying with the
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
    let edges = fn_mentions(program, &fn_names);
    let (address_taken, indirect_callers) = pointer_call_facts(program, &fn_names);

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
        // Pointer-call closure: a stored function pointer travels invisibly
        // (a fn-ptr static breaks the mention chain), so every
        // ADDRESS-TAKEN Any function inherits the contexts of every
        // function that performs an INDIRECT call -- any stored pointer
        // could reach any pointer-call site. Conservative by construction;
        // direct calls and `&f`-in-body edges stay precise above.
        let mut pool: HashSet<Context> = HashSet::new();
        for caller in &indirect_callers {
            if let Some(set) = possible.get(*caller) {
                pool.extend(set.iter().copied());
            }
        }
        if !pool.is_empty() {
            for taken in &address_taken {
                let Some(sym) = functions.get(*taken) else {
                    continue;
                };
                if sym.context != Context::Any {
                    continue; // concrete fns run in their own context (E408
                    // keeps them out of pointers, entries excepted)
                }
                if let Some(set) = possible.get_mut(*taken) {
                    for c in &pool {
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

/// Per-function mention sets over `names`: for each defined function, every
/// identifier in its body that is in `names` (exhaustive scan, the same one
/// the ceiling derivation uses). With function names this is the
/// (over-approximated) call graph -- `&f` counts as an edge; with static
/// names it is the access map. Shared by context propagation (above) and the
/// cross-core sharing check (`region.rs::check_core_sharing`).
pub(crate) fn fn_mentions<'p>(
    program: &'p Program,
    names: &HashSet<&str>,
) -> HashMap<&'p str, HashSet<&'p str>> {
    let mut out = HashMap::new();
    for item in &program.items {
        if let Item::FnDef(f) = item {
            let mut mentioned = HashSet::new();
            scan_block(&f.body, names, &mut mentioned);
            out.insert(f.name.0.as_str(), mentioned);
        }
    }
    out
}

/// Pointer-call facts for the closure in `propagate_contexts`:
/// - which function names are ADDRESS-TAKEN anywhere (mentioned as a value,
///   not as a direct callee -- includes static/const initializers, which are
///   in no function body and so invisible to `fn_mentions`);
/// - which functions contain an INDIRECT call (a callee that is not a known
///   function name: a fn-pointer local, param, or static).
fn pointer_call_facts<'p>(
    program: &'p Program,
    fn_names: &HashSet<&str>,
) -> (HashSet<&'p str>, HashSet<&'p str>) {
    let mut taken: HashSet<&'p str> = HashSet::new();
    let mut indirect: HashSet<&'p str> = HashSet::new();
    for item in &program.items {
        match item {
            Item::FnDef(f) => {
                let mut has_indirect = false;
                pcf_block(&f.body, fn_names, &mut taken, &mut has_indirect);
                if has_indirect {
                    indirect.insert(f.name.0.as_str());
                }
            }
            // Item-level initializers can take addresses but cannot call.
            Item::StaticDef(sd) => {
                if let Some(init) = &sd.init {
                    let mut sink = false;
                    pcf_expr(init, fn_names, &mut taken, &mut sink);
                }
            }
            Item::ConstDef(cd) => {
                let mut sink = false;
                pcf_expr(&cd.value, fn_names, &mut taken, &mut sink);
            }
            Item::Import(_)
            | Item::Export(_)
            | Item::Owns(_)
            | Item::StructDef(_)
            | Item::EnumDef(_)
            | Item::PeripheralDef(_)
            | Item::ExternFnDef(_)
            | Item::ComptimeAssert(_) => {}
        }
    }
    (taken, indirect)
}

fn pcf_block<'p>(
    block: &'p Block,
    fn_names: &HashSet<&str>,
    taken: &mut HashSet<&'p str>,
    indirect: &mut bool,
) {
    for stmt in &block.stmts {
        pcf_stmt(stmt, fn_names, taken, indirect);
    }
    if let Some(t) = &block.trailing {
        pcf_expr(t, fn_names, taken, indirect);
    }
}

fn pcf_stmt<'p>(
    stmt: &'p Stmt,
    fn_names: &HashSet<&str>,
    taken: &mut HashSet<&'p str>,
    indirect: &mut bool,
) {
    match stmt {
        Stmt::VarDecl(vd) => pcf_expr(&vd.init, fn_names, taken, indirect),
        Stmt::Assign(a) => {
            pcf_lvalue(&a.target, fn_names, taken, indirect);
            pcf_expr(&a.value, fn_names, taken, indirect);
        }
        Stmt::CompoundAssign(ca) => {
            pcf_lvalue(&ca.target, fn_names, taken, indirect);
            pcf_expr(&ca.value, fn_names, taken, indirect);
        }
        Stmt::Expr(e) => pcf_expr(e, fn_names, taken, indirect),
        Stmt::If(i) => {
            pcf_expr(&i.cond, fn_names, taken, indirect);
            pcf_block(&i.then_block, fn_names, taken, indirect);
            if let Some(eb) = &i.else_branch {
                pcf_stmt(eb, fn_names, taken, indirect);
            }
        }
        Stmt::Loop(l) => pcf_block(&l.body, fn_names, taken, indirect),
        Stmt::Claim(c) => pcf_block(&c.body, fn_names, taken, indirect),
        Stmt::While(w) => {
            pcf_expr(&w.cond, fn_names, taken, indirect);
            pcf_block(&w.body, fn_names, taken, indirect);
        }
        Stmt::For(f) => {
            pcf_expr(&f.start, fn_names, taken, indirect);
            pcf_expr(&f.end, fn_names, taken, indirect);
            if let Some(step) = &f.step {
                pcf_expr(step, fn_names, taken, indirect);
            }
            pcf_block(&f.body, fn_names, taken, indirect);
        }
        Stmt::Match(m) => {
            pcf_expr(&m.scrutinee, fn_names, taken, indirect);
            for arm in &m.arms {
                pcf_block(&arm.body, fn_names, taken, indirect);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                pcf_expr(v, fn_names, taken, indirect);
            }
        }
        Stmt::Asm(a) => {
            for (_, target) in &a.outputs {
                pcf_expr(target, fn_names, taken, indirect);
            }
            for (_, value) in &a.inputs {
                pcf_expr(value, fn_names, taken, indirect);
            }
        }
        Stmt::Assume(a) => pcf_expr(&a.cond, fn_names, taken, indirect),
        Stmt::Assert(a) => pcf_expr(&a.cond, fn_names, taken, indirect),
        Stmt::Block(b) => pcf_block(b, fn_names, taken, indirect),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

fn pcf_lvalue<'p>(
    lv: &'p crate::ast::LValue,
    fn_names: &HashSet<&str>,
    taken: &mut HashSet<&'p str>,
    indirect: &mut bool,
) {
    use crate::ast::LValue;
    match lv {
        LValue::Name(_) => {}
        LValue::Field(base, _) => pcf_lvalue(base, fn_names, taken, indirect),
        LValue::Index(base, idx) => {
            pcf_lvalue(base, fn_names, taken, indirect);
            pcf_expr(idx, fn_names, taken, indirect);
        }
        LValue::Deref(e) => pcf_expr(e, fn_names, taken, indirect),
    }
}

fn pcf_expr<'p>(
    expr: &'p Expr,
    fn_names: &HashSet<&str>,
    taken: &mut HashSet<&'p str>,
    indirect: &mut bool,
) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        // A bare function name in value position is an address-take.
        Expr::Ident((name, _)) => {
            if fn_names.contains(name.as_str()) {
                taken.insert(name.as_str());
            }
        }
        // The ONLY place callee position differs from value position: a
        // direct call to a known function is an ordinary edge (fn_mentions
        // covers it); anything else as callee is an indirect call.
        Expr::Call(callee, args) => {
            match callee.as_ref() {
                Expr::Ident((name, _)) if fn_names.contains(name.as_str()) => {}
                other => {
                    *indirect = true;
                    pcf_expr(other, fn_names, taken, indirect);
                }
            }
            for a in args {
                pcf_expr(a, fn_names, taken, indirect);
            }
        }
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            pcf_expr(e, fn_names, taken, indirect);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            pcf_expr(l, fn_names, taken, indirect);
            pcf_expr(r, fn_names, taken, indirect);
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            pcf_expr(base, fn_names, taken, indirect);
            if let Some(l) = len {
                pcf_expr(l, fn_names, taken, indirect);
            }
            if let Some(st) = stride {
                pcf_expr(st, fn_names, taken, indirect);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            pcf_expr(base, fn_names, taken, indirect);
            if let Some(c) = capacity {
                pcf_expr(c, fn_names, taken, indirect);
            }
            pcf_expr(head, fn_names, taken, indirect);
            pcf_expr(len, fn_names, taken, indirect);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            pcf_expr(base, fn_names, taken, indirect);
            if let Some(o) = bit_offset {
                pcf_expr(o, fn_names, taken, indirect);
            }
            if let Some(l) = len_bits {
                pcf_expr(l, fn_names, taken, indirect);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                pcf_expr(e, fn_names, taken, indirect);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                pcf_expr(e, fn_names, taken, indirect);
            }
        }
        Expr::Match(m) => {
            pcf_expr(&m.scrutinee, fn_names, taken, indirect);
            for arm in &m.arms {
                pcf_block(&arm.body, fn_names, taken, indirect);
            }
        }
        Expr::Block(b) => pcf_block(&b.block, fn_names, taken, indirect),
        Expr::If(i) => {
            pcf_expr(&i.cond, fn_names, taken, indirect);
            pcf_block(&i.then_block, fn_names, taken, indirect);
            pcf_expr(&i.else_branch, fn_names, taken, indirect);
        }
    }
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
