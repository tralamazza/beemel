use std::collections::{HashMap, HashSet};

use crate::ast::{self, Expr, Program, Stmt, StorageAnnotation};
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
            if writer_fns.contains(reader_name) {
                continue;
            }
            for writer_name in writer_fns {
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

    let mut writers: HashMap<String, HashSet<String>> = HashMap::new();

    for item in &program.items {
        if let ast::Item::FnDef(fn_def) = item {
            let fn_name = &fn_def.name.0;
            let mut collected: HashSet<String> = HashSet::new();
            collect_written_statics(&fn_def.body, &shared_statics, &mut collected);
            for static_name in collected {
                writers
                    .entry(static_name)
                    .or_default()
                    .insert(fn_name.clone());
            }
        }
    }

    writers
}

fn collect_written_statics(
    block: &ast::Block,
    shared_statics: &HashSet<String>,
    out: &mut HashSet<String>,
) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Assign(assign) => {
                if let ast::LValue::Name((name, _)) = &assign.target
                    && shared_statics.contains(name)
                {
                    out.insert(name.clone());
                }
            }
            Stmt::Expr(expr) => collect_expr_writes(expr, shared_statics, out),
            Stmt::If(if_stmt) => {
                collect_written_statics(&if_stmt.then_block, shared_statics, out);
                if let Some(else_branch) = &if_stmt.else_branch {
                    match else_branch.as_ref() {
                        Stmt::Block(b) => collect_written_statics(b, shared_statics, out),
                        Stmt::If(inner) => {
                            let wrapper = ast::Block {
                                stmts: vec![Stmt::If(inner.clone())],
                                trailing: None,
                                span: inner.cond.span(),
                            };
                            collect_written_statics(&wrapper, shared_statics, out);
                        }
                        _ => {}
                    }
                }
            }
            Stmt::For(for_stmt) => {
                collect_written_statics(&for_stmt.body, shared_statics, out);
            }
            Stmt::Loop(loop_stmt) => {
                collect_written_statics(&loop_stmt.body, shared_statics, out);
            }
            Stmt::While(while_stmt) => {
                collect_written_statics(&while_stmt.body, shared_statics, out);
            }
            Stmt::Match(match_stmt) => {
                for arm in &match_stmt.arms {
                    collect_written_statics(&arm.body, shared_statics, out);
                }
            }
            Stmt::Return(ret) => {
                if let Some(val) = &ret.value {
                    collect_expr_writes(val, shared_statics, out);
                }
            }
            Stmt::Block(inner) => {
                collect_written_statics(inner, shared_statics, out);
            }
            Stmt::VarDecl(vd) => {
                collect_expr_writes(&vd.init, shared_statics, out);
            }
            Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Asm(_)
            | Stmt::Assume(_)
            | Stmt::Assert(_) => {}
        }
    }

    if let Some(ref trailing) = block.trailing {
        collect_expr_writes(trailing, shared_statics, out);
    }
}

fn collect_expr_writes(expr: &Expr, shared_statics: &HashSet<String>, out: &mut HashSet<String>) {
    match expr {
        Expr::Unary(_, inner) => collect_expr_writes(inner, shared_statics, out),
        Expr::Binary(left, _, right) => {
            collect_expr_writes(left, shared_statics, out);
            collect_expr_writes(right, shared_statics, out);
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect_expr_writes(arg, shared_statics, out);
            }
        }
        Expr::FieldAccess(base, _) => collect_expr_writes(base, shared_statics, out),
        Expr::Index(base, index) => {
            collect_expr_writes(base, shared_statics, out);
            collect_expr_writes(index, shared_statics, out);
        }
        Expr::Group(inner) => collect_expr_writes(inner, shared_statics, out),
        Expr::Cast(inner, _) => collect_expr_writes(inner, shared_statics, out),
        Expr::ArrayInit(elems, _) => {
            for elem in elems {
                collect_expr_writes(elem, shared_statics, out);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, expr) in fields {
                collect_expr_writes(expr, shared_statics, out);
            }
        }
        Expr::Block(block_expr) => {
            collect_written_statics(&block_expr.block, shared_statics, out);
        }
        Expr::Match(match_expr) => {
            collect_expr_writes(&match_expr.scrutinee, shared_statics, out);
            for arm in &match_expr.arms {
                collect_written_statics(&arm.body, shared_statics, out);
            }
        }
        Expr::If(if_expr) => {
            collect_expr_writes(&if_expr.cond, shared_statics, out);
            collect_written_statics(&if_expr.then_block, shared_statics, out);
            collect_expr_writes(&if_expr.else_branch, shared_statics, out);
        }
        Expr::ViewNew { base, len, .. } => {
            collect_expr_writes(base, shared_statics, out);
            if let Some(len) = len {
                collect_expr_writes(len, shared_statics, out);
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
