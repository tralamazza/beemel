use std::collections::HashMap;
use std::fmt::Write;

use crate::arch::Arch;
use crate::ast::{self, Expr, LValue, Program, Stmt, StorageAnnotation};
use crate::context::Context;
use crate::resolver::{FnSymbol, SymbolTable};
use crate::source::{SourceMap, Span};
use crate::types::Type;

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
        _ => false,
    }
}

fn stmt_has_calls(stmt: &ast::Stmt) -> bool {
    match stmt {
        ast::Stmt::VarDecl(decl) => expr_has_calls(&decl.init),
        ast::Stmt::Assign(assign) => expr_has_calls(&assign.value),
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
                || block_has_calls(&for_stmt.body)
        }
        ast::Stmt::Asm(_) | ast::Stmt::Break(_) | ast::Stmt::Continue(_) => false,
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
        self.line("");
    }

    // ─── globals ─────────────────────────────────────────────────────

    fn emit_global_declarations(&mut self, program: &Program, symbols: &SymbolTable) {
        for item in &program.items {
            match item {
                ast::Item::StaticDef(s) => {
                    let llvm_ty = llvm_type(&crate::types::resolve_type_expr(
                        &s.ty,
                        &symbols.structs,
                        &symbols.enums,
                    ));
                    let init_val = if let Some(init) = &s.init {
                        expr_const_val(init)
                    } else {
                        "zeroinitializer".to_string()
                    };
                    let section_attr = s
                        .storage
                        .iter()
                        .find_map(|a| {
                            if let ast::StorageAnnotation::Section(name) = a {
                                Some(format!(", section \"{name}\""))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    self.line(&format!(
                        "@{} = global {} {}{section_attr}, align 4",
                        s.name.0, llvm_ty, init_val
                    ));
                }
                ast::Item::ConstDef(c) => {
                    let llvm_ty = llvm_type(&crate::types::resolve_type_expr(
                        &c.ty,
                        &symbols.structs,
                        &symbols.enums,
                    ));
                    let val = expr_const_val(&c.value);
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
                    crate::types::resolve_type_expr(ty_ann, &symbols.structs, &symbols.enums)
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
                let bml_type = self.expr_type(&for_stmt.start, symbols);
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
                self.collect_and_emit_allocas_block(&w.body, symbols);
            }
            Stmt::Loop(l) => {
                self.collect_and_emit_allocas_block(&l.body, symbols);
            }
            Stmt::If(i) => {
                self.collect_and_emit_allocas_block(&i.then_block, symbols);
                if let Some(else_branch) = &i.else_branch {
                    self.collect_and_emit_allocas_stmt(else_branch, symbols);
                }
            }
            Stmt::Match(m) => {
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
                self.collect_and_emit_allocas_expr(&assign.value, symbols);
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
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm(_) => {}
        }
    }

    fn collect_and_emit_allocas_expr(&mut self, expr: &Expr, symbols: &SymbolTable) {
        match expr {
            Expr::Block(block_expr) => {
                self.collect_and_emit_allocas_block(&block_expr.block, symbols);
            }
            Expr::If(if_expr) => {
                self.collect_and_emit_allocas_block(&if_expr.then_block, symbols);
                self.collect_and_emit_allocas_expr(&if_expr.else_branch, symbols);
            }
            Expr::Match(match_expr) => {
                for arm in &match_expr.arms {
                    self.collect_and_emit_allocas_block(&arm.body, symbols);
                }
            }
            Expr::Unary(_, inner) => self.collect_and_emit_allocas_expr(inner, symbols),
            Expr::Binary(left, _, right) => {
                self.collect_and_emit_allocas_expr(left, symbols);
                self.collect_and_emit_allocas_expr(right, symbols);
            }
            Expr::Call(_, args) => {
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
                let init_reg = self.emit_expr(&vd.init, symbols, fn_name);
                let init_ty = self.expr_type(&vd.init, symbols);
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
                let target =
                    self.emit_store_target(&assign.target, symbols, fn_name, &val_reg, &val_ty);
                (Some(target), false)
            }

            Stmt::Expr(expr) => (Some(self.emit_expr(expr, symbols, fn_name)), false),

            Stmt::Asm(asm_stmt) => {
                let escaped = asm_stmt
                    .asm_text
                    .replace('\\', "\\\\")
                    .replace('"', "\\22")
                    .replace('\n', "\\0A");
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
                    let ty = llvm_type(&self.expr_type(val, symbols));
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
                let start_reg = self.emit_expr(&for_stmt.start, symbols, fn_name);
                let end_reg = self.emit_expr(&for_stmt.end, symbols, fn_name);
                let bml_type = self.expr_type(&for_stmt.start, symbols);
                let ty = llvm_type(&bml_type);
                let alloca = self
                    .locals
                    .get(&for_stmt.var.0)
                    .expect("for var should have entry alloca")
                    .alloca
                    .clone();
                self.line(&format!("store {ty} {start_reg}, ptr {alloca}"));

                let cond_lbl = self.new_label("for_cond");
                let body_lbl = self.new_label("for_body");
                let end_lbl = self.new_label("for_end");

                self.line(&format!("br label %{cond_lbl}"));
                self.line("");

                self.indent -= 1;
                self.line(&format!("{cond_lbl}:"));
                self.indent += 1;
                let reg = self.new_reg();
                self.line(&format!("{reg} = load {ty}, ptr {alloca}"));
                let cmp_reg = self.new_reg();
                let (cmp_op, cmp_ty) =
                    if crate::types::is_int(&self.expr_type(&for_stmt.start, symbols))
                        && matches!(
                            self.expr_type(&for_stmt.start, symbols),
                            Type::I8 | Type::I16 | Type::I32 | Type::I64
                        )
                    {
                        ("icmp slt", ty.as_str())
                    } else {
                        ("icmp ult", ty.as_str())
                    };
                self.line(&format!("{cmp_reg} = {cmp_op} {cmp_ty} {reg}, {end_reg}"));
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
                    Some(cond_lbl.as_str()),
                );
                if !body_term {
                    let inc_reg = self.new_reg();
                    self.line(&format!("{inc_reg} = add {ty} {reg}, 1"));
                    self.line(&format!("store {ty} {inc_reg}, ptr {alloca}"));
                    self.line(&format!("br label %{cond_lbl}"));
                }
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
                let width = int_bit_width_from_suffix(*suffix);
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
                        let reg = self.new_reg();
                        self.line(&format!("{reg} = load {llty}, ptr {inner_reg}"));
                        reg
                    }
                    UnaryOp::AddrOf | UnaryOp::AddrOfMut => {
                        // Take address: return pointer to the lvalue without loading
                        self.emit_lvalue_ptr(inner, symbols)
                    }
                    _ => {
                        let inner_reg = self.emit_expr(inner, symbols, fn_name);
                        let reg = self.new_reg();
                        match op {
                            UnaryOp::Neg => {
                                self.line(&format!("{reg} = sub i32 0, {inner_reg}"));
                            }
                            UnaryOp::Not => {
                                self.line(&format!("{reg} = xor i1 {inner_reg}, true"));
                            }
                            UnaryOp::BitNot => {
                                self.line(&format!("{reg} = xor i32 {inner_reg}, -1"));
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
                // Types are guaranteed same by checker when strict mode is on
                let lty = llvm_type(&left_ty);
                let reg = self.new_reg();

                let (llvm_op, result_ty) = match op {
                    BinaryOp::Add => ("add", lty.as_str()),
                    BinaryOp::Sub => ("sub", lty.as_str()),
                    BinaryOp::Mul => ("mul", lty.as_str()),
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
                    BinaryOp::And | BinaryOp::Or => ("and", "i1"),
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

                if cmp_result {
                    let (cmd, cond) = (llvm_op, result_ty);
                    self.line(&format!(
                        "{reg} = {cmd} {cond} {lty} {left_reg}, {right_reg}"
                    ));
                } else if is_logical {
                    self.line(&format!(
                        "{reg} = {llvm_op} {result_ty} {left_reg}, {right_reg}"
                    ));
                } else {
                    self.line(&format!("{reg} = {llvm_op} {lty} {left_reg}, {right_reg}"));
                }
                reg
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
                    let fn_sym = symbols.functions.get(&direct_name);
                    let mut arg_parts = Vec::new();
                    for arg in args {
                        let reg = self.emit_expr(arg, symbols, fn_name);
                        let ty = self.expr_type(arg, symbols);
                        let llty = llvm_type(&ty);
                        arg_parts.push(format!("{llty} {reg}"));
                    }
                    let arg_str = arg_parts.join(", ");

                    let ret_ty = fn_sym
                        .and_then(|s| s.ret.as_ref())
                        .map_or_else(|| alias_call_return_type(func_expr, symbols), llvm_type);

                    if ret_ty == "void" {
                        self.line(&format!("call void @{direct_name}({arg_str}){dbg_sfx}"));
                        "%void".to_string()
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

                    let mut arg_parts = Vec::new();
                    for arg in args {
                        let reg = self.emit_expr(arg, symbols, fn_name);
                        let ty = self.expr_type(arg, symbols);
                        let llty = llvm_type(&ty);
                        arg_parts.push(format!("{llty} {reg}"));
                    }
                    let arg_str = arg_parts.join(", ");

                    let ret_ty = match &callee_ty {
                        Type::Fn(_, ret) => llvm_type(ret),
                        _ => "void".to_string(),
                    };

                    let reg = self.new_reg();
                    if ret_ty == "void" {
                        self.line(&format!("call void {callee_reg}({arg_str}){dbg_sfx}"));
                    } else {
                        self.line(&format!(
                            "{reg} = call {ret_ty} {callee_reg}({arg_str}){dbg_sfx}"
                        ));
                    }
                    reg
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
                if let Type::Struct(_name, fields) = &base_ty
                    && let Some(idx) = fields.iter().position(|(n, _)| n == &field.0)
                {
                    let base_reg = self.emit_expr(base, symbols, fn_name);
                    let struct_llvm_ty = llvm_type(&base_ty);
                    let reg = self.new_reg();
                    self.line(&format!(
                        "{reg} = extractvalue {struct_llvm_ty} {base_reg}, {idx}"
                    ));
                    return reg;
                }
                // Pointer to struct field access: GEP + load
                if let Type::Ptr(inner) | Type::ConstPtr(inner) = &base_ty
                    && let Type::Struct(_name, fields) = inner.as_ref()
                    && let Some(idx) = fields.iter().position(|(n, _)| n == &field.0)
                {
                    let base_ptr = self.emit_expr(base, symbols, fn_name);
                    let struct_llvm_ty = llvm_type(inner);
                    let gep = self.new_reg();
                    self.line(&format!(
                        "{gep} = getelementptr {struct_llvm_ty}, ptr {base_ptr}, i32 0, i32 {idx}"
                    ));
                    let field_ty = &fields[idx].1;
                    let ll_field = llvm_type(field_ty);
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ll_field}, ptr {gep}"));
                    return reg;
                }
                // Fallback: struct field access via GEP
                self.emit_expr(base, symbols, fn_name);
                let reg = self.new_reg();
                self.line(&format!("{reg} = add i32 0, 0  ; field: {}", field.0));
                reg
            }

            Expr::Index(base, index) => {
                let base_ty = self.expr_type(base, symbols);
                if crate::types::is_ptr(&base_ty) {
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
                        "{gep} = getelementptr {ll_elem}, ptr {base_reg}, {} {idx_reg}",
                        llvm_type(&idx_ty)
                    ));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ll_elem}, ptr {gep}"));
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
                        "{gep} = getelementptr {ll_elem}, ptr {base_ptr}, {} {idx_reg}",
                        llvm_type(&idx_ty)
                    ));
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = load {ll_elem}, ptr {gep}"));
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
                if crate::types::is_int(&inner_ty) && crate::types::is_int(&target_ty) {
                    let inner_bits = int_bit_width(&inner_llvm);
                    let target_bits = int_bit_width(&llvm_target);
                    match target_bits.cmp(&inner_bits) {
                        std::cmp::Ordering::Greater => {
                            // Widening -- signed vs unsigned
                            let ext_op =
                                if matches!(inner_ty, Type::I8 | Type::I16 | Type::I32 | Type::I64)
                                {
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
                } else if crate::types::is_float(&inner_ty) && crate::types::is_float(&target_ty) {
                    let inner_bits = float_bit_width(&inner_llvm);
                    let target_bits = float_bit_width(&llvm_target);
                    if target_bits > inner_bits {
                        self.line(&format!(
                            "{reg} = fpext {inner_llvm} {inner_reg} to {llvm_target}"
                        ));
                    } else {
                        self.line(&format!(
                            "{reg} = fptrunc {inner_llvm} {inner_reg} to {llvm_target}"
                        ));
                    }
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
                let then_val = if then_term {
                    None
                } else if let Some(ref trailing) = if_expr.then_block.trailing {
                    Some(self.emit_expr(trailing, symbols, fn_name))
                } else {
                    Some(default_value_literal(&phi_bml_ty))
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
                    self.line(&format!(
                        "{result} = phi {phi_llvm_ty} [ {then_val}, %{then_lbl} ], [ {else_val}, %{else_edge_label} ]"
                    ));
                    result
                }
            }
            Expr::Match(match_expr) => {
                let Some(MatchDispatch {
                    end_lbl,
                    ll_ty,
                    arm_labels,
                    default_lbl,
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
                    phi_pairs.push((arm_val, arm_lbl));
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
                let struct_fields = symbols
                    .structs
                    .get(struct_name)
                    .cloned()
                    .unwrap_or_default();
                let struct_llvm_ty =
                    llvm_type(&Type::Struct(struct_name.clone(), struct_fields.clone()));
                let alloca = self.alloca(&struct_llvm_ty, &format!("struct_{struct_name}"));
                // Store each field via GEP
                for (idx, (fname, ftype)) in struct_fields.iter().enumerate() {
                    if let Some((_, init_expr)) = fields.iter().find(|(n, _)| n.0 == *fname) {
                        let init_reg = self.emit_expr(init_expr, symbols, fn_name);
                        let ll_field = llvm_type(ftype);
                        let gep = self.new_reg();
                        self.line(&format!(
                            "{gep} = getelementptr {struct_llvm_ty}, ptr {alloca}, i32 0, i32 {idx}"
                        ));
                        self.line(&format!("store {ll_field} {init_reg}, ptr {gep}"));
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

    /// Return a pointer to an expression without loading its value.
    /// Used by AddrOf/AddrOfMut. Returns SSA register holding a ptr.
    fn emit_lvalue_ptr(&mut self, expr: &Expr, symbols: &SymbolTable) -> String {
        match expr {
            Expr::Ident((name, _)) => {
                // Local variable: return the alloca pointer
                if let Some(info) = self.locals.get(name).cloned() {
                    let reg = self.new_reg();
                    self.line(&format!(
                        "{reg} = getelementptr i8, ptr {}, i32 0",
                        info.alloca
                    ));
                    return reg;
                }
                // Static: return a pointer to the global
                if symbols.statics.contains_key(name) {
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = getelementptr i8, ptr @{name}, i32 0"));
                    return reg;
                }
                // Peripheral: return inttoptr of the base address
                if let Some(p) = symbols.peripherals.get(name) {
                    let reg = self.new_reg();
                    let ptr_ty = self.ptr_type();
                    self.line(&format!("{reg} = inttoptr {ptr_ty} {} to ptr", p.base_addr));
                    return reg;
                }
                // Function: return a pointer to the function
                if symbols.functions.contains_key(name) {
                    let reg = self.new_reg();
                    self.line(&format!("{reg} = getelementptr i8, ptr @{name}, i32 0"));
                    return reg;
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
                if let Type::Struct(_, fields) = &base_ty
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
                if let Type::Struct(_, fields) = &base_ty {
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
    ) -> String {
        match lval {
            LValue::Name((name, _)) => {
                // Local variable
                if let Some(info) = self.locals.get(name) {
                    self.line(&format!(
                        "store {} {val_reg}, ptr {}",
                        info.llvm_ty, info.alloca
                    ));
                    return val_reg.to_string();
                }
                // Static
                if let Some(sym) = symbols.statics.get(name) {
                    let ty = llvm_type(sym.ty.inner());
                    let needs_cs = self.static_needs_critical_section(name, symbols);
                    if needs_cs {
                        crate::arch::arm::emit_critical_enter(self);
                    }
                    self.line(&format!("store {ty} {val_reg}, ptr @{name}"));
                    if needs_cs {
                        crate::arch::arm::emit_critical_leave(self);
                    }
                    return val_reg.to_string();
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
                        "store volatile i32 {val_reg}, ptr inttoptr ({ptr_ty} {addr} to ptr)",
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
                    // Bit-band: single-bit field within bit-band region
                    if self.has_bitband
                        && let Some(alias) =
                            crate::arch::arm::bitband_alias(addr, &field_def.bit_spec)
                    {
                        let alias_val = self.widen_to_i32(val_reg, val_ty, &field_def.ty);
                        self.line(&format!(
                            "store volatile i32 {alias_val}, ptr inttoptr ({ptr_ty} {alias} to ptr)",
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
                    let wide_val = self.widen_to_i32(val_reg, val_ty, &field_def.ty);
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
                        "store volatile i32 {new_val}, ptr inttoptr ({ptr_ty} {addr} to ptr)",
                        ptr_ty = self.ptr_type()
                    ));
                    return new_val;
                }
                // Struct field write: GEP + store
                if let LValue::Name((base_name, _)) = base.as_ref() {
                    let info = self.locals.get(base_name).cloned();
                    if let Some(info) = info
                        && let Type::Struct(_, fields) = &info.bml_type
                        && let Some(idx) = fields.iter().position(|(n, _)| n == &field.0)
                    {
                        let field_ty = &fields[idx].1;
                        let ll_field = llvm_type(field_ty);
                        let llvm_ty = info.llvm_ty.clone();
                        let alloca = info.alloca.clone();
                        let gep = self.new_reg();
                        self.line(&format!(
                            "{gep} = getelementptr {llvm_ty}, ptr {alloca}, i32 0, i32 {idx}"
                        ));
                        self.line(&format!("store {ll_field} {val_reg}, ptr {gep}"));
                        return val_reg.to_string();
                    }
                }
                val_reg.to_string()
            }
            LValue::Index(base, index) => {
                let Some((base_ptr, base_ty)) = self.lvalue_base_info(base, symbols, fn_name)
                else {
                    return val_reg.to_string();
                };
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
                self.line(&format!("store {ll_elem} {val_reg}, ptr {gep}"));
                val_reg.to_string()
            }
            LValue::Deref(inner) => {
                let ptr_reg = self.emit_expr(inner, symbols, fn_name);
                let inner_ty = self.expr_type(inner, symbols);
                let pointee_ty = match &inner_ty {
                    Type::Ptr(t) | Type::ConstPtr(t) => t.as_ref(),
                    _ => &crate::types::Type::I32,
                };
                let llty = llvm_type(pointee_ty);
                self.line(&format!("store {llty} {val_reg}, ptr {ptr_reg}"));
                val_reg.to_string()
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
            Type::Struct(name, fields) => {
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
                    if field_debug.is_empty() { String::new() } else { format!("{{{}}}", field_debug.join(", ")) }
                )
                .unwrap();
                self.type_dbg_id.insert(key, id);
                return id;
            }
            Type::Enum(_, inner_ty, _) => return self.dbg_type(inner_ty),
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

    fn expr_type(&self, expr: &Expr, symbols: &SymbolTable) -> Type {
        match expr {
            Expr::IntLiteral(_, suffix, _) => match suffix {
                crate::ast::IntSuffix::I8 => Type::I8,
                crate::ast::IntSuffix::I16 => Type::I16,
                crate::ast::IntSuffix::I32 => Type::I32,
                crate::ast::IntSuffix::I64 => Type::I64,
                crate::ast::IntSuffix::U8 => Type::U8,
                crate::ast::IntSuffix::U16 => Type::U16,
                crate::ast::IntSuffix::U32 => Type::U32,
                crate::ast::IntSuffix::U64 => Type::U64,
                crate::ast::IntSuffix::None => Type::U32,
            },
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
                if let Some(symbol) = self.alias_fn_symbols.get(name)
                    && let Some(fn_sym) = symbols.functions.get(symbol)
                {
                    let params: Vec<Type> = fn_sym.params.iter().map(|(_, t)| t.clone()).collect();
                    let ret = fn_sym.ret.clone().unwrap_or(Type::Void);
                    return Type::Fn(params, Box::new(ret));
                }
                if let Some(fn_sym) = symbols.functions.get(name) {
                    let params: Vec<Type> = fn_sym.params.iter().map(|(_, t)| t.clone()).collect();
                    let ret = fn_sym.ret.clone().unwrap_or(Type::Void);
                    return Type::Fn(params, Box::new(ret));
                }
                Type::U32 // default for locals and consts
            }
            Expr::Binary(left, op, _) => {
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
                if let Type::Struct(_, fields) = &base_ty
                    && let Some((_, field_ty)) = fields.iter().find(|(n, _)| n == &field.0)
                {
                    return field_ty.clone();
                }
                if let Type::Ptr(inner) | Type::ConstPtr(inner) = &base_ty
                    && let Type::Struct(_, fields) = inner.as_ref()
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
                    _ => Type::U32,
                }
            }
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
                if let Some(fields) = symbols.structs.get(&name.0) {
                    Type::Struct(name.0.clone(), fields.clone())
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
        Type::Struct(_, fields) => {
            let inner: Vec<String> = fields.iter().map(|(_, ty)| llvm_type(ty)).collect();
            format!("{{ {} }}", inner.join(", "))
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
        Type::Array(..) | Type::Struct(..) => "zeroinitializer".to_string(),
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
    structs: &HashMap<String, Vec<(String, Type)>>,
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
    structs: &HashMap<String, Vec<(String, Type)>>,
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

fn expr_const_val(expr: &Expr) -> String {
    match expr {
        Expr::IntLiteral(n, _, _) => format!("{n}"),
        Expr::FloatLiteral(f, suffix, _) => float_to_llvm(*f, *suffix),
        Expr::BoolLiteral(b, _) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Expr::NullLiteral(_) => "zeroinitializer".into(),
        _ => "0".into(),
    }
}

/// Format a f64 as a valid LLVM IR float constant.
/// LLVM requires 16 hex digits for all float types; for narrower types
/// the bits are placed in the upper half and the lower half is zero.
fn float_to_llvm(f: f64, suffix: crate::ast::FloatSuffix) -> String {
    let bits = match suffix {
        crate::ast::FloatSuffix::H | crate::ast::FloatSuffix::F | crate::ast::FloatSuffix::None => {
            (u64::from((f as f32).to_bits())) << 32
        }
        crate::ast::FloatSuffix::D => f.to_bits(),
    };
    format!("0x{bits:016X}")
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
