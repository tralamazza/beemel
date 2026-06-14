use std::collections::{HashMap, HashSet};

use crate::ast::{self, Expr, LValue, Program, Stmt, StorageAnnotation};
use crate::context::Context;
use crate::resolver::SymbolTable;

/// For each (`reader_fn`, `static_name)`: set of `writer_fns` that can preempt this reader.
#[derive(Debug, Clone)]
pub struct PreemptInfo {
    pub preemptable: HashMap<(String, String), HashSet<String>>,
}

/// Analyze the program to determine which `@shared` static reads could be
/// preempted by an ISR writer.
#[must_use]
pub fn analyze(program: &Program, symbols: &SymbolTable) -> PreemptInfo {
    // 1. Determine effective priority for each function.
    let priorities = compute_priorities(symbols);

    // 2. For each @shared static, build the set of functions that write to it.
    let writers = compute_writers(program, symbols);

    // 3. For each function R, determine which writer ISRs can preempt it.
    let mut preemptable: HashMap<(String, String), HashSet<String>> = HashMap::new();

    for (static_name, writer_fns) in &writers {
        for (reader_name, reader_prio) in &priorities {
            for writer_name in writer_fns {
                // A function cannot preempt itself -- but being a writer does
                // NOT exempt a reader from OTHER writers: between a thread's
                // own (CS'd) write and a later read, a higher-priority ISR
                // writer can still fire. The old reader-in-writer-set skip
                // wrongly proved such read-backs stable.
                if writer_name == reader_name {
                    continue;
                }
                if let Some(writer_prio) = priorities.get(writer_name) {
                    // Writer has higher priority (lower number) than reader
                    if writer_prio < reader_prio {
                        preemptable
                            .entry((reader_name.clone(), static_name.clone()))
                            .or_default()
                            .insert(writer_name.clone());
                    }
                }
            }
        }
    }

    PreemptInfo { preemptable }
}

/// Compute effective priority for each function.
/// - @isr(priority=N) -> N
/// - @context(thread) -> 255
/// - Any (no annotation) -> 255 (worst case: callable from thread)
fn compute_priorities(symbols: &SymbolTable) -> HashMap<String, u8> {
    let mut priorities = HashMap::new();
    for (name, sym) in &symbols.functions {
        let prio = match sym.context {
            Context::Isr(p) => p,
            Context::Thread => 255,
            Context::Any => 255,
        };
        priorities.insert(name.clone(), prio);
    }
    priorities
}

/// Build the set of functions that write to each @shared static.
fn compute_writers(program: &Program, symbols: &SymbolTable) -> HashMap<String, HashSet<String>> {
    // Identify @shared statics
    let shared_statics: HashSet<String> = symbols
        .statics
        .iter()
        .filter(|(_, sym)| {
            sym.storage
                .iter()
                .any(|a| matches!(a, StorageAnnotation::Shared(_)))
        })
        .map(|(name, _)| name.clone())
        .collect();

    let fn_names: HashSet<String> = symbols.functions.keys().cloned().collect();
    let fn_pointer_targets: HashSet<String> = symbols
        .functions
        .iter()
        .filter(|(_, sym)| sym.context == Context::Any)
        .map(|(name, _)| name.clone())
        .collect();
    let mut direct_by_fn: HashMap<String, HashSet<String>> = HashMap::new();
    let mut callees_by_fn: HashMap<String, HashSet<String>> = HashMap::new();

    for item in &program.items {
        if let ast::Item::FnDef(fn_def) = item {
            let fn_name = &fn_def.name.0;
            let mut writes: HashSet<String> = HashSet::new();
            let mut callees: HashSet<String> = HashSet::new();
            collect_written_statics(
                &fn_def.body,
                &shared_statics,
                &fn_names,
                &fn_pointer_targets,
                &mut writes,
                &mut callees,
            );
            direct_by_fn.insert(fn_name.clone(), writes);
            callees_by_fn.insert(fn_name.clone(), callees);
        }
    }

    let mut effective_by_fn = direct_by_fn.clone();
    loop {
        let mut changed = false;
        for (fn_name, callees) in &callees_by_fn {
            for callee in callees {
                let Some(callee_writes) = effective_by_fn.get(callee).cloned() else {
                    continue;
                };
                let fn_writes = effective_by_fn.entry(fn_name.clone()).or_default();
                for static_name in callee_writes {
                    changed |= fn_writes.insert(static_name);
                }
            }
        }
        if !changed {
            break;
        }
    }

    let mut writers: HashMap<String, HashSet<String>> = HashMap::new();
    for (fn_name, static_names) in effective_by_fn {
        for static_name in static_names {
            writers
                .entry(static_name)
                .or_default()
                .insert(fn_name.clone());
        }
    }

    writers
}

fn collect_written_statics(
    block: &ast::Block,
    shared_statics: &HashSet<String>,
    fn_names: &HashSet<String>,
    fn_pointer_targets: &HashSet<String>,
    out: &mut HashSet<String>,
    callees: &mut HashSet<String>,
) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Assign(assign) => {
                if let Some(name) = written_static_name(&assign.target, shared_statics) {
                    out.insert(name.clone());
                }
                collect_lvalue_effects(
                    &assign.target,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                collect_expr_writes(
                    &assign.value,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::CompoundAssign(ca) => {
                if let Some(name) = written_static_name(&ca.target, shared_statics) {
                    out.insert(name.clone());
                }
                collect_lvalue_effects(
                    &ca.target,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                collect_expr_writes(
                    &ca.value,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::Expr(expr) => collect_expr_writes(
                expr,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            ),
            Stmt::If(if_stmt) => {
                collect_expr_writes(
                    &if_stmt.cond,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                collect_written_statics(
                    &if_stmt.then_block,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                if let Some(else_branch) = &if_stmt.else_branch {
                    match else_branch.as_ref() {
                        Stmt::Block(b) => {
                            collect_written_statics(
                                b,
                                shared_statics,
                                fn_names,
                                fn_pointer_targets,
                                out,
                                callees,
                            );
                        }
                        Stmt::If(inner) => {
                            let wrapper = ast::Block {
                                stmts: vec![Stmt::If(inner.clone())],
                                trailing: None,
                                span: inner.cond.span(),
                            };
                            collect_written_statics(
                                &wrapper,
                                shared_statics,
                                fn_names,
                                fn_pointer_targets,
                                out,
                                callees,
                            );
                        }
                        _ => {}
                    }
                }
            }
            Stmt::For(for_stmt) => {
                collect_expr_writes(
                    &for_stmt.start,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                collect_expr_writes(
                    &for_stmt.end,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                if let Some(step) = &for_stmt.step {
                    collect_expr_writes(
                        step,
                        shared_statics,
                        fn_names,
                        fn_pointer_targets,
                        out,
                        callees,
                    );
                }
                collect_written_statics(
                    &for_stmt.body,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::Loop(loop_stmt) => {
                collect_written_statics(
                    &loop_stmt.body,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::Claim(c) => {
                collect_written_statics(
                    &c.body,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::While(while_stmt) => {
                collect_expr_writes(
                    &while_stmt.cond,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                collect_written_statics(
                    &while_stmt.body,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::Match(match_stmt) => {
                collect_expr_writes(
                    &match_stmt.scrutinee,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
                for arm in &match_stmt.arms {
                    collect_written_statics(
                        &arm.body,
                        shared_statics,
                        fn_names,
                        fn_pointer_targets,
                        out,
                        callees,
                    );
                }
            }
            Stmt::Return(ret) => {
                if let Some(val) = &ret.value {
                    collect_expr_writes(
                        val,
                        shared_statics,
                        fn_names,
                        fn_pointer_targets,
                        out,
                        callees,
                    );
                }
            }
            Stmt::Block(inner) => {
                collect_written_statics(
                    inner,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::VarDecl(vd) => {
                collect_expr_writes(
                    &vd.init,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::Assume(assume) => {
                collect_expr_writes(
                    &assume.cond,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::Assert(assert) => {
                collect_expr_writes(
                    &assert.cond,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            Stmt::Asm(asm_stmt) => {
                for (_, target) in &asm_stmt.outputs {
                    if let Some(target) = crate::parser::expr_to_lvalue(target.clone()) {
                        if let Some(name) = written_static_name(&target, shared_statics) {
                            out.insert(name.clone());
                        }
                        collect_lvalue_effects(
                            &target,
                            shared_statics,
                            fn_names,
                            fn_pointer_targets,
                            out,
                            callees,
                        );
                    }
                }
                for (_, value) in &asm_stmt.inputs {
                    collect_expr_writes(
                        value,
                        shared_statics,
                        fn_names,
                        fn_pointer_targets,
                        out,
                        callees,
                    );
                }
            }
            Stmt::Break(_) | Stmt::Continue(_) => {}
        }
    }

    if let Some(ref trailing) = block.trailing {
        collect_expr_writes(
            trailing,
            shared_statics,
            fn_names,
            fn_pointer_targets,
            out,
            callees,
        );
    }
}

fn collect_expr_writes(
    expr: &Expr,
    shared_statics: &HashSet<String>,
    fn_names: &HashSet<String>,
    fn_pointer_targets: &HashSet<String>,
    out: &mut HashSet<String>,
    callees: &mut HashSet<String>,
) {
    match expr {
        Expr::Unary(_, inner) => collect_expr_writes(
            inner,
            shared_statics,
            fn_names,
            fn_pointer_targets,
            out,
            callees,
        ),
        Expr::Binary(left, _, right) => {
            collect_expr_writes(
                left,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            collect_expr_writes(
                right,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        Expr::Call(callee, args) => {
            if let Expr::Ident((name, _)) = callee.as_ref()
                && fn_names.contains(name)
            {
                callees.insert(name.clone());
            } else {
                // Function pointers may target any unrestricted function. Be
                // conservative rather than missing an ISR writer.
                callees.extend(fn_pointer_targets.iter().cloned());
            }
            collect_expr_writes(
                callee,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            for arg in args {
                collect_expr_writes(
                    arg,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
        }
        Expr::FieldAccess(base, _) => {
            collect_expr_writes(
                base,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        Expr::Index(base, index) => {
            collect_expr_writes(
                base,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            collect_expr_writes(
                index,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        Expr::Group(inner) => collect_expr_writes(
            inner,
            shared_statics,
            fn_names,
            fn_pointer_targets,
            out,
            callees,
        ),
        Expr::Cast(inner, _) => collect_expr_writes(
            inner,
            shared_statics,
            fn_names,
            fn_pointer_targets,
            out,
            callees,
        ),
        Expr::ArrayInit(elems, _) => {
            for elem in elems {
                collect_expr_writes(
                    elem,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                collect_expr_writes(
                    expr,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
        }
        Expr::Block(block_expr) => {
            collect_written_statics(
                &block_expr.block,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        Expr::Match(match_expr) => {
            collect_expr_writes(
                &match_expr.scrutinee,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            for arm in &match_expr.arms {
                collect_written_statics(
                    &arm.body,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
        }
        Expr::If(if_expr) => {
            collect_expr_writes(
                &if_expr.cond,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            collect_written_statics(
                &if_expr.then_block,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            collect_expr_writes(
                &if_expr.else_branch,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            collect_expr_writes(
                base,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            if let Some(len) = len {
                collect_expr_writes(
                    len,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            if let Some(stride) = stride {
                collect_expr_writes(
                    stride,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            collect_expr_writes(
                base,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            if let Some(capacity) = capacity {
                collect_expr_writes(
                    capacity,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            collect_expr_writes(
                head,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            collect_expr_writes(
                len,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            collect_expr_writes(
                base,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            if let Some(bit_offset) = bit_offset {
                collect_expr_writes(
                    bit_offset,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
            if let Some(len_bits) = len_bits {
                collect_expr_writes(
                    len_bits,
                    shared_statics,
                    fn_names,
                    fn_pointer_targets,
                    out,
                    callees,
                );
            }
        }
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::Ident(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
    }
}

fn collect_lvalue_effects(
    lvalue: &LValue,
    shared_statics: &HashSet<String>,
    fn_names: &HashSet<String>,
    fn_pointer_targets: &HashSet<String>,
    out: &mut HashSet<String>,
    callees: &mut HashSet<String>,
) {
    match lvalue {
        LValue::Name(_) => {}
        LValue::Field(base, _) => {
            collect_lvalue_effects(
                base,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        LValue::Index(base, index) => {
            collect_lvalue_effects(
                base,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
            collect_expr_writes(
                index,
                shared_statics,
                fn_names,
                fn_pointer_targets,
                out,
                callees,
            );
        }
        LValue::Deref(expr) => collect_expr_writes(
            expr,
            shared_statics,
            fn_names,
            fn_pointer_targets,
            out,
            callees,
        ),
    }
}

fn written_static_name<'a>(
    lvalue: &'a LValue,
    shared_statics: &HashSet<String>,
) -> Option<&'a String> {
    match lvalue {
        LValue::Name((name, _)) if shared_statics.contains(name) => Some(name),
        LValue::Field(base, _) | LValue::Index(base, _) => {
            written_static_name(base, shared_statics)
        }
        LValue::Name(_) | LValue::Deref(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::errors::DiagnosticBag;
    use crate::parser::Parser;
    use crate::resolver::Resolver;
    use crate::source::SourceMap;

    use super::*;

    fn analyze_source(source: &str) -> PreemptInfo {
        let mut source_map = SourceMap::new();
        let file_id =
            source_map.add_file_with_source(PathBuf::from("test.bml"), source.to_string());
        let text = source_map.source(file_id);
        let mut diags = DiagnosticBag::new();
        let mut parser = Parser::new(text, file_id, &mut diags);
        let program = parser.parse_program();
        assert!(!diags.has_errors(), "parse failed");

        let symbols = Resolver::new().resolve(&program, &mut diags);
        assert!(!diags.has_errors(), "resolve failed");

        analyze(&program, &symbols)
    }

    #[test]
    fn projected_shared_write_marks_isr_writer() {
        let info = analyze_source(
            r"
            var X: [u32; 2] @shared(ceiling = 1) = [0u32, 0u32];

            fn main() @context(thread) {
                var y: u32 = X[0u32];
            }

            fn timer() @isr(priority = 1) {
                X[0u32] = 42u32;
            }
            ",
        );

        assert!(
            info.preemptable
                .get(&("main".to_string(), "X".to_string()))
                .is_some_and(|writers| writers.contains("timer"))
        );
    }

    #[test]
    fn shared_write_through_helper_marks_isr_caller() {
        let info = analyze_source(
            r"
            var X: u32 @shared(ceiling = 1) = 0u32;

            fn helper() {
                X = 1u32;
            }

            fn main() @context(thread) {
                var y: u32 = X;
            }

            fn timer() @isr(priority = 1) {
                helper();
            }
            ",
        );

        assert!(
            info.preemptable
                .get(&("main".to_string(), "X".to_string()))
                .is_some_and(|writers| writers.contains("timer"))
        );
    }

    #[test]
    fn shared_write_through_function_pointer_marks_isr_caller() {
        let info = analyze_source(
            r"
            var X: u32 @shared(ceiling = 1) = 0u32;

            fn helper() {
                X = 1u32;
            }

            fn main() @context(thread) {
                var y: u32 = X;
            }

            fn timer() @isr(priority = 1) {
                var fp: fn() = &helper;
                fp();
            }
            ",
        );

        assert!(
            info.preemptable
                .get(&("main".to_string(), "X".to_string()))
                .is_some_and(|writers| writers.contains("timer"))
        );
    }
}
