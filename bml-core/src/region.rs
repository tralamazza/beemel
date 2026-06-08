//! Region/agent checks that need the program, the target file, and the symbol
//! table together.
//!
//! Unlike the type checker and borrow checker, this pass takes the `Target`:
//! regions and agents are declared in the target file (the hardware physics),
//! while placement (`in <region>`) and ownership (`owns`) live in source. This
//! is the seam where the two meet.
//!
//! Implemented so far (`doc/regions-agents-plan.md`):
//! - Slice 1: placement-name check (`in <region>` names a real region), the
//!   no-initializer rule (region memory is NOBITS at startup), and the
//!   `in`/`@section` conflict.
//! - Slice 2a: `owns` path resolution against the peripheral table, and
//!   cross-module exclusivity (two modules cannot own the same register).
//!
//! The handoff-ownership-required rule (slice 2b) and the handoff provenance
//! checks (slices 3-4) extend this module.

use crate::ast::{
    Block, Expr, Item, LValue, OwnsPath, Program, StaticDef, Stmt, StorageAnnotation,
};
use crate::errors::DiagnosticBag;
use crate::resolver::SymbolTable;
use crate::source::{FileId, Span};
use crate::target::Target;
use std::collections::{HashMap, HashSet};

/// Run the region/agent checks.
pub fn check(program: &Program, symbols: &SymbolTable, target: &Target, diags: &mut DiagnosticBag) {
    for item in &program.items {
        match item {
            Item::StaticDef(s) => check_placement(s, target, diags),
            Item::Owns(o) => {
                for path in &o.paths {
                    resolve_owns_path(path, symbols, diags);
                }
            }
            _ => {}
        }
    }
    check_ownership_exclusivity(program, symbols, diags);
    check_handoff_ownership(program, symbols, target, diags);
}

// ---- slice 1: placement -----------------------------------------------------

fn check_placement(s: &StaticDef, target: &Target, diags: &mut DiagnosticBag) {
    let Some((region_name, region_span)) = &s.region else {
        return;
    };

    // E600: the placement names a region the target does not define.
    if !target.regions.iter().any(|r| &r.name == region_name) {
        let known = known_regions(target);
        diags.error(
            format!(
                "`{}` is placed `in {region_name}`, but the target defines no such region{known}",
                s.name.0
            ),
            "E600",
            *region_span,
        );
    }

    // E601: region memory is not initialized at startup. The `.region.*`
    // section links as NOBITS and is in neither the `.data` copy nor the `.bss`
    // clear, so an initializer would be silently dropped (verified: the linked
    // ELF has no PROGBITS for it). Require runtime initialization instead --
    // which is how every agent-shared buffer is set up anyway (descriptors and
    // buffers are written before the DMA engine is enabled).
    if s.init.is_some() {
        diags.error(
            format!(
                "`{}` is placed `in {region_name}` and cannot have an initializer: region \
                 memory is not initialized at startup. Drop the `= ...` and set it at runtime \
                 before the agent uses it.",
                s.name.0
            ),
            "E601",
            s.name.1,
        );
    }

    // E602: `in <region>` and an explicit `@section(...)` both set the static's
    // output section -- they would silently fight. Placement wins in codegen,
    // so reject the combination rather than ignore the `@section`.
    if s.storage
        .iter()
        .any(|a| matches!(a, StorageAnnotation::Section(_)))
    {
        diags.error(
            format!(
                "`{}` has both `in {region_name}` and `@section(...)`; a region already \
                 determines the output section. Remove the `@section`.",
                s.name.0
            ),
            "E602",
            s.name.1,
        );
    }
}

/// A " (known regions: a, b)" suffix for the diagnostic, or a hint when the
/// target declares none at all.
fn known_regions(target: &Target) -> String {
    if target.regions.is_empty() {
        " (the target file declares no [region.*] sections)".to_string()
    } else {
        let names: Vec<&str> = target.regions.iter().map(|r| r.name.as_str()).collect();
        format!(" (known regions: {})", names.join(", "))
    }
}

// ---- slice 2a: ownership ----------------------------------------------------

/// What a single `owns` path covers. Field-level ownership is not yet
/// supported (rejected in the parser), so a claim is either a whole peripheral
/// or one register.
enum Claim {
    Peripheral(String),
    Register(String, String),
}

impl Claim {
    /// Whether two claims cover any common register.
    fn overlaps(&self, other: &Claim) -> bool {
        match (self, other) {
            (Claim::Peripheral(a), Claim::Peripheral(b)) => a == b,
            (Claim::Peripheral(p), Claim::Register(rp, _))
            | (Claim::Register(rp, _), Claim::Peripheral(p)) => p == rp,
            (Claim::Register(p1, r1), Claim::Register(p2, r2)) => p1 == p2 && r1 == r2,
        }
    }

    fn display(&self) -> String {
        match self {
            Claim::Peripheral(p) => p.clone(),
            Claim::Register(p, r) => format!("{p}.{r}"),
        }
    }
}

/// Resolve an `owns` path against the peripheral table. Returns the claim when
/// it resolves; emits E603 and returns `None` otherwise.
fn resolve_owns_path(
    path: &OwnsPath,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) -> Option<Claim> {
    let pname = &path.peripheral.0;
    let Some(periph) = symbols.peripherals.get(pname) else {
        diags.error(
            format!("`owns {pname}`: no peripheral named `{pname}`"),
            "E603",
            path.span,
        );
        return None;
    };
    match &path.register {
        None => Some(Claim::Peripheral(pname.clone())),
        Some((rname, _)) => {
            if periph.regs.contains_key(rname) {
                Some(Claim::Register(pname.clone(), rname.clone()))
            } else {
                diags.error(
                    format!(
                        "`owns {pname}.{rname}`: peripheral `{pname}` has no register `{rname}`"
                    ),
                    "E603",
                    path.span,
                );
                None
            }
        }
    }
}

/// E604: a register owned by two different modules. Ownership is a claim about
/// *other* modules, so the conflict is across source files (identified by the
/// span's `FileId`); two `owns` of the same register in one file are not a
/// conflict.
fn check_ownership_exclusivity(
    program: &Program,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) {
    // Collect every resolvable claim with its file and span.
    let mut claims: Vec<(Claim, Span)> = Vec::new();
    for item in &program.items {
        if let Item::Owns(o) = item {
            for path in &o.paths {
                // Resolve quietly here: unresolved paths already reported E603
                // in the main walk; skip them rather than double-reporting.
                if let Some(claim) = resolve_owns_path_quiet(path, symbols) {
                    claims.push((claim, path.span));
                }
            }
        }
    }

    // O(n^2) over a small set: flag the first cross-file overlap for each claim.
    for (i, (claim_i, span_i)) in claims.iter().enumerate() {
        for (claim_j, span_j) in claims.iter().take(i) {
            if span_i.file != span_j.file && claim_i.overlaps(claim_j) {
                diags.error(
                    format!(
                        "`{}` is owned by two modules; ownership must be exclusive (it is a \
                         claim about other modules). One module must own it.",
                        claim_i.display()
                    ),
                    "E604",
                    *span_i,
                );
                break;
            }
        }
    }
}

/// Like `resolve_owns_path` but emits nothing -- used by the exclusivity pass,
/// where unresolved paths were already reported in the main walk.
fn resolve_owns_path_quiet(path: &OwnsPath, symbols: &SymbolTable) -> Option<Claim> {
    let periph = symbols.peripherals.get(&path.peripheral.0)?;
    match &path.register {
        None => Some(Claim::Peripheral(path.peripheral.0.clone())),
        Some((rname, _)) => periph
            .regs
            .contains_key(rname)
            .then(|| Claim::Register(path.peripheral.0.clone(), rname.clone())),
    }
}

// ---- slice 2b: handoff-ownership-required -----------------------------------

/// E605: a write to a handoff register from a module that does not own it.
/// Handoff registers are the registers whose written value an agent
/// dereferences on its own initiative; only the owning module may write them.
/// This is the rule that makes "drives" derivable from `owns` -- owning a
/// handoff register is what licenses a module to command the agent.
fn check_handoff_ownership(
    program: &Program,
    symbols: &SymbolTable,
    target: &Target,
    diags: &mut DiagnosticBag,
) {
    // (peripheral, register) -> agent name, from every agent's handoff list.
    let mut handoff_regs: HashMap<(String, String), String> = HashMap::new();
    for agent in &target.agents {
        for h in &agent.handoffs {
            if let Some((p, r)) = handoff_register_path(&h.register) {
                handoff_regs
                    .entry((p, r))
                    .or_insert_with(|| agent.name.clone());
            }
        }
    }
    if handoff_regs.is_empty() {
        return;
    }

    // Which files own each register, via `owns P` (whole peripheral) or
    // `owns P.R` (single register).
    let mut periph_owners: HashMap<String, HashSet<FileId>> = HashMap::new();
    let mut reg_owners: HashMap<(String, String), HashSet<FileId>> = HashMap::new();
    for item in &program.items {
        if let Item::Owns(o) = item {
            for path in &o.paths {
                match resolve_owns_path_quiet(path, symbols) {
                    Some(Claim::Peripheral(p)) => {
                        periph_owners.entry(p).or_default().insert(path.span.file);
                    }
                    Some(Claim::Register(p, r)) => {
                        reg_owners.entry((p, r)).or_default().insert(path.span.file);
                    }
                    None => {}
                }
            }
        }
    }

    // Every peripheral-register write in the program, with its source file.
    let mut writes = Vec::new();
    collect_peripheral_writes(program, symbols, &mut writes);

    for (periph, reg, span) in writes {
        let Some(agent) = handoff_regs.get(&(periph.clone(), reg.clone())) else {
            continue; // not a handoff register
        };
        let owned = periph_owners
            .get(&periph)
            .is_some_and(|s| s.contains(&span.file))
            || reg_owners
                .get(&(periph.clone(), reg.clone()))
                .is_some_and(|s| s.contains(&span.file));
        if !owned {
            diags.error(
                format!(
                    "`{periph}.{reg}` is a handoff register of agent `{agent}` and may only be \
                     written by the module that owns it. Add `owns {periph}.{reg};` (or \
                     `owns {periph};`) to this module."
                ),
                "E605",
                span,
            );
        }
    }
}

/// Split a handoff register string (`Peripheral.REGISTER[.FIELD]`) into its
/// peripheral and register. A handoff always names at least a register.
fn handoff_register_path(s: &str) -> Option<(String, String)> {
    let mut parts = s.split('.');
    let p = parts.next()?;
    let r = parts.next()?;
    Some((p.to_string(), r.to_string()))
}

/// Collect every peripheral register/field *write* in the program as
/// (peripheral, register, span). Walks all function bodies exhaustively,
/// including statements embedded in block/if/match expressions, so a handoff
/// write cannot hide inside an expression. Reused by later slices (encoding,
/// provenance) which also act at handoff write sites.
fn collect_peripheral_writes(
    program: &Program,
    symbols: &SymbolTable,
    out: &mut Vec<(String, String, Span)>,
) {
    for item in &program.items {
        if let Item::FnDef(f) = item {
            walk_block(&f.body, symbols, out);
        }
    }
}

fn walk_block(block: &Block, symbols: &SymbolTable, out: &mut Vec<(String, String, Span)>) {
    for stmt in &block.stmts {
        walk_stmt(stmt, symbols, out);
    }
    if let Some(trailing) = &block.trailing {
        walk_expr(trailing, symbols, out);
    }
}

fn walk_stmt(stmt: &Stmt, symbols: &SymbolTable, out: &mut Vec<(String, String, Span)>) {
    match stmt {
        Stmt::VarDecl(vd) => walk_expr(&vd.init, symbols, out),
        Stmt::Assign(a) => {
            record_write(&a.target, symbols, out);
            walk_expr(&a.value, symbols, out);
        }
        Stmt::CompoundAssign(ca) => {
            record_write(&ca.target, symbols, out);
            walk_expr(&ca.value, symbols, out);
        }
        Stmt::Expr(e) => walk_expr(e, symbols, out),
        Stmt::If(i) => {
            walk_expr(&i.cond, symbols, out);
            walk_block(&i.then_block, symbols, out);
            if let Some(eb) = &i.else_branch {
                walk_stmt(eb, symbols, out);
            }
        }
        Stmt::Loop(l) => walk_block(&l.body, symbols, out),
        Stmt::While(w) => {
            walk_expr(&w.cond, symbols, out);
            walk_block(&w.body, symbols, out);
        }
        Stmt::For(f) => {
            walk_expr(&f.start, symbols, out);
            walk_expr(&f.end, symbols, out);
            if let Some(step) = &f.step {
                walk_expr(step, symbols, out);
            }
            walk_block(&f.body, symbols, out);
        }
        Stmt::Match(m) => {
            walk_expr(&m.scrutinee, symbols, out);
            for arm in &m.arms {
                walk_block(&arm.body, symbols, out);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                walk_expr(v, symbols, out);
            }
        }
        Stmt::Asm(a) => {
            // Asm output/input operands are expression places; a handoff write
            // would not take this form, but walk them so an embedded block is
            // still reached.
            for (_, target) in &a.outputs {
                walk_expr(target, symbols, out);
            }
            for (_, value) in &a.inputs {
                walk_expr(value, symbols, out);
            }
        }
        Stmt::Assume(a) => walk_expr(&a.cond, symbols, out),
        Stmt::Assert(a) => walk_expr(&a.cond, symbols, out),
        Stmt::Block(b) => walk_block(b, symbols, out),
        Stmt::Break(_) | Stmt::Continue(_) => {}
    }
}

/// Exhaustive expression walk -- only needed to reach statements embedded in
/// block/if/match expressions, but every variant is matched (no catch-all) so
/// a new expression form cannot silently drop a nested write.
fn walk_expr(expr: &Expr, symbols: &SymbolTable, out: &mut Vec<(String, String, Span)>) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::Ident(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            walk_expr(e, symbols, out);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            walk_expr(l, symbols, out);
            walk_expr(r, symbols, out);
        }
        Expr::Call(callee, args) => {
            walk_expr(callee, symbols, out);
            for a in args {
                walk_expr(a, symbols, out);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            walk_expr(base, symbols, out);
            if let Some(l) = len {
                walk_expr(l, symbols, out);
            }
            if let Some(s) = stride {
                walk_expr(s, symbols, out);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            walk_expr(base, symbols, out);
            if let Some(c) = capacity {
                walk_expr(c, symbols, out);
            }
            walk_expr(head, symbols, out);
            walk_expr(len, symbols, out);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            walk_expr(base, symbols, out);
            if let Some(o) = bit_offset {
                walk_expr(o, symbols, out);
            }
            if let Some(l) = len_bits {
                walk_expr(l, symbols, out);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                walk_expr(e, symbols, out);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                walk_expr(e, symbols, out);
            }
        }
        Expr::Match(m) => {
            walk_expr(&m.scrutinee, symbols, out);
            for arm in &m.arms {
                walk_block(&arm.body, symbols, out);
            }
        }
        Expr::Block(b) => walk_block(&b.block, symbols, out),
        Expr::If(i) => {
            walk_expr(&i.cond, symbols, out);
            walk_block(&i.then_block, symbols, out);
            walk_expr(&i.else_branch, symbols, out);
        }
    }
}

/// If `lv` is a write to a peripheral register or one of its fields, record
/// (peripheral, register, span). `P.R = x` is `Field(Name(P), R)`; `P.R.F = x`
/// is `Field(Field(Name(P), R), F)`.
fn record_write(lv: &LValue, symbols: &SymbolTable, out: &mut Vec<(String, String, Span)>) {
    if let LValue::Field(base, field) = lv {
        match base.as_ref() {
            // P.R = ...  (field is the register name)
            LValue::Name((p, _)) if symbols.peripherals.contains_key(p) => {
                out.push((p.clone(), field.0.clone(), field.1));
            }
            // P.R.F = ...  (reg is the register, field is the field)
            LValue::Field(inner, reg) => {
                if let LValue::Name((p, _)) = inner.as_ref()
                    && symbols.peripherals.contains_key(p)
                {
                    out.push((p.clone(), reg.0.clone(), field.1));
                }
            }
            // A non-peripheral name (local/struct), an indexed place, or a
            // pointer deref: not a peripheral-register write path.
            LValue::Name(_) | LValue::Index(..) | LValue::Deref(_) => {}
        }
    }
}
