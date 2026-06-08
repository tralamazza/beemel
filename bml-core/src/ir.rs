use std::collections::HashMap;
use std::fmt::Write;

use crate::arch::Arch;
use crate::ast::{self, Expr, LValue, Program, Stmt, StorageAnnotation};
use crate::consteval::{self, ConstVal};
use crate::context::Context;
use crate::resolver::{FnSymbol, SymbolTable};
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
    alias_fn_symbols: HashMap<String, String>,
    alloca_counter: u32,
    verify_mode: bool,
    current_fn_name: String,
    preempt: Option<PreemptInfo>,
    /// MMIO `(address, or_mask)` writes emitted at the start of `reset_handler`,
    /// before `.data`/`.bss` init. See `Target::startup_init`.
    pub(crate) startup_init: Vec<(u64, u64)>,
    /// Full handoff field paths (`Peripheral.REGISTER.FIELD`) whose encoding is
    /// `word_addr`: a write of a byte address gets a compiler-inserted `>> 2`,
    /// so the programmer never hand-writes the shift. Built from the target's
    /// agent handoff lists. See `doc/regions-agents-plan.md`.
    word_addr_handoffs: std::collections::HashSet<String>,
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

impl IrEmitter {
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
            alias_fn_symbols: HashMap::new(),
            alloca_counter: 0,
            verify_mode: false,
            current_fn_name: String::new(),
            preempt: None,
            startup_init: Vec::new(),
            word_addr_handoffs: std::collections::HashSet::new(),
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
            alias_fn_symbols: HashMap::new(),
            alloca_counter: 0,
            verify_mode: true,
            current_fn_name: String::new(),
            preempt: None,
            startup_init: Vec::new(),
            word_addr_handoffs: std::collections::HashSet::new(),
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
    pub fn set_startup_init(&mut self, writes: Vec<(u64, u64)>) {
        self.startup_init = writes;
    }

    /// Install the set of `word_addr` handoff field paths
    /// (`Peripheral.REGISTER.FIELD`). A write to one of these fields gets a
    /// compiler-inserted `>> 2`, so source writes the byte address directly.
    pub fn set_word_addr_handoffs(&mut self, paths: std::collections::HashSet<String>) {
        self.word_addr_handoffs = paths;
    }

    /// If `path` is a `word_addr` handoff field, emit `lshr i32 val, 2` and
    /// return the encoded register; otherwise return `val_reg` unchanged. The
    /// value is an address (i32/u32), so an unsigned shift is correct.
    fn encode_word_addr_handoff(&mut self, val_reg: &str, path: &str) -> String {
        if self.word_addr_handoffs.contains(path) {
            let enc = self.new_reg();
            self.line(&format!("{enc} = lshr i32 {val_reg}, 2"));
            enc
        } else {
            val_reg.to_string()
        }
    }

    #[must_use]
    pub fn emit(mut self, program: &Program, symbols: &SymbolTable) -> String {
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
        self.emit_alias_function_bodies(symbols);
        self.emit_string_literals();
        if self.debug {
            self.emit_debug_module_flags();
            self.out.push_str(&self.debug_metadata);
        }
        self.out
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
                    // `@align(N)` overrides the default 4-byte alignment.
                    let align = s
                        .storage
                        .iter()
                        .find_map(|a| match a {
                            ast::StorageAnnotation::Align(n) => Some(*n),
                            _ => None,
                        })
                        .unwrap_or(4);
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
                let ret_ty = match &f.ret {
                    Some(ty) => llvm_type(&crate::types::resolve_type_expr(
                        ty,
                        &symbols.structs,
                        &symbols.enums,
                    )),
                    None => "void".to_string(),
                };
                let param_strs: Vec<String> = f
                    .params
                    .iter()
                    .map(|p| {
                        llvm_type(&crate::types::resolve_type_expr(
                            &p.ty,
                            &symbols.structs,
                            &symbols.enums,
                        ))
                    })
                    .collect();
                self.line(&format!(
                    "declare {} @{}({})",
                    ret_ty,
                    f.name.0,
                    param_strs.join(", ")
                ));
                any = true;
            }
        }
        if self.verify_mode {
            // IKOS recognizes these by name and imports them as analysis
            // intrinsics. Keep them verify-only so normal builds stay clean.
            self.line("declare void @__ikos_assert(i32)");
            self.line("declare void @__ikos_forget_mem(ptr, i32)");
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
        for item in &program.items {
            if let ast::Item::FnDef(fn_def) = item {
                self.emit_function(fn_def, symbols);
            }
        }
    }

    fn emit_alias_function_bodies(&mut self, symbols: &SymbolTable) {
        for (alias, alias_info) in &symbols.import_aliases {
            let alias_symbols = symbols_with_alias_items(symbols, alias, alias_info);
            let alias_fn_symbols = alias_function_symbols(alias, &alias_info.items);
            let previous_alias_symbols =
                std::mem::replace(&mut self.alias_fn_symbols, alias_fn_symbols);

            for item in &alias_info.items {
                match item {
                    ast::Item::FnDef(fn_def) => {
                        let mut aliased_fn = fn_def.clone();
                        aliased_fn.name.0 = alias_fn_name(alias, &fn_def.name.0);
                        self.emit_function(&aliased_fn, &alias_symbols);
                    }
                    ast::Item::ExternFnDef(extern_fn) => {
                        let ret_ty = match &extern_fn.ret {
                            Some(ty) => llvm_type(&crate::types::resolve_type_expr(
                                ty,
                                &alias_symbols.structs,
                                &alias_symbols.enums,
                            )),
                            None => "void".to_string(),
                        };
                        let param_strs: Vec<String> = extern_fn
                            .params
                            .iter()
                            .map(|p| {
                                llvm_type(&crate::types::resolve_type_expr(
                                    &p.ty,
                                    &alias_symbols.structs,
                                    &alias_symbols.enums,
                                ))
                            })
                            .collect();
                        self.line(&format!(
                            "declare {} @{}({})",
                            ret_ty,
                            alias_fn_name(alias, &extern_fn.name.0),
                            param_strs.join(", ")
                        ));
                    }
                    _ => {}
                }
            }

            self.alias_fn_symbols = previous_alias_symbols;
        }
    }

    fn emit_function(&mut self, fn_def: &ast::FnDef, symbols: &SymbolTable) {
        self.counter = 0;
        self.alloca_counter = 0;
        self.current_fn_name.clone_from(&fn_def.name.0);
        let fn_sym = symbols.functions.get(&fn_def.name.0);
        let is_isr = fn_sym.is_some_and(|s| s.context.is_isr());
        let is_naked = fn_sym.is_some_and(|s| s.naked);
        let tailchain = fn_sym.is_some_and(|s| s.tailchain);
        let has_calls = tailchain && block_has_calls(&fn_def.body);
        self.current_ctx = fn_sym.map_or(Context::Thread, |s| s.context);

        let ret_ty = fn_ret_llvm_type(fn_def, symbols);
        let param_strs: Vec<String> = fn_def
            .params
            .iter()
            .map(|p| {
                let pty = llvm_type(&crate::types::resolve_type_expr(
                    &p.ty,
                    &symbols.structs,
                    &symbols.enums,
                ));
                format!("{pty} %{}", p.name.0)
            })
            .collect();

        let fn_span = fn_def.name.1;
        let dbg_fn_suffix = if self.debug {
            let id = self.new_dbg_id();
            let cu = self.cu_id.unwrap_or(0);
            let file = self.cu_file_id.unwrap_or(0);
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
                "!{id} = distinct !DISubprogram(name: \"{}\", scope: !{cu}, file: !{file}, line: {line}, type: !{st_id}, spFlags: DISPFlagDefinition, unit: !{cu})",
                fn_def.name.0
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
        self.line(&format!(
            "define {ret_ty} @{}({}) #{}{section_attr} {}{{",
            fn_def.name.0,
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

        // Alloca for parameters
        self.locals.clear();
        for param in &fn_def.params {
            let bml_type =
                crate::types::resolve_type_expr(&param.ty, &symbols.structs, &symbols.enums);
            let pty = llvm_type(&bml_type);
            let reg = self.alloca(&pty, &param.name.0);
            let dbg_sfx = self.dbg_loc(param.name.1);
            self.line(&format!(
                "store {pty} %{}, ptr {reg}{dbg_sfx}",
                param.name.0
            ));
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

        // Emit body
        self.current_fn_params = fn_def
            .params
            .iter()
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
                let cond_reg = self.emit_expr(&if_stmt.cond, symbols, fn_name);
                let then_lbl = self.new_label("then");
                let else_lbl = self.new_label("else");
                let end_lbl = self.new_label("endif");

                self.line(&format!(
                    "br i1 {cond_reg}, label %{then_lbl}, label %{else_lbl}"
                ));
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

            Stmt::While(while_stmt) => {
                let cond_lbl = self.new_label("while_cond");
                let body_lbl = self.new_label("while_body");
                let end_lbl = self.new_label("while_end");

                self.line(&format!("br label %{cond_lbl}"));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{cond_lbl}:"));
                self.indent += 1;
                let cond_reg = self.emit_expr(&while_stmt.cond, symbols, fn_name);
                self.line(&format!(
                    "br i1 {cond_reg}, label %{body_lbl}, label %{end_lbl}"
                ));
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
                let cond_reg = self.emit_expr(&assume.cond, symbols, fn_name);
                let ok_lbl = self.new_label("assume_ok");
                let unreach_lbl = self.new_label("assume_unreach");
                self.line(&format!(
                    "br i1 {cond_reg}, label %{ok_lbl}, label %{unreach_lbl}"
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
                    let needs_cs = self.static_needs_critical_section(name, symbols);
                    if needs_cs {
                        crate::arch::arm::emit_critical_enter(self);
                    }
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ty}, ptr @{name}"));
                    if needs_cs {
                        crate::arch::arm::emit_critical_leave(self);
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
                if let Some(symbol) = self.alias_fn_symbols.get(name).cloned() {
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = getelementptr i8, ptr @{symbol}, i32 0"));
                    return reg;
                }
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
                        let inner_reg = self.emit_expr(inner, symbols, fn_name);
                        let pointee_ty = match self.expr_type(inner, symbols) {
                            Type::Ptr(inner) | Type::ConstPtr(inner) => *inner,
                            _ => crate::types::Type::I32, // fallback
                        };
                        let llty = llvm_type(&pointee_ty);
                        let dbg = self.dbg_loc(expr.span());
                        let reg = self.new_reg();
                        self.line(&format!("{reg} = load {llty}, ptr {inner_reg}{dbg}"));
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
                                self.line(&format!("{reg} = sub {inner_llvm} 0, {inner_reg}"));
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
                    BinaryOp::And => ("and", "i1"),
                    BinaryOp::Or => ("or", "i1"),
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
                // For logical ops, result is i1
                let is_logical = matches!(op, BinaryOp::And | BinaryOp::Or);

                let dbg = self.dbg_loc(expr.span());
                if cmp_result {
                    let (cmd, cond) = (llvm_op, result_ty);
                    self.line(&format!(
                        "{reg} = {cmd} {cond} {lty} {left_reg}, {right_reg}{dbg}"
                    ));
                } else if is_logical {
                    self.line(&format!(
                        "{reg} = {llvm_op} {result_ty} {left_reg}, {right_reg}{dbg}"
                    ));
                } else {
                    self.line(&format!(
                        "{reg} = {llvm_op} {lty} {left_reg}, {right_reg}{dbg}"
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
                    self.alias_fn_symbols
                        .get(name)
                        .cloned()
                        .or_else(|| symbols.functions.contains_key(name).then(|| name.clone()))
                } else if let Expr::FieldAccess(base, field) = func_expr.as_ref()
                    && let Expr::Ident((alias, _)) = base.as_ref()
                    && let Some(alias_info) = symbols.import_aliases.get(alias)
                    && matches!(
                        alias_info.exports.get(&field.0),
                        Some(ast::Item::FnDef(_) | ast::Item::ExternFnDef(_))
                    )
                {
                    Some(alias_fn_name(alias, &field.0))
                } else {
                    None
                };

                if let Some(direct_name) = direct_name {
                    let param_tys: Option<Vec<Type>> = symbols
                        .functions
                        .get(&direct_name)
                        .map(|s| s.params.iter().map(|(_, t)| t.clone()).collect());
                    let mut arg_parts = Vec::new();
                    for (i, arg) in args.iter().enumerate() {
                        let reg = self.emit_expr(arg, symbols, fn_name);
                        let ty = self.expr_type(arg, symbols);
                        // Pass each argument at its parameter's width so an i32
                        // literal lands correctly in a narrower parameter slot.
                        if let Some(pty) = param_tys.as_ref().and_then(|p| p.get(i)) {
                            let reg = self.coerce_int(reg, &ty, pty);
                            arg_parts.push(format!("{} {reg}", llvm_type(pty)));
                        } else {
                            arg_parts.push(format!("{} {reg}", llvm_type(&ty)));
                        }
                    }
                    let arg_str = arg_parts.join(", ");
                    let fn_sym = symbols.functions.get(&direct_name);

                    let ret_ty = fn_sym
                        .and_then(|s| s.ret.as_ref())
                        .map_or_else(|| alias_call_return_type(func_expr, symbols), llvm_type);

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
                            "{reg} = call {ret_ty} @{direct_name}({arg_str}){dbg_sfx}"
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
                        if let Some(pty) = param_tys.as_ref().and_then(|p| p.get(i)) {
                            let reg = self.coerce_int(reg, &ty, pty);
                            arg_parts.push(format!("{} {reg}", llvm_type(pty)));
                        } else {
                            arg_parts.push(format!("{} {reg}", llvm_type(&ty)));
                        }
                    }
                    let arg_str = arg_parts.join(", ");

                    let ret_ty = match &callee_ty {
                        Type::Fn(_, ret) => llvm_type(ret),
                        _ => "void".to_string(),
                    };

                    if ret_ty == "void" {
                        self.line(&format!("call void {callee_reg}({arg_str}){dbg_sfx}"));
                        String::new()
                    } else {
                        let reg = self.new_reg();
                        self.line(&format!(
                            "{reg} = call {ret_ty} {callee_reg}({arg_str}){dbg_sfx}"
                        ));
                        reg
                    }
                }
            }

            Expr::FieldAccess(base, field) => {
                // Handle peripheral register access: GPIOA.ODR → volatile load
                if let Expr::Ident((periph_name, _)) = base.as_ref()
                    && let Some(p) = symbols.peripherals.get(periph_name)
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
                    && let Some(p) = symbols.peripherals.get(periph_name)
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
                let base_ty = self.expr_type(base, symbols);
                let dbg = self.dbg_loc(expr.span());
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
                    // Pointer index: GEP + load
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
                    self.line(&format!("{reg} = load {ll_elem}, ptr {gep}{dbg}"));
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
                let cond_reg = self.emit_expr(&if_expr.cond, symbols, fn_name);
                let then_lbl = self.new_label("if_then");
                let else_lbl = self.new_label("if_else");
                let end_lbl = self.new_label("if_end");

                self.line(&format!(
                    "br i1 {cond_reg}, label %{then_lbl}, label %{else_lbl}"
                ));
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
            && let Some(p) = symbols.peripherals.get(periph_name)
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
                    self.emit_store_target(&lv, symbols, fn_name, &ret, ty, target.span());
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
                        self.emit_store_target(&lv, symbols, fn_name, &ev, ty, target.span());
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
                    && let Some(p) = symbols.peripherals.get(periph_name)
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
            LValue::Index(..) => None,
        }
    }

    /// Emit a store to an lvalue. Returns the register holding the stored value.
    fn emit_store_target(
        &mut self,
        lval: &LValue,
        symbols: &SymbolTable,
        fn_name: &str,
        val_reg: &str,
        val_ty: &Type,
        dbg_span: Span,
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
                    let needs_cs = self.static_needs_critical_section(name, symbols);
                    let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &target_ty);
                    if needs_cs {
                        crate::arch::arm::emit_critical_enter(self);
                    }
                    self.line(&format!("store {ty} {val_reg}, ptr @{name}{dbg}"));
                    if needs_cs {
                        crate::arch::arm::emit_critical_leave(self);
                    }
                    return val_reg;
                }
                val_reg.to_string()
            }
            LValue::Field(base, field) => {
                // Peripheral register write: GPIOA.ODR = val
                if let LValue::Name((periph_name, _)) = base.as_ref()
                    && let Some(p) = symbols.peripherals.get(periph_name)
                    && let Some(reg) = p.regs.get(&field.0)
                {
                    let addr = p.base_addr + reg.offset;
                    self.line(&format!(
                        "store volatile i32 {val_reg}, ptr inttoptr ({ptr_ty} {addr} to ptr){dbg}",
                        ptr_ty = self.ptr_type()
                    ));
                    return val_reg.to_string();
                }
                // Peripheral field write: GPIOA.ODR.ODR3 = val
                if let LValue::Field(inner_base, reg_field) = base.as_ref()
                    && let LValue::Name((periph_name, _)) = inner_base.as_ref()
                    && let Some(p) = symbols.peripherals.get(periph_name)
                    && let Some(reg) = p.regs.get(&reg_field.0)
                    && let Some(field_def) = reg.fields.get(&field.0)
                {
                    let addr = p.base_addr + reg.offset;
                    let handoff_path = format!("{periph_name}.{}.{}", reg_field.0, field.0);
                    // Bit-band: single-bit field within bit-band region. A
                    // word_addr handoff field is 30 bits wide, so it never takes
                    // this single-bit path -- the encoding lives in the RMW path.
                    if self.has_bitband
                        && let Some(alias) =
                            crate::arch::arm::bitband_alias(addr, &field_def.bit_spec)
                    {
                        let alias_val = self.widen_to_i32(val_reg, val_ty, &field_def.ty);
                        self.line(&format!(
                            "store volatile i32 {alias_val}, ptr inttoptr ({ptr_ty} {alias} to ptr){dbg}",
                            ptr_ty = self.arch.ptr_type()
                        ));
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
                    // word_addr handoff: the field holds (byte address >> 2).
                    // Encode the widened (i32) address so source writes the byte
                    // address; the field's bit position (`shl`, below) re-aligns
                    // it. This removes the hand-written `>> 2`.
                    let wide_val = self.encode_word_addr_handoff(&widened, &handoff_path);
                    // shift new value into position
                    let shifted = self.new_reg();
                    if shift > 0 {
                        self.line(&format!("{shifted} = shl i32 {wide_val}, {shift}"));
                    } else {
                        self.line(&format!("{shifted} = add i32 {wide_val}, 0"));
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
                let idx_reg = self.emit_expr(index, symbols, fn_name);
                let idx_ty = self.expr_type(index, symbols);
                let gep = self.new_reg();
                let ll_elem = llvm_type(&elem_ty);
                self.line(&format!(
                    "{gep} = getelementptr {ll_elem}, ptr {base_ptr}, {} {idx_reg}",
                    llvm_type(&idx_ty)
                ));
                let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &elem_ty);
                self.line(&format!("store {ll_elem} {val_reg}, ptr {gep}{dbg}"));
                val_reg
            }
            LValue::Deref(inner) => {
                let ptr_reg = self.emit_expr(inner, symbols, fn_name);
                let inner_ty = self.expr_type(inner, symbols);
                let pointee_ty = match &inner_ty {
                    Type::Ptr(t) | Type::ConstPtr(t) => (**t).clone(),
                    _ => crate::types::Type::I32,
                };
                let llty = llvm_type(&pointee_ty);
                let val_reg = self.coerce_int(val_reg.to_string(), val_ty, &pointee_ty);
                self.line(&format!("store {llty} {val_reg}, ptr {ptr_reg}{dbg}"));
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

    fn static_needs_critical_section(&self, name: &str, symbols: &SymbolTable) -> bool {
        if let Some(sym) = symbols.statics.get(name) {
            for ann in &sym.storage {
                if let StorageAnnotation::Shared(ceiling) = ann {
                    return self.current_ctx.needs_critical_section(*ceiling);
                }
            }
        }
        false
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
        let phys = self.ring_physical_index(agg, ty, cap_hint, &sum);
        (ptr_field, phys)
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
            | Type::Dma(inner)
            | Type::External(inner) => {
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
                if let Some(symbol) = self.alias_fn_symbols.get(name)
                    && let Some(fn_sym) = symbols.functions.get(symbol)
                {
                    return fn_sym.fn_pointer_type();
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
                // Peripheral field type lookup
                if let Expr::FieldAccess(inner, reg_field) = base.as_ref()
                    && let Expr::Ident((periph_name, _)) = inner.as_ref()
                    && let Some(p) = symbols.peripherals.get(periph_name)
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
                if let Expr::FieldAccess(base, field) = func_expr.as_ref()
                    && let Expr::Ident((alias, _)) = base.as_ref()
                    && let Some(alias_info) = symbols.import_aliases.get(alias)
                    && let Some(item) = alias_info.exports.get(&field.0)
                {
                    let (structs, enums) = crate::types::alias_type_defs(
                        &alias_info.items,
                        &symbols.structs,
                        &symbols.enums,
                    );
                    let ret = match item {
                        ast::Item::FnDef(f) => f.ret.as_ref(),
                        ast::Item::ExternFnDef(f) => f.ret.as_ref(),
                        _ => None,
                    };
                    return ret.map_or(Type::Void, |ty| {
                        crate::types::resolve_type_expr(ty, &structs, &enums)
                    });
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
        | Type::Dma(inner)
        | Type::External(inner) => llvm_type(inner),
        Type::Null => "ptr".into(),
        // Post-resolver these shouldn't appear; if they do, emit a safe i32
        // so we still produce valid (if meaningless) IR for already-broken
        // input rather than panicking.
        Type::Unresolved(_) | Type::Error(_) => "i32".into(),
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
        | Type::B8 => "0".to_string(),
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
        | Type::Dma(inner)
        | Type::External(inner) => default_value_literal(inner),
        Type::Null => "null".to_string(),
        Type::Void | Type::Unresolved(_) | Type::Error(_) => "0".to_string(),
    }
}

fn alias_fn_name(alias: &str, name: &str) -> String {
    format!("__bml.alias.{alias}.{name}")
}

fn alias_function_symbols(alias: &str, items: &[ast::Item]) -> HashMap<String, String> {
    let mut symbols = HashMap::new();
    for item in items {
        match item {
            ast::Item::FnDef(f) => {
                symbols.insert(f.name.0.clone(), alias_fn_name(alias, &f.name.0));
            }
            ast::Item::ExternFnDef(f) => {
                symbols.insert(f.name.0.clone(), alias_fn_name(alias, &f.name.0));
            }
            _ => {}
        }
    }
    symbols
}

fn alias_call_return_type(func_expr: &Expr, symbols: &SymbolTable) -> String {
    if let Expr::FieldAccess(base, field) = func_expr
        && let Expr::Ident((alias, _)) = base.as_ref()
        && let Some(alias_info) = symbols.import_aliases.get(alias)
        && let Some(item) = alias_info.exports.get(&field.0)
    {
        let (structs, enums) =
            crate::types::alias_type_defs(&alias_info.items, &symbols.structs, &symbols.enums);
        let ret = match item {
            ast::Item::FnDef(f) => f.ret.as_ref(),
            ast::Item::ExternFnDef(f) => f.ret.as_ref(),
            _ => None,
        };
        return ret.map_or_else(
            || "void".to_string(),
            |ty| llvm_type(&crate::types::resolve_type_expr(ty, &structs, &enums)),
        );
    }
    "void".to_string()
}

fn symbols_with_alias_items(
    symbols: &SymbolTable,
    alias: &str,
    alias_info: &crate::imports::AliasInfo,
) -> SymbolTable {
    let (structs, enums) =
        crate::types::alias_type_defs(&alias_info.items, &symbols.structs, &symbols.enums);
    let mut alias_symbols = symbols.clone();
    alias_symbols.structs = structs;
    alias_symbols.enums = enums;

    for item in &alias_info.items {
        match item {
            ast::Item::FnDef(f) => {
                let sym = fn_symbol_from_fn_def(f, &alias_symbols.structs, &alias_symbols.enums);
                alias_symbols
                    .functions
                    .insert(f.name.0.clone(), sym.clone());
                alias_symbols
                    .functions
                    .insert(alias_fn_name(alias, &f.name.0), sym);
            }
            ast::Item::ExternFnDef(f) => {
                let sym = fn_symbol_from_extern_fn(f, &alias_symbols.structs, &alias_symbols.enums);
                alias_symbols
                    .functions
                    .insert(f.name.0.clone(), sym.clone());
                alias_symbols
                    .functions
                    .insert(alias_fn_name(alias, &f.name.0), sym);
            }
            _ => {}
        }
    }

    alias_symbols
}

fn fn_symbol_from_fn_def(
    f: &ast::FnDef,
    structs: &HashMap<String, crate::types::StructInfo>,
    enums: &crate::types::EnumDefs,
) -> FnSymbol {
    let context = if let Some(isr) = &f.isr {
        Context::Isr(isr.priority)
    } else {
        context_from_ast(&f.context)
    };
    let params = f
        .params
        .iter()
        .map(|p| {
            (
                p.name.0.clone(),
                crate::types::resolve_type_expr(&p.ty, structs, enums),
            )
        })
        .collect();
    let ret = f
        .ret
        .as_ref()
        .map(|ty| crate::types::resolve_type_expr(ty, structs, enums));

    FnSymbol {
        context,
        params,
        ret,
        isr_label: f.isr.as_ref().and_then(|i| i.label.clone()),
        naked: f.naked,
        section: f.section.clone(),
        tailchain: f.isr.as_ref().is_some_and(|i| i.tailchain),
        has_calls: false,
        local_frame: 0,
        callees: Vec::new(),
        max_depth: 0,
    }
}

fn fn_symbol_from_extern_fn(
    f: &ast::ExternFnDef,
    structs: &HashMap<String, crate::types::StructInfo>,
    enums: &crate::types::EnumDefs,
) -> FnSymbol {
    let context = if let Some(isr) = &f.isr {
        Context::Isr(isr.priority)
    } else if let Some(ctx) = &f.context {
        context_from_ast(ctx)
    } else {
        Context::Any
    };
    let params = f
        .params
        .iter()
        .map(|p| {
            (
                p.name.0.clone(),
                crate::types::resolve_type_expr(&p.ty, structs, enums),
            )
        })
        .collect();
    let ret = f
        .ret
        .as_ref()
        .map(|ty| crate::types::resolve_type_expr(ty, structs, enums));

    FnSymbol {
        context,
        params,
        ret,
        isr_label: f.isr.as_ref().and_then(|i| i.label.clone()),
        naked: false,
        section: None,
        tailchain: false,
        has_calls: false,
        local_frame: 0,
        callees: Vec::new(),
        max_depth: 0,
    }
}

fn context_from_ast(ctx: &ast::ContextExpr) -> Context {
    match ctx {
        ast::ContextExpr::Thread => Context::Thread,
        ast::ContextExpr::Any => Context::Any,
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
        if matches!(t, Type::Unresolved(_)) {
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
        BinaryOp::Add => "add",
        BinaryOp::Sub => "sub",
        BinaryOp::Mul => "mul",
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
