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
//! - Slice 2b/3: handoff write checks -- ownership-required (E605) and the
//!   double-shift guard on auto-encoded `word_addr` handoffs (E606), both off a
//!   single exhaustive write walk.
//! - In-memory handoffs: an `addr in R` struct field must name a real region
//!   (E607), and a descriptor delivered to an agent (`handoff = &RX`) must not
//!   contain an `addr in R` field for a region that agent cannot reach (E608) --
//!   the transitive-reach step, off the same write walk.
//!
//! The handoff provenance checks (slice 4) extend this module.

use crate::ast::{
    BinaryOp, Block, Expr, Item, LValue, OwnsPath, Program, StaticDef, Stmt, StorageAnnotation,
    UnaryOp,
};
use crate::errors::DiagnosticBag;
use crate::resolver::SymbolTable;
use crate::source::{FileId, Span};
use crate::target::{Agent, AgentKind, HandoffEncoding, Region, Target};
use crate::types::Type;
use std::collections::{HashMap, HashSet};

/// Derive `@dma`-style protection from placement (usage dictates declaration).
///
/// `@dma`'s load-bearing property is the index-read restriction: a `Dma`-wrapped
/// value cannot be indexed as an rvalue (the type checker's `index_element_type`
/// accepts only `Array`/`Ptr`/views, so `Dma(Array(..))` is rejected with E326),
/// while the store path unwraps the `Dma` first -- so `BUF[i] = x` is legal but
/// `let v = BUF[i]` is not. That stops software aliasing memory it has handed to
/// an agent.
///
/// Placement is otherwise orthogonal to type, so a `[u32;N] in R` would lose
/// that protection. Here we re-establish it without the hand-written `@dma`: an
/// array static placed `in R`, where `R`'s memory is operated on by a
/// concurrently-mutating agent (a DMA engine or external bus master, the agents
/// `@dma`/`@external` modeled), is wrapped in `Type::Dma`. The existing E326
/// machinery then applies unchanged.
///
/// Runs after resolution and before the type checker, only when a target is
/// present (`bml check` has no target and skips it, like the other region
/// checks). Scoped to array types: E326 is an indexing restriction, and
/// agent-shared memory holds buffers/descriptors (`[u8;N]`/`[RxDesc;N]`).
pub fn apply_derived_move(program: &Program, target: &Target, symbols: &mut SymbolTable) {
    for item in &program.items {
        let Item::StaticDef(s) = item else {
            continue;
        };
        let Some((region_name, _)) = &s.region else {
            continue;
        };
        let Some(region) = target.regions.iter().find(|r| &r.name == region_name) else {
            continue;
        };
        if !region_concurrently_mutated(region, target) {
            continue;
        }
        let Some(sym) = symbols.statics.get_mut(&s.name.0) else {
            continue;
        };
        // Only arrays, and never double-wrap a hand-written `@dma`/`@external`
        // static (whose type is already a Move carrier, not a bare `Array`).
        if matches!(sym.ty, Type::Array(..)) {
            let inner = sym.ty.clone();
            sym.ty = Type::Dma(Box::new(inner));
        }
    }
}

/// Whether `region`'s memory is operated on by a concurrently-mutating agent (a
/// DMA engine or external bus master). A CPU- or debug-probe-only region is
/// normal memory and gets no derived protection.
fn region_concurrently_mutated(region: &Region, target: &Target) -> bool {
    region.agents.iter().any(|agent_name| {
        target.agents.iter().any(|a| {
            &a.name == agent_name && matches!(a.kind, AgentKind::Dma | AgentKind::External)
        })
    })
}

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
            // E607: an `addr in R` struct field must name a real region, else
            // the in-memory handoff obligation would be silently skipped.
            Item::StructDef(s) => {
                for field in &s.fields {
                    if let crate::ast::TypeExpr::Addr((region, span)) = &field.ty
                        && !target.regions.iter().any(|r| &r.name == region)
                    {
                        diags.error(
                            format!(
                                "field `{}.{}` is `addr in {region}`, but the target defines no \
                                 such region{}",
                                s.name.0,
                                field.name.0,
                                known_regions(target)
                            ),
                            "E607",
                            *span,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    check_ownership_exclusivity(program, symbols, diags);
    check_handoff_writes(program, symbols, target, diags);
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

// ---- slices 2b + 3: handoff write checks ------------------------------------

/// One peripheral register/field write, with the source `FileId` (via `span`)
/// and whether its right-hand side is a literal `>> 2`. `field` is `None` for a
/// whole-register write (`P.R = x`) and `Some(F)` for a field write
/// (`P.R.F = x`). Produced by one exhaustive walk and consumed by the ownership
/// rule (E605), the double-shift guard (E606), and the descriptor-reach check
/// (E608). `rhs_static` is the name of the static whose address is delivered
/// (`= &RX_DESC`), used by E608 to find the descriptor handed to an agent.
struct PeriphWrite {
    periph: String,
    reg: String,
    field: Option<String>,
    span: Span,
    rhs_is_shr2: bool,
    rhs_static: Option<String>,
}

/// Handoff write checks. Both rules act at peripheral-register write sites, so
/// they share a single walk:
/// - E605: a write to a handoff register from a file that does not own it.
/// - E606: a source-level `>> 2` feeding a `word_addr` handoff field, which the
///   compiler already encodes (slice 3) -- a hand-written shift double-shifts.
fn check_handoff_writes(
    program: &Program,
    symbols: &SymbolTable,
    target: &Target,
    diags: &mut DiagnosticBag,
) {
    // (peripheral, register) -> agent name, and the set of word_addr handoff
    // field paths "P.R.F", from every agent's handoff list.
    let mut handoff_regs: HashMap<(String, String), String> = HashMap::new();
    let mut word_addr_fields: HashSet<String> = HashSet::new();
    for agent in &target.agents {
        for h in &agent.handoffs {
            if let Some((p, r)) = handoff_register_path(&h.register) {
                handoff_regs
                    .entry((p, r))
                    .or_insert_with(|| agent.name.clone());
            }
            if h.encoding == HandoffEncoding::WordAddr {
                word_addr_fields.insert(h.register.clone());
            }
        }
    }
    if handoff_regs.is_empty() && word_addr_fields.is_empty() {
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

    let mut writes = Vec::new();
    collect_peripheral_writes(program, symbols, &mut writes);

    for w in &writes {
        let key = (w.periph.clone(), w.reg.clone());

        // E605: handoff register written without ownership.
        if let Some(agent_name) = handoff_regs.get(&key) {
            let owned = periph_owners
                .get(&w.periph)
                .is_some_and(|s| s.contains(&w.span.file))
                || reg_owners
                    .get(&key)
                    .is_some_and(|s| s.contains(&w.span.file));
            if !owned {
                diags.error(
                    format!(
                        "`{}.{}` is a handoff register of agent `{agent_name}` and may only be \
                         written by the module that owns it. Add `owns {}.{};` (or \
                         `owns {};`) to this module.",
                        w.periph, w.reg, w.periph, w.reg, w.periph
                    ),
                    "E605",
                    w.span,
                );
            }

            // E608: the write delivers a descriptor base to the agent
            // (`handoff = &RX_DESC`). The agent walks that descriptor and
            // dereferences any `addr in R` field inside it, so it must be able
            // to reach every such region. This is the transitive step past the
            // field-level E607 (field names a real region) and validate_regions
            // (the region's own mem is reachable): the field may point into a
            // *different* region the walking agent cannot reach.
            if let Some(static_name) = &w.rhs_static
                && let Some(agent) = target.agents.iter().find(|a| &a.name == agent_name)
            {
                check_descriptor_reach(static_name, agent, symbols, target, w.span, diags);
            }
        }

        // E606: pre-shifted value into an auto-encoded word_addr handoff field.
        if w.rhs_is_shr2
            && let Some(field) = &w.field
            && word_addr_fields.contains(&format!("{}.{}.{}", w.periph, w.reg, field))
        {
            diags.error(
                format!(
                    "`{}.{}.{}` is a word_addr handoff: the compiler inserts the `>> 2`. \
                     Writing a pre-shifted value double-shifts the address; write the byte \
                     address directly (drop the `>> 2`).",
                    w.periph, w.reg, field
                ),
                "E606",
                w.span,
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

/// Collect every peripheral register/field *write* in the program. Walks all
/// function bodies exhaustively, including statements embedded in
/// block/if/match expressions, so a handoff write cannot hide inside an
/// expression. Reused by the provenance slice, which also acts at write sites.
fn collect_peripheral_writes(program: &Program, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
    for item in &program.items {
        if let Item::FnDef(f) = item {
            walk_block(&f.body, symbols, out);
        }
    }
}

fn walk_block(block: &Block, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
    for stmt in &block.stmts {
        walk_stmt(stmt, symbols, out);
    }
    if let Some(trailing) = &block.trailing {
        walk_expr(trailing, symbols, out);
    }
}

fn walk_stmt(stmt: &Stmt, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
    match stmt {
        Stmt::VarDecl(vd) => walk_expr(&vd.init, symbols, out),
        Stmt::Assign(a) => {
            record_write(&a.target, Some(&a.value), symbols, out);
            walk_expr(&a.value, symbols, out);
        }
        Stmt::CompoundAssign(ca) => {
            // The RHS of a compound assign is not the stored value (it is one
            // operand of `target OP= value`), so it is not a "pre-shifted
            // address" for the E606 sense; pass None.
            record_write(&ca.target, None, symbols, out);
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
fn walk_expr(expr: &Expr, symbols: &SymbolTable, out: &mut Vec<PeriphWrite>) {
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

/// If `lv` is a write to a peripheral register or one of its fields, record it.
/// `P.R = x` is `Field(Name(P), R)` (field `None`); `P.R.F = x` is
/// `Field(Field(Name(P), R), F)` (field `Some(F)`). `rhs`, when present, lets
/// the double-shift guard see a literal `>> 2`.
fn record_write(
    lv: &LValue,
    rhs: Option<&Expr>,
    symbols: &SymbolTable,
    out: &mut Vec<PeriphWrite>,
) {
    let shr2 = rhs.is_some_and(is_shr_two);
    let rhs_static = rhs.and_then(addr_of_static).map(str::to_string);
    if let LValue::Field(base, field) = lv {
        match base.as_ref() {
            // P.R = ...  (field is the register name)
            LValue::Name((p, _)) if symbols.peripherals.contains_key(p) => {
                out.push(PeriphWrite {
                    periph: p.clone(),
                    reg: field.0.clone(),
                    field: None,
                    span: field.1,
                    rhs_is_shr2: shr2,
                    rhs_static: rhs_static.clone(),
                });
            }
            // P.R.F = ...  (reg is the register, field is the field)
            LValue::Field(inner, reg) => {
                if let LValue::Name((p, _)) = inner.as_ref()
                    && symbols.peripherals.contains_key(p)
                {
                    out.push(PeriphWrite {
                        periph: p.clone(),
                        reg: reg.0.clone(),
                        field: Some(field.0.clone()),
                        span: field.1,
                        rhs_is_shr2: shr2,
                        rhs_static: rhs_static.clone(),
                    });
                }
            }
            // A non-peripheral name (local/struct), an indexed place, or a
            // pointer deref: not a peripheral-register write path.
            LValue::Name(_) | LValue::Index(..) | LValue::Deref(_) => {}
        }
    }
}

/// Whether `e` is, at top level, a right shift by the literal 2 (`x >> 2`),
/// possibly wrapped in parentheses -- the hand-written word-address encoding.
fn is_shr_two(e: &Expr) -> bool {
    match e {
        Expr::Group(inner) => is_shr_two(inner),
        Expr::Binary(_, BinaryOp::Shr, rhs) => {
            matches!(rhs.as_ref(), Expr::IntLiteral(2, _, _))
        }
        _ => false,
    }
}

/// If `e` is the address of a static (`&S`, `&mut S`, or `&S[..]`, possibly
/// through parentheses or a cast like `&RX_DESC as u32`), return the static's
/// name. This is the descriptor-base delivery form for an in-memory handoff:
/// `Agent.HANDOFF = &RX_DESC` hands the agent the base of `RX_DESC`.
fn addr_of_static(e: &Expr) -> Option<&str> {
    match e {
        Expr::Group(inner) | Expr::Cast(inner, _) => addr_of_static(inner),
        Expr::Unary(UnaryOp::AddrOf | UnaryOp::AddrOfMut, inner) => match inner.as_ref() {
            Expr::Ident((name, _)) => Some(name.as_str()),
            // `&RX_DESC[0]` -- the base element of an array still delivers the
            // whole descriptor block to the agent.
            Expr::Index(base, _) => match base.as_ref() {
                Expr::Ident((name, _)) => Some(name.as_str()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// E608: the agent walks the delivered descriptor and dereferences every
/// `addr in R` field inside it, so it must reach each such region's mem block.
/// `validate_regions` already ensures the descriptor's *own* region is
/// reachable; this catches a field that points into a *different* region the
/// walking agent cannot reach (the DTCM footgun one level deeper).
fn check_descriptor_reach(
    static_name: &str,
    agent: &Agent,
    symbols: &SymbolTable,
    target: &Target,
    span: Span,
    diags: &mut DiagnosticBag,
) {
    let Some(sym) = symbols.statics.get(static_name) else {
        return;
    };
    let mut fields = Vec::new();
    collect_addr_fields(&sym.ty, String::new(), &mut fields);
    for (field_path, region_name) in fields {
        // An unknown region is already reported as E607 at the struct def; skip
        // it here rather than double-reporting against the delivery site.
        let Some(region) = target.regions.iter().find(|r| r.name == region_name) else {
            continue;
        };
        if !agent.reaches(&region.mem) {
            diags.error(
                format!(
                    "`{static_name}` is handed to agent `{}`, but its field `{field_path}` is \
                     `addr in {region_name}` (mem `{}`), which `{}` cannot reach. The agent \
                     would dereference an address outside its reach.",
                    agent.name, region.mem, agent.name
                ),
                "E608",
                span,
            );
        }
    }
}

/// Collect every `addr in R` field reachable in `ty` as (dotted-field-path,
/// region-name), descending through structs and arrays so a descriptor that
/// nests another descriptor (or an array of slots) is not a silent gap. The
/// catch-all covers scalar/view/pointer types, none of which carry an `addr`
/// slot (`addr` is field-only today; see `doc/regions-agents-plan.md`).
fn collect_addr_fields(ty: &Type, prefix: String, out: &mut Vec<(String, String)>) {
    match ty {
        Type::Addr(region) => out.push((prefix, region.clone())),
        Type::Struct(_, _, struct_fields) => {
            for (fname, fty) in struct_fields {
                let path = if prefix.is_empty() {
                    fname.clone()
                } else {
                    format!("{prefix}.{fname}")
                };
                collect_addr_fields(fty, path, out);
            }
        }
        Type::Array(inner, _) => collect_addr_fields(inner, prefix, out),
        // Storage wrappers are layout-transparent; descend so a Dma-wrapped
        // descriptor (a region static gets `Type::Dma` from derived-Move before
        // this check runs) still exposes its `addr in R` fields.
        Type::Exclusive(inner)
        | Type::Mmio(inner)
        | Type::Dma(inner)
        | Type::External(inner)
        | Type::Shared(inner, _) => collect_addr_fields(inner, prefix, out),
        _ => {}
    }
}
