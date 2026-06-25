use std::collections::HashMap;
use std::fmt::Write;

use crate::arch::Arch;
use crate::ast::{self, Expr, LValue, Program, Stmt, StorageAnnotation};
use crate::consteval::{self, ConstVal};
use crate::context::Context;
use crate::resolver::SymbolTable;
use crate::source::{SourceMap, Span};
use crate::types::Type;
use crate::verify::preempt::PreemptInfo;

/// Emits textual LLVM IR from a validated AST + symbol table.
pub struct IrEmitter {
    out: String,
    pub(crate) counter: u32,
    str_counter: u32,
    strings: Vec<String>,
    pub(crate) indent: u32,
    locals: HashMap<String, LocalInfo>,
    label_counter: u32,
    pub(crate) arch: Arch,
    pub(crate) target_interrupts: std::collections::HashMap<String, u16>,
    has_bitband: bool,
    debug: bool,
    source_map: Option<SourceMap>,
    debug_metadata: String,
    debug_counter: u32,
    file_dbg_id: HashMap<crate::source::FileId, u32>,
    fn_scope_id: Option<u32>,
    type_dbg_id: HashMap<String, u32>,
    cu_file_id: Option<u32>,
    cu_id: Option<u32>,
    current_ctx: Context,
    current_label: Option<String>,
    current_fn_params: Vec<(String, String)>,
    alloca_counter: u32,
    verify_mode: bool,
    current_fn_name: String,
    /// Locals in the current function holding raw pointers into agent-shared
    /// memory (`region::agent_ptr_locals`). Accesses through them are lowered
    /// volatile: the agent is a concurrent writer the optimizer cannot see
    /// (a hoisted OWN-bit spin became an infinite branch on the H723).
    agent_ptr_locals: std::collections::HashSet<String>,
    /// While emitting a monomorphized function (a driver specialized for a
    /// concrete instance), maps each `peripheral_type` handle parameter name to
    /// its comptime `Binding` (a peripheral instance), so `u.REG` lowers as
    /// `INSTANCE.REG`. Empty otherwise (slice 2).
    handle_subst: HashMap<String, Binding>,
    /// Monomorphization worklist: handle-parameter functions still to emit, as
    /// (fn name, one `Binding` per handle parameter, in order). Pushed when a
    /// call to a handle fn is lowered; drained after the ordinary functions.
    /// `handle_spec_done` guards against re-emitting one (slice 2).
    handle_spec_queue: Vec<(String, Vec<Binding>)>,
    handle_spec_done: std::collections::HashSet<(String, Vec<Binding>)>,
    /// Resolved `const` values (the `const_values` fixpoint), populated once at
    /// the start of emission so call lowering can const-evaluate `comptime`
    /// value arguments. See `IrConstEnv`.
    const_vals: HashMap<String, ConstVal>,
    /// Errors found during codegen: a `comptime` arg/condition/scrutinee that fails
    /// to evaluate, or overflows its declared type, at a specialization (the checker
    /// can only check param-dependent exprs structurally). `(message, code, span)`,
    /// drained by the driver after `emit`; codegen output is discarded if non-empty.
    comptime_errors: Vec<(String, String, crate::source::Span)>,
    /// Spans of COMPILER-generated arithmetic that wraps by design, recorded
    /// in verify mode only: the ring view's `head + i` (bounded by the
    /// subsequent `% cap` regardless of wrap) and the bit view's
    /// `bit_offset + i` (bounded by the index assume + byte select). The
    /// verify driver merges these with the user's `Program::wrap_spans` so
    /// V130 is not raised for index math the user never wrote -- without
    /// this, any exported fn taking a ring/bits param fails the overflow
    /// contract on synthetic entry-point havoc.
    pub generated_wrap_spans: Vec<crate::source::Span>,
    /// See `set_ecc_scrub_blocks`.
    pub(crate) ecc_scrub_blocks: Vec<(u64, u64)>,
    preempt: Option<PreemptInfo>,
    /// MMIO `(address, or_mask)` writes emitted at the start of `reset_handler`,
    /// before `.data`/`.bss` init. See `Target::startup_init`.
    pub(crate) startup_init: Vec<(u64, u64)>,
    /// Non-cacheable MPU regions `(base, size)` from `Target::mpu_regions`,
    /// programmed at the start of `reset_handler` so a CPU with caches on stays
    /// coherent with the DMA agents sharing the block.
    pub(crate) mpu_regions: Vec<(u64, u64)>,
    /// NVIC priority field width (`Target::priority_bits`); positions the
    /// `@isr(priority=N)` value in the IPR byte emitted in `reset_handler`.
    pub(crate) priority_bits: u8,
    /// MPU programming model for the `mpu_regions` emission (`PMSAv7` `RASR` vs
    /// `PMSAv8` `RBAR`/`RLAR`+`MAIR`). From `Target::mpu_flavor`.
    pub(crate) mpu_flavor: crate::arch::MpuFlavor,
    /// Cross-core lock assignment: `@shared` statics reachable from multiple
    /// cores -> hardware spinlock index (`region::cross_core_locks`). A claim
    /// over one of these additionally spin-acquires/releases its lock.
    pub(crate) cross_core_locks: std::collections::HashMap<String, u32>,
    /// Base address of the hardware spinlock bank (`Target::spinlock_base`).
    pub(crate) spinlock_base: u64,
    /// `(irq, priority)` pairs collected at vector-table assembly. The reset
    /// handler programs them into the NVIC IPR; declared core entries repeat
    /// the sequence in their prologue (banked per-core NVIC -- a secondary
    /// core never runs the reset handler).
    pub(crate) isr_priorities: Vec<(u16, u8)>,
    /// `(slot, priority)` for configurable system-exception `@isr`s
    /// (SVC/PendSV/SysTick and the v7-M faults). Their priority lives in the SCB
    /// SHPR registers, not the NVIC IPR, so it is programmed separately
    /// (`emit_shpr_stores`) -- in the reset handler and every core entry, since
    /// the SCB is banked per core like the NVIC.
    pub(crate) shpr_priorities: Vec<(usize, u8)>,
    /// Nesting depth of `claim` blocks at the current emission point. Inside
    /// a claim (depth > 0) the per-access `@shared` critical sections are
    /// suppressed -- the claim's own cpsid/cpsie pair covers them, and an
    /// inner cpsie would unmask the window early.
    pub(crate) claim_depth: u32,
    /// Names of the statics claimed by the enclosing `claim` windows (a
    /// stack). In verify mode, reads of a CLAIMED static are not havoc'd:
    /// the window's mask stops local preemption and its spinlock (cross-core
    /// statics) excludes the other core, so the value is stable in-window --
    /// the precision the window exists to provide. Other statics read inside
    /// the window keep their havoc (conservative).
    claimed_statics: Vec<String>,
    /// Region-placed static name -> `[lo, hi)` byte range of its region's mem
    /// block. In verify mode, taking `&X as u32` of such a static emits an
    /// `assume` that the address is in this range -- the load-bearing provenance
    /// fact IKOS propagates to the handoff obligation. Empty outside verify.
    region_addr_ranges: HashMap<String, (u64, u64)>,
    /// Handoff register path (`P.R`) -> `[lo, hi)` bounding range of the owning
    /// agent's reachable mem blocks. In verify mode, a write to the register
    /// emits an `assert` that the byte address is in this range. Empty outside
    /// verify.
    handoff_reach_bounds: HashMap<String, (u64, u64)>,
    /// Declared handoff registers (`PERIPH.REG`, from the target's agents).
    /// A store to one is followed by a `dsb`: arming an agent is a posted
    /// Device write, and one left in flight while the bus stays busy was an
    /// observed imprecise-BusFault source (H723 ETH tail pointers,
    /// 2026-06-11). Completion, not just ordering -- a `dmb` is not enough.
    handoff_regs: std::collections::HashSet<String>,
    /// Registers containing a declared clock gate (`enabled_by`, as
    /// `(periph, reg)`). A write to one is followed by a volatile read-back
    /// of the same register: the first peripheral write issued while an
    /// enable is still propagating is silently dropped by the bus (observed
    /// on the H723: the TIM2 PSC write vanished when a scheduling change
    /// closed the gap after the RCC enable). The read forces completion.
    gate_regs: std::collections::HashSet<(String, String)>,
    /// Region name -> `[lo, hi)` byte range of its mem block. In verify mode, a
    /// write to an `addr in R` struct field (an in-memory handoff) asserts the
    /// stored address is in `R`'s range. Empty outside verify.
    region_ranges: HashMap<String, (u64, u64)>,
    /// Extent obligations (verify mode): `(periph, reg)` of a handoff
    /// register -> name of its capacity shadow global. A handoff write of a
    /// direct `&X as u32` stores `sizeof(X)` into the shadow (unknown
    /// deliveries store `u32::MAX` = unconstrained); the extent-field write
    /// asserts against it. Empty outside verify.
    extent_cap_shadows: HashMap<(String, String), String>,
    /// `(periph, reg, field)` of an agent's `extent_by` count field ->
    /// `(byte scale, capacity shadows of the agent's handoff registers)`.
    /// Writing the field asserts `value * scale <= capacity` for each shadow
    /// -- the agent cannot be armed past the buffer it was handed. Empty
    /// outside verify.
    extent_asserts: HashMap<(String, String, String), (u32, Vec<String>)>,
    /// Descriptor extents (verify mode), from `@extent` struct-field
    /// annotations: `(struct, addr field)` -> capacity shadow name, and
    /// `(struct, length field)` -> `(byte scale, optional AND-mask, shadow name)`.
    /// The in-memory analogue of `extent_cap_shadows`/`extent_asserts`; built
    /// from the AST at `emit()`. Empty outside verify.
    desc_cap_shadows: HashMap<(String, String), String>,
    desc_extent_asserts: HashMap<(String, String), (u32, Option<u64>, String)>,
    /// Region name -> derived alignment floor (bytes). A static placed `in R`
    /// gets at least this alignment, so the source need not hand-write
    /// `@align(N)`: alignment is physics (cache line of cacheable memory shared
    /// with a non-coherent agent), derived from the target. See
    /// `doc/regions-agents.md`.
    region_alignments: HashMap<String, u32>,
}

#[derive(Clone)]
pub(crate) struct LocalInfo {
    alloca: String,
    llvm_ty: String,
    bml_type: Type,
}

struct MatchDispatch {
    end_lbl: String,
    ll_ty: String,
    arm_labels: Vec<String>,
    default_lbl: String,
}

/// If `expr` is `&NAME` / `&mut NAME` (possibly wrapped in groups), return
/// `NAME`. Used to recognize `&X as u32` of a region-placed static so the
/// provenance assume lands at the address-of site.
/// The static delivered by an assignment RHS of the form `&X as u32` (through
/// grouping parens). Used by the extent obligation's delivery side; an RHS
/// that is anything else (helper-call result, arithmetic) yields `None` =
/// unknown capacity.
fn delivered_static_name(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Group(inner) => delivered_static_name(inner),
        ast::Expr::Cast(inner, _) => addr_of_static_name(inner),
        _ => None,
    }
}

/// Strip storage wrappers (Shared/AgentShared/...) down to the value type.
fn strip_storage(ty: &Type) -> &Type {
    let mut t = ty;
    loop {
        let inner = t.inner();
        if std::ptr::eq(inner, t) {
            return t;
        }
        t = inner;
    }
}

fn addr_of_static_name(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Group(inner) => addr_of_static_name(inner),
        ast::Expr::Unary(ast::UnaryOp::AddrOf | ast::UnaryOp::AddrOfMut, inner) => {
            match inner.as_ref() {
                ast::Expr::Ident((name, _)) => Some(name.as_str()),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Walk an expression tree looking for function calls.
fn expr_has_calls(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::Call(..) => true,
        ast::Expr::Unary(_, inner) => expr_has_calls(inner),
        ast::Expr::Binary(left, _, right) => expr_has_calls(left) || expr_has_calls(right),
        ast::Expr::FieldAccess(base, _) => expr_has_calls(base),
        ast::Expr::Index(base, index) => expr_has_calls(base) || expr_has_calls(index),
        ast::Expr::Group(inner) => expr_has_calls(inner),
        ast::Expr::Cast(inner, _) => expr_has_calls(inner),
        ast::Expr::ArrayInit(elems, _) => elems.iter().any(expr_has_calls),
        ast::Expr::ArrayRepeat(value, count, _) => expr_has_calls(value) || expr_has_calls(count),
        ast::Expr::StructInit { fields, .. } => fields.iter().any(|(_, e)| expr_has_calls(e)),
        ast::Expr::Block(block_expr) => block_has_calls(&block_expr.block),
        ast::Expr::Match(match_expr) => {
            expr_has_calls(&match_expr.scrutinee)
                || match_expr.arms.iter().any(|arm| block_has_calls(&arm.body))
        }
        ast::Expr::If(if_expr) => {
            expr_has_calls(&if_expr.cond)
                || block_has_calls(&if_expr.then_block)
                || expr_has_calls(&if_expr.else_branch)
        }
        ast::Expr::ViewNew {
            base, len, stride, ..
        } => {
            expr_has_calls(base)
                || len.as_ref().is_some_and(|len| expr_has_calls(len))
                || stride.as_ref().is_some_and(|stride| expr_has_calls(stride))
        }
        ast::Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            expr_has_calls(base)
                || capacity
                    .as_ref()
                    .is_some_and(|capacity| expr_has_calls(capacity))
                || expr_has_calls(head)
                || expr_has_calls(len)
        }
        ast::Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            expr_has_calls(base)
                || bit_offset
                    .as_ref()
                    .is_some_and(|bit_offset| expr_has_calls(bit_offset))
                || len_bits
                    .as_ref()
                    .is_some_and(|len_bits| expr_has_calls(len_bits))
        }
        ast::Expr::IntLiteral(..)
        | ast::Expr::FloatLiteral(..)
        | ast::Expr::BoolLiteral(..)
        | ast::Expr::StringLiteral(..)
        | ast::Expr::NullLiteral(_)
        | ast::Expr::Ident(_)
        | ast::Expr::EnumVariant { .. }
        | ast::Expr::SizeOf(..) => false,
    }
}

fn stmt_has_calls(stmt: &ast::Stmt) -> bool {
    match stmt {
        ast::Stmt::VarDecl(decl) => expr_has_calls(&decl.init),
        ast::Stmt::Assign(assign) => expr_has_calls(&assign.value),
        ast::Stmt::CompoundAssign(ca) => {
            expr_has_calls(&ca.value) || expr_has_calls(&ca.target.to_expr())
        }
        ast::Stmt::Expr(expr) => expr_has_calls(expr),
        ast::Stmt::Return(ret) => ret.value.as_ref().is_some_and(expr_has_calls),
        ast::Stmt::Block(block) => block_has_calls(block),
        ast::Stmt::If(if_stmt) => {
            expr_has_calls(&if_stmt.cond)
                || block_has_calls(&if_stmt.then_block)
                || if_stmt
                    .else_branch
                    .as_ref()
                    .is_some_and(|alt| stmt_has_calls(alt))
        }
        ast::Stmt::Match(match_stmt) => {
            expr_has_calls(&match_stmt.scrutinee)
                || match_stmt.arms.iter().any(|arm| block_has_calls(&arm.body))
        }
        ast::Stmt::Loop(loop_stmt) => block_has_calls(&loop_stmt.body),
        ast::Stmt::Claim(c) => block_has_calls(&c.body),
        ast::Stmt::While(while_stmt) => {
            expr_has_calls(&while_stmt.cond) || block_has_calls(&while_stmt.body)
        }
        ast::Stmt::For(for_stmt) => {
            expr_has_calls(&for_stmt.start)
                || expr_has_calls(&for_stmt.end)
                || for_stmt.step.as_ref().is_some_and(expr_has_calls)
                || block_has_calls(&for_stmt.body)
        }
        ast::Stmt::Asm(asm_stmt) => {
            asm_stmt
                .outputs
                .iter()
                .any(|(_, expr)| expr_has_calls(expr))
                || asm_stmt.inputs.iter().any(|(_, expr)| expr_has_calls(expr))
        }
        ast::Stmt::Break(_) | ast::Stmt::Continue(_) => false,
        ast::Stmt::Assume(assume) => expr_has_calls(&assume.cond),
        ast::Stmt::Assert(assert) => expr_has_calls(&assert.cond),
    }
}

fn block_has_calls(block: &ast::Block) -> bool {
    block.stmts.iter().any(stmt_has_calls)
        || block.trailing.as_ref().is_some_and(|e| expr_has_calls(e))
}

// --- peripheral_type monomorphization (slice 2) ---

/// A comptime binding: the concrete compile-time value a monomorphized function
/// was specialized for -- one per comptime parameter, in declaration order.
/// Today the only kind is a peripheral instance (whose comptime value is its
/// base address); a `ConstInt` kind arrives with explicit `comptime` value
/// parameters (rung 1). The specialization key and name mangling are keyed on a
/// `Vec<Binding>`, so adding a kind is local to this enum, `mangle`, and the
/// substitution sites.
/// Backstop against runaway `comptime` recursion (a missing base case): the total
/// number of monomorphized specializations is capped. Reasonable unrolls are far
/// below this.
const COMPTIME_SPEC_LIMIT: usize = 4096;

/// `(bits, signed)` for an integer (or int-backed enum) type, else `None`.
fn int_bits_signed(ty: &Type) -> Option<(u32, bool)> {
    Some(match ty {
        Type::U8 => (8, false),
        Type::U16 => (16, false),
        Type::U32 => (32, false),
        Type::U64 => (64, false),
        Type::I8 => (8, true),
        Type::I16 => (16, true),
        Type::I32 => (32, true),
        Type::I64 => (64, true),
        Type::B1 => (1, false),
        Type::B8 => (8, false),
        Type::Enum(_, backing, _) => return int_bits_signed(backing),
        _ => return None,
    })
}

/// Whether `v` is representable in integer type `ty`, so a `comptime` value (eval'd
/// in i128) does not silently overflow its declared width at codegen.
fn comptime_value_fits(v: i128, ty: &Type) -> bool {
    match int_bits_signed(ty) {
        None => true,
        Some((bits, true)) => {
            let max = (1i128 << (bits - 1)) - 1;
            let min = -(1i128 << (bits - 1));
            v >= min && v <= max
        }
        Some((bits, false)) => v >= 0 && v < (1i128 << bits),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Binding {
    /// A concrete peripheral instance, e.g. `USART1`; `u.REG` in the body lowers
    /// through this instance's base address (see `subst_periph`).
    PeripheralInstance(String),
    /// A `comptime` integer value parameter bound to a concrete constant; the
    /// specialized body materializes it as a local literal (see the param
    /// alloca loop in `emit_function`).
    ConstInt(i128),
}

impl Binding {
    /// The name-mangling component: `usart_init$USART1` or `delay$1000`.
    fn mangle(&self) -> String {
        match self {
            Binding::PeripheralInstance(inst) => inst.clone(),
            // `-` is not a legal unquoted LLVM identifier char, so mangle a
            // negative value as `nN`.
            Binding::ConstInt(v) if *v < 0 => format!("n{}", v.unsigned_abs()),
            Binding::ConstInt(v) => v.to_string(),
        }
    }
}

/// Select the match arm whose pattern matches a comptime-known integer scrutinee,
/// for folding a `comptime match`. (Enum/variant scrutinees are not yet
/// comptime-folded; the checker (E411) rejects them.)
fn comptime_match_arm<'a>(
    scrut: i128,
    arms: &'a [ast::MatchArm],
    symbols: &SymbolTable,
) -> Option<&'a ast::MatchArm> {
    arms.iter().find(|arm| {
        arm.patterns.iter().any(|p| match p {
            ast::MatchPattern::Int(v, _) => *v == scrut,
            ast::MatchPattern::Range(lo, hi, _) => *lo <= scrut && scrut <= *hi,
            ast::MatchPattern::Wildcard(_) => true,
            ast::MatchPattern::Variant(enum_name, variant) => {
                symbols.enum_variant_discriminant(&enum_name.0, &variant.0) == Some(scrut)
            }
        })
    })
}

/// Whether a parameter names a `peripheral_type` (a handle parameter).
fn is_handle_param(param: &ast::Param, symbols: &SymbolTable) -> bool {
    matches!(&param.ty, ast::TypeExpr::Named((n, _)) if symbols.peripheral_types.contains_key(n))
}

/// Whether a parameter is comptime -- erased from the runtime ABI: a
/// `peripheral_type` handle, or an explicit `comptime` value parameter.
fn is_comptime_param(param: &ast::Param, symbols: &SymbolTable) -> bool {
    is_handle_param(param, symbols) || param.comptime
}

fn fn_has_comptime_params(fn_def: &ast::FnDef, symbols: &SymbolTable) -> bool {
    fn_def.params.iter().any(|p| is_comptime_param(p, symbols))
}

/// Positions of a function's comptime parameters (a `peripheral_type` handle or
/// a `comptime` value), from its resolved signature. These are dropped from the
/// runtime ABI and resolved per specialization.
fn comptime_param_positions(fname: &str, symbols: &SymbolTable) -> Vec<usize> {
    symbols.functions.get(fname).map_or_else(Vec::new, |s| {
        s.params
            .iter()
            .enumerate()
            .filter(|(i, (_, t))| {
                matches!(t, Type::PeripheralHandle(_))
                    || s.comptime.get(*i).copied().unwrap_or(false)
            })
            .map(|(i, _)| i)
            .collect()
    })
}

/// Mangled name of a monomorphized function: `fn$BIND0$BIND1...`.
fn mangle_spec(name: &str, bindings: &[Binding]) -> String {
    let mut s = name.to_string();
    for b in bindings {
        s.push('$');
        s.push_str(&b.mangle());
    }
    s
}

/// Map each handle parameter name to its concrete binding for a specialization.
fn build_handle_subst(
    fn_def: &ast::FnDef,
    bindings: &[Binding],
    symbols: &SymbolTable,
) -> HashMap<String, Binding> {
    let mut m = HashMap::new();
    let mut it = bindings.iter();
    for p in &fn_def.params {
        if is_comptime_param(p, symbols)
            && let Some(b) = it.next()
        {
            m.insert(p.name.0.clone(), b.clone());
        }
    }
    m
}

impl IrEmitter {
    /// Resolve a base identifier to the peripheral it denotes, applying the
    /// active handle substitution: `u` -> the instance the current specialized
    /// function was emitted for. A non-handle name is returned unchanged. Owned
    /// so the result does not extend a `&self` borrow into a `&mut self` body
    /// inside a let-chain.
    fn subst_periph(&self, name: &str) -> String {
        match self.handle_subst.get(name) {
            Some(Binding::PeripheralInstance(inst)) => inst.clone(),
            // A `comptime` int param is a value, not a peripheral; leave the
            // name unchanged (it is materialized as a local).
            Some(Binding::ConstInt(_)) | None => name.to_string(),
        }
    }

    /// Const values visible during the current specialization: the module consts
    /// plus the active `comptime`-param bindings, so `comptime if (i < N)` folds
    /// with `i` bound. With an empty subst (a non-specialized function) this is
    /// just the module consts.
    fn spec_consts(&self) -> HashMap<String, ConstVal> {
        let mut m = self.const_vals.clone();
        for (name, binding) in &self.handle_subst {
            if let Binding::ConstInt(v) = binding {
                m.insert(name.clone(), ConstVal::Int(*v));
            }
        }
        m
    }

    /// For `P.REG` naming an array register, return `(reg_base_addr, stride)`
    /// where `reg_base_addr = peripheral base + reg offset`. `P.REG[i]` then
    /// addresses `reg_base_addr + stride*i`. `None` for a non-array register.
    fn array_reg_addr(&self, periph: &str, reg: &str, symbols: &SymbolTable) -> Option<(u64, u64)> {
        let p = symbols.peripherals.get(&self.subst_periph(periph))?;
        let r = p.regs.get(reg)?;
        let (_len, stride) = r.array?;
        Some((p.base_addr + r.offset, stride))
    }

    /// Emit the pointer for `P.REG[i]`: `reg_base + stride*i`, as `inttoptr`.
    /// Emits the index expression and coerces it to the pointer width (U32)
    /// before the multiply -- a `u8`/`u16`/`u64` index would otherwise produce
    /// an operand-type mismatch against the hardcoded `i32` arithmetic. Returns
    /// the ptr SSA.
    fn emit_reg_index_ptr(
        &mut self,
        reg_base: u64,
        stride: u64,
        index: &Expr,
        symbols: &SymbolTable,
        fn_name: &str,
    ) -> String {
        let idx_raw = self.emit_expr(index, symbols, fn_name);
        let idx_ty = self.expr_type(index, symbols);
        let idx = self.coerce_int(idx_raw, &idx_ty, &Type::U32);
        let pty = self.ptr_type();
        let scaled = self.new_reg();
        self.line(&format!("{scaled} = mul {pty} {idx}, {stride}"));
        let addr = self.new_reg();
        self.line(&format!("{addr} = add {pty} {scaled}, {reg_base}"));
        let ptr = self.new_reg();
        self.line(&format!("{ptr} = inttoptr {pty} {addr} to ptr"));
        ptr
    }

    /// Read a register field at a runtime `ptr`: volatile load + mask + shift +
    /// narrow. Used for `P.REG[i].FIELD` on an array register. Array registers
    /// are not handoff/gate/bitband registers, so this is the plain field read
    /// without the constant-address special cases.
    fn emit_field_read_at_ptr(
        &mut self,
        ptr: &str,
        bit_spec: &crate::ast::BitSpec,
        ty: &Type,
        dbg: &str,
    ) -> String {
        let val = self.new_reg();
        self.line(&format!("{val} = load volatile i32, ptr {ptr}{dbg}"));
        let (mask, shift) = crate::arch::arm::bit_mask_shift(bit_spec);
        let masked = self.new_reg();
        self.line(&format!("{masked} = and i32 {val}, {mask}"));
        let result = self.new_reg();
        if shift > 0 {
            self.line(&format!("{result} = lshr i32 {masked}, {shift}"));
        } else {
            self.line(&format!("{result} = add i32 {masked}, 0"));
        }
        self.narrow_from_i32(&result, ty)
    }

    /// Read-modify-write a register field at a runtime `ptr`. The `P.REG[i].F = v`
    /// counterpart of `emit_field_read_at_ptr`.
    fn emit_field_rmw_at_ptr(
        &mut self,
        ptr: &str,
        bit_spec: &crate::ast::BitSpec,
        field_ty: &Type,
        val_reg: &str,
        val_ty: &Type,
        dbg: &str,
    ) -> String {
        let (mask, shift) = crate::arch::arm::bit_mask_shift(bit_spec);
        let inv_mask = !mask;
        let old = self.new_reg();
        self.line(&format!("{old} = load volatile i32, ptr {ptr}"));
        let cleared = self.new_reg();
        self.line(&format!("{cleared} = and i32 {old}, {inv_mask}"));
        let widened = self.widen_to_i32(val_reg, val_ty, field_ty);
        let shifted = self.new_reg();
        if shift > 0 {
            self.line(&format!("{shifted} = shl i32 {widened}, {shift}"));
        } else {
            self.line(&format!("{shifted} = add i32 {widened}, 0"));
        }
        let masked_val = self.new_reg();
        self.line(&format!("{masked_val} = and i32 {shifted}, {mask}"));
        let new_val = self.new_reg();
        self.line(&format!("{new_val} = or i32 {cleared}, {masked_val}"));
        self.line(&format!("store volatile i32 {new_val}, ptr {ptr}{dbg}"));
        new_val
    }

    /// `(reg_base, stride, idx_expr)` if `base` is `P.REG[i]` naming an array
    /// register (read side: `Expr::Index` of `Expr::FieldAccess`). The caller
    /// emits the index and the ptr.
    fn indexed_array_reg<'e>(
        &self,
        base: &'e Expr,
        symbols: &SymbolTable,
    ) -> Option<(u64, u64, &'e Expr)> {
        if let Expr::Index(arr, idx) = base
            && let Expr::FieldAccess(p, reg) = arr.as_ref()
            && let Expr::Ident((pname, _)) = p.as_ref()
            && let Some((reg_base, stride)) = self.array_reg_addr(pname, &reg.0, symbols)
        {
            return Some((reg_base, stride, idx));
        }
        None
    }

    /// Lower a call to a `peripheral_type` driver: resolve each handle argument
    /// to its concrete instance (the active substitution for a pass-through,
    /// else the global instance name), queue the specialization, and emit a call
    /// to the mangled name with the handle arguments dropped (slice 2).
    fn emit_handle_call(
        &mut self,
        callee: &str,
        args: &[Expr],
        comptime_positions: &[usize],
        symbols: &SymbolTable,
        fn_name: &str,
        dbg_sfx: &str,
    ) -> String {
        let param_tys: Vec<Type> = symbols.functions.get(callee).map_or_else(Vec::new, |s| {
            s.params.iter().map(|(_, t)| t.clone()).collect()
        });
        // One binding per comptime parameter: a peripheral instance (resolved from
        // the argument identifier, applying any active pass-through subst) or a
        // `comptime` int (const-evaluated from the argument). A for-loop (not `.map`)
        // so an eval failure / out-of-range value can be recorded as an error.
        let mut bindings: Vec<Binding> = Vec::with_capacity(comptime_positions.len());
        for &i in comptime_positions {
            if let Some(Type::PeripheralHandle(_)) = param_tys.get(i) {
                // The checker rejects a non-identifier handle argument (E308) and
                // codegen only runs on error-free programs, so the else is dead.
                let b = if let Expr::Ident((name, _)) = &args[i] {
                    self.handle_subst
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| Binding::PeripheralInstance(name.clone()))
                } else {
                    unreachable!(
                        "non-identifier peripheral handle argument reached codegen: {:?}",
                        &args[i]
                    )
                };
                bindings.push(b);
            } else {
                // A `comptime` int parameter: const-evaluate the argument over
                // `spec_consts`, so an expression over the enclosing fn's comptime
                // params (e.g. `f(i + 1)`) folds with `i` bound.
                let evaluated = {
                    let consts = self.spec_consts();
                    let env = IrConstEnv {
                        symbols,
                        consts: &consts,
                    };
                    consteval::eval_int(&args[i], &env)
                };
                let v = if let Some(v) = evaluated {
                    v
                } else {
                    // The checker accepts a param-dependent expr structurally; it
                    // can still fail to evaluate here (a param-dependent divisor of
                    // 0, or overflow during specialization).
                    self.comptime_errors.push((
                        "comptime argument does not evaluate to a constant (division by zero or overflow during specialization)".to_string(),
                        "E410".to_string(),
                        args[i].span(),
                    ));
                    0
                };
                // Fix A: the value must fit the parameter's declared type, else the
                // fold would use a wider (i128) value than runtime semantics allow.
                if let Some(pty) = param_tys.get(i)
                    && !comptime_value_fits(v, pty)
                {
                    self.comptime_errors.push((
                        format!(
                            "comptime argument value {v} does not fit parameter type `{pty:?}`"
                        ),
                        "E410".to_string(),
                        args[i].span(),
                    ));
                }
                bindings.push(Binding::ConstInt(v));
            }
        }
        let mangled = mangle_spec(callee, &bindings);
        let key = (callee.to_string(), bindings);
        if self.handle_spec_done.insert(key.clone()) {
            if self.handle_spec_done.len() > COMPTIME_SPEC_LIMIT {
                self.comptime_errors.push((
                    format!(
                        "comptime specialization limit ({COMPTIME_SPEC_LIMIT}) exceeded -- a `comptime` recursion is likely missing a base case (in `{callee}`)"
                    ),
                    "E413".to_string(),
                    args[comptime_positions[0]].span(),
                ));
            } else {
                self.handle_spec_queue.push(key);
            }
        }

        let mut arg_parts = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if comptime_positions.contains(&i) {
                continue; // comptime args carry no runtime value
            }
            let reg = self.emit_expr(arg, symbols, fn_name);
            let ty = self.expr_type(arg, symbols);
            // Match the monomorphized callee's `define`, which applies the same
            // AAPCS narrow-int extension (see abi_ext).
            if let Some(pty) = param_tys.get(i) {
                let reg = self.coerce_int(reg, &ty, pty);
                arg_parts.push(format!("{}{} {reg}", llvm_type(pty), abi_param_suffix(pty)));
            } else {
                arg_parts.push(format!("{} {reg}", llvm_type(&ty)));
            }
        }
        let arg_str = arg_parts.join(", ");
        let ret = symbols.functions.get(callee).and_then(|s| s.ret.as_ref());
        let ret_ty = ret.map_or_else(|| "void".to_string(), llvm_type);
        let ret_prefix = ret.map_or_else(String::new, abi_ret_prefix);
        if ret_ty == "void" {
            self.line(&format!("call void @{mangled}({arg_str}){dbg_sfx}"));
            String::new()
        } else {
            let reg = self.new_reg();
            self.line(&format!(
                "{reg} = call {ret_prefix}{ret_ty} @{mangled}({arg_str}){dbg_sfx}"
            ));
            reg
        }
    }

    #[must_use]
    pub fn new(
        arch: Arch,
        target_interrupts: std::collections::HashMap<String, u16>,
        has_bitband: bool,
        debug: bool,
        source_map: Option<SourceMap>,
    ) -> Self {
        IrEmitter {
            out: String::new(),
            counter: 0,
            str_counter: 0,
            strings: Vec::new(),
            indent: 0,
            locals: HashMap::new(),
            label_counter: 0,
            arch,
            target_interrupts,
            has_bitband,
            debug,
            source_map,
            debug_metadata: String::new(),
            debug_counter: 0,
            file_dbg_id: HashMap::new(),
            fn_scope_id: None,
            type_dbg_id: HashMap::new(),
            cu_file_id: None,
            cu_id: None,
            current_ctx: Context::Thread,
            current_label: None,
            current_fn_params: Vec::new(),
            alloca_counter: 0,
            verify_mode: false,
            current_fn_name: String::new(),
            agent_ptr_locals: std::collections::HashSet::new(),
            handle_subst: HashMap::new(),
            handle_spec_queue: Vec::new(),
            handle_spec_done: std::collections::HashSet::new(),
            const_vals: HashMap::new(),
            comptime_errors: Vec::new(),
            generated_wrap_spans: Vec::new(),
            ecc_scrub_blocks: Vec::new(),
            preempt: None,
            startup_init: Vec::new(),
            mpu_regions: Vec::new(),
            priority_bits: 4,
            claim_depth: 0,
            claimed_statics: Vec::new(),
            isr_priorities: Vec::new(),
            shpr_priorities: Vec::new(),
            cross_core_locks: std::collections::HashMap::new(),
            spinlock_base: 0,
            mpu_flavor: crate::arch::MpuFlavor::Pmsa7,
            region_addr_ranges: HashMap::new(),
            region_alignments: HashMap::new(),
            handoff_reach_bounds: HashMap::new(),
            handoff_regs: std::collections::HashSet::new(),
            gate_regs: std::collections::HashSet::new(),
            region_ranges: HashMap::new(),
            extent_cap_shadows: HashMap::new(),
            extent_asserts: HashMap::new(),
            desc_cap_shadows: HashMap::new(),
            desc_extent_asserts: HashMap::new(),
        }
    }

    #[must_use]
    pub fn new_with_verify(
        arch: Arch,
        target_interrupts: std::collections::HashMap<String, u16>,
        has_bitband: bool,
        debug: bool,
        source_map: Option<SourceMap>,
    ) -> Self {
        IrEmitter {
            out: String::new(),
            counter: 0,
            str_counter: 0,
            strings: Vec::new(),
            indent: 0,
            locals: HashMap::new(),
            label_counter: 0,
            arch,
            target_interrupts,
            has_bitband,
            debug,
            source_map,
            debug_metadata: String::new(),
            debug_counter: 0,
            file_dbg_id: HashMap::new(),
            fn_scope_id: None,
            type_dbg_id: HashMap::new(),
            cu_file_id: None,
            cu_id: None,
            current_ctx: Context::Thread,
            current_label: None,
            current_fn_params: Vec::new(),
            alloca_counter: 0,
            verify_mode: true,
            current_fn_name: String::new(),
            agent_ptr_locals: std::collections::HashSet::new(),
            handle_subst: HashMap::new(),
            handle_spec_queue: Vec::new(),
            handle_spec_done: std::collections::HashSet::new(),
            const_vals: HashMap::new(),
            comptime_errors: Vec::new(),
            generated_wrap_spans: Vec::new(),
            ecc_scrub_blocks: Vec::new(),
            preempt: None,
            startup_init: Vec::new(),
            mpu_regions: Vec::new(),
            priority_bits: 4,
            claim_depth: 0,
            claimed_statics: Vec::new(),
            isr_priorities: Vec::new(),
            shpr_priorities: Vec::new(),
            cross_core_locks: std::collections::HashMap::new(),
            spinlock_base: 0,
            mpu_flavor: crate::arch::MpuFlavor::Pmsa7,
            region_addr_ranges: HashMap::new(),
            region_alignments: HashMap::new(),
            handoff_reach_bounds: HashMap::new(),
            handoff_regs: std::collections::HashSet::new(),
            gate_regs: std::collections::HashSet::new(),
            region_ranges: HashMap::new(),
            extent_cap_shadows: HashMap::new(),
            extent_asserts: HashMap::new(),
            desc_cap_shadows: HashMap::new(),
            desc_extent_asserts: HashMap::new(),
        }
    }

    /// Install preemption analysis results so verify-mode IR only invalidates
    /// `@shared` reads that an ISR with strictly higher priority can actually
    /// write. Without this, every `@shared` read is unconditionally havoc'd.
    pub fn set_preempt(&mut self, preempt: PreemptInfo) {
        self.preempt = Some(preempt);
    }

    /// Install target-specific MMIO writes (`(address, or_mask)`) to apply at the
    /// start of `reset_handler`, before `.data`/`.bss` init. See
    /// `Target::startup_init`.
    pub fn set_handoff_regs(&mut self, regs: std::collections::HashSet<String>) {
        self.handoff_regs = regs;
    }

    /// Install the declared clock gates (`enabled_by` paths, `!`-prefix
    /// stripped: read-back is about write propagation, not polarity).
    pub fn set_enable_gates(&mut self, paths: &[String]) {
        for p in paths {
            let p = p.trim_start_matches('!');
            let parts: Vec<&str> = p.split('.').collect();
            if parts.len() == 3 {
                self.gate_regs
                    .insert((parts[0].to_string(), parts[1].to_string()));
            }
        }
    }

    pub fn set_startup_init(&mut self, writes: Vec<(u64, u64)>) {
        self.startup_init = writes;
    }

    /// RAM blocks `(base, size)` the generated `reset_handler` word-zeroes at
    /// boot to establish valid ECC (see `Target::ecc_scrub_blocks`). Not set
    /// in verify mode: boot scrub is below the verifier's abstraction (IKOS
    /// models statics from their initializers), and the scrub loop's
    /// SP-clamped bound would only feed V130 noise.
    pub fn set_ecc_scrub_blocks(&mut self, blocks: Vec<(u64, u64)>) {
        self.ecc_scrub_blocks = blocks;
    }

    /// Install the non-cacheable MPU regions (base, size). Programmed at the
    /// start of `reset_handler`. See `Target::mpu_regions`.
    pub fn set_mpu_regions(&mut self, regions: Vec<(u64, u64)>) {
        self.mpu_regions = regions;
    }

    /// NVIC priority field width (`Target::priority_bits`), used when
    /// programming `@isr` priorities into the IPR bytes in `reset_handler`.
    pub fn set_priority_bits(&mut self, bits: u8) {
        self.priority_bits = bits;
    }

    /// MPU programming model (see `Target::mpu_flavor`).
    pub fn set_mpu_flavor(&mut self, flavor: crate::arch::MpuFlavor) {
        self.mpu_flavor = flavor;
    }

    /// Cross-core claim wiring: lock indices per `@shared` static plus the
    /// spinlock bank base (see `region::cross_core_locks`).
    pub fn set_cross_core_locks(
        &mut self,
        locks: std::collections::HashMap<String, u32>,
        base: u64,
    ) {
        self.cross_core_locks = locks;
        self.spinlock_base = base;
    }

    /// Install the per-region alignment floors (region name -> bytes). A static
    /// placed `in R` is emitted with at least `floor` alignment, replacing a
    /// hand-written `@align(N)`. Built from the target (see `region_alignments`
    /// in `bml/src/main.rs`).
    pub fn set_region_alignments(&mut self, aligns: HashMap<String, u32>) {
        self.region_alignments = aligns;
    }

    /// Install the verify-mode region/handoff obligation maps (see the fields).
    /// Only meaningful in verify mode; the build path leaves them empty.
    pub fn set_handoff_obligations(
        &mut self,
        region_addr_ranges: HashMap<String, (u64, u64)>,
        handoff_reach_bounds: HashMap<String, (u64, u64)>,
        region_ranges: HashMap<String, (u64, u64)>,
    ) {
        self.region_addr_ranges = region_addr_ranges;
        self.handoff_reach_bounds = handoff_reach_bounds;
        self.region_ranges = region_ranges;
    }

    /// Build the descriptor-extent maps from `@extent` struct-field
    /// annotations (see the fields). One shadow per `(struct, addr field)`
    /// pair: element-agnostic, like the register shadows -- the most recent
    /// delivery through that field is what a length write is checked against.
    fn collect_desc_extents(&mut self, program: &Program) {
        for item in &program.items {
            let ast::Item::StructDef(sd) = item else {
                continue;
            };
            for field in &sd.fields {
                let Some(ext) = &field.extent else { continue };
                let shadow = format!("__bml_cap_{}_{}", sd.name.0, ext.addr_field.0);
                self.desc_cap_shadows.insert(
                    (sd.name.0.clone(), ext.addr_field.0.clone()),
                    shadow.clone(),
                );
                self.desc_extent_asserts.insert(
                    (sd.name.0.clone(), field.name.0.clone()),
                    (ext.scale, ext.mask, shadow),
                );
            }
        }
    }

    /// Install the verify-mode transfer-extent obligation maps (see the
    /// fields).
    pub fn set_extent_obligations(
        &mut self,
        cap_shadows: HashMap<(String, String), String>,
        asserts: HashMap<(String, String, String), (u32, Vec<String>)>,
    ) {
        self.extent_cap_shadows = cap_shadows;
        self.extent_asserts = asserts;
    }

    /// Emit `assume(lo <= value < hi)` as the branch-to-unreachable pattern IKOS
    /// reads as a fact. `value_reg` is an i32 address; comparisons are unsigned.
    fn emit_range_assume(&mut self, value_reg: &str, lo: u64, hi: u64) {
        let lo_ok = self.new_reg();
        let hi_ok = self.new_reg();
        let both = self.new_reg();
        self.line(&format!("{lo_ok} = icmp uge i32 {value_reg}, {lo}"));
        self.line(&format!("{hi_ok} = icmp ult i32 {value_reg}, {hi}"));
        self.line(&format!("{both} = and i1 {lo_ok}, {hi_ok}"));
        let ok_lbl = self.new_label("assume_ok");
        let unreach_lbl = self.new_label("assume_unreach");
        self.line(&format!(
            "br i1 {both}, label %{ok_lbl}, label %{unreach_lbl}"
        ));
        self.line("");
        self.indent -= 1;
        self.line(&format!("{unreach_lbl}:"));
        self.indent += 1;
        self.line("unreachable");
        self.line("");
        self.indent -= 1;
        self.line(&format!("{ok_lbl}:"));
        self.indent += 1;
    }

    /// Emit `assert(lo <= value < hi)` via `__ikos_assert`. IKOS reports it as
    /// proven (silent), unproven (warning), or violated (error). `value` is
    /// passed as a witness operand so an unproven/violated report carries its
    /// inferred range.
    fn emit_range_assert(&mut self, value_reg: &str, lo: u64, hi: u64, dbg: &str) {
        let lo_ok = self.new_reg();
        let hi_ok = self.new_reg();
        let both = self.new_reg();
        let zext = self.new_reg();
        self.line(&format!("{lo_ok} = icmp uge i32 {value_reg}, {lo}"));
        self.line(&format!("{hi_ok} = icmp ult i32 {value_reg}, {hi}"));
        self.line(&format!("{both} = and i1 {lo_ok}, {hi_ok}"));
        self.line(&format!("{zext} = zext i1 {both} to i32"));
        self.line(&format!(
            "call void @__ikos_assert(i32 {zext}, i32 {value_reg}){dbg}"
        ));
    }

    /// Errors recorded during codegen: a `comptime` arg/condition/scrutinee that
    /// failed to evaluate, or overflowed its declared type, at a specialization.
    /// The driver drains these after `emit` and discards the output if any exist.
    #[must_use]
    pub fn comptime_errors(&self) -> &[(String, String, crate::source::Span)] {
        &self.comptime_errors
    }

    #[must_use]
    pub fn emit(&mut self, program: &Program, symbols: &SymbolTable) -> String {
        if self.verify_mode {
            self.collect_desc_extents(program);
        }
        self.emit_module_header();
        if self.debug {
            self.emit_debug_compile_unit(program);
        }
        self.emit_global_declarations(program, symbols);
        if !self.verify_mode {
            // In verify mode, IKOS only needs user functions. Startup/runtime
            // code adds noise and can introduce irrelevant inline assembly.
            self.emit_vector_table(program, symbols);
        }
        self.emit_extern_function_declarations(program, symbols);
        self.emit_function_bodies(program, symbols);
        self.emit_string_literals();
        if self.debug {
            self.emit_debug_module_flags();
            let metadata = std::mem::take(&mut self.debug_metadata);
            self.out.push_str(&metadata);
        }
        std::mem::take(&mut self.out)
    }

    // ─── module header ───────────────────────────────────────────────

    fn emit_module_header(&mut self) {
        self.line("; Module generated by bml compiler");
        self.line(&format!(
            "target triple = \"{}\"",
            self.arch.llvm_target_triple()
        ));
        self.line(&format!(
            "target datalayout = \"{}\"",
            self.arch.datalayout()
        ));
        if self.debug {
            self.line("");
            self.line("declare void @llvm.dbg.declare(metadata, metadata, metadata) #2");
            self.line("");
            self.line("attributes #2 = { nounwind readnone speculatable }");
        }
        // Byte-swap intrinsics for `@be` struct fields. Declared unconditionally;
        // unused declarations are harmless and dropped by the backend.
        self.line("");
        self.line("declare i16 @llvm.bswap.i16(i16)");
        self.line("declare i32 @llvm.bswap.i32(i32)");
        self.line("declare i64 @llvm.bswap.i64(i64)");
        self.line("");
    }

    // ─── globals ─────────────────────────────────────────────────────

    fn emit_global_declarations(&mut self, program: &Program, symbols: &SymbolTable) {
        // Resolve every `const`'s integer value once; the fixpoint scans all
        // items, so recomputing it per declaration would be quadratic.
        let consts = const_values(&program.items, symbols);
        // Cache the resolved const values so `comptime` value arguments can be
        // const-evaluated during call lowering (see `IrConstEnv`).
        self.const_vals.clone_from(&consts);
        // Map each `const` to its resolved type and initializer so an
        // initializer that names another `const` (e.g. `var s = LUT;` or
        // `const Y: f32 = X;`) can be inlined to that const's value.
        let const_defs: HashMap<String, (Type, &Expr)> = program
            .items
            .iter()
            .filter_map(|item| match item {
                ast::Item::ConstDef(c) => {
                    let ty =
                        crate::types::resolve_type_expr(&c.ty, &symbols.structs, &symbols.enums);
                    Some((c.name.0.clone(), (ty, &c.value)))
                }
                _ => None,
            })
            .collect();
        for item in &program.items {
            match item {
                ast::Item::StaticDef(s) => {
                    let resolved_ty =
                        crate::types::resolve_type_expr(&s.ty, &symbols.structs, &symbols.enums);
                    let llvm_ty = llvm_type(&resolved_ty);
                    let init_val = if let Some(init) = &s.init {
                        const_init(&resolved_ty, init, symbols, &consts, &const_defs)
                    } else {
                        "zeroinitializer".to_string()
                    };
                    // `in <region>` places the static in `.region.<name>`; the
                    // linker script maps that section to the region's mem block.
                    // An explicit `@section(...)` otherwise wins. The two are
                    // mutually exclusive in practice (placement vs raw section).
                    let section_attr = if let Some((name, _)) = &s.region {
                        format!(", section \".region.{name}\"")
                    } else {
                        s.storage
                            .iter()
                            .find_map(|a| {
                                if let ast::StorageAnnotation::Section(name) = a {
                                    Some(format!(", section \"{name}\""))
                                } else {
                                    None
                                }
                            })
                            .unwrap_or_default()
                    };
                    // `@align(N)` overrides the default 4-byte alignment; a
                    // static placed `in R` is floored at the region's derived
                    // alignment (cache-line physics), so the source need not
                    // hand-write the `@align`. An explicit `@align` can still
                    // raise it above the floor.
                    let explicit = s.storage.iter().find_map(|a| match a {
                        ast::StorageAnnotation::Align(n) => Some(*n),
                        _ => None,
                    });
                    let region_floor = s
                        .region
                        .as_ref()
                        .and_then(|(r, _)| self.region_alignments.get(r))
                        .copied()
                        .unwrap_or(0);
                    let align = explicit.unwrap_or(4).max(region_floor);
                    self.line(&format!(
                        "@{} = global {} {}{section_attr}, align {align}",
                        s.name.0, llvm_ty, init_val
                    ));
                }
                ast::Item::ConstDef(c) => {
                    let resolved_ty =
                        crate::types::resolve_type_expr(&c.ty, &symbols.structs, &symbols.enums);
                    let llvm_ty = llvm_type(&resolved_ty);
                    let val = const_init(&resolved_ty, &c.value, symbols, &consts, &const_defs);
                    self.line(&format!(
                        "@{} = constant {} {}, align 4",
                        c.name.0, llvm_ty, val
                    ));
                }
                _ => {}
            }
        }
        if !self.out.ends_with("\n\n") && self.out.ends_with('\n') {
            self.line("");
        }
    }

    // ─── function declarations ───────────────────────────────────────

    fn emit_extern_function_declarations(&mut self, program: &Program, symbols: &SymbolTable) {
        let mut any = false;
        for item in &program.items {
            if let ast::Item::ExternFnDef(f) = item {
                // AAPCS narrow-int extension: prefix sub-word params/return
                // with zeroext/signext (see abi_ext).
                let (ret_prefix, ret_ty) = match &f.ret {
                    Some(ty) => {
                        let r =
                            crate::types::resolve_type_expr(ty, &symbols.structs, &symbols.enums);
                        (abi_ret_prefix(&r), llvm_type(&r))
                    }
                    None => (String::new(), "void".to_string()),
                };
                let param_strs: Vec<String> = f
                    .params
                    .iter()
                    .map(|p| {
                        let r = crate::types::resolve_type_expr(
                            &p.ty,
                            &symbols.structs,
                            &symbols.enums,
                        );
                        format!("{}{}", llvm_type(&r), abi_param_suffix(&r))
                    })
                    .collect();
                self.line(&format!(
                    "declare {ret_prefix}{ret_ty} @{}({})",
                    f.name.0,
                    param_strs.join(", ")
                ));
                any = true;
            }
        }
        if self.verify_mode {
            // IKOS recognizes these by name and imports them as analysis
            // intrinsics. Keep them verify-only so normal builds stay clean.
            // __ikos_assert is variadic: arg0 is the checked condition; any
            // trailing i32 args are witness operands the fork reports as
            // inferred intervals (see emit_range_assert and the extent sites).
            self.line("declare void @__ikos_assert(i32, ...)");
            self.line("declare void @__ikos_forget_mem(ptr, i32)");
            // Capacity shadows for the extent obligation, initialized to
            // u32::MAX (= unconstrained until a handoff delivers a buffer).
            let mut shadows: Vec<&String> = self
                .extent_cap_shadows
                .values()
                .chain(self.desc_cap_shadows.values())
                .collect();
            shadows.sort();
            shadows.dedup();
            let lines: Vec<String> = shadows
                .iter()
                .map(|sh| format!("@{sh} = internal global i32 -1, align 4"))
                .collect();
            for l in lines {
                self.line(&l);
            }
            any = true;
        }
        if any {
            self.line("");
        }
    }

    // ─── string literals ────────────────────────────────────────────

    fn emit_string_literals(&mut self) {
        let strings = self.strings.clone();
        for s in &strings {
            self.line(s);
        }
        if !strings.is_empty() {
            self.line("");
        }
        self.strings.clear();
    }

    // ─── function bodies ─────────────────────────────────────────────

    /// Pre-emit alloca instructions for every local variable and for-loop
    /// variable in the function entry block so that LLVM's codegen never
    /// produces a dynamic alloca (which would grow the stack at runtime).
    fn emit_entry_allocas(&mut self, fn_def: &ast::FnDef, symbols: &SymbolTable) {
        self.collect_and_emit_allocas_block(&fn_def.body, symbols);
    }

    fn collect_and_emit_allocas_block(&mut self, block: &ast::Block, symbols: &SymbolTable) {
        for stmt in &block.stmts {
            self.collect_and_emit_allocas_stmt(stmt, symbols);
        }
        if let Some(ref trailing) = block.trailing {
            self.collect_and_emit_allocas_expr(trailing, symbols);
        }
    }

    fn collect_and_emit_allocas_stmt(&mut self, stmt: &Stmt, symbols: &SymbolTable) {
        match stmt {
            Stmt::VarDecl(vd) => {
                let bml_type = if let Some(ty_ann) = &vd.ty_ann {
                    let annotated =
                        crate::types::resolve_type_expr(ty_ann, &symbols.structs, &symbols.enums);
                    // A `ring T` annotation carries no capacity hint (capacity
                    // is not in the type syntax), but the array-backed
                    // initializer does. Recover it from the init so the
                    // power-of-two mask optimization still fires on annotated
                    // ring locals. Sound because the hint is a value-level fact,
                    // not type identity.
                    if let Type::RingView(elem, mutable, None) = &annotated
                        && let Type::RingView(_, _, hint @ Some(_)) =
                            self.expr_type(&vd.init, symbols)
                    {
                        Type::RingView(elem.clone(), *mutable, hint)
                    } else {
                        annotated
                    }
                } else {
                    self.expr_type(&vd.init, symbols)
                };
                let llvm_ty = llvm_type(&bml_type);
                let alloca = self.alloca(&llvm_ty, &vd.name.0);
                self.locals.insert(
                    vd.name.0.clone(),
                    LocalInfo {
                        alloca,
                        llvm_ty,
                        bml_type,
                    },
                );
                self.collect_and_emit_allocas_expr(&vd.init, symbols);
            }
            Stmt::For(for_stmt) => {
                self.collect_and_emit_allocas_expr(&for_stmt.start, symbols);
                self.collect_and_emit_allocas_expr(&for_stmt.end, symbols);
                if let Some(step) = &for_stmt.step {
                    self.collect_and_emit_allocas_expr(step, symbols);
                }
                let bml_type =
                    crate::types::resolve_type_expr(&for_stmt.ty, &symbols.structs, &symbols.enums);
                let llvm_ty = llvm_type(&bml_type);
                let alloca = self.alloca(&llvm_ty, &for_stmt.var.0);
                self.locals.insert(
                    for_stmt.var.0.clone(),
                    LocalInfo {
                        alloca,
                        llvm_ty,
                        bml_type,
                    },
                );
                self.collect_and_emit_allocas_block(&for_stmt.body, symbols);
            }
            Stmt::While(w) => {
                self.collect_and_emit_allocas_expr(&w.cond, symbols);
                self.collect_and_emit_allocas_block(&w.body, symbols);
            }
            Stmt::Loop(l) => {
                self.collect_and_emit_allocas_block(&l.body, symbols);
            }
            Stmt::Claim(c) => {
                // Locals inside the claim are typed against the patched view
                // of the world (the claimed static unwrapped), matching the
                // body's emission below.
                let patched = symbols.with_claimed(&c.name.0);
                self.collect_and_emit_allocas_block(&c.body, &patched);
            }
            Stmt::If(i) => {
                self.collect_and_emit_allocas_expr(&i.cond, symbols);
                self.collect_and_emit_allocas_block(&i.then_block, symbols);
                if let Some(else_branch) = &i.else_branch {
                    self.collect_and_emit_allocas_stmt(else_branch, symbols);
                }
            }
            Stmt::Match(m) => {
                self.collect_and_emit_allocas_expr(&m.scrutinee, symbols);
                for arm in &m.arms {
                    self.collect_and_emit_allocas_block(&arm.body, symbols);
                }
            }
            Stmt::Block(inner) => {
                self.collect_and_emit_allocas_block(inner, symbols);
            }
            Stmt::Return(ret) => {
                if let Some(ref val) = ret.value {
                    self.collect_and_emit_allocas_expr(val, symbols);
                }
            }
            Stmt::Assign(assign) => {
                self.collect_and_emit_allocas_expr(&assign.target.to_expr(), symbols);
                self.collect_and_emit_allocas_expr(&assign.value, symbols);
            }
            Stmt::CompoundAssign(ca) => {
                self.collect_and_emit_allocas_expr(&ca.target.to_expr(), symbols);
                self.collect_and_emit_allocas_expr(&ca.value, symbols);
            }
            Stmt::Expr(expr) => {
                self.collect_and_emit_allocas_expr(expr, symbols);
            }
            Stmt::Assume(assume) => {
                self.collect_and_emit_allocas_expr(&assume.cond, symbols);
            }
            Stmt::Assert(assert) => {
                self.collect_and_emit_allocas_expr(&assert.cond, symbols);
            }
            Stmt::Asm(asm_stmt) => {
                for (_, expr) in &asm_stmt.outputs {
                    self.collect_and_emit_allocas_expr(expr, symbols);
                }
                for (_, expr) in &asm_stmt.inputs {
                    self.collect_and_emit_allocas_expr(expr, symbols);
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }

    fn collect_and_emit_allocas_expr(&mut self, expr: &Expr, symbols: &SymbolTable) {
        match expr {
            Expr::Block(block_expr) => {
                self.collect_and_emit_allocas_block(&block_expr.block, symbols);
            }
            Expr::If(if_expr) => {
                self.collect_and_emit_allocas_expr(&if_expr.cond, symbols);
                self.collect_and_emit_allocas_block(&if_expr.then_block, symbols);
                self.collect_and_emit_allocas_expr(&if_expr.else_branch, symbols);
            }
            Expr::Match(match_expr) => {
                self.collect_and_emit_allocas_expr(&match_expr.scrutinee, symbols);
                for arm in &match_expr.arms {
                    self.collect_and_emit_allocas_block(&arm.body, symbols);
                }
            }
            Expr::Unary(_, inner) => self.collect_and_emit_allocas_expr(inner, symbols),
            Expr::Binary(left, _, right) => {
                self.collect_and_emit_allocas_expr(left, symbols);
                self.collect_and_emit_allocas_expr(right, symbols);
            }
            Expr::Call(func_expr, args) => {
                self.collect_and_emit_allocas_expr(func_expr, symbols);
                for arg in args {
                    self.collect_and_emit_allocas_expr(arg, symbols);
                }
            }
            Expr::FieldAccess(base, _) => self.collect_and_emit_allocas_expr(base, symbols),
            Expr::Index(base, index) => {
                self.collect_and_emit_allocas_expr(base, symbols);
                self.collect_and_emit_allocas_expr(index, symbols);
            }
            Expr::Group(inner) => self.collect_and_emit_allocas_expr(inner, symbols),
            Expr::Cast(inner, _) => self.collect_and_emit_allocas_expr(inner, symbols),
            Expr::ArrayInit(elems, _) => {
                for elem in elems {
                    self.collect_and_emit_allocas_expr(elem, symbols);
                }
            }
            Expr::StructInit { fields, .. } => {
                for (_, expr) in fields {
                    self.collect_and_emit_allocas_expr(expr, symbols);
                }
            }
            Expr::ViewNew {
                base, len, stride, ..
            } => {
                self.collect_and_emit_allocas_expr(base, symbols);
                if let Some(len) = len {
                    self.collect_and_emit_allocas_expr(len, symbols);
                }
                if let Some(stride) = stride {
                    self.collect_and_emit_allocas_expr(stride, symbols);
                }
            }
            Expr::RingNew {
                base,
                capacity,
                head,
                len,
                ..
            } => {
                self.collect_and_emit_allocas_expr(base, symbols);
                if let Some(capacity) = capacity {
                    self.collect_and_emit_allocas_expr(capacity, symbols);
                }
                self.collect_and_emit_allocas_expr(head, symbols);
                self.collect_and_emit_allocas_expr(len, symbols);
            }
            Expr::BitNew {
                base,
                bit_offset,
                len_bits,
                ..
            } => {
                self.collect_and_emit_allocas_expr(base, symbols);
                if let Some(bit_offset) = bit_offset {
                    self.collect_and_emit_allocas_expr(bit_offset, symbols);
                }
                if let Some(len_bits) = len_bits {
                    self.collect_and_emit_allocas_expr(len_bits, symbols);
                }
            }
            _ => {}
        }
    }

    fn emit_function_bodies(&mut self, program: &Program, symbols: &SymbolTable) {
        // Ordinary functions. A function with `peripheral_type` parameters is a
        // driver template -- not emitted directly; lowering a call to it queues
        // a per-instance specialization (slice 2).
        for item in &program.items {
            if let ast::Item::FnDef(fn_def) = item
                && !fn_has_comptime_params(fn_def, symbols)
            {
                self.emit_function(fn_def, symbols, None);
            }
        }
        // Drain the monomorphization worklist; a specialization's own body may
        // queue further specializations (transitive: `a(u){ b(u) }`).
        while let Some((fname, bindings)) = self.handle_spec_queue.pop() {
            if let Some(fn_def) = program.items.iter().find_map(|it| match it {
                ast::Item::FnDef(f) if f.name.0 == fname => Some(f),
                _ => None,
            }) {
                let mangled = mangle_spec(&fname, &bindings);
                self.handle_subst = build_handle_subst(fn_def, &bindings, symbols);
                self.emit_function(fn_def, symbols, Some(&mangled));
                self.handle_subst.clear();
            }
        }
    }

    /// Emit a function. `name_override` is the mangled name for a monomorphized
    /// driver specialization; `None` emits the function under its own name.
    /// `peripheral_type` (handle) parameters are dropped from the signature --
    /// they carry no runtime value (the body uses `self.handle_subst`).
    fn emit_function(
        &mut self,
        fn_def: &ast::FnDef,
        symbols: &SymbolTable,
        name_override: Option<&str>,
    ) {
        self.counter = 0;
        self.alloca_counter = 0;
        // current_fn_name stays the generic name: the verify preempt map is
        // keyed on it. The mangled name is used only for the emitted symbol.
        self.current_fn_name.clone_from(&fn_def.name.0);
        let emit_name = name_override.unwrap_or(&fn_def.name.0);
        self.agent_ptr_locals = crate::region::agent_ptr_locals(&fn_def.body, symbols);
        let fn_sym = symbols.functions.get(&fn_def.name.0);
        let is_isr = fn_sym.is_some_and(|s| s.context.is_isr());
        let is_naked = fn_sym.is_some_and(|s| s.naked);
        let tailchain = fn_sym.is_some_and(|s| s.tailchain);
        let has_calls = tailchain && block_has_calls(&fn_def.body);
        self.current_ctx = fn_sym.map_or(Context::Thread, |s| s.context);

        let ret_ty = fn_ret_llvm_type(fn_def, symbols);
        // AAPCS narrow-int extension, applied to every signature (see abi_ext).
        let ret_prefix = match &fn_def.ret {
            Some(ty) => abi_ret_prefix(&crate::types::resolve_type_expr(
                ty,
                &symbols.structs,
                &symbols.enums,
            )),
            None => String::new(),
        };
        let param_strs: Vec<String> = fn_def
            .params
            .iter()
            .filter(|p| !is_comptime_param(p, symbols))
            .map(|p| {
                let pty = crate::types::resolve_type_expr(&p.ty, &symbols.structs, &symbols.enums);
                format!(
                    "{}{} %{}",
                    llvm_type(&pty),
                    abi_param_suffix(&pty),
                    p.name.0
                )
            })
            .collect();

        let fn_span = fn_def.name.1;
        let dbg_fn_suffix = if self.debug {
            let cu = self.cu_id.unwrap_or(0);
            // The function's OWN file, not the compile unit's: DILocations
            // inherit their file from the subprogram scope, so pinning every
            // fn to the CU file mis-attributes multi-module findings (a
            // timer.bml overflow reported in eth_dma.bml at the same line).
            let file = self.dbg_file(fn_span.file);
            let id = self.new_dbg_id();
            let line = if let Some(ref sm) = self.source_map {
                sm.span_location(fn_span).start.line
            } else {
                0usize
            };
            let ret_ty_id = if let Some(ref ret) = fn_def.ret {
                let bml_ret =
                    crate::types::resolve_type_expr(ret, &symbols.structs, &symbols.enums);
                self.dbg_type(&bml_ret)
            } else {
                0 // null
            };
            let param_type_ids: Vec<String> = fn_def
                .params
                .iter()
                .filter(|p| !is_comptime_param(p, symbols))
                .map(|p| {
                    let bml_ty =
                        crate::types::resolve_type_expr(&p.ty, &symbols.structs, &symbols.enums);
                    format!("!{}", self.dbg_type(&bml_ty))
                })
                .collect();
            let st_id = self.new_dbg_id();
            let ret_str = if ret_ty_id == 0 {
                "null".to_string()
            } else {
                format!("!{ret_ty_id}")
            };
            let all_types = std::iter::once(ret_str)
                .chain(param_type_ids)
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                self.debug_metadata,
                "!{st_id} = !DISubroutineType(types: !{{{all_types}}})"
            )
            .unwrap();
            writeln!(
                self.debug_metadata,
                "!{id} = distinct !DISubprogram(name: \"{emit_name}\", scope: !{cu}, file: !{file}, line: {line}, type: !{st_id}, spFlags: DISPFlagDefinition, unit: !{cu})"
            )
            .unwrap();
            self.fn_scope_id = Some(id);
            format!("!dbg !{id} ")
        } else {
            String::new()
        };

        let attr_num = u32::from(is_isr && !is_naked && !tailchain);
        let section_attr = fn_def
            .section
            .as_ref()
            .map(|s| format!(" section \"{s}\""))
            .unwrap_or_default();
        // `internal` linkage lets globaldce (at -O2/-Os/-Oz) strip a function once
        // it is inlined away or otherwise uncalled. It is SAFE whenever no
        // reference can NAME the function from OUTSIDE this module's analyzable
        // graph: globaldce's own reachability keeps EVERY in-module reference alive
        // -- a direct call, a `&fn` / fn-pointer table, a value escaped to extern
        // C, the KEEP'd @vector_table, AND a second core's `entry` whose address
        // the launcher takes for the SIO-FIFO handshake (see region.rs). So a user
        // `Default_Handler`, a WIRED @isr, and a launched core entry are all kept
        // by globaldce regardless of linkage. The functions that nonetheless force
        // external linkage:
        //   - @isr / @naked / @section (attr_external): a wired @isr is kept by the
        //     vector table, but @isr is excluded conservatively (an UNWIRED handler
        //     has no in-module reference and would be silently stripped); @naked /
        //     @section need an external trampoline / placement surface. These
        //     propagate from a generic to its specialization, gating both.
        //   - `reset_handler`: named by `ENTRY(reset_handler)` in the linker script
        //     -- the one by-name reference globaldce cannot see. A reset written as
        //     @isr("Reset") is caught by is_isr; a plain-named `reset_handler` (the
        //     bml convention) is is_isr=false and needs this name.
        //   - an `export`ed fn: the declared public surface a separately-linked C
        //     TU may reference by name.
        //   - a core ENTRY POINT (`is_core_entry`): core0's `main` plus any
        //     secondary-core `entry` from the target `[agent] entry=`. globaldce
        //     would keep both anyway (reset's in-module `call @main`; the launcher's
        //     in-module `&entry`), but a core entry is a real entry point we keep
        //     external on purpose -- a stable, un-inlined symbol for debuggers/maps.
        //     `main` is NOT folded into `entry_fns` itself: that set carries
        //     launched-secondary semantics (NVIC IPR grounding at ir.rs ~1548, E408)
        //     that are wrong for `main`, which runs AFTER the reset handler.
        // A monomorphized specialization (`name_override`) needs only the attribute
        // gate: E309 forbids taking the address of a handle-param fn, and its
        // mangled `fn$INST` symbol is never an export/entry/handler/asm name.
        // KNOWN LIMITATION: a function referenced ONLY by name from an inline-asm
        // string (`asm { bl helper }`) is invisible to globaldce; if internalized
        // it is stripped and the asm relocation fails to LINK -- a loud error, not
        // a silent miscompile. Mark such a function `export` to keep it external.
        let attr_external = is_isr || is_naked || fn_def.section.is_some();
        let name = fn_def.name.0.as_str();
        // Core entry points (core0's `main` + secondary-core `entry=` from the
        // target). Kept external; see the `entry_fns` note above for why `main`
        // is a separate clause rather than a member of the set.
        let is_core_entry = name == "main" || symbols.entry_fns.contains(name);
        let internal = if name_override.is_some() {
            !attr_external
        } else {
            !attr_external && !fn_def.exported && !is_core_entry && name != "reset_handler"
        };
        let linkage = if internal { "internal " } else { "" };
        self.line(&format!(
            "define {linkage}{ret_prefix}{ret_ty} @{}({}) #{}{section_attr} {}{{",
            emit_name,
            param_strs.join(", "),
            attr_num,
            dbg_fn_suffix
        ));

        self.indent += 1;
        self.line("entry:");

        // Tailchain with calls: save LR before body
        if tailchain {
            crate::arch::arm::emit_tailchain_prologue(self, has_calls);
        }

        // Alloca for parameters. Handle parameters are dropped (no runtime
        // value); `u.REG` accesses resolve through `self.handle_subst` instead.
        self.locals.clear();
        for param in fn_def
            .params
            .iter()
            .filter(|p| !is_handle_param(p, symbols))
        {
            let bml_type =
                crate::types::resolve_type_expr(&param.ty, &symbols.structs, &symbols.enums);
            let pty = llvm_type(&bml_type);
            let reg = self.alloca(&pty, &param.name.0);
            let dbg_sfx = self.dbg_loc(param.name.1);
            // A `comptime` value parameter has no incoming SSA argument: it is
            // materialized from its bound constant, so it reads as an ordinary
            // local everywhere downstream. (Handle params are filtered out above.)
            let stored = match self.handle_subst.get(&param.name.0) {
                Some(Binding::ConstInt(v)) => v.to_string(),
                _ => format!("%{}", param.name.0),
            };
            self.line(&format!("store {pty} {stored}, ptr {reg}{dbg_sfx}"));
            self.dbg_declare(&reg, &param.name.0, &bml_type, param.name.1);
            self.locals.insert(
                param.name.0.clone(),
                LocalInfo {
                    alloca: reg,
                    llvm_ty: pty,
                    bml_type,
                },
            );
        }

        // Pre-emit allocas for all local variables in the entry block
        self.emit_entry_allocas(fn_def, symbols);

        // Declared core entries ground the banked NVIC IPRs and SCB SHPRs in
        // their prologue -- this core never ran the reset handler (see
        // arm::emit_ipr_stores). Both are programmed unconditionally on the
        // emitter (even under a user reset handler), so this still fires.
        if !self.verify_mode
            && symbols.entry_fns.contains(&fn_def.name.0)
            && !(self.isr_priorities.is_empty() && self.shpr_priorities.is_empty())
        {
            let prios = self.isr_priorities.clone();
            crate::arch::arm::emit_ipr_stores(self, &prios);
            let shpr = self.shpr_priorities.clone();
            crate::arch::arm::emit_shpr_stores(self, &shpr);
        }

        // Emit body. Handle parameters are dropped (no runtime value), so they
        // are excluded here too -- otherwise the implicit inline-asm param->reg
        // mapping would shift every following parameter by one slot.
        self.current_fn_params = fn_def
            .params
            .iter()
            .filter(|p| !is_comptime_param(p, symbols))
            .map(|p| {
                let bml_type =
                    crate::types::resolve_type_expr(&p.ty, &symbols.structs, &symbols.enums);
                (p.name.0.clone(), llvm_type(&bml_type))
            })
            .collect();
        let (_, body_term) = self.emit_block(&fn_def.body, symbols, &fn_def.name.0, None, None);
        self.current_fn_params.clear();

        // Default return or tailchain return sequence (only if body didn't already terminate)
        if !body_term {
            if tailchain {
                crate::arch::arm::emit_tailchain_epilogue(self, has_calls);
            } else if is_naked {
                self.line("unreachable");
            } else if ret_ty == "void" {
                self.line("ret void");
            } else {
                self.line(&format!("ret {ret_ty} 0"));
            }
        }

        self.indent -= 1;
        self.line("}");
        self.line("");
    }

    fn emit_block(
        &mut self,
        block: &ast::Block,
        symbols: &SymbolTable,
        fn_name: &str,
        break_label: Option<&str>,
        continue_label: Option<&str>,
    ) -> (Option<String>, bool) {
        let mut last_reg: Option<String> = None;
        let mut terminated = false;

        for stmt in &block.stmts {
            let (lr, term) = self.emit_stmt(stmt, symbols, fn_name, break_label, continue_label);
            last_reg = lr;
            if term {
                terminated = true;
                break;
            }
        }

        (last_reg, terminated)
    }

    /// Emit switch dispatch for a match. Returns arm labels + end label
    /// on success, or `None` if the scrutinee is not an enum (fallback emitted).
    fn emit_match_dispatch(
        &mut self,
        scrutinee: &Expr,
        arms: &[ast::MatchArm],
        symbols: &SymbolTable,
        fn_name: &str,
        is_expr: bool,
    ) -> Option<MatchDispatch> {
        let scrutinee_reg = self.emit_expr(scrutinee, symbols, fn_name);
        let end_lbl = self.new_label("match_end");

        let scrutinee_ty = self.expr_type(scrutinee, symbols);

        // Integer scrutinee: an ordered if-chain (handles ranges and overlaps,
        // first match wins). Enum scrutinee: an LLVM `switch` below.
        if crate::types::is_int(&scrutinee_ty) {
            return Some(self.emit_match_int_chain(
                &scrutinee_reg,
                &scrutinee_ty,
                arms,
                end_lbl,
                is_expr,
            ));
        }

        let Type::Enum(_, inner_ty, variants) = scrutinee_ty else {
            self.line(&format!("br label %{end_lbl}"));
            self.line("");
            self.indent -= 1;
            self.line(&format!("{end_lbl}:"));
            self.indent += 1;
            if is_expr {
                let reg = self.new_reg();
                self.line(&format!("{reg} = add i32 0, 0  ; match fallback"));
            }
            return None;
        };

        let ll_ty = llvm_type(&inner_ty);
        let mut disc_map: std::collections::HashMap<&str, i64> = std::collections::HashMap::new();
        for (vname, disc) in &variants {
            disc_map.insert(vname.as_str(), *disc);
        }

        let mut arm_labels = Vec::new();
        for _ in 0..arms.len() {
            arm_labels.push(self.new_label("match_arm"));
        }

        let default_lbl = match arms.iter().position(|arm| {
            arm.patterns
                .iter()
                .any(|p| matches!(p, ast::MatchPattern::Wildcard(_)))
        }) {
            Some(idx) => arm_labels[idx].clone(),
            None if is_expr => self.new_label("match_default"),
            None => end_lbl.clone(),
        };

        self.line(&format!(
            "switch {ll_ty} {scrutinee_reg}, label %{default_lbl} ["
        ));
        self.indent += 1;
        for (i, arm) in arms.iter().enumerate() {
            if !arm
                .patterns
                .iter()
                .any(|p| matches!(p, ast::MatchPattern::Wildcard(_)))
            {
                for pat in &arm.patterns {
                    if let ast::MatchPattern::Variant(_, (vname, _)) = pat
                        && let Some(&disc) = disc_map.get(vname.as_str())
                    {
                        self.line(&format!("{ll_ty} {disc}, label %{}", arm_labels[i]));
                    }
                }
            }
        }
        self.indent -= 1;
        self.line("]");
        self.line("");

        Some(MatchDispatch {
            end_lbl,
            ll_ty,
            arm_labels,
            default_lbl,
        })
    }

    /// Dispatch an integer match as an ordered if-chain: each arm's patterns are
    /// OR'd into a condition; the first arm whose condition holds is taken. A
    /// `_` arm is the unconditional catch-all (and stops the chain).
    fn emit_match_int_chain(
        &mut self,
        scrutinee_reg: &str,
        scrutinee_ty: &Type,
        arms: &[ast::MatchArm],
        end_lbl: String,
        is_expr: bool,
    ) -> MatchDispatch {
        let ll_ty = llvm_type(scrutinee_ty);
        let signed = matches!(scrutinee_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64);

        let arm_labels: Vec<String> = (0..arms.len())
            .map(|_| self.new_label("match_arm"))
            .collect();
        let wildcard_idx = arms.iter().position(|a| {
            a.patterns
                .iter()
                .any(|p| matches!(p, ast::MatchPattern::Wildcard(_)))
        });
        let default_lbl = match wildcard_idx {
            Some(idx) => arm_labels[idx].clone(),
            None if is_expr => self.new_label("match_default"),
            None => end_lbl.clone(),
        };

        for (i, arm) in arms.iter().enumerate() {
            if arm
                .patterns
                .iter()
                .any(|p| matches!(p, ast::MatchPattern::Wildcard(_)))
            {
                // Catch-all: jump unconditionally; any later arms are dead.
                self.line(&format!("br label %{}", arm_labels[i]));
                self.line("");
                return MatchDispatch {
                    end_lbl,
                    ll_ty,
                    arm_labels,
                    default_lbl,
                };
            }
            let cond = self.emit_match_arm_cond(scrutinee_reg, &ll_ty, signed, &arm.patterns);
            let next_lbl = self.new_label("match_next");
            self.line(&format!(
                "br i1 {cond}, label %{}, label %{next_lbl}",
                arm_labels[i]
            ));
            self.line("");
            self.indent -= 1;
            self.line(&format!("{next_lbl}:"));
            self.indent += 1;
        }
        // No `_` arm matched anything: fall through.
        self.line(&format!("br label %{default_lbl}"));
        self.line("");
        MatchDispatch {
            end_lbl,
            ll_ty,
            arm_labels,
            default_lbl,
        }
    }

    /// Build an `i1` register that is true when the scrutinee matches any of an
    /// arm's integer / range patterns.
    fn emit_match_arm_cond(
        &mut self,
        scrutinee_reg: &str,
        ll_ty: &str,
        signed: bool,
        patterns: &[ast::MatchPattern],
    ) -> String {
        let mut acc: Option<String> = None;
        for pat in patterns {
            let cond = match pat {
                ast::MatchPattern::Int(v, _) => {
                    let r = self.new_reg();
                    self.line(&format!("{r} = icmp eq {ll_ty} {scrutinee_reg}, {v}"));
                    r
                }
                ast::MatchPattern::Range(lo, hi, _) => {
                    let (ge, le) = if signed {
                        ("sge", "sle")
                    } else {
                        ("uge", "ule")
                    };
                    let a = self.new_reg();
                    self.line(&format!("{a} = icmp {ge} {ll_ty} {scrutinee_reg}, {lo}"));
                    let b = self.new_reg();
                    self.line(&format!("{b} = icmp {le} {ll_ty} {scrutinee_reg}, {hi}"));
                    let r = self.new_reg();
                    self.line(&format!("{r} = and i1 {a}, {b}"));
                    r
                }
                // Non-wildcard arm: variant/wildcard don't occur here.
                _ => continue,
            };
            acc = Some(match acc {
                None => cond,
                Some(prev) => {
                    let r = self.new_reg();
                    self.line(&format!("{r} = or i1 {prev}, {cond}"));
                    r
                }
            });
        }
        acc.unwrap_or_else(|| {
            let r = self.new_reg();
            self.line(&format!("{r} = add i1 0, 0"));
            r
        })
    }

    fn emit_stmt(
        &mut self,
        stmt: &Stmt,
        symbols: &SymbolTable,
        fn_name: &str,
        break_label: Option<&str>,
        continue_label: Option<&str>,
    ) -> (Option<String>, bool) {
        match stmt {
            Stmt::VarDecl(vd) => {
                let (alloca_name, llvm_ty, bml_type) = {
                    let info = self
                        .locals
                        .get(&vd.name.0)
                        .expect("var should have entry alloca");
                    (
                        info.alloca.clone(),
                        info.llvm_ty.clone(),
                        info.bml_type.clone(),
                    )
                };
                // Array literal with a declared element type: store each element
                // coerced to that type, so `var b: [u8; 4] = [0, 0, 0, 0]` works
                // (bare literals are typed i32 and would otherwise mismatch).
                if let (Expr::ArrayInit(elems, _), Type::Array(elem_ty, _)) = (&vd.init, &bml_type)
                {
                    let ll_elem = llvm_type(elem_ty);
                    for (i, e) in elems.iter().enumerate() {
                        let r = self.emit_expr(e, symbols, fn_name);
                        let ety = self.expr_type(e, symbols);
                        let r = self.coerce_int(r, &ety, elem_ty);
                        let gep = self.new_reg();
                        self.line(&format!(
                            "{gep} = getelementptr {llvm_ty}, ptr {alloca_name}, i32 0, i32 {i}"
                        ));
                        self.line(&format!("store {ll_elem} {r}, ptr {gep}"));
                    }
                    self.dbg_declare(&alloca_name, &vd.name.0, &bml_type, vd.name.1);
                    return (None, false);
                }
                let init_reg = self.emit_expr(&vd.init, symbols, fn_name);
                let init_ty = self.expr_type(&vd.init, symbols);
                let init_llvm = llvm_type(&init_ty);
                let final_reg = if init_llvm == llvm_ty {
                    init_reg
                } else if crate::types::is_float(&init_ty) && crate::types::is_float(&bml_type) {
                    let reg = self.new_reg();
                    if float_bit_width(&llvm_ty) > float_bit_width(&init_llvm) {
                        self.line(&format!(
                            "{reg} = fpext {init_llvm} {init_reg} to {llvm_ty}"
                        ));
                    } else {
                        self.line(&format!(
                            "{reg} = fptrunc {init_llvm} {init_reg} to {llvm_ty}"
                        ));
                    }
                    reg
                } else if crate::types::is_int(&init_ty) && crate::types::is_int(&bml_type) {
                    let init_bits = int_bit_width(&init_llvm);
                    let target_bits = int_bit_width(&llvm_ty);
                    let reg = self.new_reg();
                    if target_bits > init_bits {
                        let ext_op =
                            if matches!(init_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64) {
                                "sext"
                            } else {
                                "zext"
                            };
                        self.line(&format!(
                            "{reg} = {ext_op} {init_llvm} {init_reg} to {llvm_ty}"
                        ));
                    } else {
                        self.line(&format!(
                            "{reg} = trunc {init_llvm} {init_reg} to {llvm_ty}"
                        ));
                    }
                    reg
                } else {
                    init_reg
                };
                let dbg_sfx = self.dbg_loc(vd.init.span());
                self.line(&format!(
                    "store {llvm_ty} {final_reg}, ptr {alloca_name}{dbg_sfx}"
                ));
                self.dbg_declare(&alloca_name, &vd.name.0, &bml_type, vd.name.1);
                (Some(final_reg), false)
            }

            Stmt::Assign(assign) => {
                let val_reg = self.emit_expr(&assign.value, symbols, fn_name);
                let val_ty = self.expr_type(&assign.value, symbols);
                let dbg_span = assign.target.span();
                let target = self.emit_store_target(
                    &assign.target,
                    symbols,
                    fn_name,
                    &val_reg,
                    &val_ty,
                    dbg_span,
                    Some(&assign.value),
                );
                (Some(target), false)
            }

            Stmt::CompoundAssign(ca) => {
                self.emit_compound_assign(ca, symbols, fn_name);
                (None, false)
            }

            Stmt::Expr(expr) => (Some(self.emit_expr(expr, symbols, fn_name)), false),

            Stmt::Asm(asm_stmt) => {
                let escaped = asm_stmt
                    .asm_text
                    .replace('\\', "\\\\")
                    .replace('"', "\\22")
                    .replace('\n', "\\0A");
                // Explicit operands take the structured path; otherwise keep the
                // legacy implicit behavior (bind the function's params to r0-r3).
                if !asm_stmt.outputs.is_empty()
                    || !asm_stmt.inputs.is_empty()
                    || !asm_stmt.clobbers.is_empty()
                {
                    self.emit_asm_operands(asm_stmt, &escaped, symbols, fn_name);
                    return (None, false);
                }
                if self.current_fn_params.is_empty() {
                    self.line(&format!(
                        "call void asm sideeffect \"{escaped}\", \"~{{memory}}\"()"
                    ));
                } else {
                    let param_infos: Vec<_> = self
                        .current_fn_params
                        .iter()
                        .filter_map(|(name, _)| self.locals.get(name).cloned())
                        .collect();
                    if param_infos.is_empty() {
                        self.line(&format!(
                            "call void asm sideeffect \"{escaped}\", \"~{{memory}}\"()"
                        ));
                    } else {
                        let reg_names = self.arch.asm_param_regs();
                        let mut constraints = Vec::new();
                        let mut operands = Vec::new();
                        for (i, info) in param_infos.iter().enumerate() {
                            let reg = self.new_reg();
                            self.line(&format!(
                                "{reg} = load {}, ptr {}",
                                info.llvm_ty, info.alloca
                            ));
                            let constraint = if i < 4 { reg_names[i] } else { "r" };
                            constraints.push(constraint);
                            operands.push(format!("{} {}", info.llvm_ty, reg));
                        }
                        constraints.push("~{memory}");
                        self.line(&format!(
                            "call void asm sideeffect \"{escaped}\", \"{}\"({})",
                            constraints.join(","),
                            operands.join(", ")
                        ));
                    }
                }
                (None, false)
            }

            Stmt::Return(ret) => {
                let dbg_sfx = match &ret.value {
                    Some(val) => self.dbg_loc(val.span()),
                    None => String::new(),
                };
                if let Some(val) = &ret.value {
                    let reg = self.emit_expr(val, symbols, fn_name);
                    let val_ty = self.expr_type(val, symbols);
                    // Return the value at the function's declared return width,
                    // coercing (e.g. an i32 literal returned from an i8 fn).
                    let ret_ty = symbols
                        .functions
                        .get(fn_name)
                        .and_then(|f| f.ret.clone())
                        .unwrap_or_else(|| val_ty.clone());
                    let reg = self.coerce_int(reg, &val_ty, &ret_ty);
                    let ty = llvm_type(&ret_ty);
                    self.line(&format!("ret {ty} {reg}{dbg_sfx}"));
                } else {
                    self.line(&format!("ret void{dbg_sfx}"));
                }
                (None, true)
            }

            Stmt::Break(_) => {
                if let Some(lbl) = break_label {
                    self.line(&format!("br label %{lbl}"));
                }
                (None, true)
            }
            Stmt::Continue(_) => {
                if let Some(lbl) = continue_label {
                    self.line(&format!("br label %{lbl}"));
                }
                (None, true)
            }

            Stmt::If(if_stmt) => {
                // `comptime if`: fold to the taken branch when the condition is
                // const-evaluable; the other branch is not emitted. A structurally
                // const but non-evaluable condition (e.g. div-by-zero) falls through
                // to the runtime lowering -- identical to a plain `if` on that
                // expression. A clean E411-at-check for that edge arrives with the
                // Slice 2 eval plumbing (see doc/comptime.md).
                if if_stmt.comptime {
                    let folded = {
                        let consts = self.spec_consts();
                        let env = IrConstEnv {
                            symbols,
                            consts: &consts,
                        };
                        consteval::eval_bool(&if_stmt.cond, &env)
                    };
                    if let Some(taken) = folded {
                        if taken {
                            return self.emit_block(
                                &if_stmt.then_block,
                                symbols,
                                fn_name,
                                break_label,
                                continue_label,
                            );
                        } else if let Some(else_branch) = &if_stmt.else_branch {
                            return self.emit_stmt(
                                else_branch,
                                symbols,
                                fn_name,
                                break_label,
                                continue_label,
                            );
                        }
                        return (None, false);
                    }
                    // Comptime-shaped but did not evaluate at this specialization
                    // (a param-dependent divisor of 0, or overflow).
                    self.comptime_errors.push((
                        "comptime if condition does not evaluate to a constant (division by zero or overflow during specialization)".to_string(),
                        "E411".to_string(),
                        if_stmt.cond.span(),
                    ));
                }
                let then_lbl = self.new_label("then");
                let else_lbl = self.new_label("else");
                let end_lbl = self.new_label("endif");

                self.emit_branch_cond(&if_stmt.cond, &then_lbl, &else_lbl, symbols, fn_name);
                self.line("");

                self.indent -= 1;
                self.line(&format!("{then_lbl}:"));
                self.indent += 1;
                let (_, then_term) = self.emit_block(
                    &if_stmt.then_block,
                    symbols,
                    fn_name,
                    break_label,
                    continue_label,
                );
                if !then_term {
                    self.line(&format!("br label %{end_lbl}"));
                }
                self.line("");

                self.indent -= 1;
                self.line(&format!("{else_lbl}:"));
                self.indent += 1;
                let mut else_term = false;
                if let Some(else_branch) = &if_stmt.else_branch {
                    match else_branch.as_ref() {
                        Stmt::Block(block) => {
                            let (_, term) = self.emit_block(
                                block,
                                symbols,
                                fn_name,
                                break_label,
                                continue_label,
                            );
                            else_term = term;
                        }
                        Stmt::If(_inner_if) => {
                            let (_, term) = self.emit_stmt(
                                else_branch,
                                symbols,
                                fn_name,
                                break_label,
                                continue_label,
                            );
                            else_term = term;
                        }
                        _ => {}
                    }
                }
                if !else_term {
                    self.line(&format!("br label %{end_lbl}"));
                }
                self.line("");

                self.indent -= 1;
                self.line(&format!("{end_lbl}:"));
                self.indent += 1;
                (None, false)
            }

            Stmt::For(for_stmt) => {
                let bml_type =
                    crate::types::resolve_type_expr(&for_stmt.ty, &symbols.structs, &symbols.enums);
                let ty = llvm_type(&bml_type);
                let signed = matches!(bml_type, Type::I8 | Type::I16 | Type::I32 | Type::I64);
                // Bounds and step may be integer literals (emitted as i32) or
                // wider expressions; coerce each to the loop variable's width so
                // the store/compare/step all agree on type.
                let start_ty = self.expr_type(&for_stmt.start, symbols);
                let start_reg = self.emit_expr(&for_stmt.start, symbols, fn_name);
                let start_reg = self.coerce_int(start_reg, &start_ty, &bml_type);
                let end_ty = self.expr_type(&for_stmt.end, symbols);
                let end_reg = self.emit_expr(&for_stmt.end, symbols, fn_name);
                let end_reg = self.coerce_int(end_reg, &end_ty, &bml_type);
                let step_reg = if let Some(step) = &for_stmt.step {
                    let step_ty = self.expr_type(step, symbols);
                    let reg = self.emit_expr(step, symbols, fn_name);
                    self.coerce_int(reg, &step_ty, &bml_type)
                } else {
                    "1".to_string()
                };
                let alloca = self
                    .locals
                    .get(&for_stmt.var.0)
                    .expect("for var should have entry alloca")
                    .alloca
                    .clone();
                self.line(&format!("store {ty} {start_reg}, ptr {alloca}"));

                let cond_lbl = self.new_label("for_cond");
                let body_lbl = self.new_label("for_body");
                let step_lbl = self.new_label("for_step");
                let end_lbl = self.new_label("for_end");

                self.line(&format!("br label %{cond_lbl}"));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{cond_lbl}:"));
                self.indent += 1;
                let cond_reg = self.new_reg();
                self.line(&format!("{cond_reg} = load {ty}, ptr {alloca}"));
                let cmp_reg = self.new_reg();
                let cmp_op = match (for_stmt.direction, signed) {
                    (ast::ForDirection::Upto, true) => "icmp slt",
                    (ast::ForDirection::Upto, false) => "icmp ult",
                    (ast::ForDirection::Downto, true) => "icmp sgt",
                    (ast::ForDirection::Downto, false) => "icmp ugt",
                };
                self.line(&format!("{cmp_reg} = {cmp_op} {ty} {cond_reg}, {end_reg}"));
                self.line(&format!(
                    "br i1 {cmp_reg}, label %{body_lbl}, label %{end_lbl}"
                ));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{body_lbl}:"));
                self.indent += 1;
                let (_, body_term) = self.emit_block(
                    &for_stmt.body,
                    symbols,
                    fn_name,
                    Some(end_lbl.as_str()),
                    Some(step_lbl.as_str()),
                );
                if !body_term {
                    self.line(&format!("br label %{step_lbl}"));
                }
                self.line("");

                self.indent -= 1;
                self.line(&format!("{step_lbl}:"));
                self.indent += 1;
                let step_load = self.new_reg();
                self.line(&format!("{step_load} = load {ty}, ptr {alloca}"));
                let step_op = match for_stmt.direction {
                    ast::ForDirection::Upto => "add",
                    ast::ForDirection::Downto => "sub",
                };
                let next_reg = self.new_reg();
                self.line(&format!(
                    "{next_reg} = {step_op} {ty} {step_load}, {step_reg}"
                ));
                self.line(&format!("store {ty} {next_reg}, ptr {alloca}"));
                self.line(&format!("br label %{cond_lbl}"));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{end_lbl}:"));
                self.indent += 1;
                (None, false)
            }

            Stmt::Loop(loop_stmt) => {
                let loop_lbl = self.new_label("loop");
                let body_lbl = self.new_label("loop_body");
                let end_lbl = self.new_label("loop_end");

                self.line(&format!("br label %{loop_lbl}"));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{loop_lbl}:"));
                self.indent += 1;
                self.line(&format!("br label %{body_lbl}"));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{body_lbl}:"));
                self.indent += 1;
                let (_, body_term) = self.emit_block(
                    &loop_stmt.body,
                    symbols,
                    fn_name,
                    Some(end_lbl.as_str()),
                    Some(loop_lbl.as_str()),
                );
                if !body_term {
                    self.line(&format!("br label %{loop_lbl}"));
                }
                self.line("");

                self.indent -= 1;
                self.line(&format!("{end_lbl}:"));
                self.indent += 1;
                (None, false)
            }

            Stmt::Claim(c) => {
                // One masked window for the whole block (the CPU-side
                // `reclaim`; see ast::Stmt::Claim). One mask pair is sound:
                // the checker rejects calls and escapes inside (E614), so
                // nothing can drop the mask early or skip the leave, and
                // per-access critical sections inside are suppressed
                // (claim_depth). A nested claim adds no second pair. The
                // mask is BASEPRI to the static's ceiling on v7-M (local
                // contenders are bounded by the ceiling by definition),
                // PRIMASK otherwise.
                //
                // CROSS-CORE statics additionally take their hardware
                // spinlock (read = try-acquire, 0 = held; write = release),
                // at any nesting depth -- the mask only excludes this core.
                // Spinning with interrupts masked is sound: the holder is
                // the other core, whose progress does not need our IRQs.
                let cs = if self.claim_depth == 0 {
                    let ceiling = Self::shared_ceiling(&c.name.0, symbols);
                    Some(crate::arch::arm::emit_critical_enter(self, ceiling))
                } else {
                    None
                };
                self.claim_depth += 1;
                self.claimed_statics.push(c.name.0.clone());
                let lock_addr = self
                    .cross_core_locks
                    .get(&c.name.0)
                    .map(|idx| self.spinlock_base + 4 * u64::from(*idx));
                if let Some(addr) = lock_addr {
                    let ptr_ty = self.arch.ptr_type();
                    let spin = self.new_label("spinlock_try");
                    let acq = self.new_label("spinlock_acq");
                    self.line(&format!("br label %{spin}"));
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{spin}:"));
                    self.indent += 1;
                    let r = self.new_reg();
                    self.line(&format!(
                        "{r} = load volatile i32, ptr inttoptr ({ptr_ty} {addr} to ptr)"
                    ));
                    let z = self.new_reg();
                    self.line(&format!("{z} = icmp eq i32 {r}, 0"));
                    self.line(&format!("br i1 {z}, label %{spin}, label %{acq}"));
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{acq}:"));
                    self.indent += 1;
                }
                let patched = symbols.with_claimed(&c.name.0);
                let (_, body_term) =
                    self.emit_block(&c.body, &patched, fn_name, break_label, continue_label);
                if let Some(addr) = lock_addr {
                    let ptr_ty = self.arch.ptr_type();
                    self.line(&format!(
                        "store volatile i32 1, ptr inttoptr ({ptr_ty} {addr} to ptr)"
                    ));
                }
                self.claimed_statics.pop();
                self.claim_depth -= 1;
                if let Some(token) = cs {
                    crate::arch::arm::emit_critical_leave(self, token);
                }
                (None, body_term)
            }

            Stmt::While(while_stmt) => {
                let cond_lbl = self.new_label("while_cond");
                let body_lbl = self.new_label("while_body");
                let end_lbl = self.new_label("while_end");

                self.line(&format!("br label %{cond_lbl}"));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{cond_lbl}:"));
                self.indent += 1;
                self.emit_branch_cond(&while_stmt.cond, &body_lbl, &end_lbl, symbols, fn_name);
                self.line("");

                self.indent -= 1;
                self.line(&format!("{body_lbl}:"));
                self.indent += 1;
                let (_, body_term) = self.emit_block(
                    &while_stmt.body,
                    symbols,
                    fn_name,
                    Some(end_lbl.as_str()),
                    Some(cond_lbl.as_str()),
                );
                if !body_term {
                    self.line(&format!("br label %{cond_lbl}"));
                }
                self.line("");

                self.indent -= 1;
                self.line(&format!("{end_lbl}:"));
                self.indent += 1;
                (None, false)
            }

            Stmt::Match(match_stmt) => {
                // `comptime match`: select the arm at compile time and emit only
                // its body when the scrutinee is const-evaluable. A non-evaluable
                // scrutinee falls through to the runtime match lowering (see the
                // `comptime if` note above; doc/comptime.md).
                if match_stmt.comptime {
                    let folded = {
                        let consts = self.spec_consts();
                        let env = IrConstEnv {
                            symbols,
                            consts: &consts,
                        };
                        consteval::eval_int(&match_stmt.scrutinee, &env)
                    };
                    if let Some(scrut) = folded {
                        let sty = self.expr_type(&match_stmt.scrutinee, symbols);
                        if !comptime_value_fits(scrut, &sty) {
                            self.comptime_errors.push((
                                format!("comptime match scrutinee value {scrut} does not fit type `{sty:?}`"),
                                "E411".to_string(),
                                match_stmt.scrutinee.span(),
                            ));
                        }
                        if let Some(arm) = comptime_match_arm(scrut, &match_stmt.arms, symbols) {
                            return self.emit_block(
                                &arm.body,
                                symbols,
                                fn_name,
                                break_label,
                                continue_label,
                            );
                        }
                    } else {
                        self.comptime_errors.push((
                            "comptime match scrutinee does not evaluate to a constant (division by zero or overflow during specialization)".to_string(),
                            "E411".to_string(),
                            match_stmt.scrutinee.span(),
                        ));
                    }
                }
                let Some(MatchDispatch {
                    end_lbl,
                    arm_labels,
                    ..
                }) = self.emit_match_dispatch(
                    &match_stmt.scrutinee,
                    &match_stmt.arms,
                    symbols,
                    fn_name,
                    false,
                )
                else {
                    return (None, false);
                };

                for (i, arm) in match_stmt.arms.iter().enumerate() {
                    self.indent -= 1;
                    self.line(&format!("{}:", arm_labels[i]));
                    self.indent += 1;
                    let (_, arm_term) =
                        self.emit_block(&arm.body, symbols, fn_name, break_label, continue_label);
                    if !arm_term {
                        self.line(&format!("br label %{end_lbl}"));
                    }
                    self.line("");
                }

                self.indent -= 1;
                self.line(&format!("{end_lbl}:"));
                self.indent += 1;
                (None, false)
            }

            Stmt::Assume(assume) => {
                let ok_lbl = self.new_label("assume_ok");
                let unreach_lbl = self.new_label("assume_unreach");
                self.emit_branch_cond(&assume.cond, &ok_lbl, &unreach_lbl, symbols, fn_name);
                self.line("");
                self.indent -= 1;
                self.line(&format!("{unreach_lbl}:"));
                self.indent += 1;
                self.line("unreachable");
                self.line("");
                self.indent -= 1;
                self.line(&format!("{ok_lbl}:"));
                self.indent += 1;
                (None, false)
            }

            Stmt::Assert(assert) => {
                if self.verify_mode {
                    let cond_reg = self.emit_expr(&assert.cond, symbols, fn_name);
                    let dbg = self.dbg_loc(assert.cond.span());
                    let zext_reg = self.new_reg();
                    self.line(&format!("{zext_reg} = zext i1 {cond_reg} to i32"));
                    self.line(&format!("call void @__ikos_assert(i32 {zext_reg}){dbg}"));
                }
                (None, false)
            }

            Stmt::Block(inner) => {
                self.emit_block(inner, symbols, fn_name, break_label, continue_label)
            }
        }
    }

    // ─── expressions ─────────────────────────────────────────────────

    fn emit_expr(&mut self, expr: &Expr, symbols: &SymbolTable, fn_name: &str) -> String {
        use crate::ast::BinaryOp;

        match expr {
            Expr::IntLiteral(n, suffix, _span) => {
                let reg = self.new_reg();
                // An unsuffixed literal defaults to 32-bit, but a value that does
                // not fit in 32 bits would be truncated by `add i32 0, N` before
                // any widening. Such a literal is only accepted by the checker in
                // a 64-bit context, so materialize it at 64 bits. Keep `expr_type`
                // below in sync.
                let width = match suffix {
                    crate::ast::IntSuffix::None if *n > u64::from(u32::MAX) => 64,
                    _ => int_bit_width_from_suffix(*suffix),
                };
                let val = match suffix {
                    crate::ast::IntSuffix::U8 | crate::ast::IntSuffix::I8 => *n & 0xFF,
                    crate::ast::IntSuffix::U16 | crate::ast::IntSuffix::I16 => *n & 0xFFFF,
                    _ => *n,
                };
                self.line(&format!("{reg} = add i{width} 0, {val}"));
                reg
            }
            Expr::FloatLiteral(f, suffix, _span) => {
                let reg = self.new_reg();
                let (llvm_op, llvm_ty) = match suffix {
                    crate::ast::FloatSuffix::H => ("fadd", "half"),
                    crate::ast::FloatSuffix::F | crate::ast::FloatSuffix::None => ("fadd", "float"),
                    crate::ast::FloatSuffix::D => ("fadd", "double"),
                };
                self.line(&format!(
                    "{reg} = {llvm_op} {llvm_ty} 0.0, {}",
                    float_to_llvm(*f, *suffix)
                ));
                reg
            }
            Expr::BoolLiteral(b, _) => {
                let reg = self.new_reg();
                let v = u32::from(*b);
                self.line(&format!("{reg} = add i1 0, {v}"));
                reg
            }
            Expr::NullLiteral(_) => {
                let reg = self.new_reg();
                self.line(&format!("{reg} = getelementptr i8, ptr null, i32 0"));
                reg
            }
            Expr::StringLiteral(s, _) => {
                let id = self.new_str_id();
                let escaped = escape_llvm_string(s);
                let len = s.len() + 1; // +1 for null terminator
                self.strings.push(format!(
                    "@.str.{id} = private unnamed_addr constant [{len} x i8] c\"{escaped}\\00\", align 1"
                ));
                let ptr = self.new_reg();
                self.line(&format!(
                    "{ptr} = getelementptr [{len} x i8], ptr @.str.{id}, i32 0, i32 0"
                ));
                ptr
            }
            Expr::Ident((name, _)) => {
                // Check locals
                let local = self.locals.get(name).cloned();
                if let Some(info) = local {
                    let reg = self.new_reg();
                    self.line(&format!(
                        "{reg} = load {}, ptr {}",
                        info.llvm_ty, info.alloca
                    ));
                    return reg;
                }
                // Check peripherals -- for peripheral name, return the base address
                if let Some(p) = symbols.peripherals.get(name) {
                    let reg = self.new_reg();
                    let ptr_ty = self.ptr_type();
                    self.line(&format!("{reg} = add {ptr_ty} 0, {}", p.base_addr));
                    return reg;
                }
                // Check statics (global load)
                if let Some(sym) = symbols.statics.get(name) {
                    let ty = llvm_type(sym.ty.inner());
                    if sym
                        .storage
                        .iter()
                        .any(|ann| matches!(ann, StorageAnnotation::Shared(_)))
                    {
                        self.emit_verify_forget_shared_static(name, sym.ty.inner());
                    }
                    let cs = self
                        .critical_section_ceiling(name, symbols)
                        .map(|ceiling| crate::arch::arm::emit_critical_enter(self, Some(ceiling)));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ty}, ptr @{name}"));
                    if let Some(token) = cs {
                        crate::arch::arm::emit_critical_leave(self, token);
                    }
                    return reg;
                }
                // Check consts
                if let Some(csym) = symbols.consts.get(name) {
                    let ty = llvm_type(&csym.ty);
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ty}, ptr @{name}"));
                    return reg;
                }
                // Functions: return function address as pointer
                if symbols.functions.contains_key(name) {
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = getelementptr i8, ptr @{name}, i32 0"));
                    return reg;
                }
                // Fallback -- should be unreachable since checker validated names
                unreachable!("identifier not found in any symbol table: {name}")
            }

            Expr::Unary(op, inner) => {
                use crate::ast::UnaryOp;
                match op {
                    UnaryOp::Deref => {
                        let vol = self.vol_expr(inner, symbols);
                        let inner_reg = self.emit_expr(inner, symbols, fn_name);
                        let pointee_ty = match self.expr_type(inner, symbols) {
                            Type::Ptr(inner) | Type::ConstPtr(inner) => *inner,
                            _ => crate::types::Type::I32, // fallback
                        };
                        let llty = llvm_type(&pointee_ty);
                        let dbg = self.dbg_loc(expr.span());
                        let reg = self.new_reg();
                        self.line(&format!("{reg} = load{vol} {llty}, ptr {inner_reg}{dbg}"));
                        reg
                    }
                    UnaryOp::AddrOf | UnaryOp::AddrOfMut => {
                        // Take address: return pointer to the lvalue without loading
                        self.emit_lvalue_ptr(inner, symbols)
                    }
                    _ => {
                        let inner_reg = self.emit_expr(inner, symbols, fn_name);
                        // Negation and bitwise-not must operate at the operand's
                        // own width; hardcoding i32 produces invalid IR for i8/i16.
                        let inner_ty = self.expr_type(inner, symbols);
                        let inner_llvm = llvm_type(&inner_ty);
                        let reg = self.new_reg();
                        match op {
                            UnaryOp::Neg if crate::types::is_float(&inner_ty) => {
                                self.line(&format!("{reg} = fneg {inner_llvm} {inner_reg}"));
                            }
                            UnaryOp::Neg => {
                                // Signed negation gets `nsw` in verify IR
                                // (sio instead of uio false positives; see
                                // the Binary arm). Runtime IR stays plain.
                                let nsw =
                                    if self.verify_mode && crate::types::is_signed_int(&inner_ty) {
                                        " nsw"
                                    } else {
                                        ""
                                    };
                                self.line(&format!("{reg} = sub{nsw} {inner_llvm} 0, {inner_reg}"));
                            }
                            UnaryOp::Not => {
                                self.line(&format!("{reg} = xor i1 {inner_reg}, true"));
                            }
                            UnaryOp::BitNot => {
                                self.line(&format!("{reg} = xor {inner_llvm} {inner_reg}, -1"));
                            }
                            _ => {}
                        }
                        reg
                    }
                }
            }

            Expr::Binary(left, op, right) => {
                let left_ty = self.expr_type(left, symbols);
                let right_ty = self.expr_type(right, symbols);

                // SHORT-CIRCUIT logical operators: `a && b` does not evaluate
                // `b` when `a` is false (dually for `||`). This is load-
                // bearing for an MMIO language -- with the old eager
                // `and i1` lowering, `a && P.SR.X` read the register even
                // when `a` was false, a real hazard for read-to-clear status
                // registers (and `a && f()` always called f). Lowered as a
                // branch around the RHS with an i1 phi at the join.
                if matches!(op, BinaryOp::And | BinaryOp::Or) {
                    let is_and = *op == BinaryOp::And;
                    let dbg = self.dbg_loc(expr.span());
                    let lhs_reg = self.emit_expr(left, symbols, fn_name);
                    let lhs_edge = self
                        .current_label
                        .clone()
                        .unwrap_or_else(|| "entry".to_string());
                    let rhs_lbl = self.new_label(if is_and { "and_rhs" } else { "or_rhs" });
                    let end_lbl = self.new_label(if is_and { "and_end" } else { "or_end" });
                    if is_and {
                        self.line(&format!(
                            "br i1 {lhs_reg}, label %{rhs_lbl}, label %{end_lbl}{dbg}"
                        ));
                    } else {
                        self.line(&format!(
                            "br i1 {lhs_reg}, label %{end_lbl}, label %{rhs_lbl}{dbg}"
                        ));
                    }
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{rhs_lbl}:"));
                    self.indent += 1;
                    let rhs_reg = self.emit_expr(right, symbols, fn_name);
                    let rhs_edge = self.current_label.clone().unwrap_or(rhs_lbl);
                    self.line(&format!("br label %{end_lbl}"));
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{end_lbl}:"));
                    self.indent += 1;
                    // The short value is the LHS-decided one: false for &&,
                    // true for ||.
                    let short_val = if is_and { "false" } else { "true" };
                    let result = self.new_reg();
                    self.line(&format!(
                        "{result} = phi i1 [ {short_val}, %{lhs_edge} ], [ {rhs_reg}, %{rhs_edge} ]"
                    ));
                    return result;
                }

                // Pointer arithmetic: GEP
                if crate::types::is_ptr(&left_ty)
                    && crate::types::is_int(&right_ty)
                    && matches!(op, BinaryOp::Add | BinaryOp::Sub)
                {
                    let left_reg = self.emit_expr(left, symbols, fn_name);
                    let right_reg = self.emit_expr(right, symbols, fn_name);
                    let pointee_ty = match &left_ty {
                        Type::Ptr(t) | Type::ConstPtr(t) => t.as_ref(),
                        _ => &crate::types::Type::I32,
                    };
                    let ll_elem = llvm_type(pointee_ty);
                    let reg = self.new_reg();
                    let neg_idx = if *op == BinaryOp::Sub {
                        let neg = self.new_reg();
                        self.line(&format!(
                            "{neg} = sub {} 0, {right_reg}",
                            llvm_type(&right_ty)
                        ));
                        neg
                    } else {
                        right_reg
                    };
                    self.line(&format!(
                        "{reg} = getelementptr {ll_elem}, ptr {left_reg}, {} {neg_idx}",
                        llvm_type(&right_ty)
                    ));
                    return reg;
                }

                // Pointer diff: p - q
                if crate::types::is_ptr(&left_ty)
                    && crate::types::is_ptr(&right_ty)
                    && *op == BinaryOp::Sub
                {
                    let left_reg = self.emit_expr(left, symbols, fn_name);
                    let right_reg = self.emit_expr(right, symbols, fn_name);
                    let pointee_ty = match &left_ty {
                        Type::Ptr(t) | Type::ConstPtr(t) => t.as_ref(),
                        _ => &crate::types::Type::I32,
                    };
                    let elem_size = crate::types::element_size(pointee_ty);
                    let left_int = self.new_reg();
                    let right_int = self.new_reg();
                    let ptr_ty = self.ptr_type();
                    self.line(&format!("{left_int} = ptrtoint ptr {left_reg} to {ptr_ty}"));
                    self.line(&format!(
                        "{right_int} = ptrtoint ptr {right_reg} to {ptr_ty}"
                    ));
                    let diff = self.new_reg();
                    self.line(&format!("{diff} = sub {ptr_ty} {left_int}, {right_int}"));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = sdiv i32 {diff}, {elem_size}"));
                    return reg;
                }

                let left_reg = self.emit_expr(left, symbols, fn_name);
                let right_reg = self.emit_expr(right, symbols, fn_name);
                // Arithmetic operands are same-typed by the checker, but bitwise
                // and shift ops only require both sides to be integers, so the
                // shift count / operand may be a different width -- reconcile it
                // to the left operand's type (LLVM requires matching widths).
                let right_reg = if crate::types::is_int(&left_ty) {
                    self.coerce_int(right_reg, &right_ty, &left_ty)
                } else {
                    right_reg
                };
                let lty = llvm_type(&left_ty);
                let reg = self.new_reg();

                let is_float = crate::types::is_float(&left_ty);
                let (llvm_op, result_ty) = match op {
                    BinaryOp::Add => (if is_float { "fadd" } else { "add" }, lty.as_str()),
                    BinaryOp::Sub => (if is_float { "fsub" } else { "sub" }, lty.as_str()),
                    BinaryOp::Mul => (if is_float { "fmul" } else { "mul" }, lty.as_str()),
                    // Wrapping ops lower identically to the plain ops (LLVM
                    // add/sub/mul without nsw/nuw already wrap); the
                    // difference is declared intent, consumed by the verifier
                    // via Program::wrap_spans. Checker guarantees integer
                    // operands (E336), so no float branch.
                    BinaryOp::AddWrap => ("add", lty.as_str()),
                    BinaryOp::SubWrap => ("sub", lty.as_str()),
                    BinaryOp::MulWrap => ("mul", lty.as_str()),
                    BinaryOp::Div => {
                        if crate::types::is_int(&left_ty) {
                            if matches!(left_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64) {
                                ("sdiv", lty.as_str())
                            } else {
                                ("udiv", lty.as_str())
                            }
                        } else {
                            ("fdiv", lty.as_str())
                        }
                    }
                    BinaryOp::Mod => {
                        if crate::types::is_int(&left_ty) {
                            if matches!(left_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64) {
                                ("srem", lty.as_str())
                            } else {
                                ("urem", lty.as_str())
                            }
                        } else {
                            ("frem", lty.as_str())
                        }
                    }
                    BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::Gt
                    | BinaryOp::LtEq
                    | BinaryOp::GtEq => {
                        if crate::types::is_float(&left_ty) {
                            let fop = match op {
                                BinaryOp::Eq => "oeq",
                                BinaryOp::NotEq => "one",
                                BinaryOp::Lt => "olt",
                                BinaryOp::Gt => "ogt",
                                BinaryOp::LtEq => "ole",
                                BinaryOp::GtEq => "oge",
                                _ => unreachable!(),
                            };
                            ("fcmp", fop)
                        } else {
                            let iop = match op {
                                BinaryOp::Eq => "eq",
                                BinaryOp::NotEq => "ne",
                                BinaryOp::Lt
                                    if matches!(
                                        left_ty,
                                        Type::I8 | Type::I16 | Type::I32 | Type::I64
                                    ) =>
                                {
                                    "slt"
                                }
                                BinaryOp::Lt => "ult",
                                BinaryOp::Gt
                                    if matches!(
                                        left_ty,
                                        Type::I8 | Type::I16 | Type::I32 | Type::I64
                                    ) =>
                                {
                                    "sgt"
                                }
                                BinaryOp::Gt => "ugt",
                                BinaryOp::LtEq
                                    if matches!(
                                        left_ty,
                                        Type::I8 | Type::I16 | Type::I32 | Type::I64
                                    ) =>
                                {
                                    "sle"
                                }
                                BinaryOp::LtEq => "ule",
                                BinaryOp::GtEq
                                    if matches!(
                                        left_ty,
                                        Type::I8 | Type::I16 | Type::I32 | Type::I64
                                    ) =>
                                {
                                    "sge"
                                }
                                BinaryOp::GtEq => "uge",
                                _ => unreachable!(),
                            };
                            ("icmp", iop)
                        }
                    }
                    // Logical && / || are short-circuited above and never
                    // reach the eager opcode path.
                    BinaryOp::And | BinaryOp::Or => unreachable!("short-circuited above"),
                    BinaryOp::BitAnd => ("and", lty.as_str()),
                    BinaryOp::BitOr => ("or", lty.as_str()),
                    BinaryOp::BitXor => ("xor", lty.as_str()),
                    BinaryOp::Shl => ("shl", lty.as_str()),
                    BinaryOp::Shr => {
                        if matches!(left_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64) {
                            ("ashr", lty.as_str())
                        } else {
                            ("lshr", lty.as_str())
                        }
                    }
                };

                // Emit comparison as icmp/fcmp
                let cmp_result = matches!(
                    op,
                    BinaryOp::Eq
                        | BinaryOp::NotEq
                        | BinaryOp::Lt
                        | BinaryOp::Gt
                        | BinaryOp::LtEq
                        | BinaryOp::GtEq
                );
                let dbg = self.dbg_loc(expr.span());
                if cmp_result {
                    let (cmd, cond) = (llvm_op, result_ty);
                    self.line(&format!(
                        "{reg} = {cmd} {cond} {lty} {left_reg}, {right_reg}{dbg}"
                    ));
                } else {
                    // VERIFY IR ONLY: tag signed add/sub/mul with `nsw` so
                    // IKOS runs its SIGNED overflow check (sio) with branch
                    // narrowing, instead of reading the signless op as
                    // unsigned and flagging every legitimate negative value
                    // (uio false positives -- measured: plain `5 - 7` as i32
                    // reports a definite unsigned underflow; with nsw the
                    // same op proves clean and a real i32 overflow still
                    // reports sio). The RUNTIME IR stays flag-free: BML
                    // defines wrap, and nsw would license UB-based
                    // optimization. Wrap ops (`+%`) stay unflagged even on
                    // signed types -- wrap is their declared semantics.
                    let nsw = if self.verify_mode
                        && matches!(op, BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul)
                        && crate::types::is_signed_int(&left_ty)
                    {
                        " nsw"
                    } else {
                        ""
                    };
                    self.line(&format!(
                        "{reg} = {llvm_op}{nsw} {lty} {left_reg}, {right_reg}{dbg}"
                    ));
                }
                reg
            }

            Expr::Call(func_expr, args) if consteval::is_len_call(func_expr) => {
                self.emit_len_builtin(args, symbols, fn_name, expr.span())
            }

            Expr::Call(func_expr, args) => {
                let call_span = func_expr.span();
                let dbg_sfx = self.dbg_loc(call_span);

                // Determine if this is a direct call to a known function
                let direct_name = if let Expr::Ident((name, _)) = func_expr.as_ref() {
                    symbols.functions.contains_key(name).then(|| name.clone())
                } else {
                    None
                };

                if let Some(direct_name) = direct_name {
                    // Call to a `peripheral_type` driver: monomorphize. Resolve
                    // the concrete instance for each handle argument, queue the
                    // specialization, and call the mangled name with the handle
                    // arguments dropped (slice 2).
                    let comptime_positions = comptime_param_positions(&direct_name, symbols);
                    if !comptime_positions.is_empty() {
                        return self.emit_handle_call(
                            &direct_name,
                            args,
                            &comptime_positions,
                            symbols,
                            fn_name,
                            &dbg_sfx,
                        );
                    }
                    let fn_sym = symbols.functions.get(&direct_name);
                    let param_tys: Option<Vec<Type>> =
                        fn_sym.map(|s| s.params.iter().map(|(_, t)| t.clone()).collect());
                    let mut arg_parts = Vec::new();
                    for (i, arg) in args.iter().enumerate() {
                        let reg = self.emit_expr(arg, symbols, fn_name);
                        let ty = self.expr_type(arg, symbols);
                        // Pass each argument at its parameter's width so an i32
                        // literal lands correctly in a narrower parameter slot,
                        // with the AAPCS narrow-int extension (see abi_ext).
                        if let Some(pty) = param_tys.as_ref().and_then(|p| p.get(i)) {
                            let reg = self.coerce_int(reg, &ty, pty);
                            arg_parts.push(format!(
                                "{}{} {reg}",
                                llvm_type(pty),
                                abi_param_suffix(pty)
                            ));
                        } else {
                            arg_parts.push(format!("{} {reg}", llvm_type(&ty)));
                        }
                    }
                    let arg_str = arg_parts.join(", ");

                    let ret = fn_sym.and_then(|s| s.ret.as_ref());
                    let ret_ty = ret.map_or_else(|| "void".to_string(), llvm_type);
                    let ret_prefix = ret.map_or_else(String::new, abi_ret_prefix);

                    if ret_ty == "void" {
                        self.line(&format!("call void @{direct_name}({arg_str}){dbg_sfx}"));
                        // No SSA value; callers may not consume this. The
                        // type checker forbids using a void call's result as
                        // a value, so the empty string is never embedded in
                        // emitted IR.
                        String::new()
                    } else {
                        let reg = self.new_reg();
                        self.line(&format!(
                            "{reg} = call {ret_prefix}{ret_ty} @{direct_name}({arg_str}){dbg_sfx}"
                        ));
                        reg
                    }
                } else {
                    // Indirect call: emit callee FIRST so its register is
                    // defined before appearing in the call instruction.
                    let callee_reg = self.emit_expr(func_expr, symbols, fn_name);
                    let callee_ty = self.expr_type(func_expr, symbols);
                    let param_tys = match &callee_ty {
                        Type::Fn(ps, _) => Some(ps.clone()),
                        _ => None,
                    };

                    let mut arg_parts = Vec::new();
                    for (i, arg) in args.iter().enumerate() {
                        let reg = self.emit_expr(arg, symbols, fn_name);
                        let ty = self.expr_type(arg, symbols);
                        // Same AAPCS extension as the direct path. The pointee's
                        // own `define` applies the identical type-driven rule, so
                        // call site and callee agree whether it is bml or C.
                        if let Some(pty) = param_tys.as_ref().and_then(|p| p.get(i)) {
                            let reg = self.coerce_int(reg, &ty, pty);
                            arg_parts.push(format!(
                                "{}{} {reg}",
                                llvm_type(pty),
                                abi_param_suffix(pty)
                            ));
                        } else {
                            arg_parts.push(format!("{} {reg}", llvm_type(&ty)));
                        }
                    }
                    let arg_str = arg_parts.join(", ");

                    let (ret_prefix, ret_ty) = match &callee_ty {
                        Type::Fn(_, ret) => (abi_ret_prefix(ret), llvm_type(ret)),
                        _ => (String::new(), "void".to_string()),
                    };

                    if ret_ty == "void" {
                        self.line(&format!("call void {callee_reg}({arg_str}){dbg_sfx}"));
                        String::new()
                    } else {
                        let reg = self.new_reg();
                        self.line(&format!(
                            "{reg} = call {ret_prefix}{ret_ty} {callee_reg}({arg_str}){dbg_sfx}"
                        ));
                        reg
                    }
                }
            }

            Expr::FieldAccess(base, field) => {
                // Indexed array-register field read: `P.REG[i].FIELD` -> volatile
                // load at base+offset+stride*i, then mask/shift the field out.
                if let Some((reg_base, stride, idx_expr)) = self.indexed_array_reg(base, symbols)
                    && let Expr::Index(arr, _) = base.as_ref()
                    && let Expr::FieldAccess(p, reg) = arr.as_ref()
                    && let Expr::Ident((pname, _)) = p.as_ref()
                    && let Some(field_def) = symbols
                        .peripherals
                        .get(&self.subst_periph(pname))
                        .and_then(|pp| pp.regs.get(&reg.0))
                        .and_then(|rr| rr.fields.get(&field.0))
                {
                    let bit_spec = field_def.bit_spec.clone();
                    let ty = field_def.ty.clone();
                    let dbg = self.dbg_loc(expr.span());
                    let ptr = self.emit_reg_index_ptr(reg_base, stride, idx_expr, symbols, fn_name);
                    return self.emit_field_read_at_ptr(&ptr, &bit_spec, &ty, &dbg);
                }
                // Handle peripheral register access: GPIOA.ODR → volatile load
                if let Expr::Ident((periph_name, _)) = base.as_ref()
                    && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
                    && let Some(reg) = p.regs.get(&field.0)
                {
                    let addr = p.base_addr + reg.offset;
                    let reg_name = self.new_reg();
                    self.line(&format!(
                        "{reg_name} = load volatile i32, ptr inttoptr ({ptr_ty} {addr} to ptr)",
                        ptr_ty = self.ptr_type()
                    ));
                    return reg_name;
                }
                // Handle peripheral field read: GPIOA.ODR.ODR3 → volatile load + bit extract
                if let Expr::FieldAccess(inner, reg_field) = base.as_ref()
                    && let Expr::Ident((periph_name, _)) = inner.as_ref()
                    && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
                    && let Some(reg) = p.regs.get(&reg_field.0)
                    && let Some(field_def) = reg.fields.get(&field.0)
                {
                    let addr = p.base_addr + reg.offset;
                    // Bit-band: single-bit field within bit-band region
                    if self.has_bitband
                        && let Some(alias) =
                            crate::arch::arm::bitband_alias(addr, &field_def.bit_spec)
                    {
                        let val_reg = self.new_reg();
                        self.line(&format!(
                            "{val_reg} = load volatile i32, ptr inttoptr ({ptr_ty} {alias} to ptr)",
                            ptr_ty = self.arch.ptr_type()
                        ));
                        return self.narrow_from_i32(&val_reg, &field_def.ty);
                    }
                    // Fallback RMW read
                    let val_reg = self.new_reg();
                    self.line(&format!(
                        "{val_reg} = load volatile i32, ptr inttoptr ({ptr_ty} {addr} to ptr)",
                        ptr_ty = self.arch.ptr_type()
                    ));
                    let (mask, shift) = crate::arch::arm::bit_mask_shift(&field_def.bit_spec);
                    let masked = self.new_reg();
                    self.line(&format!("{masked} = and i32 {val_reg}, {mask}"));
                    let result = self.new_reg();
                    if shift > 0 {
                        self.line(&format!("{result} = lshr i32 {masked}, {shift}"));
                    } else {
                        self.line(&format!("{result} = add i32 {masked}, 0"));
                    }
                    return self.narrow_from_i32(&result, &field_def.ty);
                }
                // Struct field access: extractvalue from loaded struct
                let base_ty = self.expr_type(base, symbols);
                if let Type::Struct(struct_name, _, fields) = &base_ty
                    && let Some(idx) = fields.iter().position(|(n, _)| n == &field.0)
                {
                    let field_ty = fields[idx].1.clone();
                    let endian = Self::field_endian(struct_name, idx, symbols);
                    let base_reg = self.emit_expr(base, symbols, fn_name);
                    let struct_llvm_ty = llvm_type(&base_ty);
                    let reg = self.new_reg();
                    self.line(&format!(
                        "{reg} = extractvalue {struct_llvm_ty} {base_reg}, {idx}"
                    ));
                    // A `@be` field's raw bits are byte-swapped; decode to native.
                    return self.maybe_bswap(reg, &field_ty, endian);
                }
                // Pointer to struct field access: GEP + load
                if let Type::Ptr(inner) | Type::ConstPtr(inner) = &base_ty
                    && let Type::Struct(struct_name, repr, fields) = inner.as_ref()
                    && let Some(idx) = fields.iter().position(|(n, _)| n == &field.0)
                {
                    let endian = Self::field_endian(struct_name, idx, symbols);
                    let base_ptr = self.emit_expr(base, symbols, fn_name);
                    let struct_llvm_ty = llvm_type(inner);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {struct_llvm_ty}, ptr {base_ptr}, i32 0, i32 {idx}"
                    ));
                    let field_ty = &fields[idx].1;
                    let ll_field = llvm_type(field_ty);
                    let reg = self.new_reg();
                    let align = if *repr == ast::StructRepr::Packed {
                        ", align 1"
                    } else {
                        ""
                    };
                    self.line(&format!("{reg} = load {ll_field}, ptr {gep}{align}"));
                    return self.maybe_bswap(reg, field_ty, endian);
                }
                // Fallback: struct field access via GEP
                self.emit_expr(base, symbols, fn_name);
                let reg = self.new_reg();
                self.line(&format!("{reg} = add i32 0, 0  ; field: {}", field.0));
                reg
            }

            Expr::Index(base, index) => {
                let dbg = self.dbg_loc(expr.span());
                // Indexed register read: `P.REG[i]` -> volatile load i32 at
                // `base + offset + stride*i` (stays on the MMIO path).
                if let Expr::FieldAccess(p, reg) = base.as_ref()
                    && let Expr::Ident((pname, _)) = p.as_ref()
                    && let Some((reg_base, stride)) = self.array_reg_addr(pname, &reg.0, symbols)
                {
                    let ptr = self.emit_reg_index_ptr(reg_base, stride, index, symbols, fn_name);
                    let out = self.new_reg();
                    self.line(&format!("{out} = load volatile i32, ptr {ptr}{dbg}"));
                    return out;
                }
                let base_ty = self.expr_type(base, symbols);
                if let Type::LinearView(elem_ty, _) = &base_ty {
                    // Read a linear view: pull { ptr, len } out of the
                    // descriptor, assume the index is in range so the verifier
                    // can prove the access, then typed GEP + load.
                    let agg = self.emit_expr(base, symbols, fn_name);
                    let (ptr_field, idx_i32) =
                        self.view_ptr_len_checked(&agg, index, symbols, fn_name);
                    let ll_elem = llvm_type(elem_ty);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {ptr_field}, i32 {idx_i32}{dbg}"
                    ));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ll_elem}, ptr {gep}{dbg}"));
                    reg
                } else if let Type::StridedView(elem_ty, _, k) = &base_ty {
                    // Read a strided view: same { ptr, len } descriptor as the
                    // linear view, but the backing index is `i * K` (K the
                    // compile-time element stride). The multiply by a constant
                    // keeps the GEP typed, so the verifier bounds it just like
                    // the contiguous case. assume(i < len) bounds the logical i.
                    let agg = self.emit_expr(base, symbols, fn_name);
                    let (ptr_field, idx_i32) =
                        self.view_ptr_len_checked(&agg, index, symbols, fn_name);
                    let scaled = self.new_reg();
                    self.line(&format!("{scaled} = mul i32 {idx_i32}, {k}"));
                    let ll_elem = llvm_type(elem_ty);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {ptr_field}, i32 {scaled}{dbg}"
                    ));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ll_elem}, ptr {gep}{dbg}"));
                    reg
                } else if let Type::RingView(elem_ty, _, cap_hint) = &base_ty {
                    // Read a ring view: physical = (head + i) % capacity. The
                    // urem bounds physical to [0, capacity); with a constant
                    // capacity tracing to the backing array, the verifier proves
                    // the typed GEP in range. (Array form: capacity is constant,
                    // so no division-by-zero either.) When the capacity is a
                    // compile-time power of two we instead mask with the constant
                    // `(cap - 1)`, which is cheaper than urem and bounds physical
                    // to [0, cap) trivially for IKOS.
                    let agg = self.emit_expr(base, symbols, fn_name);
                    let ty = "{ ptr, i32, i32, i32 }";
                    let (ptr_field, phys) =
                        self.view_ring_addr(&agg, ty, *cap_hint, index, symbols, fn_name);
                    let ll_elem = llvm_type(elem_ty);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {ptr_field}, i32 {phys}{dbg}"
                    ));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ll_elem}, ptr {gep}{dbg}"));
                    reg
                } else if let Type::BitView(_) = &base_ty {
                    // Read a bit view: assume(i < len_bits), then byte =
                    // (bit_offset + i) / 8, load that byte, extract the bit. The
                    // assume bounds the byte access so the verifier proves it.
                    let agg = self.emit_expr(base, symbols, fn_name);
                    let ty = "{ ptr, i32, i32 }";
                    let ptr_field = self.new_reg();
                    self.line(&format!("{ptr_field} = extractvalue {ty} {agg}, 0"));
                    let off_field = self.new_reg();
                    self.line(&format!("{off_field} = extractvalue {ty} {agg}, 1"));
                    let len_field = self.new_reg();
                    self.line(&format!("{len_field} = extractvalue {ty} {agg}, 2"));
                    let idx_reg = self.emit_expr(index, symbols, fn_name);
                    let idx_ty = self.expr_type(index, symbols);
                    let idx_i32 = self.coerce_int(idx_reg, &idx_ty, &Type::U32);
                    // assume(idx < len_bits), unsigned.
                    let cond = self.new_reg();
                    self.line(&format!("{cond} = icmp ult i32 {idx_i32}, {len_field}"));
                    let ok_lbl = self.new_label("bit_idx_ok");
                    let oob_lbl = self.new_label("bit_idx_oob");
                    self.line(&format!("br i1 {cond}, label %{ok_lbl}, label %{oob_lbl}"));
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{oob_lbl}:"));
                    self.indent += 1;
                    self.line("unreachable");
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{ok_lbl}:"));
                    self.indent += 1;
                    let bit = self.new_reg();
                    self.line(&format!("{bit} = add i32 {off_field}, {idx_i32}"));
                    if self.verify_mode {
                        self.generated_wrap_spans.push(index.span());
                    }
                    let byteidx = self.new_reg();
                    self.line(&format!("{byteidx} = lshr i32 {bit}, 3"));
                    let bib = self.new_reg();
                    self.line(&format!("{bib} = and i32 {bit}, 7"));
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr i8, ptr {ptr_field}, i32 {byteidx}{dbg}"
                    ));
                    let byte = self.new_reg();
                    self.line(&format!("{byte} = load i8, ptr {gep}{dbg}"));
                    let bib8 = self.new_reg();
                    self.line(&format!("{bib8} = trunc i32 {bib} to i8"));
                    let shifted = self.new_reg();
                    self.line(&format!("{shifted} = lshr i8 {byte}, {bib8}"));
                    let masked = self.new_reg();
                    self.line(&format!("{masked} = and i8 {shifted}, 1"));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = trunc i8 {masked} to i1"));
                    reg
                } else if crate::types::is_ptr(&base_ty) {
                    // Pointer index: GEP + load (volatile when the base is an
                    // agent pointer -- see vol_expr).
                    let vol = self.vol_expr(base, symbols);
                    let base_reg = self.emit_expr(base, symbols, fn_name);
                    let idx_reg = self.emit_expr(index, symbols, fn_name);
                    let idx_ty = self.expr_type(index, symbols);
                    let pointee_ty = match &base_ty {
                        Type::Ptr(t) | Type::ConstPtr(t) => t.as_ref(),
                        _ => &crate::types::Type::I32,
                    };
                    let ll_elem = llvm_type(pointee_ty);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {base_reg}, {} {idx_reg}{dbg}",
                        llvm_type(&idx_ty)
                    ));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load{vol} {ll_elem}, ptr {gep}{dbg}"));
                    reg
                } else if matches!(&base_ty, Type::Array(_, _)) {
                    // Array value: get lvalue pointer, GEP, load
                    let base_ptr = self.emit_lvalue_ptr(base, symbols);
                    let idx_reg = self.emit_expr(index, symbols, fn_name);
                    let idx_ty = self.expr_type(index, symbols);
                    let elem_ty = match &base_ty {
                        Type::Array(inner, _) => inner.as_ref(),
                        _ => &crate::types::Type::U32,
                    };
                    let ll_elem = llvm_type(elem_ty);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {base_ptr}, {} {idx_reg}{dbg}",
                        llvm_type(&idx_ty)
                    ));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ll_elem}, ptr {gep}{dbg}"));
                    reg
                } else {
                    // Fallback
                    self.emit_expr(base, symbols, fn_name);
                    self.emit_expr(index, symbols, fn_name);
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = add i32 0, 0  ; index"));
                    reg
                }
            }

            Expr::Cast(inner, ty_expr) => {
                let inner_reg = self.emit_expr(inner, symbols, fn_name);
                let inner_ty = self.expr_type(inner, symbols);
                let target_ty =
                    crate::types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
                let llvm_target = llvm_type(&target_ty);
                let reg = self.new_reg();
                let inner_llvm = llvm_type(&inner_ty);
                // Enums carry an underlying integer type; cast them as that
                // integer so widening uses zext/sext rather than an invalid
                // same-or-different-width bitcast.
                let inner_num = scalar_repr(&inner_ty);
                let target_num = scalar_repr(&target_ty);
                if crate::types::is_int(&inner_num) && crate::types::is_int(&target_num) {
                    let inner_bits = int_bit_width(&inner_llvm);
                    let target_bits = int_bit_width(&llvm_target);
                    match target_bits.cmp(&inner_bits) {
                        std::cmp::Ordering::Greater => {
                            // Widening -- signed vs unsigned
                            let ext_op = if matches!(
                                inner_num,
                                Type::I8 | Type::I16 | Type::I32 | Type::I64
                            ) {
                                "sext"
                            } else {
                                "zext"
                            };
                            self.line(&format!(
                                "{reg} = {ext_op} {inner_llvm} {inner_reg} to {llvm_target}"
                            ));
                        }
                        std::cmp::Ordering::Less => {
                            self.line(&format!(
                                "{reg} = trunc {inner_llvm} {inner_reg} to {llvm_target}"
                            ));
                        }
                        std::cmp::Ordering::Equal => return inner_reg,
                    }
                } else if matches!(inner_num, Type::B1 | Type::B8)
                    && (crate::types::is_int(&target_num)
                        || matches!(target_num, Type::B1 | Type::B8))
                {
                    // bool → int or bool → bool: a bool is 0 or 1, so adjust the
                    // width by zext/trunc (never sext, never an invalid
                    // same-family bitcast). `int_bit_width` doesn't know the i1
                    // width, so size bools explicitly.
                    let bits = |t: &Type, llvm: &str| match t {
                        Type::B1 => 1u32,
                        Type::B8 => 8,
                        _ => int_bit_width(llvm),
                    };
                    let inner_bits = bits(&inner_num, &inner_llvm);
                    let target_bits = bits(&target_num, &llvm_target);
                    match target_bits.cmp(&inner_bits) {
                        std::cmp::Ordering::Greater => self.line(&format!(
                            "{reg} = zext {inner_llvm} {inner_reg} to {llvm_target}"
                        )),
                        std::cmp::Ordering::Less => self.line(&format!(
                            "{reg} = trunc {inner_llvm} {inner_reg} to {llvm_target}"
                        )),
                        std::cmp::Ordering::Equal => return inner_reg,
                    }
                } else if crate::types::is_float(&inner_ty) && crate::types::is_float(&target_ty) {
                    let inner_bits = float_bit_width(&inner_llvm);
                    let target_bits = float_bit_width(&llvm_target);
                    match target_bits.cmp(&inner_bits) {
                        // same float type is a no-op (a same-width fpext/fptrunc
                        // would be invalid IR)
                        std::cmp::Ordering::Equal => return inner_reg,
                        std::cmp::Ordering::Greater => self.line(&format!(
                            "{reg} = fpext {inner_llvm} {inner_reg} to {llvm_target}"
                        )),
                        std::cmp::Ordering::Less => self.line(&format!(
                            "{reg} = fptrunc {inner_llvm} {inner_reg} to {llvm_target}"
                        )),
                    }
                } else if (crate::types::is_int(&inner_num)
                    || matches!(inner_num, Type::B1 | Type::B8))
                    && crate::types::is_float(&target_ty)
                {
                    // int/bool → float: signed sources use sitofp, unsigned ints
                    // and bools (0/1) use uitofp. (A plain `bitcast` here is
                    // invalid -- the families differ.)
                    let op = if matches!(inner_num, Type::I8 | Type::I16 | Type::I32 | Type::I64) {
                        "sitofp"
                    } else {
                        "uitofp"
                    };
                    self.line(&format!(
                        "{reg} = {op} {inner_llvm} {inner_reg} to {llvm_target}"
                    ));
                } else if crate::types::is_float(&inner_ty) && crate::types::is_int(&target_num) {
                    // float → int: signed targets use fptosi, unsigned fptoui.
                    let op = if matches!(target_num, Type::I8 | Type::I16 | Type::I32 | Type::I64) {
                        "fptosi"
                    } else {
                        "fptoui"
                    };
                    self.line(&format!(
                        "{reg} = {op} {inner_llvm} {inner_reg} to {llvm_target}"
                    ));
                } else if crate::types::is_ptr(&inner_ty) && crate::types::is_int(&target_ty) {
                    // pointer → int
                    self.line(&format!(
                        "{reg} = ptrtoint ptr {inner_reg} to {llvm_target}"
                    ));
                    // Verify mode: if this is `&X as u32` for a region-placed
                    // static, assume the address is within its region's mem
                    // block. The assume MUST sit here (at the address-of site):
                    // the probe showed ptrtoints of the same global do not
                    // unify across functions, so a fact attached elsewhere does
                    // not reach the handoff. From here the range propagates
                    // through calls/returns/arithmetic to the obligation.
                    if self.verify_mode
                        && let Some(name) = addr_of_static_name(inner)
                        && let Some(&(lo, hi)) = self.region_addr_ranges.get(name)
                    {
                        self.emit_range_assume(&reg, lo, hi);
                    }
                } else if crate::types::is_int(&inner_ty) && crate::types::is_ptr(&target_ty) {
                    // int → pointer
                    self.line(&format!("{reg} = inttoptr {inner_llvm} {inner_reg} to ptr"));
                } else {
                    // Cross-family -- bitcast (int↔float, ptr↔ptr, etc.)
                    self.line(&format!(
                        "{reg} = bitcast {inner_llvm} {inner_reg} to {llvm_target}"
                    ));
                }
                reg
            }
            Expr::SizeOf(ty_expr, _span) => {
                let target_ty =
                    crate::types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
                let size = crate::types::element_size(&target_ty);
                let reg = self.new_reg();
                self.line(&format!("{reg} = add i32 0, {size}"));
                reg
            }
            Expr::ViewNew {
                base, len, stride, ..
            } => {
                // Build the { ptr, i32 } descriptor as a first-class aggregate.
                let (ptr_reg, len_i32) = if let Some(stride) = stride {
                    // view(arr, stride K): pointer to element 0, logical length
                    // N/K (the stride lives in the type, not the descriptor).
                    let ptr_reg = self.emit_lvalue_ptr(base, symbols);
                    let n = match self.expr_type(base, symbols).inner().clone() {
                        Type::Array(_, n) => n,
                        _ => 0,
                    };
                    let k = match stride.as_ref() {
                        Expr::IntLiteral(v, _, _) => usize::try_from(*v).unwrap_or(1).max(1),
                        _ => 1,
                    };
                    let len_reg = self.new_reg();
                    self.line(&format!("{len_reg} = add i32 0, {}", n / k));
                    (ptr_reg, len_reg)
                } else if let Some(len) = len {
                    // view(ptr, len): explicit pointer and length.
                    let ptr_reg = self.emit_expr(base, symbols, fn_name);
                    let len_reg = self.emit_expr(len, symbols, fn_name);
                    let len_ty = self.expr_type(len, symbols);
                    (ptr_reg, self.coerce_int(len_reg, &len_ty, &Type::U32))
                } else {
                    // view(arr): pointer to element 0, compile-known length.
                    let ptr_reg = self.emit_lvalue_ptr(base, symbols);
                    // `.inner()` sees through a storage wrapper (`@shared`/`@dma`
                    // /`@external`/`@exclusive`) so a view over a storage-class
                    // array still gets its compile-known length.
                    let n = match self.expr_type(base, symbols).inner().clone() {
                        Type::Array(_, n) => n,
                        _ => 0,
                    };
                    let len_reg = self.new_reg();
                    self.line(&format!("{len_reg} = add i32 0, {n}"));
                    (ptr_reg, len_reg)
                };
                let agg0 = self.new_reg();
                self.line(&format!(
                    "{agg0} = insertvalue {{ ptr, i32 }} undef, ptr {ptr_reg}, 0"
                ));
                let agg1 = self.new_reg();
                self.line(&format!(
                    "{agg1} = insertvalue {{ ptr, i32 }} {agg0}, i32 {len_i32}, 1"
                ));
                agg1
            }
            Expr::RingNew {
                base,
                capacity,
                head,
                len,
                ..
            } => {
                // Build the { ptr, capacity, head, len } descriptor. For the
                // array form the capacity is the compile-known array length
                // emitted as a constant, which lets sroa propagate it so IKOS
                // bounds the `(head+i) % capacity` access.
                let (ptr_reg, cap_i32) = if let Some(capacity) = capacity {
                    let ptr_reg = self.emit_expr(base, symbols, fn_name);
                    let cap_reg = self.emit_expr(capacity, symbols, fn_name);
                    let cap_ty = self.expr_type(capacity, symbols);
                    (ptr_reg, self.coerce_int(cap_reg, &cap_ty, &Type::U32))
                } else {
                    let ptr_reg = self.emit_lvalue_ptr(base, symbols);
                    // `.inner()` sees through a storage wrapper (`@shared`/`@dma`
                    // /`@external`/`@exclusive`) so a view over a storage-class
                    // array still gets its compile-known length.
                    let n = match self.expr_type(base, symbols).inner().clone() {
                        Type::Array(_, n) => n,
                        _ => 0,
                    };
                    let cap_reg = self.new_reg();
                    self.line(&format!("{cap_reg} = add i32 0, {n}"));
                    (ptr_reg, cap_reg)
                };
                let head_reg = self.emit_expr(head, symbols, fn_name);
                let head_ty = self.expr_type(head, symbols);
                let head_i32 = self.coerce_int(head_reg, &head_ty, &Type::U32);
                let len_reg = self.emit_expr(len, symbols, fn_name);
                let len_ty = self.expr_type(len, symbols);
                let len_i32 = self.coerce_int(len_reg, &len_ty, &Type::U32);
                let ty = "{ ptr, i32, i32, i32 }";
                let agg0 = self.new_reg();
                self.line(&format!(
                    "{agg0} = insertvalue {ty} undef, ptr {ptr_reg}, 0"
                ));
                let agg1 = self.new_reg();
                self.line(&format!(
                    "{agg1} = insertvalue {ty} {agg0}, i32 {cap_i32}, 1"
                ));
                let agg2 = self.new_reg();
                self.line(&format!(
                    "{agg2} = insertvalue {ty} {agg1}, i32 {head_i32}, 2"
                ));
                let agg3 = self.new_reg();
                self.line(&format!(
                    "{agg3} = insertvalue {ty} {agg2}, i32 {len_i32}, 3"
                ));
                agg3
            }
            Expr::BitNew {
                base,
                bit_offset,
                len_bits,
                ..
            } => {
                // Build the { ptr, bit_offset, len_bits } descriptor. For the
                // array form bit_offset is 0 and len_bits is the compile-known
                // byte count times 8, emitted as a constant so sroa propagates
                // it and IKOS bounds the `(off+i)/8` byte access.
                let (ptr_reg, off_i32, len_i32) =
                    if let (Some(bit_offset), Some(len_bits)) = (bit_offset, len_bits) {
                        let ptr_reg = self.emit_expr(base, symbols, fn_name);
                        let off_reg = self.emit_expr(bit_offset, symbols, fn_name);
                        let off_ty = self.expr_type(bit_offset, symbols);
                        let off_i32 = self.coerce_int(off_reg, &off_ty, &Type::U32);
                        let len_reg = self.emit_expr(len_bits, symbols, fn_name);
                        let len_ty = self.expr_type(len_bits, symbols);
                        let len_i32 = self.coerce_int(len_reg, &len_ty, &Type::U32);
                        (ptr_reg, off_i32, len_i32)
                    } else {
                        let ptr_reg = self.emit_lvalue_ptr(base, symbols);
                        // `.inner()` sees through a storage wrapper so a bit view
                        // over a storage-class byte array gets its length.
                        let n = match self.expr_type(base, symbols).inner().clone() {
                            Type::Array(_, n) => n,
                            _ => 0,
                        };
                        let off_reg = self.new_reg();
                        self.line(&format!("{off_reg} = add i32 0, 0"));
                        let len_reg = self.new_reg();
                        self.line(&format!("{len_reg} = add i32 0, {}", n * 8));
                        (ptr_reg, off_reg, len_reg)
                    };
                let ty = "{ ptr, i32, i32 }";
                let agg0 = self.new_reg();
                self.line(&format!(
                    "{agg0} = insertvalue {ty} undef, ptr {ptr_reg}, 0"
                ));
                let agg1 = self.new_reg();
                self.line(&format!(
                    "{agg1} = insertvalue {ty} {agg0}, i32 {off_i32}, 1"
                ));
                let agg2 = self.new_reg();
                self.line(&format!(
                    "{agg2} = insertvalue {ty} {agg1}, i32 {len_i32}, 2"
                ));
                agg2
            }
            // Valid `[v; N]` is desugared to an ArrayInit by constfold; a residual
            // ArrayRepeat is rejected by the checker (E348), so codegen never sees one.
            Expr::ArrayRepeat(..) => {
                unreachable!("ArrayRepeat must be desugared by constfold or rejected (E348)")
            }
            Expr::ArrayInit(elems, _) => {
                let elem_ty = elems
                    .first()
                    .map_or(Type::U32, |e| self.expr_type(e, symbols));
                let ll_elem = llvm_type(&elem_ty);
                let len = elems.len();
                let arr_ty = format!("[{len} x {ll_elem}]");
                let tmp = self.new_anon_alloca(&arr_ty);
                for (i, elem) in elems.iter().enumerate() {
                    let elem_reg = self.emit_expr(elem, symbols, fn_name);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {arr_ty}, ptr {tmp}, i32 0, i32 {i}"
                    ));
                    self.line(&format!("store {ll_elem} {elem_reg}, ptr {gep}"));
                }
                let reg = self.new_reg();
                self.line(&format!("{reg} = load {arr_ty}, ptr {tmp}"));
                reg
            }
            Expr::Group(inner) => self.emit_expr(inner, symbols, fn_name),
            Expr::Block(block_expr) => {
                let (_, term) = self.emit_block(&block_expr.block, symbols, fn_name, None, None);
                if term {
                    return default_value_literal(&self.expr_type(expr, symbols));
                }
                if let Some(ref trailing) = block_expr.block.trailing {
                    self.emit_expr(trailing, symbols, fn_name)
                } else {
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = add i32 0, 0  ; empty block"));
                    reg
                }
            }
            Expr::If(if_expr) => {
                let then_lbl = self.new_label("if_then");
                let else_lbl = self.new_label("if_else");
                let end_lbl = self.new_label("if_end");

                self.emit_branch_cond(&if_expr.cond, &then_lbl, &else_lbl, symbols, fn_name);
                self.line("");

                self.indent -= 1;
                self.line(&format!("{then_lbl}:"));
                self.indent += 1;
                let (_, then_term) =
                    self.emit_block(&if_expr.then_block, symbols, fn_name, None, None);
                // Phi type comes from the else branch; if then's trailing is missing
                // (checker should have rejected with E328) we still need a value of
                // the right LLVM type so the phi verifies.
                let phi_bml_ty = self.expr_type(&if_expr.else_branch, symbols);
                let (then_val, then_edge_label) = if then_term {
                    (None, None)
                } else if let Some(ref trailing) = if_expr.then_block.trailing {
                    let value = self.emit_expr(trailing, symbols, fn_name);
                    let label = self
                        .current_label
                        .clone()
                        .unwrap_or_else(|| then_lbl.clone());
                    (Some(value), Some(label))
                } else {
                    let label = self
                        .current_label
                        .clone()
                        .unwrap_or_else(|| then_lbl.clone());
                    (Some(default_value_literal(&phi_bml_ty)), Some(label))
                };
                // When then terminates we skip the join entirely and let the caller
                // continue emitting into the else block; otherwise both arms branch
                // to end_lbl and we phi the results.
                if !then_term {
                    self.line(&format!("br label %{end_lbl}"));
                }
                self.line("");

                self.indent -= 1;
                self.line(&format!("{else_lbl}:"));
                self.indent += 1;
                let else_val = self.emit_expr(&if_expr.else_branch, symbols, fn_name);
                let else_edge_label = self.current_label.clone().unwrap_or(else_lbl);
                if !then_term {
                    self.line(&format!("br label %{end_lbl}"));
                }
                self.line("");

                if then_term {
                    else_val
                } else {
                    self.indent -= 1;
                    self.line(&format!("{end_lbl}:"));
                    self.indent += 1;

                    let result = self.new_reg();
                    let phi_llvm_ty = llvm_type(&phi_bml_ty);
                    let then_val = then_val.expect("then_val is Some whenever then_term is false");
                    let then_edge_label = then_edge_label
                        .expect("then_edge_label is Some whenever then_term is false");
                    self.line(&format!(
                        "{result} = phi {phi_llvm_ty} [ {then_val}, %{then_edge_label} ], [ {else_val}, %{else_edge_label} ]"
                    ));
                    result
                }
            }
            Expr::Match(match_expr) => {
                // `comptime match` expression: emit only the selected arm and
                // yield its value when the scrutinee is const-evaluable. A
                // non-evaluable scrutinee falls through to the runtime match
                // lowering (see the `comptime if` note; doc/comptime.md).
                if match_expr.comptime {
                    let folded = {
                        let consts = self.spec_consts();
                        let env = IrConstEnv {
                            symbols,
                            consts: &consts,
                        };
                        consteval::eval_int(&match_expr.scrutinee, &env)
                    };
                    if let Some(scrut) = folded {
                        let sty = self.expr_type(&match_expr.scrutinee, symbols);
                        if !comptime_value_fits(scrut, &sty) {
                            self.comptime_errors.push((
                                format!("comptime match scrutinee value {scrut} does not fit type `{sty:?}`"),
                                "E411".to_string(),
                                match_expr.scrutinee.span(),
                            ));
                        }
                        if let Some(arm) = comptime_match_arm(scrut, &match_expr.arms, symbols) {
                            // Emit the arm body's statements, then its trailing value
                            // expression (mirrors the `Expr::Block` value path).
                            let (_, term) =
                                self.emit_block(&arm.body, symbols, fn_name, None, None);
                            if term {
                                return default_value_literal(&self.expr_type(expr, symbols));
                            }
                            return if let Some(ref trailing) = arm.body.trailing {
                                self.emit_expr(trailing, symbols, fn_name)
                            } else {
                                // No trailing value (a diverging arm); yield a
                                // correctly-typed default rather than a hardcoded i32 0.
                                default_value_literal(&self.expr_type(expr, symbols))
                            };
                        }
                    } else {
                        self.comptime_errors.push((
                            "comptime match scrutinee does not evaluate to a constant (division by zero or overflow during specialization)".to_string(),
                            "E411".to_string(),
                            match_expr.scrutinee.span(),
                        ));
                    }
                }
                let Some(MatchDispatch {
                    end_lbl,
                    arm_labels,
                    default_lbl,
                    ..
                }) = self.emit_match_dispatch(
                    &match_expr.scrutinee,
                    &match_expr.arms,
                    symbols,
                    fn_name,
                    true,
                )
                else {
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = add i32 0, 0  ; match fallback"));
                    return reg;
                };
                // The phi is over the arm *result* type (the match's value type),
                // not the scrutinee type -- those differ when, e.g., a `u8` is
                // matched into `u32` arms.
                let ll_ty = llvm_type(&self.expr_type(expr, symbols));

                let has_wildcard = match_expr.arms.iter().any(|arm| {
                    arm.patterns
                        .iter()
                        .any(|p| matches!(p, ast::MatchPattern::Wildcard(_)))
                });

                let mut phi_pairs: Vec<(String, String)> = Vec::new();

                if !has_wildcard {
                    self.indent -= 1;
                    self.line(&format!("{default_lbl}:"));
                    self.indent += 1;
                    let undef_reg = self.new_reg();
                    self.line(&format!("{undef_reg} = add {ll_ty} 0, 0  ; unreachable"));
                    phi_pairs.push((undef_reg, default_lbl));
                    self.line(&format!("br label %{end_lbl}"));
                    self.line("");
                }

                for (i, arm) in match_expr.arms.iter().enumerate() {
                    let arm_lbl = arm_labels[i].clone();
                    self.indent -= 1;
                    self.line(&format!("{arm_lbl}:"));
                    self.indent += 1;
                    let (_, arm_term) = self.emit_block(&arm.body, symbols, fn_name, None, None);
                    if arm_term {
                        self.line("");
                        continue;
                    }
                    let arm_val = if let Some(ref trailing) = arm.body.trailing {
                        self.emit_expr(trailing, symbols, fn_name)
                    } else {
                        let reg = self.new_reg();
                        self.line(&format!("{reg} = add {ll_ty} 0, 0  ; no trailing"));
                        reg
                    };
                    let arm_edge_label = self.current_label.clone().unwrap_or(arm_lbl);
                    phi_pairs.push((arm_val, arm_edge_label));
                    if !arm_term {
                        self.line(&format!("br label %{end_lbl}"));
                    }
                    self.line("");
                }

                self.indent -= 1;
                self.line(&format!("{end_lbl}:"));
                self.indent += 1;

                if phi_pairs.is_empty() {
                    return default_value_literal(&self.expr_type(expr, symbols));
                }

                let result = self.new_reg();
                let phi_args: Vec<String> = phi_pairs
                    .iter()
                    .map(|(val, lbl)| format!("[ {val}, %{lbl} ]"))
                    .collect();
                self.line(&format!("{result} = phi {ll_ty} {}", phi_args.join(", ")));
                result
            }
            Expr::StructInit { name, fields, .. } => {
                let struct_name = &name.0;
                let struct_info = symbols.structs.get(struct_name).cloned();
                let (repr, struct_fields) = struct_info
                    .map_or((ast::StructRepr::Explicit, Vec::new()), |info| {
                        (info.repr, info.fields)
                    });
                let struct_llvm_ty = llvm_type(&Type::Struct(
                    struct_name.clone(),
                    repr,
                    struct_fields.clone(),
                ));
                let alloca = self.alloca(&struct_llvm_ty, &format!("struct_{struct_name}"));
                self.line(&format!(
                    "store {struct_llvm_ty} zeroinitializer, ptr {alloca}"
                ));
                // Store each field via GEP
                for (idx, (fname, ftype)) in struct_fields.iter().enumerate() {
                    if let Some((_, init_expr)) = fields.iter().find(|(n, _)| n.0 == *fname) {
                        let init_reg = self.emit_expr(init_expr, symbols, fn_name);
                        let init_ty = self.expr_type(init_expr, symbols);
                        let init_reg = self.coerce_int(init_reg, &init_ty, ftype);
                        // Encode `@be` fields to their stored (byte-swapped) form.
                        let endian = Self::field_endian(struct_name, idx, symbols);
                        let init_reg = self.maybe_bswap(init_reg, ftype, endian);
                        let ll_field = llvm_type(ftype);
                        let gep = self.new_reg();
                        self.line(&format!(
                            "{gep} = getelementptr {struct_llvm_ty}, ptr {alloca}, i32 0, i32 {idx}"
                        ));
                        let align = if repr == ast::StructRepr::Packed {
                            ", align 1"
                        } else {
                            ""
                        };
                        self.line(&format!("store {ll_field} {init_reg}, ptr {gep}{align}"));
                    }
                }
                // Load the whole struct and return
                let reg = self.new_reg();
                self.line(&format!("{reg} = load {struct_llvm_ty}, ptr {alloca}"));
                reg
            }

            Expr::EnumVariant {
                enum_name: (name, _),
                variant: (vname, _),
                ..
            } => {
                if let Some((inner_ty, variants)) = symbols.enums.get(name)
                    && let Some((_, disc)) = variants.iter().find(|(n, _)| n == vname)
                {
                    let ll_ty = llvm_type(inner_ty);
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = add {ll_ty} 0, {disc}"));
                    return reg;
                }
                let reg = self.new_reg();
                self.line(&format!("{reg} = add i32 0, 0  ; enum: {name}@{vname}"));
                reg
            }
        }
    }

    fn emit_len_builtin(
        &mut self,
        args: &[Expr],
        symbols: &SymbolTable,
        fn_name: &str,
        span: Span,
    ) -> String {
        let Some(arg) = args.first() else {
            let reg = self.new_reg();
            self.line(&format!("{reg} = add i32 0, 0"));
            return reg;
        };
        if args.len() != 1 {
            let reg = self.new_reg();
            self.line(&format!("{reg} = add i32 0, 0"));
            return reg;
        }

        let arg_ty = self.expr_type(arg, symbols);
        match arg_ty.inner() {
            Type::Array(_, n) => {
                let reg = self.new_reg();
                self.line(&format!("{reg} = add i32 0, {n}"));
                reg
            }
            Type::LinearView(..) | Type::StridedView(..) => {
                let agg = self.emit_expr(arg, symbols, fn_name);
                let reg = self.new_reg();
                self.line(&format!("{reg} = extractvalue {{ ptr, i32 }} {agg}, 1"));
                reg
            }
            Type::RingView(..) => {
                let agg = self.emit_expr(arg, symbols, fn_name);
                let ty = llvm_type(arg_ty.inner());
                let reg = self.new_reg();
                self.line(&format!("{reg} = extractvalue {ty} {agg}, 3"));
                reg
            }
            Type::BitView(_) => {
                let agg = self.emit_expr(arg, symbols, fn_name);
                let ty = llvm_type(arg_ty.inner());
                let reg = self.new_reg();
                self.line(&format!("{reg} = extractvalue {ty} {agg}, 2"));
                reg
            }
            _ => {
                let dbg = self.dbg_loc(span);
                let reg = self.new_reg();
                self.line(&format!("{reg} = add i32 0, 0{dbg} ; invalid len fallback"));
                reg
            }
        }
    }

    /// Lower `target OP= value` as a single-evaluation read-modify-write.
    ///
    /// A peripheral-field target reads its register **once** (volatile),
    /// modifies the field, and writes once -- avoiding the second volatile read
    /// the `target = target OP value` desugar would do (which matters for
    /// read-sensitive registers). Every other target falls back to that desugar:
    /// for non-volatile memory places LLVM's GVN folds the duplicated address,
    /// so the only residual cost is a side-effecting index being evaluated twice.
    fn emit_compound_assign(
        &mut self,
        ca: &ast::CompoundAssignStmt,
        symbols: &SymbolTable,
        fn_name: &str,
    ) {
        // Peripheral field: P.REG.FIELD OP= value
        if let ast::LValue::Field(base, field) = &ca.target
            && let ast::LValue::Field(inner, reg_name) = base.as_ref()
            && let ast::LValue::Name((periph_name, _)) = inner.as_ref()
            && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
            && let Some(reg) = p.regs.get(&reg_name.0)
            && let Some(field_def) = reg.fields.get(&field.0)
        {
            let addr = p.base_addr + reg.offset;
            let (mask, shift) = crate::arch::arm::bit_mask_shift(&field_def.bit_spec);
            let inv_mask = !mask;
            let field_ty = field_def.ty.clone();
            let ptr_ty = self.ptr_type().to_string();

            // One volatile read of the whole register.
            let old = self.new_reg();
            self.line(&format!(
                "{old} = load volatile i32, ptr inttoptr ({ptr_ty} {addr} to ptr)"
            ));
            // Current field value: (old & mask) >> shift.
            let masked_old = self.new_reg();
            self.line(&format!("{masked_old} = and i32 {old}, {mask}"));
            let field_old = self.new_reg();
            if shift > 0 {
                self.line(&format!("{field_old} = lshr i32 {masked_old}, {shift}"));
            } else {
                self.line(&format!("{field_old} = add i32 {masked_old}, 0"));
            }

            // Apply the operator (fields are unsigned).
            let rhs_ty = self.expr_type(&ca.value, symbols);
            let rhs_reg = self.emit_expr(&ca.value, symbols, fn_name);
            let rhs_i32 = self.widen_to_i32(&rhs_reg, &rhs_ty, &field_ty);
            let new_field = self.new_reg();
            self.line(&format!(
                "{new_field} = {} i32 {field_old}, {rhs_i32}",
                compound_unsigned_opcode(ca.op)
            ));

            // Insert the new field into the loaded register and write once.
            let cleared = self.new_reg();
            self.line(&format!("{cleared} = and i32 {old}, {inv_mask}"));
            let shifted = self.new_reg();
            if shift > 0 {
                self.line(&format!("{shifted} = shl i32 {new_field}, {shift}"));
            } else {
                self.line(&format!("{shifted} = add i32 {new_field}, 0"));
            }
            let masked_new = self.new_reg();
            self.line(&format!("{masked_new} = and i32 {shifted}, {mask}"));
            let result = self.new_reg();
            self.line(&format!("{result} = or i32 {cleared}, {masked_new}"));
            self.line(&format!(
                "store volatile i32 {result}, ptr inttoptr ({ptr_ty} {addr} to ptr)"
            ));
            return;
        }

        // General case: behaves exactly like `target = target OP value`.
        let value_expr = Expr::Binary(
            Box::new(ca.target.to_expr()),
            ca.op,
            Box::new(ca.value.clone()),
        );
        let val_reg = self.emit_expr(&value_expr, symbols, fn_name);
        let val_ty = self.expr_type(&value_expr, symbols);
        self.emit_store_target(
            &ca.target,
            symbols,
            fn_name,
            &val_reg,
            &val_ty,
            ca.target.span(),
            None,
        );
    }

    /// Lower an `asm` block with explicit GCC/LLVM-style operands:
    /// `asm { "...$0..." } : outputs : inputs : clobbers`. Outputs are returned
    /// by the asm call (a single value, or a struct for 2+) and stored into
    /// their lvalues; inputs are passed as call arguments.
    fn emit_asm_operands(
        &mut self,
        asm: &ast::AsmStmt,
        escaped: &str,
        symbols: &SymbolTable,
        fn_name: &str,
    ) {
        // Inputs: evaluate each to an SSA value (owned, so no borrow is held
        // across the later emits).
        let inputs: Vec<(String, String, String)> = asm
            .inputs
            .iter()
            .map(|(constraint, expr)| {
                let reg = self.emit_expr(expr, symbols, fn_name);
                let llvm = llvm_type(&self.expr_type(expr, symbols));
                (llvm, reg, constraint.clone())
            })
            .collect();

        // Output types, in declaration order.
        let out_tys: Vec<(String, Type)> = asm
            .outputs
            .iter()
            .map(|(_, expr)| {
                let ty = self.expr_type(expr, symbols);
                (llvm_type(&ty), ty)
            })
            .collect();

        // Constraint string: outputs, then inputs, then clobbers.
        let mut cons: Vec<String> = Vec::new();
        for (c, _) in &asm.outputs {
            cons.push(c.clone());
        }
        for (_, _, c) in &inputs {
            cons.push(c.clone());
        }
        for cl in &asm.clobbers {
            cons.push(format!("~{{{cl}}}"));
        }
        let cons_str = cons.join(",");
        let args_str = inputs
            .iter()
            .map(|(llvm, reg, _)| format!("{llvm} {reg}"))
            .collect::<Vec<_>>()
            .join(", ");

        match out_tys.len() {
            0 => {
                self.line(&format!(
                    "call void asm sideeffect \"{escaped}\", \"{cons_str}\"({args_str})"
                ));
            }
            1 => {
                let (llvm, ty) = &out_tys[0];
                let ret = self.new_reg();
                self.line(&format!(
                    "{ret} = call {llvm} asm sideeffect \"{escaped}\", \"{cons_str}\"({args_str})"
                ));
                let target = &asm.outputs[0].1;
                if let Some(lv) = crate::parser::expr_to_lvalue(target.clone()) {
                    self.emit_store_target(&lv, symbols, fn_name, &ret, ty, target.span(), None);
                }
            }
            _ => {
                let struct_ty = format!(
                    "{{ {} }}",
                    out_tys
                        .iter()
                        .map(|(l, _)| l.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let ret = self.new_reg();
                self.line(&format!(
                    "{ret} = call {struct_ty} asm sideeffect \"{escaped}\", \"{cons_str}\"({args_str})"
                ));
                for (i, (_, ty)) in out_tys.iter().enumerate() {
                    let ev = self.new_reg();
                    self.line(&format!("{ev} = extractvalue {struct_ty} {ret}, {i}"));
                    let target = &asm.outputs[i].1;
                    if let Some(lv) = crate::parser::expr_to_lvalue(target.clone()) {
                        self.emit_store_target(&lv, symbols, fn_name, &ev, ty, target.span(), None);
                    }
                }
            }
        }
    }

    /// Return a pointer to an expression without loading its value.
    /// Used by AddrOf/AddrOfMut. Returns SSA register holding a ptr.
    fn emit_lvalue_ptr(&mut self, expr: &Expr, symbols: &SymbolTable) -> String {
        match expr {
            Expr::Ident((name, _)) => {
                // Local variable: the alloca name *is* the pointer. Routing
                // it through `getelementptr i8, ptr X, i32 0` was a relic of
                // typed-pointer LLVM; with opaque pointers it just strips
                // alignment info the alloca carries (sroa adds align N),
                // which made IKOS report spurious V150 unaligned-pointer
                // warnings on every array index.
                if let Some(info) = self.locals.get(name).cloned() {
                    return info.alloca;
                }
                if symbols.statics.contains_key(name) {
                    return format!("@{name}");
                }
                if symbols.consts.contains_key(name) {
                    return format!("@{name}");
                }
                if let Some(p) = symbols.peripherals.get(name) {
                    let reg = self.new_reg();
                    let ptr_ty = self.ptr_type();
                    self.line(&format!("{reg} = inttoptr {ptr_ty} {} to ptr", p.base_addr));
                    return reg;
                }
                if symbols.functions.contains_key(name) {
                    return format!("@{name}");
                }
                let reg = self.new_reg();
                self.line(&format!(
                    "{reg} = getelementptr i8, ptr null, i32 0  ; AddrOf unknown: {name}"
                ));
                reg
            }
            Expr::Index(base, index) => {
                // Address-of an indexed array register: &P.REG[i] -> the MMIO
                // pointer base+offset+stride*i (correct stride, unlike the
                // generic GEP below which assumes element-size stride).
                if let Some((reg_base, stride, idx_expr)) = self.indexed_array_reg(expr, symbols) {
                    return self.emit_reg_index_ptr(reg_base, stride, idx_expr, symbols, "");
                }
                let base_ptr = self.emit_lvalue_ptr(base, symbols);
                let idx_reg = self.emit_expr(index, symbols, "");
                let idx_ty = self.expr_type(index, symbols);
                let base_ty = self.expr_type(base, symbols);
                let elem_ty = match base_ty {
                    Type::Array(inner, _) | Type::Ptr(inner) | Type::ConstPtr(inner) => *inner,
                    _ => crate::types::Type::I32,
                };
                let ll_elem = llvm_type(&elem_ty);
                let reg = self.new_reg();
                self.line(&format!(
                    "{reg} = getelementptr {ll_elem}, ptr {base_ptr}, {} {idx_reg}",
                    llvm_type(&idx_ty)
                ));
                reg
            }
            Expr::FieldAccess(base, field) => {
                // Peripheral register address-of: &GPIOA.ODR
                if let Expr::Ident((periph_name, _)) = base.as_ref()
                    && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
                    && let Some(reg) = p.regs.get(&field.0)
                {
                    let addr = p.base_addr + reg.offset;
                    let reg_name = self.new_reg();
                    let ptr_ty = self.ptr_type();
                    self.line(&format!("{reg_name} = inttoptr {ptr_ty} {addr} to ptr"));
                    return reg_name;
                }
                // Get pointer to the base struct, then GEP to the field
                let base_ptr = self.emit_lvalue_ptr(base, symbols);
                let base_ty = self.expr_type(base, symbols);
                if let Type::Struct(_, _, fields) = &base_ty
                    && let Some(idx) = fields.iter().position(|(n, _)| n == &field.0)
                {
                    let struct_llvm_ty = llvm_type(&base_ty);
                    let reg = self.new_reg();
                    self.line(&format!(
                        "{reg} = getelementptr {struct_llvm_ty}, ptr {base_ptr}, i32 0, i32 {idx}"
                    ));
                    return reg;
                }
                // Fallback
                let reg = self.new_reg();
                self.line(&format!(
                    "{reg} = getelementptr i8, ptr null, i32 0  ; field addr: {}",
                    field.0
                ));
                reg
            }
            _ => {
                // For other expressions (like deref), just emit the value
                self.emit_expr(expr, symbols, "")
            }
        }
    }

    /// Return (`pointer_ssa`, `element_type`) for an `LValue`.
    /// For `Name` → alloca pointer, `Field` → GEP into base, `Deref` → loaded pointer.
    fn lvalue_base_info(
        &mut self,
        lval: &LValue,
        symbols: &SymbolTable,
        fn_name: &str,
    ) -> Option<(String, Type)> {
        match lval {
            LValue::Name((name, _)) => {
                if let Some(info) = self.locals.get(name).cloned() {
                    let reg = self.new_reg();
                    self.line(&format!(
                        "{reg} = getelementptr i8, ptr {}, i32 0",
                        info.alloca
                    ));
                    return Some((reg, info.bml_type));
                }
                if let Some(sym) = symbols.statics.get(name) {
                    let ty = sym.ty.inner().clone();
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = getelementptr i8, ptr @{name}, i32 0"));
                    return Some((reg, ty));
                }
                None
            }
            LValue::Field(base, field) => {
                let (base_ptr, base_ty) = self.lvalue_base_info(base, symbols, fn_name)?;
                if let Type::Struct(_, _, fields) = &base_ty {
                    let idx = fields.iter().position(|(n, _)| n == &field.0)?;
                    let field_ty = fields[idx].1.clone();
                    let struct_llvm_ty = llvm_type(&base_ty);
                    let reg = self.new_reg();
                    self.line(&format!(
                        "{reg} = getelementptr {struct_llvm_ty}, ptr {base_ptr}, i32 0, i32 {idx}"
                    ));
                    Some((reg, field_ty))
                } else {
                    None
                }
            }
            LValue::Deref(inner) => {
                let ptr_reg = self.emit_expr(inner, symbols, fn_name);
                let inner_ty = self.expr_type(inner, symbols);
                let pointee_ty = match &inner_ty {
                    Type::Ptr(t) | Type::ConstPtr(t) => t.as_ref().clone(),
                    _ => return None,
                };
                Some((ptr_reg, pointee_ty))
            }
            // `arr[i]` / `p[i]` as a store base (e.g. `RX[i].buf1 = ...`).
            // Previously `None`, which silently dropped the enclosing
            // field/index store. The element GEP mirrors the read side
            // (`emit_lvalue_ptr` Index) so reads and writes hit the same slot.
            LValue::Index(base, index) => {
                let (base_ptr, base_ty) = self.lvalue_base_info(base, symbols, fn_name)?;
                let (data_ptr, elem_ty) = match &base_ty {
                    Type::Array(inner, _) => (base_ptr, inner.as_ref().clone()),
                    // A pointer base addresses the pointer's storage; load the
                    // data pointer before indexing.
                    Type::Ptr(inner) | Type::ConstPtr(inner) => {
                        let loaded = self.new_reg();
                        self.line(&format!("{loaded} = load ptr, ptr {base_ptr}"));
                        (loaded, inner.as_ref().clone())
                    }
                    _ => return None,
                };
                let idx_reg = self.emit_expr(index, symbols, fn_name);
                let idx_ty = self.expr_type(index, symbols);
                let ll_elem = llvm_type(&elem_ty);
                let reg = self.new_reg();
                self.line(&format!(
                    "{reg} = getelementptr {ll_elem}, ptr {data_ptr}, {} {idx_reg}",
                    llvm_type(&idx_ty)
                ));
                Some((reg, elem_ty))
            }
        }
    }

    /// Emit a store to an lvalue. Returns the register holding the stored
    /// value. `value_expr` is the assignment's RHS when syntactically
    /// available -- the extent obligation reads the delivered static
    /// (`= &X as u32`) from it; `None` (compound assigns, internal callers)
    /// just means an unknown delivery.
    #[allow(clippy::too_many_arguments)]
    fn emit_store_target(
        &mut self,
        lval: &LValue,
        symbols: &SymbolTable,
        fn_name: &str,
        val_reg: &str,
        val_ty: &Type,
        dbg_span: Span,
        value_expr: Option<&ast::Expr>,
    ) -> String {
        let dbg = self.dbg_loc(dbg_span);
        match lval {
            LValue::Name((name, _)) => {
                // Local variable
                if let Some(info) = self.locals.get(name) {
                    let target_ty = info.bml_type.clone();
                    let llvm_ty = info.llvm_ty.clone();
                    let alloca = info.alloca.clone();
                    let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &target_ty);
                    self.line(&format!("store {llvm_ty} {val_reg}, ptr {alloca}{dbg}"));
                    return val_reg;
                }
                // Static
                if let Some(sym) = symbols.statics.get(name) {
                    let target_ty = sym.ty.inner().clone();
                    let ty = llvm_type(&target_ty);
                    let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &target_ty);
                    let cs = self
                        .critical_section_ceiling(name, symbols)
                        .map(|ceiling| crate::arch::arm::emit_critical_enter(self, Some(ceiling)));
                    self.line(&format!("store {ty} {val_reg}, ptr @{name}{dbg}"));
                    if let Some(token) = cs {
                        crate::arch::arm::emit_critical_leave(self, token);
                    }
                    return val_reg;
                }
                val_reg.to_string()
            }
            LValue::Field(base, field) => {
                // Indexed array-register field write: `P.REG[i].FIELD = v` ->
                // read-modify-write at base+offset+stride*i.
                if let LValue::Index(arr, idx_expr) = base.as_ref()
                    && let LValue::Field(p, reg) = arr.as_ref()
                    && let LValue::Name((pname, _)) = p.as_ref()
                    && let Some((reg_base, stride)) = self.array_reg_addr(pname, &reg.0, symbols)
                    && let Some(field_def) = symbols
                        .peripherals
                        .get(&self.subst_periph(pname))
                        .and_then(|pp| pp.regs.get(&reg.0))
                        .and_then(|rr| rr.fields.get(&field.0))
                {
                    let bit_spec = field_def.bit_spec.clone();
                    let field_ty = field_def.ty.clone();
                    let ptr = self.emit_reg_index_ptr(reg_base, stride, idx_expr, symbols, fn_name);
                    return self
                        .emit_field_rmw_at_ptr(&ptr, &bit_spec, &field_ty, val_reg, val_ty, &dbg);
                }
                // Peripheral register write: GPIOA.ODR = val
                if let LValue::Name((periph_name, _)) = base.as_ref()
                    && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
                    && let Some(reg) = p.regs.get(&field.0)
                {
                    let addr = p.base_addr + reg.offset;
                    // Inside a monomorphized driver `periph_name` is the handle
                    // parameter; resolve it to the concrete instance so the
                    // handoff/extent/gate obligations (and the post-write fence)
                    // key on the real peripheral, not `u` (slice 2).
                    let pname = self.subst_periph(periph_name);
                    // Verify mode: a write to a handoff register asserts the
                    // stored byte address lies within the owning agent's
                    // reachable memory. IKOS discharges it from the provenance
                    // assume at `&X as u32`. The full address is stored verbatim
                    // -- the register's reserved low bits are ignored by
                    // hardware, so no encoding/shift is applied.
                    if self.verify_mode
                        && let Some(&(lo, hi)) = self
                            .handoff_reach_bounds
                            .get(&format!("{pname}.{}", field.0))
                    {
                        self.emit_range_assert(val_reg, lo, hi, &dbg);
                    }
                    // Extent obligation, delivery side: remember how big the
                    // buffer handed to this handoff register is. A direct
                    // `= &X as u32` delivery has a compile-time size; anything
                    // else resets the shadow to unconstrained (u32::MAX).
                    if self.verify_mode
                        && let Some(shadow) = self
                            .extent_cap_shadows
                            .get(&(pname.clone(), field.0.clone()))
                    {
                        let cap = value_expr
                            .and_then(delivered_static_name)
                            .and_then(|n| symbols.statics.get(n))
                            .map_or(-1i64, |sym| {
                                i64::from(crate::types::element_size(strip_storage(&sym.ty)))
                            });
                        let line = format!("store i32 {cap}, ptr @{shadow}");
                        self.line(&line);
                    }
                    self.line(&format!(
                        "store volatile i32 {val_reg}, ptr inttoptr ({ptr_ty} {addr} to ptr){dbg}",
                        ptr_ty = self.ptr_type()
                    ));
                    self.emit_handoff_completion(&pname, &field.0);
                    self.emit_gate_readback(&pname, &field.0, addr);
                    return val_reg.to_string();
                }
                // Peripheral field write: GPIOA.ODR.ODR3 = val
                if let LValue::Field(inner_base, reg_field) = base.as_ref()
                    && let LValue::Name((periph_name, _)) = inner_base.as_ref()
                    && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
                    && let Some(reg) = p.regs.get(&reg_field.0)
                    && let Some(field_def) = reg.fields.get(&field.0)
                {
                    let addr = p.base_addr + reg.offset;
                    // Resolve a handle parameter to its concrete instance for the
                    // obligation/fence keying (slice 2; see the register-write path).
                    let pname = self.subst_periph(periph_name);
                    // Bit-band: single-bit field within bit-band region.
                    if self.has_bitband
                        && let Some(alias) =
                            crate::arch::arm::bitband_alias(addr, &field_def.bit_spec)
                    {
                        let alias_val = self.widen_to_i32(val_reg, val_ty, &field_def.ty);
                        self.line(&format!(
                            "store volatile i32 {alias_val}, ptr inttoptr ({ptr_ty} {alias} to ptr){dbg}",
                            ptr_ty = self.arch.ptr_type()
                        ));
                        self.emit_gate_readback(&pname, &reg_field.0, addr);
                        return alias_val;
                    }
                    // Fallback RMW write
                    let (mask, shift) = crate::arch::arm::bit_mask_shift(&field_def.bit_spec);
                    let inv_mask = !mask;
                    // volatile load
                    let old = self.new_reg();
                    self.line(&format!(
                        "{old} = load volatile i32, ptr inttoptr ({ptr_ty} {addr} to ptr)",
                        ptr_ty = self.ptr_type()
                    ));
                    // clear field bits
                    let cleared = self.new_reg();
                    self.line(&format!("{cleared} = and i32 {old}, {inv_mask}"));
                    // widen narrow value to i32 for RMW math
                    let widened = self.widen_to_i32(val_reg, val_ty, &field_def.ty);
                    // Extent obligation, arming side: the count written here,
                    // scaled to bytes, must fit the buffer last delivered to
                    // each of the agent's handoff registers. IKOS discharges
                    // it against the capacity shadows (sizeof is a constant,
                    // so the interval domain needs no base/limit relation).
                    if self.verify_mode {
                        let key = (pname.clone(), reg_field.0.clone(), field.0.clone());
                        if let Some((scale, shadows)) = self.extent_asserts.get(&key).cloned() {
                            let bytes = self.new_reg();
                            self.line(&format!("{bytes} = mul i32 {widened}, {scale}"));
                            for shadow in shadows {
                                let cap = self.new_reg();
                                self.line(&format!("{cap} = load i32, ptr @{shadow}"));
                                let ok = self.new_reg();
                                self.line(&format!("{ok} = icmp ule i32 {bytes}, {cap}"));
                                let z = self.new_reg();
                                self.line(&format!("{z} = zext i1 {ok} to i32"));
                                self.line(&format!(
                                    "call void @__ikos_assert(i32 {z}, i32 {bytes}, i32 {cap}){dbg}"
                                ));
                            }
                        }
                    }
                    // shift new value into the field's bit position
                    let shifted = self.new_reg();
                    if shift > 0 {
                        self.line(&format!("{shifted} = shl i32 {widened}, {shift}"));
                    } else {
                        self.line(&format!("{shifted} = add i32 {widened}, 0"));
                    }
                    // mask shifted value to field width
                    let masked_val = self.new_reg();
                    self.line(&format!("{masked_val} = and i32 {shifted}, {mask}"));
                    // combine
                    let new_val = self.new_reg();
                    self.line(&format!("{new_val} = or i32 {cleared}, {masked_val}"));
                    // volatile store back
                    self.line(&format!(
                        "store volatile i32 {new_val}, ptr inttoptr ({ptr_ty} {addr} to ptr){dbg}",
                        ptr_ty = self.ptr_type()
                    ));
                    self.emit_handoff_completion(&pname, &reg_field.0);
                    self.emit_gate_readback(&pname, &reg_field.0, addr);
                    return new_val;
                }
                // Struct field write: GEP + store. Resolve the base to the
                // struct's *address* whether it is a struct place (`s.field`), an
                // explicit deref (`(*p).field`), or a pointer auto-deref
                // (`p.field`). A previous version only handled the local-struct
                // place, so writes through a pointer silently emitted no store.
                if let Some((base_ptr, base_ty)) = self.lvalue_base_info(base, symbols, fn_name) {
                    let (struct_addr, struct_ty) = match &base_ty {
                        Type::Struct(..) => (base_ptr, base_ty.clone()),
                        Type::Ptr(inner) | Type::ConstPtr(inner)
                            if matches!(inner.as_ref(), Type::Struct(..)) =>
                        {
                            let loaded = self.new_reg();
                            self.line(&format!("{loaded} = load ptr, ptr {base_ptr}"));
                            (loaded, inner.as_ref().clone())
                        }
                        _ => return val_reg.to_string(),
                    };
                    if let Type::Struct(struct_name, repr, fields) = &struct_ty
                        && let Some(idx) = fields.iter().position(|(n, _)| n == &field.0)
                    {
                        let field_ty = fields[idx].1.clone();
                        let ll_field = llvm_type(&field_ty);
                        let struct_llvm_ty = llvm_type(&struct_ty);
                        let gep = self.new_reg();
                        self.line(&format!(
                            "{gep} = getelementptr {struct_llvm_ty}, ptr {struct_addr}, i32 0, i32 {idx}"
                        ));
                        let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &field_ty);
                        // In-memory handoff: a write to an `addr in R` field is
                        // an address handed to an agent through memory it walks.
                        // Verify mode asserts the stored address is in region R;
                        // IKOS discharges it from the provenance assume at
                        // `&BUFFER as u32` (slice 4), same machinery as register
                        // handoffs. `addr` is a byte address -- no encoding.
                        if self.verify_mode
                            && let Type::Addr(region) = &field_ty
                            && let Some(&(lo, hi)) = self.region_ranges.get(region)
                        {
                            self.emit_range_assert(&val_reg, lo, hi, &dbg);
                        }
                        // Descriptor extent, delivery side: an `addr` field
                        // armed by an `@extent` sibling remembers the size of
                        // the buffer delivered here (direct `&X as u32` only;
                        // anything else resets to unconstrained).
                        if self.verify_mode
                            && let Some(shadow) = self
                                .desc_cap_shadows
                                .get(&(struct_name.clone(), field.0.clone()))
                        {
                            let cap = value_expr
                                .and_then(delivered_static_name)
                                .and_then(|n| symbols.statics.get(n))
                                .map_or(-1i64, |sym| {
                                    i64::from(crate::types::element_size(strip_storage(&sym.ty)))
                                });
                            let line = format!("store i32 {cap}, ptr @{shadow}");
                            self.line(&line);
                        }
                        // Descriptor extent, arming side: a write to the
                        // `@extent` length field asserts the byte length fits
                        // the buffer last delivered through the addr sibling.
                        if self.verify_mode
                            && let Some((scale, mask, shadow)) = self
                                .desc_extent_asserts
                                .get(&(struct_name.clone(), field.0.clone()))
                                .cloned()
                        {
                            // A descriptor control word packs the length sub-field
                            // with control bits (EQOS TDES2: B1L bits 13:0 vs TTSE
                            // bit 30). `@extent(.., mask N)` isolates the length
                            // bits BEFORE scaling, so a set control bit cannot
                            // inflate the byte count into a false overrun (V200).
                            let count = if let Some(m) = mask {
                                let masked = self.new_reg();
                                self.line(&format!("{masked} = and i32 {val_reg}, {m}"));
                                masked
                            } else {
                                val_reg.clone()
                            };
                            let bytes = self.new_reg();
                            self.line(&format!("{bytes} = mul i32 {count}, {scale}"));
                            let cap = self.new_reg();
                            self.line(&format!("{cap} = load i32, ptr @{shadow}"));
                            let ok = self.new_reg();
                            self.line(&format!("{ok} = icmp ule i32 {bytes}, {cap}"));
                            let z = self.new_reg();
                            self.line(&format!("{z} = zext i1 {ok} to i32"));
                            self.line(&format!(
                                "call void @__ikos_assert(i32 {z}, i32 {bytes}, i32 {cap}){dbg}"
                            ));
                        }
                        // Encode a `@be` field to its stored (byte-swapped) form.
                        let endian = Self::field_endian(struct_name, idx, symbols);
                        let val_reg = self.maybe_bswap(val_reg, &field_ty, endian);
                        let align = if *repr == ast::StructRepr::Packed {
                            ", align 1"
                        } else {
                            ""
                        };
                        self.line(&format!(
                            "store {ll_field} {val_reg}, ptr {gep}{align}{dbg}"
                        ));
                        return val_reg;
                    }
                }
                val_reg.to_string()
            }
            LValue::Index(base, index) => {
                // Indexed register write: `P.REG[i] = v` -> volatile store i32 at
                // `base + offset + stride*i`. The value is widened to the 32-bit
                // register width (top bits ignored by hardware), exactly like the
                // SDK's `instr_mem[i] = uint16` store.
                if let LValue::Field(p, reg) = base.as_ref()
                    && let LValue::Name((pname, _)) = p.as_ref()
                    && let Some((reg_base, stride)) = self.array_reg_addr(pname, &reg.0, symbols)
                {
                    let ptr = self.emit_reg_index_ptr(reg_base, stride, index, symbols, fn_name);
                    let v = self.coerce_int(val_reg.to_string(), val_ty, &Type::U32);
                    self.line(&format!("store volatile i32 {v}, ptr {ptr}{dbg}"));
                    return val_reg.to_string();
                }
                let Some((base_ptr, base_ty)) = self.lvalue_base_info(base, symbols, fn_name)
                else {
                    return val_reg.to_string();
                };
                // Write through a mutable linear view: load the descriptor,
                // extract { ptr, len }, assume the index is in range (so the
                // verifier can prove the access), then typed GEP + store. The
                // assume mirrors the read path (ir.rs Index/load).
                if let Type::LinearView(elem_ty, _) = &base_ty {
                    let ll_elem = llvm_type(elem_ty);
                    let agg = self.new_reg();
                    self.line(&format!("{agg} = load {{ ptr, i32 }}, ptr {base_ptr}"));
                    let (ptr_field, idx_i32) =
                        self.view_ptr_len_checked(&agg, index, symbols, fn_name);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {ptr_field}, i32 {idx_i32}{dbg}"
                    ));
                    let val_reg = self.coerce_int(val_reg.to_string(), val_ty, elem_ty);
                    self.line(&format!("store {ll_elem} {val_reg}, ptr {gep}{dbg}"));
                    return val_reg;
                }
                // Write through a mutable strided view: backing index is `i * K`
                // (K the compile-time stride), typed GEP + store. Mirrors the
                // strided read path; assume(i < len) bounds the logical index.
                if let Type::StridedView(elem_ty, _, k) = &base_ty {
                    let ll_elem = llvm_type(elem_ty);
                    let agg = self.new_reg();
                    self.line(&format!("{agg} = load {{ ptr, i32 }}, ptr {base_ptr}"));
                    let (ptr_field, idx_i32) =
                        self.view_ptr_len_checked(&agg, index, symbols, fn_name);
                    let scaled = self.new_reg();
                    self.line(&format!("{scaled} = mul i32 {idx_i32}, {k}"));
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {ptr_field}, i32 {scaled}{dbg}"
                    ));
                    let val_reg = self.coerce_int(val_reg.to_string(), val_ty, elem_ty);
                    self.line(&format!("store {ll_elem} {val_reg}, ptr {gep}{dbg}"));
                    return val_reg;
                }
                // Write through a mutable ring view: physical = (head+i) % cap,
                // then typed GEP + store. Mirrors the ring read path.
                if let Type::RingView(elem_ty, _, cap_hint) = &base_ty {
                    let ll_elem = llvm_type(elem_ty);
                    let ty = "{ ptr, i32, i32, i32 }";
                    let agg = self.new_reg();
                    self.line(&format!("{agg} = load {ty}, ptr {base_ptr}"));
                    let (ptr_field, phys) =
                        self.view_ring_addr(&agg, ty, *cap_hint, index, symbols, fn_name);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {ll_elem}, ptr {ptr_field}, i32 {phys}{dbg}"
                    ));
                    let val_reg = self.coerce_int(val_reg.to_string(), val_ty, elem_ty);
                    self.line(&format!("store {ll_elem} {val_reg}, ptr {gep}{dbg}"));
                    return val_reg;
                }
                // Write through a mutable bit view: assume(i < len_bits), then
                // read-modify-write the single byte holding bit (off+i). NOTE:
                // the RMW is not atomic; concurrent writers to the same byte race.
                if let Type::BitView(_) = &base_ty {
                    let ty = "{ ptr, i32, i32 }";
                    let agg = self.new_reg();
                    self.line(&format!("{agg} = load {ty}, ptr {base_ptr}"));
                    let ptr_field = self.new_reg();
                    self.line(&format!("{ptr_field} = extractvalue {ty} {agg}, 0"));
                    let off_field = self.new_reg();
                    self.line(&format!("{off_field} = extractvalue {ty} {agg}, 1"));
                    let len_field = self.new_reg();
                    self.line(&format!("{len_field} = extractvalue {ty} {agg}, 2"));
                    let idx_reg = self.emit_expr(index, symbols, fn_name);
                    let idx_ty = self.expr_type(index, symbols);
                    let idx_i32 = self.coerce_int(idx_reg, &idx_ty, &Type::U32);
                    let cond = self.new_reg();
                    self.line(&format!("{cond} = icmp ult i32 {idx_i32}, {len_field}"));
                    let ok_lbl = self.new_label("bit_idx_ok");
                    let oob_lbl = self.new_label("bit_idx_oob");
                    self.line(&format!("br i1 {cond}, label %{ok_lbl}, label %{oob_lbl}"));
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{oob_lbl}:"));
                    self.indent += 1;
                    self.line("unreachable");
                    self.line("");
                    self.indent -= 1;
                    self.line(&format!("{ok_lbl}:"));
                    self.indent += 1;
                    let bit = self.new_reg();
                    self.line(&format!("{bit} = add i32 {off_field}, {idx_i32}"));
                    if self.verify_mode {
                        self.generated_wrap_spans.push(index.span());
                    }
                    let byteidx = self.new_reg();
                    self.line(&format!("{byteidx} = lshr i32 {bit}, 3"));
                    let bib = self.new_reg();
                    self.line(&format!("{bib} = and i32 {bit}, 7"));
                    let bib8 = self.new_reg();
                    self.line(&format!("{bib8} = trunc i32 {bib} to i8"));
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr i8, ptr {ptr_field}, i32 {byteidx}{dbg}"
                    ));
                    let old = self.new_reg();
                    self.line(&format!("{old} = load i8, ptr {gep}{dbg}"));
                    let mask = self.new_reg();
                    self.line(&format!("{mask} = shl i8 1, {bib8}"));
                    let notmask = self.new_reg();
                    self.line(&format!("{notmask} = xor i8 {mask}, -1"));
                    let cleared = self.new_reg();
                    self.line(&format!("{cleared} = and i8 {old}, {notmask}"));
                    // Coerce the assigned value to a single bit, then place it.
                    let val_i1 = self.coerce_int(val_reg.to_string(), val_ty, &Type::B1);
                    let val8 = self.new_reg();
                    self.line(&format!("{val8} = zext i1 {val_i1} to i8"));
                    let valsh = self.new_reg();
                    self.line(&format!("{valsh} = shl i8 {val8}, {bib8}"));
                    let newbyte = self.new_reg();
                    self.line(&format!("{newbyte} = or i8 {cleared}, {valsh}"));
                    self.line(&format!("store i8 {newbyte}, ptr {gep}{dbg}"));
                    return val_i1;
                }
                let elem_ty = match &base_ty {
                    Type::Array(inner, _) | Type::Ptr(inner) | Type::ConstPtr(inner) => {
                        inner.as_ref().clone()
                    }
                    _ => return val_reg.to_string(),
                };
                // Volatile when storing through an agent pointer (vol_lvalue;
                // the array arm is a direct static access, never tainted).
                let vol = self.vol_lvalue(base);
                let idx_reg = self.emit_expr(index, symbols, fn_name);
                let idx_ty = self.expr_type(index, symbols);
                let gep = self.new_reg();
                let ll_elem = llvm_type(&elem_ty);
                self.line(&format!(
                    "{gep} = getelementptr {ll_elem}, ptr {base_ptr}, {} {idx_reg}",
                    llvm_type(&idx_ty)
                ));
                let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &elem_ty);
                self.line(&format!("store{vol} {ll_elem} {val_reg}, ptr {gep}{dbg}"));
                val_reg
            }
            LValue::Deref(inner) => {
                let vol = self.vol_expr(inner, symbols);
                let ptr_reg = self.emit_expr(inner, symbols, fn_name);
                let inner_ty = self.expr_type(inner, symbols);
                let pointee_ty = match &inner_ty {
                    Type::Ptr(t) | Type::ConstPtr(t) => (**t).clone(),
                    _ => crate::types::Type::I32,
                };
                let llty = llvm_type(&pointee_ty);
                let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &pointee_ty);
                self.line(&format!("store{vol} {llty} {val_reg}, ptr {ptr_reg}{dbg}"));
                val_reg
            }
        }
    }

    // ─── vector table ────────────────────────────────────────────────

    fn emit_vector_table(&mut self, program: &Program, symbols: &SymbolTable) {
        let interrupts = self.target_interrupts.clone();
        crate::arch::arm::emit_vector_table(self, program, symbols, &interrupts);
    }

    // ─── debug info module-level emission ─────────────────────────────

    fn emit_debug_module_flags(&mut self) {
        let id0 = self.new_dbg_id();
        let id1 = self.new_dbg_id();
        writeln!(
            self.out,
            "!{id0} = !{{i32 2, !\"Debug Info Version\", i32 3}}"
        )
        .unwrap();
        writeln!(self.out, "!{id1} = !{{i32 2, !\"Dwarf Version\", i32 4}}").unwrap();
        writeln!(self.out, "!llvm.module.flags = !{{!{id0}, !{id1}}}").unwrap();
        self.out.push('\n');
    }

    fn emit_debug_compile_unit(&mut self, program: &Program) {
        // Find file ID from the first item's span
        let file_id = program
            .items
            .iter()
            .find_map(|item| match item {
                ast::Item::FnDef(f) => Some(f.name.1.file),
                ast::Item::StructDef(s) => Some(s.name.1.file),
                ast::Item::EnumDef(e) => Some(e.name.1.file),
                _ => None,
            })
            .unwrap_or_else(|| match self.source_map {
                Some(ref _sm) => {
                    // Fallback: add a virtual file. This shouldn't happen in practice.
                    crate::source::FileId::new()
                }
                None => crate::source::FileId::new(),
            });

        let dbg_file_id = self.dbg_file(file_id);
        self.cu_file_id = Some(dbg_file_id);

        let cu_id = self.new_dbg_id();
        self.cu_id = Some(cu_id);
        writeln!(
            self.debug_metadata,
            "!{cu_id} = distinct !DICompileUnit(language: DW_LANG_C, file: !{dbg_file_id}, producer: \"bml compiler\", isOptimized: false, runtimeVersion: 0, emissionKind: FullDebug)"
        )
        .unwrap();
        writeln!(self.debug_metadata, "!llvm.dbg.cu = !{{!{cu_id}}}").unwrap();
        self.out.push('\n');
    }

    // ─── helpers ─────────────────────────────────────────────────────

    fn ptr_type(&self) -> &'static str {
        self.arch.ptr_type()
    }

    fn emit_verify_forget_shared_static(&mut self, name: &str, ty: &Type) {
        if !self.verify_mode {
            return;
        }
        // Inside this static's own `claim` window the value is stable: the
        // mask stops local preemption, and for cross-core statics the
        // spinlock excludes the other core. Havocing here would erase
        // exactly the consistency the window provides.
        if self.claimed_statics.iter().any(|n| n == name) {
            return;
        }
        // Without preemption info we have no choice but to over-approximate.
        // With it, only havoc when a higher-priority ISR can actually write
        // this static while the current function is reading it.
        if let Some(preempt) = &self.preempt {
            let key = (self.current_fn_name.clone(), name.to_string());
            if !preempt.preemptable.contains_key(&key) {
                return;
            }
        }
        let size = crate::types::element_size(ty);
        self.line(&format!(
            "call void @__ikos_forget_mem(ptr @{name}, i32 {size})"
        ));
    }

    /// `Some(ceiling)` when this access needs its own critical section; the
    /// ceiling picks the mask instrument (BASEPRI to the ceiling on v7-M,
    /// PRIMASK otherwise -- see `arm::emit_critical_enter`).
    /// `" volatile"` when the expression is an agent pointer (tainted local
    /// or direct cast of an agent-shared static's address): the agent
    /// mutates the pointee concurrently, so the access must not be hoisted,
    /// merged, or eliminated.
    fn vol_expr(&self, e: &ast::Expr, symbols: &SymbolTable) -> &'static str {
        if crate::region::is_agent_ptr_expr(e, &self.agent_ptr_locals, symbols) {
            " volatile"
        } else {
            ""
        }
    }

    fn vol_lvalue(&self, lv: &ast::LValue) -> &'static str {
        if matches!(lv, ast::LValue::Name((n, _)) if self.agent_ptr_locals.contains(n)) {
            " volatile"
        } else {
            ""
        }
    }

    /// `dsb` after a store to a declared handoff register (see
    /// `handoff_regs`). Emitted in verify mode too: IKOS treats the asm call
    /// as opaque, same as the critical-section masks.
    fn emit_handoff_completion(&mut self, periph: &str, reg: &str) {
        if self.handoff_regs.contains(&format!("{periph}.{reg}")) {
            self.line("call void asm sideeffect \"dsb\", \"~{memory}\"()");
        }
    }

    /// Volatile read-back after a write to a register holding a declared
    /// clock gate (see `gate_regs`): forces the enable write to complete
    /// before the newly-clocked peripheral is touched.
    fn emit_gate_readback(&mut self, periph: &str, reg: &str, addr: u64) {
        if self
            .gate_regs
            .contains(&(periph.to_string(), reg.to_string()))
        {
            let r = self.new_reg();
            self.line(&format!(
                "{r} = load volatile i32, ptr inttoptr ({ptr_ty} {addr} to ptr)",
                ptr_ty = self.ptr_type()
            ));
        }
    }

    fn critical_section_ceiling(&self, name: &str, symbols: &SymbolTable) -> Option<u8> {
        // Inside a `claim` window everything is already masked; a per-access
        // pair here would drop the mask early and break the window.
        if self.claim_depth > 0 {
            return None;
        }
        if let Some(sym) = symbols.statics.get(name) {
            for ann in &sym.storage {
                if let StorageAnnotation::Shared(ceiling) = ann {
                    let ceiling = ceiling.expect("@shared ceiling materialized at resolve");
                    return self
                        .current_ctx
                        .needs_critical_section(ceiling)
                        .then_some(ceiling);
                }
            }
        }
        None
    }

    /// Ceiling of a `@shared` static for `claim` windows: unlike per-access
    /// sections the window is emitted unconditionally, so this only selects
    /// the mask instrument. `None` (not `@shared`, or no materialized
    /// ceiling) selects the conservative PRIMASK mask.
    fn shared_ceiling(name: &str, symbols: &SymbolTable) -> Option<u8> {
        symbols.statics.get(name).and_then(|sym| {
            sym.storage.iter().find_map(|ann| match ann {
                StorageAnnotation::Shared(ceiling) => *ceiling,
                _ => None,
            })
        })
    }

    pub(crate) fn line(&mut self, s: &str) {
        for _ in 0..self.indent {
            self.out.push_str("  ");
        }
        self.out.push_str(s);
        self.out.push('\n');
        if s.ends_with(':') && !s.starts_with(' ') {
            self.current_label = Some(s.trim_end_matches(':').to_string());
        }
    }

    pub(crate) fn new_reg(&mut self) -> String {
        let r = self.counter;
        self.counter += 1;
        format!("%{r}")
    }

    /// Create a new unnamed register and emit `%N = <instruction>` in one call.
    /// Returns the register name (e.g. `%0`).
    pub(crate) fn emit_line(&mut self, instruction: &str) -> String {
        let reg = self.new_reg();
        self.line(&format!("{reg} = {instruction}"));
        reg
    }

    pub(crate) fn new_label(&mut self, prefix: &str) -> String {
        let n = self.label_counter;
        self.label_counter += 1;
        format!("{prefix}.{n}")
    }

    /// Extract `{ ptr, i32 }` from a linear or strided view descriptor `agg`,
    /// lower `index`, and emit `assume(idx < len)` as a branch to `unreachable`
    /// on out-of-range (so the verifier can prove the access). Returns the base
    /// pointer register and the in-range `i32` index, leaving the builder in
    /// the in-range block. Shared by reads (`Expr::Index`) and writes
    /// (`LValue::Index`); the caller adds the typed GEP, scaling the index by
    /// the stride for strided views.
    fn view_ptr_len_checked(
        &mut self,
        agg: &str,
        index: &Expr,
        symbols: &SymbolTable,
        fn_name: &str,
    ) -> (String, String) {
        let ptr_field = self.new_reg();
        self.line(&format!(
            "{ptr_field} = extractvalue {{ ptr, i32 }} {agg}, 0"
        ));
        let len_field = self.new_reg();
        self.line(&format!(
            "{len_field} = extractvalue {{ ptr, i32 }} {agg}, 1"
        ));
        let idx_reg = self.emit_expr(index, symbols, fn_name);
        let idx_ty = self.expr_type(index, symbols);
        let idx_i32 = self.coerce_int(idx_reg, &idx_ty, &Type::U32);
        // assume(idx < len), unsigned: also rules out a negative index.
        let cond = self.new_reg();
        self.line(&format!("{cond} = icmp ult i32 {idx_i32}, {len_field}"));
        let ok_lbl = self.new_label("view_idx_ok");
        let oob_lbl = self.new_label("view_idx_oob");
        self.line(&format!("br i1 {cond}, label %{ok_lbl}, label %{oob_lbl}"));
        self.line("");
        self.indent -= 1;
        self.line(&format!("{oob_lbl}:"));
        self.indent += 1;
        self.line("unreachable");
        self.line("");
        self.indent -= 1;
        self.line(&format!("{ok_lbl}:"));
        self.indent += 1;
        (ptr_field, idx_i32)
    }

    /// Extract the base pointer and compute the physical element index for a
    /// ring view descriptor `agg` of LLVM type `ty`:
    /// `phys = (head + i) % capacity` (a constant mask when the capacity is a
    /// power of two). Returns the base pointer and physical-index registers.
    /// Shared by ring reads and writes; the caller adds the typed GEP.
    fn view_ring_addr(
        &mut self,
        agg: &str,
        ty: &str,
        cap_hint: Option<u32>,
        index: &Expr,
        symbols: &SymbolTable,
        fn_name: &str,
    ) -> (String, String) {
        let ptr_field = self.new_reg();
        self.line(&format!("{ptr_field} = extractvalue {ty} {agg}, 0"));
        let head_field = self.new_reg();
        self.line(&format!("{head_field} = extractvalue {ty} {agg}, 2"));
        let idx_reg = self.emit_expr(index, symbols, fn_name);
        let idx_ty = self.expr_type(index, symbols);
        let idx_i32 = self.coerce_int(idx_reg, &idx_ty, &Type::U32);
        let sum = self.new_reg();
        self.line(&format!("{sum} = add i32 {head_field}, {idx_i32}"));
        if self.verify_mode {
            self.generated_wrap_spans.push(index.span());
        }
        let phys = self.ring_physical_index(agg, ty, cap_hint, &sum);
        (ptr_field, phys)
    }

    /// Lower a boolean condition in BRANCH position as a short-circuit
    /// branch tree: `&&` / `||` / `!` become control flow with two targets
    /// and NO phi. Each leaf decision is a same-block value + `br`, which is
    /// both the standard condition lowering (no materialized boolean) and
    /// the shape the verifier refines through: on the false edge of
    /// `a || b`, "both operands are false" is encoded in the CFG itself,
    /// where the phi form needs per-edge reasoning IKOS does not do (the
    /// servo's divergence-gate else branch lost its `off` bounds when `||`
    /// briefly lowered to a phi). Value-position `&&`/`||` keep the phi
    /// lowering in `emit_expr`.
    fn emit_branch_cond(
        &mut self,
        e: &Expr,
        true_lbl: &str,
        false_lbl: &str,
        symbols: &SymbolTable,
        fn_name: &str,
    ) {
        match e {
            Expr::Group(inner) => {
                self.emit_branch_cond(inner, true_lbl, false_lbl, symbols, fn_name);
            }
            Expr::Binary(l, crate::ast::BinaryOp::And, r) => {
                let mid = self.new_label("cond_and");
                self.emit_branch_cond(l, &mid, false_lbl, symbols, fn_name);
                self.line("");
                self.indent -= 1;
                self.line(&format!("{mid}:"));
                self.indent += 1;
                self.emit_branch_cond(r, true_lbl, false_lbl, symbols, fn_name);
            }
            Expr::Binary(l, crate::ast::BinaryOp::Or, r) => {
                let mid = self.new_label("cond_or");
                self.emit_branch_cond(l, true_lbl, &mid, symbols, fn_name);
                self.line("");
                self.indent -= 1;
                self.line(&format!("{mid}:"));
                self.indent += 1;
                self.emit_branch_cond(r, true_lbl, false_lbl, symbols, fn_name);
            }
            Expr::Unary(crate::ast::UnaryOp::Not, inner) => {
                self.emit_branch_cond(inner, false_lbl, true_lbl, symbols, fn_name);
            }
            other => {
                let reg = self.emit_expr(other, symbols, fn_name);
                self.line(&format!(
                    "br i1 {reg}, label %{true_lbl}, label %{false_lbl}"
                ));
            }
        }
    }

    /// Lower a ring view's physical index from `sum = head + i`. With a
    /// compile-time power-of-two capacity (`cap_hint`), emit the constant mask
    /// `sum & (cap - 1)` -- cheaper than `urem` and trivially bounded to
    /// `[0, cap)` for the verifier. Otherwise extract the runtime capacity
    /// field (index 1) from the descriptor `agg` and emit `urem`. Returns the
    /// physical-index register.
    fn ring_physical_index(
        &mut self,
        agg: &str,
        ty: &str,
        cap_hint: Option<u32>,
        sum: &str,
    ) -> String {
        // Allocate registers in the same order their defining lines are
        // emitted: LLVM numbers unnamed temporaries by textual definition order,
        // so allocating `phys` before `cap_field` while emitting `cap_field`
        // first produces an out-of-order `%N` that llc rejects.
        if let Some(cap) = cap_hint {
            let phys = self.new_reg();
            self.line(&format!("{phys} = and i32 {sum}, {}", cap - 1));
            phys
        } else {
            let cap_field = self.new_reg();
            self.line(&format!("{cap_field} = extractvalue {ty} {agg}, 1"));
            let phys = self.new_reg();
            self.line(&format!("{phys} = urem i32 {sum}, {cap_field}"));
            phys
        }
    }

    pub(crate) fn new_str_id(&mut self) -> u32 {
        let id = self.str_counter;
        self.str_counter += 1;
        id
    }

    pub(crate) fn alloca(&mut self, ty: &str, name: &str) -> String {
        let n = self.alloca_counter;
        self.alloca_counter += 1;
        let alloca_name = format!("%__{name}.{n}");
        self.line(&format!("{alloca_name} = alloca {ty}"));
        alloca_name
    }

    fn new_anon_alloca(&mut self, ty: &str) -> String {
        let n = self.alloca_counter;
        self.alloca_counter += 1;
        let alloca_name = format!("%__arr.tmp.{n}");
        self.line(&format!("{alloca_name} = alloca {ty}"));
        alloca_name
    }

    // ─── lvalue helpers ───────────────────────────────────────────────

    fn new_dbg_id(&mut self) -> u32 {
        let n = self.debug_counter;
        self.debug_counter += 1;
        n
    }

    fn dbg_file(&mut self, file_id: crate::source::FileId) -> u32 {
        if let Some(&id) = self.file_dbg_id.get(&file_id) {
            return id;
        }
        let id = self.new_dbg_id();
        if let Some(ref sm) = self.source_map {
            let path = sm.get_path(file_id);
            let filename = path.file_name().map_or_else(
                || "unknown.bml".to_string(),
                |n| n.to_string_lossy().to_string(),
            );
            let directory = path
                .parent()
                .map_or_else(|| ".".to_string(), |p| p.to_string_lossy().to_string());
            // Escape backslashes and quotes for LLVM metadata
            let filename = filename.replace('\\', "\\\\").replace('"', "\\\"");
            let directory = directory.replace('\\', "\\\\").replace('"', "\\\"");
            writeln!(
                self.debug_metadata,
                "!{id} = !DIFile(filename: \"{filename}\", directory: \"{directory}\")"
            )
            .unwrap();
        } else {
            writeln!(
                self.debug_metadata,
                "!{id} = !DIFile(filename: \"unknown.bml\", directory: \".\")"
            )
            .unwrap();
        }
        self.file_dbg_id.insert(file_id, id);
        id
    }

    fn dbg_type(&mut self, ty: &Type) -> u32 {
        let key = format!("{ty:?}");
        if let Some(&id) = self.type_dbg_id.get(&key) {
            return id;
        }
        let id = self.new_dbg_id();
        let (name, size, encoding) = match ty {
            Type::Void => {
                let id = self.new_dbg_id();
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DIBasicType(tag: DW_TAG_unspecified_type, name: \"void\")"
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::I8 => ("i8", 8, 5), // DW_ATE_signed
            Type::I16 => ("i16", 16, 5),
            Type::I32 => ("i32", 32, 5),
            Type::I64 => ("i64", 64, 5),
            Type::U8 => ("u8", 8, 7), // DW_ATE_unsigned
            Type::U16 => ("u16", 16, 7),
            Type::U32 => ("u32", 32, 7),
            Type::U64 => ("u64", 64, 7),
            Type::F16 => ("f16", 16, 4), // DW_ATE_float
            Type::F32 => ("f32", 32, 4),
            Type::F64 => ("f64", 64, 4),
            Type::B1 => ("b1", 1, 2), // DW_ATE_boolean
            Type::B8 => ("b8", 8, 2),
            Type::Ptr(inner)
            | Type::ConstPtr(inner)
            | Type::Exclusive(inner)
            | Type::Shared(inner, _)
            | Type::Mmio(inner)
            | Type::AgentShared(inner) => {
                let inner_id = self.dbg_type(inner);
                let id = self.new_dbg_id();
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DIDerivedType(tag: DW_TAG_pointer_type, baseType: !{inner_id}, size: 32)"
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::Array(inner, len) => {
                let elem_id = self.dbg_type(inner);
                let range_id = self.new_dbg_id();
                writeln!(
                    self.debug_metadata,
                    "!{range_id} = !DISubrange(count: {len})"
                )
                .unwrap();
                let total_bits = crate::types::element_size(ty) * 8;
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DICompositeType(tag: DW_TAG_array_type, baseType: !{elem_id}, size: {total_bits}, elements: !{{!{range_id}}})"
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::Struct(name, repr, fields) => {
                let mut offset_bits: u32 = 0;
                let field_debug: Vec<String> = fields
                    .iter()
                    .map(|(fname, fty)| {
                        let fty_id = self.dbg_type(fty);
                        let size_bits = match fty {
                            Type::B1 => 1, Type::B8 => 8, Type::I8|Type::U8 => 8,
                            Type::I16|Type::U16|Type::F16 => 16,
                            Type::I32|Type::U32|Type::F32 => 32,
                            Type::I64|Type::U64|Type::F64 => 64,
                            _ => crate::types::element_size(fty) * 8,
                        };
                        if *repr == ast::StructRepr::C {
                            offset_bits = crate::types::align_to(offset_bits, crate::types::align_of(fty) * 8);
                        }
                        let s = format!("!DIDerivedType(tag: DW_TAG_member, name: \"{fname}\", scope: !{id}, file: !{}, line: 0, baseType: !{fty_id}, size: {size_bits}, offset: {offset_bits})",
                            self.cu_file_id.unwrap_or(0));
                        offset_bits += size_bits;
                        s
                    })
                    .collect();
                let total_bits = crate::types::element_size(ty) * 8;
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DICompositeType(tag: DW_TAG_structure_type, name: \"{name}\", file: !{}, line: 0, size: {total_bits}, elements: !{{{}}})",
                    self.cu_file_id.unwrap_or(0),
                    field_debug.join(", ")
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::Enum(_, inner_ty, _) => return self.dbg_type(inner_ty),
            Type::LinearView(elem, _) | Type::StridedView(elem, _, _) => {
                // Descriptor is { data: ptr-to-elem, len: u32 }. Emit it as a
                // 2-field structure so the debug type matches the { ptr, i32 }
                // aggregate (an integer DIBasicType here makes IKOS reject the
                // module).
                let data_ptr_ty = Type::ConstPtr(elem.clone());
                let data_id = self.dbg_type(&data_ptr_ty);
                let len_id = self.dbg_type(&Type::U32);
                let data_member = format!(
                    "!DIDerivedType(tag: DW_TAG_member, name: \"data\", scope: !{id}, file: !{f}, line: 0, baseType: !{data_id}, size: 32, offset: 0)",
                    f = self.cu_file_id.unwrap_or(0)
                );
                let len_member = format!(
                    "!DIDerivedType(tag: DW_TAG_member, name: \"len\", scope: !{id}, file: !{f}, line: 0, baseType: !{len_id}, size: 32, offset: 32)",
                    f = self.cu_file_id.unwrap_or(0)
                );
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DICompositeType(tag: DW_TAG_structure_type, name: \"view\", file: !{f}, line: 0, size: 64, elements: !{{{data_member}, {len_member}}})",
                    f = self.cu_file_id.unwrap_or(0)
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::RingView(elem, _, _) => {
                // Descriptor is { data: ptr-to-elem, capacity, head, len }, all
                // i32 after the pointer. Emit a 4-field structure (matching the
                // { ptr, i32, i32, i32 } aggregate) so IKOS accepts the module.
                let f = self.cu_file_id.unwrap_or(0);
                let data_id = self.dbg_type(&Type::ConstPtr(elem.clone()));
                let u32_id = self.dbg_type(&Type::U32);
                let members = ["data", "capacity", "head", "len"]
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let base = if i == 0 { data_id } else { u32_id };
                        format!(
                            "!DIDerivedType(tag: DW_TAG_member, name: \"{name}\", scope: !{id}, file: !{f}, line: 0, baseType: !{base}, size: 32, offset: {})",
                            i * 32
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DICompositeType(tag: DW_TAG_structure_type, name: \"ring\", file: !{f}, line: 0, size: 128, elements: !{{{members}}})"
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::BitView(_) => {
                // Descriptor is { data: byte ptr, bit_offset, len_bits }. Emit a
                // 3-field structure (matching the { ptr, i32, i32 } aggregate) so
                // IKOS accepts the module.
                let f = self.cu_file_id.unwrap_or(0);
                let data_id = self.dbg_type(&Type::ConstPtr(Box::new(Type::U8)));
                let u32_id = self.dbg_type(&Type::U32);
                let members = ["data", "bit_offset", "len_bits"]
                    .iter()
                    .enumerate()
                    .map(|(i, name)| {
                        let base = if i == 0 { data_id } else { u32_id };
                        format!(
                            "!DIDerivedType(tag: DW_TAG_member, name: \"{name}\", scope: !{id}, file: !{f}, line: 0, baseType: !{base}, size: 32, offset: {})",
                            i * 32
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DICompositeType(tag: DW_TAG_structure_type, name: \"bits\", file: !{f}, line: 0, size: 96, elements: !{{{members}}})"
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::Fn(params, ret) => {
                // A function pointer lowers to `ptr`; its debug type must be a
                // pointer type, NOT the integer fallback below. An integer
                // DIBasicType on a non-integer `ptr` value makes the LLVM
                // verifier (and IKOS, under `bml verify`) reject the module --
                // the same hazard the view arms above avoid for aggregates.
                let ret_str = if matches!(**ret, Type::Void) {
                    "null".to_string()
                } else {
                    format!("!{}", self.dbg_type(ret))
                };
                let param_strs: Vec<String> = params
                    .iter()
                    .map(|p| format!("!{}", self.dbg_type(p)))
                    .collect();
                let types = std::iter::once(ret_str)
                    .chain(param_strs)
                    .collect::<Vec<_>>()
                    .join(", ");
                let sub_id = self.new_dbg_id();
                writeln!(
                    self.debug_metadata,
                    "!{sub_id} = !DISubroutineType(types: !{{{types}}})"
                )
                .unwrap();
                let id = self.new_dbg_id();
                writeln!(
                    self.debug_metadata,
                    "!{id} = !DIDerivedType(tag: DW_TAG_pointer_type, baseType: !{sub_id}, size: 32)"
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            // The remaining types all lower to a 32-bit integer (`Addr`, and the
            // post-resolver-unreachable `PeripheralHandle`/`Unresolved`/`Error`),
            // so an i32 DIBasicType matches. Types that lower to `ptr` must NOT
            // reach here -- they need a pointer DIType (see the `Fn`/`Ptr` arms).
            _ => ("i32", 32, 5),
        };
        writeln!(
            self.debug_metadata,
            "!{id} = !DIBasicType(name: \"{name}\", size: {size}, encoding: {encoding})"
        )
        .unwrap();
        self.type_dbg_id.insert(key, id);
        id
    }

    fn dbg_loc(&mut self, span: Span) -> String {
        if !self.debug || self.fn_scope_id.is_none() {
            return String::new();
        }
        let (line, col) = if let Some(ref sm) = self.source_map {
            let loc = sm.span_location(span);
            (loc.start.line, loc.start.column)
        } else {
            (0, 0)
        };
        let id = self.new_dbg_id();
        let scope = self.fn_scope_id.unwrap();
        writeln!(
            self.debug_metadata,
            "!{id} = !DILocation(line: {line}, column: {col}, scope: !{scope})"
        )
        .unwrap();
        format!(", !dbg !{id}")
    }

    fn dbg_declare(&mut self, alloca: &str, var_name: &str, ty: &Type, span: Span) {
        if !self.debug || self.fn_scope_id.is_none() {
            return;
        }
        // In verify mode a view/ring/bits descriptor is an SSA aggregate passed
        // by value. After `opt`'s mem2reg/sroa, a `dbg.declare` on the
        // descriptor alloca can become a `dbg.value` whose metadata operand is
        // the whole aggregate (e.g. `{ ptr, i32, i32, i32 }` for a ring). That
        // aggregate-typed dbg metadata crashes IKOS's LLVM-AR frontend with
        // "invalid ar bitcast: from kind=7 to kind=7". IKOS does not need the
        // descriptor variable for bounds analysis (it reasons over the SSA
        // struct values and the load-bearing `assume`s), so drop the declare
        // here. Normal `-g` builds keep it, so the DWARF composite-type tests
        // are unaffected.
        if self.verify_mode
            && matches!(
                ty,
                Type::LinearView(..)
                    | Type::StridedView(..)
                    | Type::RingView(..)
                    | Type::BitView(..)
            )
        {
            return;
        }
        let (line, _col) = if let Some(ref sm) = self.source_map {
            let loc = sm.span_location(span);
            (loc.start.line, 0)
        } else {
            (0, 0)
        };
        let var_id = self.new_dbg_id();
        let ty_id = self.dbg_type(ty);
        let scope = self.fn_scope_id.unwrap();
        let file_id = self.cu_file_id.unwrap_or(0);
        writeln!(
            self.debug_metadata,
            "!{var_id} = !DILocalVariable(name: \"{var_name}\", scope: !{scope}, file: !{file_id}, line: {line}, type: !{ty_id})"
        )
        .unwrap();
        let loc_id = self.new_dbg_id();
        writeln!(
            self.debug_metadata,
            "!{loc_id} = !DILocation(line: {line}, column: 0, scope: !{scope})"
        )
        .unwrap();
        self.line(&format!(
            "call void @llvm.dbg.declare(metadata ptr {alloca}, metadata !{var_id}, metadata !DIExpression()), !dbg !{loc_id}"
        ));
    }

    /// Stored byte order of struct `struct_name`'s field at index `idx`.
    /// Endianness lives in `StructInfo` (not `Type`), so it is looked up here by
    /// name + index rather than carried on the field type.
    fn field_endian(
        struct_name: &str,
        idx: usize,
        symbols: &SymbolTable,
    ) -> crate::ast::FieldEndian {
        symbols
            .structs
            .get(struct_name)
            .and_then(|si| si.field_endian.get(idx))
            .copied()
            .unwrap_or(crate::ast::FieldEndian::Native)
    }

    /// Byte-swap `reg` (a value of `field_ty`) when the field's declared byte
    /// order differs from the target's native order. A field already in native
    /// order passes through unchanged. `field_ty` is always a multi-byte integer
    /// here (the checker enforces E359), so the intrinsic suffix is i16/i32/i64.
    fn maybe_bswap(
        &mut self,
        reg: String,
        field_ty: &Type,
        endian: crate::ast::FieldEndian,
    ) -> String {
        if !self.arch.endianness().swaps(endian) {
            return reg;
        }
        let ll = llvm_type(field_ty);
        let out = self.new_reg();
        self.line(&format!("{out} = call {ll} @llvm.bswap.{ll}({ll} {reg})"));
        out
    }

    /// Widen or narrow an integer register from `from` to `to`, emitting the
    /// appropriate sext/zext/trunc. No-op when the widths already match or
    /// either side is not an integer.
    fn coerce_int(&mut self, reg: String, from: &Type, to: &Type) -> String {
        if !(crate::types::is_int(from) && crate::types::is_int(to)) {
            return reg;
        }
        let from_llvm = llvm_type(from);
        let to_llvm = llvm_type(to);
        if from_llvm == to_llvm {
            return reg;
        }
        let from_bits = int_bit_width(&from_llvm);
        let to_bits = int_bit_width(&to_llvm);
        let out = self.new_reg();
        if to_bits > from_bits {
            let ext_op = if matches!(from, Type::I8 | Type::I16 | Type::I32 | Type::I64) {
                "sext"
            } else {
                "zext"
            };
            self.line(&format!("{out} = {ext_op} {from_llvm} {reg} to {to_llvm}"));
        } else {
            self.line(&format!("{out} = trunc {from_llvm} {reg} to {to_llvm}"));
        }
        out
    }

    fn expr_type(&self, expr: &Expr, symbols: &SymbolTable) -> Type {
        match expr {
            Expr::IntLiteral(n, suffix, _) => {
                crate::types::int_suffix_type(*suffix).unwrap_or({
                    // Matches the emit width: a >32-bit unsuffixed literal is
                    // 64-bit (it only type-checks in a 64-bit context).
                    if *n > u64::from(u32::MAX) {
                        Type::U64
                    } else {
                        Type::U32
                    }
                })
            }
            Expr::FloatLiteral(_, suffix, _) => match suffix {
                crate::ast::FloatSuffix::H => Type::F16,
                crate::ast::FloatSuffix::F | crate::ast::FloatSuffix::None => Type::F32,
                crate::ast::FloatSuffix::D => Type::F64,
            },
            Expr::BoolLiteral(_, _) => Type::B1,
            Expr::StringLiteral(_, _) => Type::ConstPtr(Box::new(Type::U8)),
            Expr::NullLiteral(_) => Type::Null,
            Expr::SizeOf(ty_expr, _) => {
                let _ = crate::types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
                Type::U32
            }
            Expr::Ident((name, _)) => {
                // Check local variables first (for struct types)
                if let Some(info) = self.locals.get(name) {
                    return info.bml_type.clone();
                }
                if let Some(sym) = symbols.statics.get(name) {
                    return sym.ty.inner().clone();
                }
                if let Some(sym) = symbols.consts.get(name) {
                    return sym.ty.clone();
                }
                if let Some(fn_sym) = symbols.functions.get(name) {
                    return fn_sym.fn_pointer_type();
                }
                Type::U32 // default for unresolved locals
            }
            Expr::Binary(left, op, right) => {
                use crate::ast::BinaryOp;
                match op {
                    BinaryOp::Eq
                    | BinaryOp::NotEq
                    | BinaryOp::Lt
                    | BinaryOp::Gt
                    | BinaryOp::LtEq
                    | BinaryOp::GtEq
                    | BinaryOp::And
                    | BinaryOp::Or => Type::B1,
                    // pointer - pointer yields an integer element count, not a
                    // pointer (matches the checker); pointer +/- int stays ptr.
                    BinaryOp::Add | BinaryOp::Sub => {
                        let left_ty = self.expr_type(left, symbols);
                        if *op == BinaryOp::Sub
                            && crate::types::is_ptr(&left_ty)
                            && crate::types::is_ptr(&self.expr_type(right, symbols))
                        {
                            Type::I32
                        } else {
                            left_ty
                        }
                    }
                    _ => self.expr_type(left, symbols),
                }
            }
            Expr::FieldAccess(base, field) => {
                // Peripheral field type lookup: scalar `P.REG.FIELD`.
                if let Expr::FieldAccess(inner, reg_field) = base.as_ref()
                    && let Expr::Ident((periph_name, _)) = inner.as_ref()
                    && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
                    && let Some(reg) = p.regs.get(&reg_field.0)
                    && let Some(field_sym) = reg.fields.get(&field.0)
                {
                    return field_sym.ty.clone();
                }
                // Indexed array-register field `P.REG[i].FIELD`. Without this the
                // type falls through to U32 while codegen narrows the value to the
                // field's real (possibly sub-u32) type -> a verifier mismatch.
                if let Expr::Index(arr, _) = base.as_ref()
                    && let Expr::FieldAccess(inner, reg_field) = arr.as_ref()
                    && let Expr::Ident((periph_name, _)) = inner.as_ref()
                    && let Some(p) = symbols.peripherals.get(&self.subst_periph(periph_name))
                    && let Some(reg) = p.regs.get(&reg_field.0)
                    && let Some(field_sym) = reg.fields.get(&field.0)
                {
                    return field_sym.ty.clone();
                }
                let base_ty = self.expr_type(base, symbols);
                if let Type::Struct(_, _, fields) = &base_ty
                    && let Some((_, field_ty)) = fields.iter().find(|(n, _)| n == &field.0)
                {
                    return field_ty.clone();
                }
                if let Type::Ptr(inner) | Type::ConstPtr(inner) = &base_ty
                    && let Type::Struct(_, _, fields) = inner.as_ref()
                    && let Some((_, field_ty)) = fields.iter().find(|(n, _)| n == &field.0)
                {
                    return field_ty.clone();
                }
                Type::U32 // registers are 32-bit
            }
            Expr::Index(base, _) => {
                let base_ty = self.expr_type(base, symbols);
                match &base_ty {
                    Type::Array(inner, _) => *inner.clone(),
                    Type::Ptr(inner) | Type::ConstPtr(inner) => *inner.clone(),
                    Type::LinearView(inner, _)
                    | Type::StridedView(inner, _, _)
                    | Type::RingView(inner, _, _) => *inner.clone(),
                    Type::BitView(_) => Type::B1,
                    _ => Type::U32,
                }
            }
            // The mutability flag is irrelevant to lowering (the descriptor and
            // index math are identical); a view over `*mut T` is reported as
            // mutable, everything else as readonly, only for completeness.
            // `.inner()` sees through a storage wrapper so a view over a
            // storage-class array reports the right element type.
            Expr::ViewNew { base, stride, .. } => {
                let base_inner = self.expr_type(base, symbols).inner().clone();
                if let Some(stride) = stride {
                    // Strided view: element type from the array, stride K from
                    // the literal. Mutability is irrelevant to lowering.
                    let k = match stride.as_ref() {
                        Expr::IntLiteral(v, _, _) => u32::try_from(*v).unwrap_or(1).max(1),
                        _ => 1,
                    };
                    let elem = match base_inner {
                        Type::Array(inner, _) => *inner,
                        _ => Type::U32,
                    };
                    Type::StridedView(Box::new(elem), false, k)
                } else {
                    match base_inner {
                        Type::Ptr(inner) => Type::LinearView(inner, true),
                        Type::ConstPtr(inner) | Type::Array(inner, _) => {
                            Type::LinearView(inner, false)
                        }
                        _ => Type::LinearView(Box::new(Type::U32), false),
                    }
                }
            }
            Expr::RingNew { base, capacity, .. } => {
                match self.expr_type(base, symbols).inner().clone() {
                    Type::Ptr(inner) => Type::RingView(inner, true, None),
                    Type::ConstPtr(inner) => Type::RingView(inner, false, None),
                    // Array-backed form: the capacity is the array length. Carry it
                    // as a hint only when it is a power of two (and only for the
                    // array form, where there is no explicit `capacity` argument),
                    // which enables the `& (n - 1)` mask at the index site.
                    Type::Array(inner, n) => {
                        let cap_hint = capacity
                            .is_none()
                            .then(|| u32::try_from(n).ok().filter(|_| n.is_power_of_two()))
                            .flatten();
                        Type::RingView(inner, false, cap_hint)
                    }
                    _ => Type::RingView(Box::new(Type::U32), false, None),
                }
            }
            Expr::BitNew { base, .. } => match self.expr_type(base, symbols) {
                Type::Ptr(_) => Type::BitView(true),
                _ => Type::BitView(false),
            },
            Expr::Cast(_, ty_expr) => {
                crate::types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums)
            }
            Expr::ArrayInit(elems, _) => {
                let elem_ty = elems
                    .first()
                    .map_or(Type::U32, |e| self.expr_type(e, symbols));
                Type::Array(Box::new(elem_ty), elems.len())
            }
            // Desugared to ArrayInit by constfold, or rejected by the checker (E348).
            Expr::ArrayRepeat(value, _, _) => {
                Type::Array(Box::new(self.expr_type(value, symbols)), 0)
            }
            Expr::Group(inner) => self.expr_type(inner, symbols),
            Expr::StructInit { name, .. } => {
                if let Some(info) = symbols.structs.get(&name.0) {
                    Type::Struct(name.0.clone(), info.repr, info.fields.clone())
                } else {
                    // Checker should have reported the unknown struct already.
                    Type::Error(crate::errors::ErrorGuaranteed::unchecked_claim_error_was_emitted())
                }
            }
            Expr::EnumVariant {
                enum_name: (name, _),
                ..
            } => {
                if let Some((inner_ty, variants)) = symbols.enums.get(name) {
                    Type::Enum(name.clone(), Box::new(inner_ty.clone()), variants.clone())
                } else {
                    Type::Error(crate::errors::ErrorGuaranteed::unchecked_claim_error_was_emitted())
                }
            }
            Expr::Unary(op, inner) => match op {
                crate::ast::UnaryOp::AddrOf => {
                    let inner_ty = self.expr_type(inner, symbols);
                    Type::ConstPtr(Box::new(inner_ty))
                }
                crate::ast::UnaryOp::AddrOfMut => {
                    let inner_ty = self.expr_type(inner, symbols);
                    Type::Ptr(Box::new(inner_ty))
                }
                crate::ast::UnaryOp::Deref => {
                    let inner_ty = self.expr_type(inner, symbols);
                    match inner_ty {
                        Type::Ptr(t) | Type::ConstPtr(t) => *t,
                        _ => Type::U32,
                    }
                }
                _ => self.expr_type(inner, symbols),
            },
            Expr::Match(match_expr) => {
                let scrutinee_ty = self.expr_type(&match_expr.scrutinee, symbols);
                if let Type::Enum(_, inner_ty, _) = &scrutinee_ty {
                    *inner_ty.clone()
                } else {
                    Type::U32
                }
            }
            Expr::Block(block_expr) => {
                if let Some(ref trailing) = block_expr.block.trailing {
                    self.expr_type(trailing, symbols)
                } else {
                    Type::U32
                }
            }
            Expr::If(if_expr) => {
                if let Some(ref trailing) = if_expr.then_block.trailing {
                    self.expr_type(trailing, symbols)
                } else {
                    self.expr_type(&if_expr.else_branch, symbols)
                }
            }
            Expr::Call(func_expr, _) if consteval::is_len_call(func_expr) => Type::U32,
            Expr::Call(func_expr, _) => {
                if let Expr::Ident((name, _)) = func_expr.as_ref()
                    && let Some(fn_sym) = symbols.functions.get(name)
                {
                    return fn_sym.ret.clone().unwrap_or(Type::Void);
                }
                // Indirect call: the result type is the function pointer's
                // return type. Without this an indirect call to a narrow-
                // returning pointer is mistyped as u32, and consuming the i8/i16
                // result coerces it as i32 (`trunc i32 %r to i8`), which llc
                // rejects. Independent of the AAPCS extension work; surfaced by it.
                if let Type::Fn(_, ret) = self.expr_type(func_expr, symbols) {
                    return *ret;
                }
                Type::U32
            }
        }
    }
}

fn field_llvm_width(ty: &Type) -> usize {
    match ty {
        Type::B1 => 1,
        Type::I8 | Type::U8 | Type::B8 => 8,
        Type::I16 | Type::U16 => 16,
        Type::I32 | Type::U32 => 32,
        Type::I64 | Type::U64 => 64,
        Type::Enum(_, inner, _) => field_llvm_width(inner),
        _ => {
            debug_assert!(false, "field_llvm_width: unexpected type {ty:?}");
            32
        }
    }
}

impl IrEmitter {
    /// Truncate an i32 RMW result down to the field's LLVM type width.
    /// Returns the original register name when the field is already i32-wide.
    /// Peripheral fields wider than i32 are not supported by the i32-based RMW
    /// path and trigger a debug assertion.
    fn narrow_from_i32(&mut self, val: &str, field_ty: &Type) -> String {
        let w = field_llvm_width(field_ty);
        debug_assert!(w <= 32, "narrow_from_i32: field type wider than i32");
        if w >= 32 {
            return val.to_string();
        }
        let llvm_to = llvm_type(field_ty);
        let r = self.new_reg();
        self.line(&format!("{r} = trunc i32 {val} to {llvm_to}"));
        r
    }

    /// Convert a value of `val_ty` to i32 for use in i32 RMW math or a 32-bit
    /// volatile store. When `val_ty` is strictly wider than `field_ty`, the
    /// source is truncated to the field type first; the result is then
    /// zero-extended to i32 if it is still narrower. Both types must fit in
    /// i32; wider types are unsupported and trigger a debug assertion.
    fn widen_to_i32(&mut self, val: &str, val_ty: &Type, field_ty: &Type) -> String {
        let field_w = field_llvm_width(field_ty);
        let val_w = field_llvm_width(val_ty);
        debug_assert!(
            field_w <= 32 && val_w <= 32,
            "widen_to_i32: value or field type wider than i32"
        );
        let mut cur = val.to_string();
        let mut cur_w = val_w;
        let mut cur_llvm = llvm_type(val_ty);
        if val_w > field_w {
            let llvm_to = llvm_type(field_ty);
            let r = self.new_reg();
            self.line(&format!("{r} = trunc {cur_llvm} {cur} to {llvm_to}"));
            cur = r;
            cur_w = field_w;
            cur_llvm = llvm_to;
        }
        if cur_w < 32 {
            let r = self.new_reg();
            self.line(&format!("{r} = zext {cur_llvm} {cur} to i32"));
            return r;
        }
        cur
    }
}

/// Reduce a type to the scalar it is represented by for arithmetic/casts:
/// an enum becomes its underlying integer type; everything else is unchanged.
/// The integer-family type a scalar casts as. Enums cast as their underlying
/// integer; a `b8` is an 8-bit byte that behaves exactly like `u8` (same `i8`
/// representation, unsigned). Normalizing here lets every cast branch treat
/// `b8` as an integer instead of falling through to an invalid `bitcast … to i8`.
/// Only the cast lowering uses this; the emitted llvm types come from the
/// original type, so widths are unaffected.
fn scalar_repr(ty: &Type) -> Type {
    match ty {
        Type::Enum(_, inner, _) => (**inner).clone(),
        Type::B8 => Type::U8,
        other => other.clone(),
    }
}

/// AAPCS treatment of sub-word integers: the *caller* zero/sign-extends an
/// argument to a full 32-bit register, and the *callee* extends a sub-word
/// return. LLVM encodes that with the `zeroext`/`signext` parameter/return
/// attributes. This is a property of beemel's calling convention (AAPCS), NOT
/// of "extern-ness": beemel emits one convention for all Cortex-M code, so the
/// attribute is applied uniformly to every function signature and call site
/// (`emit_function`, the extern `declare`s, and the direct/indirect/handle call
/// sites). Gating on a boundary fails for function pointers -- at an indirect
/// call or a `define` you cannot tell whether the other end is C or beemel.
/// Applying it everywhere makes beemel's ABI == AAPCS, so every crossing (incl.
/// GCC/clang HALs like CMSIS or libopencm3, and bml callbacks invoked from C)
/// is correct; internal calls just carry a redundant, optimizer-elided mask.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AbiExt {
    None,
    Zero,
    Sign,
}

impl AbiExt {
    fn token(self) -> &'static str {
        match self {
            AbiExt::None => "",
            AbiExt::Zero => "zeroext",
            AbiExt::Sign => "signext",
        }
    }
}

/// The extension for a value of this type, derived from the LOWERED integer
/// (width + signedness) by recursing through enums and storage wrappers exactly
/// as `llvm_type` does -- so it can never disagree with the emitted type. A flat
/// match on surface variants would silently miss `repr u8` enums and wrapped
/// types. `b1`/`i1` is intentionally `None`: it cannot cross the C ABI (E356),
/// so there is no foreign expectation to meet, and internal i1 calls are
/// self-consistent without it.
fn abi_ext(ty: &Type) -> AbiExt {
    match ty {
        Type::I8 | Type::I16 => AbiExt::Sign,
        Type::U8 | Type::U16 | Type::B8 => AbiExt::Zero,
        Type::Enum(_, inner, _)
        | Type::Exclusive(inner)
        | Type::Shared(inner, _)
        | Type::Mmio(inner)
        | Type::AgentShared(inner) => abi_ext(inner),
        _ => AbiExt::None,
    }
}

/// Return-position prefix, which precedes the type (`zeroext i8 @f()`):
/// `"zeroext "` / `"signext "` (trailing space) or `""`.
fn abi_ret_prefix(ty: &Type) -> String {
    match abi_ext(ty) {
        AbiExt::None => String::new(),
        e => format!("{} ", e.token()),
    }
}

/// Parameter/argument-position suffix, which follows the type (`@f(i8 zeroext)`):
/// `" zeroext"` / `" signext"` (leading space) or `""`.
fn abi_param_suffix(ty: &Type) -> String {
    match abi_ext(ty) {
        AbiExt::None => String::new(),
        e => format!(" {}", e.token()),
    }
}

fn llvm_type(ty: &Type) -> String {
    match ty {
        Type::I8 | Type::U8 => "i8".into(),
        Type::I16 | Type::U16 => "i16".into(),
        Type::I32 | Type::U32 => "i32".into(),
        Type::I64 | Type::U64 => "i64".into(),
        Type::F16 => "half".into(),
        Type::F32 => "float".into(),
        Type::F64 => "double".into(),
        Type::B1 => "i1".into(),
        Type::B8 => "i8".into(),
        Type::Void => "void".into(),
        Type::Array(inner, size) => format!("[{} x {}]", size, llvm_type(inner)),
        Type::Ptr(_inner) => "ptr".to_string(),
        Type::ConstPtr(_inner) => "ptr".to_string(),
        Type::Fn(..) => "ptr".to_string(),
        // Linear view descriptor: { data pointer, length }. Kept as a
        // first-class aggregate (not boxed behind a pointer) so mem2reg/sroa
        // preserve pointer provenance for the verifier. Same layout for
        // readonly and mutable views.
        Type::LinearView(_, _) | Type::StridedView(_, _, _) => "{ ptr, i32 }".to_string(),
        // Ring view descriptor: { data pointer, capacity, head, len }. Same
        // SSA-transparent aggregate treatment as the linear view.
        Type::RingView(_, _, _) => "{ ptr, i32, i32, i32 }".to_string(),
        // Bit view descriptor: { byte pointer, bit_offset, len_bits }. Same
        // SSA-transparent aggregate treatment as the other views.
        Type::BitView(_) => "{ ptr, i32, i32 }".to_string(),
        Type::Exclusive(inner)
        | Type::Shared(inner, _)
        | Type::Mmio(inner)
        | Type::AgentShared(inner) => llvm_type(inner),
        Type::Null => "ptr".into(),
        // A byte-address slot is a plain i32 (it holds an address as an integer).
        Type::Addr(_) => "i32".into(),
        // Post-resolver these shouldn't appear; if they do, emit a safe i32
        // so we still produce valid (if meaningless) IR for already-broken
        // input rather than panicking. A peripheral_type handle is monomorphized
        // away (its param is dropped), so it never reaches codegen as a value.
        Type::PeripheralHandle(_) | Type::Unresolved(_) | Type::Error(_) => "i32".into(),
        Type::Struct(_, repr, fields) => {
            let inner: Vec<String> = fields.iter().map(|(_, ty)| llvm_type(ty)).collect();
            if *repr == ast::StructRepr::Packed {
                format!("<{{ {} }}>", inner.join(", "))
            } else {
                format!("{{ {} }}", inner.join(", "))
            }
        }
        Type::Enum(_, inner_ty, _) => llvm_type(inner_ty),
    }
}

fn default_value_literal(ty: &Type) -> String {
    match ty {
        Type::I8
        | Type::I16
        | Type::I32
        | Type::I64
        | Type::U8
        | Type::U16
        | Type::U32
        | Type::U64
        | Type::B1
        | Type::B8
        | Type::Addr(_) => "0".to_string(),
        Type::F16 | Type::F32 | Type::F64 => "0.0".to_string(),
        Type::Ptr(_) | Type::ConstPtr(_) | Type::Fn(..) => "null".to_string(),
        Type::Array(..)
        | Type::Struct(..)
        | Type::LinearView(_, _)
        | Type::StridedView(_, _, _)
        | Type::RingView(_, _, _)
        | Type::BitView(_) => "zeroinitializer".to_string(),
        Type::Enum(_, inner, _) => default_value_literal(inner),
        Type::Exclusive(inner)
        | Type::Shared(inner, _)
        | Type::Mmio(inner)
        | Type::AgentShared(inner) => default_value_literal(inner),
        Type::Null => "null".to_string(),
        Type::Void | Type::PeripheralHandle(_) | Type::Unresolved(_) | Type::Error(_) => {
            "0".to_string()
        }
    }
}

fn fn_ret_llvm_type(fn_def: &ast::FnDef, symbols: &SymbolTable) -> String {
    match &fn_def.ret {
        Some(ty) => llvm_type(&crate::types::resolve_type_expr(
            ty,
            &symbols.structs,
            &symbols.enums,
        )),
        None => "void".into(),
    }
}

/// Byte-swap a constant integer string for a field whose declared byte order
/// differs from the target's native order, so the emitted global initializer
/// carries the right bytes. Native-order fields and values that do not reduce to
/// a non-negative integer pass through unchanged.
fn byteswap_const(
    value: &str,
    field_ty: &Type,
    endian: crate::ast::FieldEndian,
    native: crate::arch::Endianness,
) -> String {
    if !native.swaps(endian) {
        return value.to_string();
    }
    let Ok(n) = value.parse::<u64>() else {
        return value.to_string();
    };
    let swapped = match field_ty {
        Type::U16 | Type::I16 => u64::from(u16::try_from(n & 0xFFFF).unwrap_or(0).swap_bytes()),
        Type::U32 | Type::I32 => {
            u64::from(u32::try_from(n & 0xFFFF_FFFF).unwrap_or(0).swap_bytes())
        }
        Type::U64 | Type::I64 => n.swap_bytes(),
        _ => return value.to_string(),
    };
    swapped.to_string()
}

/// Emit an LLVM constant initializer for a global of type `ty`. Needed for
/// aggregate statics (arrays): `expr_const_val` only knows scalars, so an array
/// initializer like `[1, 2, 3, 4]` would otherwise collapse to `0`. The element
/// type is taken from `ty` (so unsuffixed literals get the right width), and
/// `.inner()` sees through a storage wrapper. An aggregate initializer that just
/// names another `const` (e.g. `var s = LUT;`) is inlined to that const's
/// value via `const_defs`. Falls back to the scalar path for non-aggregate types.
fn const_init(
    ty: &Type,
    expr: &Expr,
    symbols: &SymbolTable,
    consts: &HashMap<String, ConstVal>,
    const_defs: &HashMap<String, (Type, &Expr)>,
) -> String {
    match (ty.inner(), expr) {
        (Type::Array(elem, _), Expr::ArrayInit(elems, _)) => {
            let ell = llvm_type(elem);
            let parts: Vec<String> = elems
                .iter()
                .map(|e| format!("{ell} {}", const_init(elem, e, symbols, consts, const_defs)))
                .collect();
            format!("[{}]", parts.join(", "))
        }
        (Type::Struct(struct_name, repr, fields), Expr::StructInit { fields: init, .. }) => {
            let parts: Vec<String> = fields
                .iter()
                .enumerate()
                .map(|(idx, (name, field_ty))| {
                    let value = init
                        .iter()
                        .find(|(field_name, _)| field_name.0 == *name)
                        .map_or("zeroinitializer".to_string(), |(_, value)| {
                            const_init(field_ty, value, symbols, consts, const_defs)
                        });
                    // A compile-time `@be` field has no runtime bswap to lean on,
                    // so the swapped bytes must be baked into the global constant.
                    let endian = symbols
                        .structs
                        .get(struct_name)
                        .and_then(|si| si.field_endian.get(idx))
                        .copied()
                        .unwrap_or(ast::FieldEndian::Native);
                    let value = byteswap_const(&value, field_ty, endian, symbols.target_endianness);
                    format!("{} {value}", llvm_type(field_ty))
                })
                .collect();
            if *repr == ast::StructRepr::Packed {
                format!("<{{ {} }}>", parts.join(", "))
            } else {
                format!("{{ {} }}", parts.join(", "))
            }
        }
        // An aggregate `const`/`static` initialized by naming another `const`:
        // inline that const's initializer. Emitting a bare scalar here would
        // produce invalid IR (`[N x T] 0`), so fall back to a valid zero.
        (Type::Array(..) | Type::Struct(..), Expr::Ident((name, _))) => {
            const_defs.get(name).map_or_else(
                || "zeroinitializer".to_string(),
                |(ref_ty, ref_expr)| const_init(ref_ty, ref_expr, symbols, consts, const_defs),
            )
        }
        _ => expr_const_val(ty.inner(), expr, symbols, consts, const_defs),
    }
}

fn expr_const_val(
    ty: &Type,
    expr: &Expr,
    symbols: &SymbolTable,
    consts: &HashMap<String, ConstVal>,
    const_defs: &HashMap<String, (Type, &Expr)>,
) -> String {
    match expr {
        Expr::IntLiteral(n, _, _) => format!("{n}"),
        Expr::Unary(..)
        | Expr::Binary(..)
        | Expr::Group(_)
        | Expr::Cast(_, _)
        | Expr::Ident(_)
        | Expr::SizeOf(_, _)
        | Expr::Call(_, _)
            if matches!(
                ty,
                Type::I8
                    | Type::I16
                    | Type::I32
                    | Type::I64
                    | Type::U8
                    | Type::U16
                    | Type::U32
                    | Type::U64
                    | Type::B8
                    | Type::Enum(..)
            ) =>
        {
            consteval::eval_int(expr, &IrConstEnv { symbols, consts })
                .map_or_else(|| "0".to_string(), |v| v.to_string())
        }
        Expr::FloatLiteral(f, suffix, _) => float_to_llvm(*f, *suffix),
        // A float `const` initialized by naming another float `const`: inline it.
        Expr::Ident((name, _)) if matches!(ty, Type::F16 | Type::F32 | Type::F64) => {
            const_defs.get(name).map_or_else(
                || "0.0".to_string(),
                |(ref_ty, ref_expr)| expr_const_val(ref_ty, ref_expr, symbols, consts, const_defs),
            )
        }
        Expr::BoolLiteral(b, _) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Expr::Unary(..)
        | Expr::Binary(..)
        | Expr::Group(_)
        | Expr::Cast(_, _)
        | Expr::Ident(_)
        | Expr::SizeOf(_, _)
        | Expr::Call(_, _)
            if matches!(ty, Type::B1) =>
        {
            consteval::eval_bool(expr, &IrConstEnv { symbols, consts })
                .map_or_else(|| "0".to_string(), |v| u32::from(v).to_string())
        }
        Expr::NullLiteral(_) => "zeroinitializer".into(),
        // Aggregate types must never collapse to a bare `0` (invalid IR); emit a
        // valid zero. Scalars keep the `0` fallback.
        _ if matches!(ty, Type::Array(..) | Type::Struct(..)) => "zeroinitializer".into(),
        _ => "0".into(),
    }
}

/// Constant-evaluation environment for the IR emitter: values come from the
/// [`const_values`] fixpoint, names and types from the symbol table. See
/// [`crate::consteval`] for the shared evaluator.
struct IrConstEnv<'a> {
    symbols: &'a SymbolTable,
    consts: &'a HashMap<String, ConstVal>,
}

impl consteval::Env for IrConstEnv<'_> {
    fn const_int(&self, name: &str) -> Option<i128> {
        match self.consts.get(name) {
            Some(ConstVal::Int(v)) => Some(*v),
            _ => None,
        }
    }
    fn const_bool(&self, name: &str) -> Option<bool> {
        match self.consts.get(name) {
            Some(ConstVal::Bool(b)) => Some(*b),
            _ => None,
        }
    }
    fn enum_variant(&self, enum_name: &str, variant: &str) -> Option<i128> {
        self.symbols.enum_variant_discriminant(enum_name, variant)
    }
    fn array_len(&self, name: &str) -> Option<i128> {
        self.symbols
            .consts
            .get(name)
            .map(|s| &s.ty)
            .or_else(|| self.symbols.statics.get(name).map(|s| &s.ty))
            .and_then(|ty| match ty.inner() {
                Type::Array(_, n) => Some(*n as i128),
                _ => None,
            })
    }
    fn sizeof(&self, ty: &ast::TypeExpr) -> Option<i128> {
        let t = crate::types::resolve_type_expr(ty, &self.symbols.structs, &self.symbols.enums);
        // Refuse a type with any unresolved component (a typo inside `sizeof`)
        // rather than let `element_size`'s catch-all guess a size.
        if crate::types::type_has_unresolved(&t) {
            return None;
        }
        Some(i128::from(crate::types::element_size(&t)))
    }
}

fn const_values(items: &[ast::Item], symbols: &SymbolTable) -> HashMap<String, ConstVal> {
    let mut vals = HashMap::new();
    loop {
        let mut changed = false;
        for item in items {
            if let ast::Item::ConstDef(c) = item
                && !vals.contains_key(&c.name.0)
                && let Some(v) = consteval::eval(
                    &c.value,
                    &IrConstEnv {
                        symbols,
                        consts: &vals,
                    },
                )
            {
                vals.insert(c.name.0.clone(), v);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    vals
}

/// Format a f64 as a valid LLVM IR floating-point constant.
///
/// LLVM's hex float syntax is type-specific: `double` and `float` are written
/// as the *64-bit double* bit pattern of the value (for `float` the value must
/// be exactly representable, so it is snapped through f32 first), while `half`
/// uses the `0xH` prefix followed by its 16-bit encoding. The previous version
/// left-padded the f32 bits into 64 bits, which is a different (usually wrong)
/// double value -- e.g. `1000.0f` became ~1.6e21.
fn float_to_llvm(f: f64, suffix: crate::ast::FloatSuffix) -> String {
    match suffix {
        crate::ast::FloatSuffix::H => format!("0xH{:04X}", f32_to_f16_bits(f as f32)),
        crate::ast::FloatSuffix::F | crate::ast::FloatSuffix::None => {
            format!("0x{:016X}", f64::from(f as f32).to_bits())
        }
        crate::ast::FloatSuffix::D => format!("0x{:016X}", f.to_bits()),
    }
}

/// Convert an `f32` to its IEEE-754 half-precision (binary16) bit pattern,
/// round-to-nearest-even. Rust has no stable `f16`, so this is done by hand.
fn f32_to_f16_bits(value: f32) -> u16 {
    let x = value.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = i32::try_from((x >> 23) & 0xFF).unwrap();
    let mant = x & 0x007F_FFFF;

    if exp == 0xFF {
        // Inf or NaN (preserve NaN-ness with a set mantissa bit).
        return sign | 0x7C00 | if mant != 0 { 0x0200 } else { 0 };
    }

    // Rebias the exponent from f32 (bias 127) to f16 (bias 15): e = exp - 112.
    let e = exp - 112;
    if e >= 0x1F {
        return sign | 0x7C00; // overflow -> Inf
    }
    if e <= 0 {
        // Subnormal or zero.
        if e < -10 {
            return sign; // underflow -> signed zero
        }
        let full = mant | 0x0080_0000; // restore implicit leading 1
        let shift = u32::try_from(14 - e).unwrap(); // 14..=24 for e in 0..=-10
        let mut h = full >> shift;
        let round = 1u32 << (shift - 1);
        if (full & round) != 0 && ((full & (round - 1)) != 0 || (h & 1) != 0) {
            h += 1;
        }
        return sign | h as u16;
    }

    // Normal.
    let mut h = mant >> 13;
    let mut e16 = u16::try_from(e).unwrap();
    let round = 1u32 << 12;
    if (mant & round) != 0 && ((mant & (round - 1)) != 0 || (h & 1) != 0) {
        h += 1;
        if h == 0x0400 {
            h = 0;
            e16 += 1;
            if e16 >= 0x1F {
                return sign | 0x7C00;
            }
        }
    }
    sign | (e16 << 10) | h as u16
}

/// LLVM opcode for a compound-assignment operator on an unsigned field value
/// (peripheral fields are unsigned). Comparisons/logical ops cannot appear in a
/// compound assignment, so they map to a harmless default.
fn compound_unsigned_opcode(op: crate::ast::BinaryOp) -> &'static str {
    use crate::ast::BinaryOp;
    match op {
        BinaryOp::Add | BinaryOp::AddWrap => "add",
        BinaryOp::Sub | BinaryOp::SubWrap => "sub",
        BinaryOp::Mul | BinaryOp::MulWrap => "mul",
        BinaryOp::Div => "udiv",
        BinaryOp::Mod => "urem",
        BinaryOp::BitAnd => "and",
        BinaryOp::BitOr => "or",
        BinaryOp::BitXor => "xor",
        BinaryOp::Shl => "shl",
        BinaryOp::Shr => "lshr",
        _ => "add",
    }
}

fn int_bit_width_from_suffix(suffix: crate::ast::IntSuffix) -> u32 {
    match suffix {
        crate::ast::IntSuffix::U8 | crate::ast::IntSuffix::I8 => 8,
        crate::ast::IntSuffix::U16 | crate::ast::IntSuffix::I16 => 16,
        crate::ast::IntSuffix::U32 | crate::ast::IntSuffix::I32 => 32,
        crate::ast::IntSuffix::U64 | crate::ast::IntSuffix::I64 => 64,
        crate::ast::IntSuffix::None => 32,
    }
}

fn int_bit_width(llvm_ty: &str) -> u32 {
    match llvm_ty {
        "i8" => 8,
        "i16" => 16,
        "i32" => 32,
        "i64" => 64,
        _ => 32,
    }
}

/// Escape a string for use inside LLVM IR string constant (c"...\\00").
fn escape_llvm_string(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\22"),
            '\n' => out.push_str("\\0A"),
            '\t' => out.push_str("\\09"),
            '\r' => out.push_str("\\0D"),
            '\0' => out.push_str("\\00"),
            c if c.is_ascii_graphic() || c == ' ' => out.push(c),
            c => write!(out, "\\{:02X}", c as u8).unwrap(),
        }
    }
    out
}

fn float_bit_width(llvm_ty: &str) -> u32 {
    match llvm_ty {
        "half" => 16,
        "float" => 32,
        "double" => 64,
        _ => 32,
    }
}
