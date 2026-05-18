use std::collections::HashSet;

use crate::ast::{self, Expr, LValue, Program, Stmt, StorageAnnotation};
use crate::context::Context;
use crate::errors::DiagnosticBag;
use crate::resolver::{FnSymbol, SymbolTable};
use crate::source::Span;

pub struct BorrowChecker;

impl BorrowChecker {
    pub fn check(program: &Program, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
        for item in &program.items {
            if let ast::Item::FnDef(fn_def) = item {
                let fn_name = &fn_def.name.0;
                let context = symbols
                    .functions
                    .get(fn_name)
                    .map_or(Context::Thread, |s| s.context);

                let mut moved: HashSet<String> = HashSet::new();
                let mut scope_stack: Vec<HashSet<String>> = vec![HashSet::new()];

                check_fn_body(
                    &fn_def.body,
                    fn_name,
                    context,
                    symbols,
                    &mut moved,
                    &mut scope_stack,
                    diags,
                );
            }
        }
    }
}

fn check_fn_body(
    block: &ast::Block,
    current_fn: &str,
    current_ctx: Context,
    symbols: &SymbolTable,
    moved: &mut HashSet<String>,
    scope_stack: &mut Vec<HashSet<String>>,
    diags: &mut DiagnosticBag,
) {
    scope_stack.push(HashSet::new());

    for stmt in &block.stmts {
        check_stmt(
            stmt,
            current_fn,
            current_ctx,
            symbols,
            moved,
            scope_stack,
            diags,
        );
    }

    if let Some(ref trailing) = block.trailing {
        check_expr(
            trailing,
            current_fn,
            current_ctx,
            symbols,
            moved,
            scope_stack,
            diags,
        );
    }

    scope_stack.pop();
}

fn check_stmt(
    stmt: &Stmt,
    current_fn: &str,
    current_ctx: Context,
    symbols: &SymbolTable,
    moved: &mut HashSet<String>,
    scope_stack: &mut Vec<HashSet<String>>,
    diags: &mut DiagnosticBag,
) {
    match stmt {
        Stmt::VarDecl(vd) => {
            check_expr(
                &vd.init,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            // Track the new variable in the current scope for move tracking
            scope_stack.last_mut().unwrap().insert(vd.name.0.clone());
        }

        Stmt::Assign(assign) => {
            check_lvalue(
                &assign.target,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_expr(
                &assign.value,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Stmt::Expr(expr) => {
            check_expr(
                expr,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Stmt::If(if_stmt) => {
            check_expr(
                &if_stmt.cond,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_fn_body(
                &if_stmt.then_block,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            if let Some(else_branch) = &if_stmt.else_branch {
                match else_branch.as_ref() {
                    Stmt::Block(block) => check_fn_body(
                        block,
                        current_fn,
                        current_ctx,
                        symbols,
                        moved,
                        scope_stack,
                        diags,
                    ),
                    Stmt::If(inner_if) => {
                        // else if -- wrap in a block and recurse
                        let wrapper = ast::Block {
                            stmts: vec![Stmt::If(inner_if.clone())],
                            trailing: None,
                            span: inner_if.cond.span(),
                        };
                        check_fn_body(
                            &wrapper,
                            current_fn,
                            current_ctx,
                            symbols,
                            moved,
                            scope_stack,
                            diags,
                        );
                    }
                    _ => {}
                }
            }
        }

        Stmt::For(for_stmt) => {
            check_expr(
                &for_stmt.start,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_expr(
                &for_stmt.end,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            scope_stack
                .last_mut()
                .unwrap()
                .insert(for_stmt.var.0.clone());
            check_fn_body(
                &for_stmt.body,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Stmt::Loop(loop_stmt) => {
            check_fn_body(
                &loop_stmt.body,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Stmt::While(while_stmt) => {
            check_expr(
                &while_stmt.cond,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_fn_body(
                &while_stmt.body,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Stmt::Match(match_stmt) => {
            check_expr(
                &match_stmt.scrutinee,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            for arm in &match_stmt.arms {
                check_fn_body(
                    &arm.body,
                    current_fn,
                    current_ctx,
                    symbols,
                    moved,
                    scope_stack,
                    diags,
                );
            }
        }

        Stmt::Return(ret) => {
            if let Some(val) = &ret.value {
                check_expr(
                    val,
                    current_fn,
                    current_ctx,
                    symbols,
                    moved,
                    scope_stack,
                    diags,
                );
            }
        }

        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm(_) => {}

        Stmt::Block(inner) => {
            check_fn_body(
                inner,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }
    }
}

fn check_expr(
    expr: &Expr,
    current_fn: &str,
    current_ctx: Context,
    symbols: &SymbolTable,
    moved: &mut HashSet<String>,
    scope_stack: &mut Vec<HashSet<String>>,
    diags: &mut DiagnosticBag,
) {
    match expr {
        Expr::Ident((name, span)) => {
            // Check if this is a static access that needs borrow validation
            if let Some(sym) = symbols.statics.get(name) {
                check_static_access(name, span, sym, current_fn, current_ctx, diags);
            }

            // Check if this variable was moved
            if moved.contains(name) {
                diags.error(format!("use of moved value: `{name}`"), "E400", *span);
            }

            // If this is a Move-typed local, mark it as moved
            if is_move_typed_local(name, symbols) && is_local_in_scope(name, scope_stack) {
                moved.insert(name.clone());
            }
        }

        Expr::Unary(_, inner) => {
            check_expr(
                inner,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Expr::Binary(left, _, right) => {
            check_expr(
                left,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_expr(
                right,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Expr::Call(func_expr, args) => {
            // Check if calling a function with incompatible context
            if let Expr::Ident((callee, span)) = func_expr.as_ref()
                && let Some(fn_sym) = symbols.functions.get(callee)
            {
                check_context_compat(callee, span, fn_sym, current_fn, current_ctx, diags);
            }
            if let Expr::FieldAccess(base, field) = func_expr.as_ref()
                && let Expr::Ident((alias, _)) = base.as_ref()
                && let Some(alias_info) = symbols.import_aliases.get(alias)
                && let Some(item) = alias_info.exports.get(&field.0)
                && let Some(context) = alias_item_context(item)
            {
                check_context_compat_context(
                    &format!("{alias}.{}", field.0),
                    &field.1,
                    context,
                    current_fn,
                    current_ctx,
                    diags,
                );
            }
            for arg in args {
                check_expr(
                    arg,
                    current_fn,
                    current_ctx,
                    symbols,
                    moved,
                    scope_stack,
                    diags,
                );
            }
        }

        Expr::FieldAccess(base, _) => {
            check_expr(
                base,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Expr::Index(base, index) => {
            check_expr(
                base,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_expr(
                index,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Expr::Group(inner) => {
            check_expr(
                inner,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Expr::Match(match_expr) => {
            check_expr(
                &match_expr.scrutinee,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            for arm in &match_expr.arms {
                check_fn_body(
                    &arm.body,
                    current_fn,
                    current_ctx,
                    symbols,
                    moved,
                    scope_stack,
                    diags,
                );
            }
        }

        Expr::Block(block_expr) => {
            check_fn_body(
                &block_expr.block,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        Expr::If(if_expr) => {
            check_expr(
                &if_expr.cond,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_fn_body(
                &if_expr.then_block,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_expr(
                &if_expr.else_branch,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }

        // Literals never need borrow checks
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(_, _)
        | Expr::StringLiteral(_, _)
        | Expr::NullLiteral(_)
        | Expr::SizeOf(..)
        | Expr::Cast(_, _)
        | Expr::ArrayInit(_, _)
        | Expr::StructInit { .. }
        | Expr::EnumVariant { .. } => {}
    }
}

fn check_lvalue(
    lval: &LValue,
    current_fn: &str,
    current_ctx: Context,
    symbols: &SymbolTable,
    moved: &mut HashSet<String>,
    scope_stack: &mut Vec<HashSet<String>>,
    diags: &mut DiagnosticBag,
) {
    match lval {
        LValue::Name((name, span)) => {
            // Check static access
            if let Some(sym) = symbols.statics.get(name) {
                check_static_access(name, span, sym, current_fn, current_ctx, diags);
            }
            // Check moved
            if moved.contains(name) {
                diags.error(format!("use of moved value: `{name}`"), "E400", *span);
            }
        }
        LValue::Field(base, _) => {
            check_lvalue(
                base,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }
        LValue::Index(base, index) => {
            check_lvalue(
                base,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
            check_expr(
                index,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }
        LValue::Deref(inner) => {
            check_expr(
                inner,
                current_fn,
                current_ctx,
                symbols,
                moved,
                scope_stack,
                diags,
            );
        }
    }
}

/// Verify that accessing a static from the current function is allowed.
fn check_static_access(
    name: &str,
    span: &Span,
    sym: &crate::resolver::StaticSymbol,
    current_fn: &str,
    current_ctx: Context,
    diags: &mut DiagnosticBag,
) {
    // If no storage annotation, the static is implicitly thread-only
    if sym.storage.is_empty() && current_ctx.is_isr() {
        diags.error(
            format!(
                "static `{name}` has no annotations and is thread-only; cannot access from ISR `{current_fn}`"
            ),
            "E404",
            *span,
        );
        return;
    }

    for ann in &sym.storage {
        match ann {
            StorageAnnotation::Exclusive((owner, _owner_span)) => {
                if owner != current_fn {
                    diags.error(
                        format!(
                            "static `{name}` is @exclusive to `{owner}`, cannot access from `{current_fn}`"
                        ),
                        "E401",
                        *span,
                    );
                }
            }
            StorageAnnotation::Shared(ceiling) => {
                let level = current_ctx.level();
                if !current_ctx.can_access(*ceiling) {
                    diags.error(
                        format!(
                            "static `{name}` has @shared(ceiling={ceiling}), but current priority is {level} (lower = higher priority in ARM)"
                        ),
                        "E402",
                        *span,
                    );
                }
            }
            StorageAnnotation::Dma | StorageAnnotation::External => {
                // No context restrictions for DMA/external
            }
            StorageAnnotation::Section(_) => {}
        }
    }
}

/// Check that calling `callee` from `current_fn` is context-compatible.
fn check_context_compat(
    callee: &str,
    span: &Span,
    fn_sym: &FnSymbol,
    current_fn: &str,
    current_ctx: Context,
    diags: &mut DiagnosticBag,
) {
    check_context_compat_context(callee, span, fn_sym.context, current_fn, current_ctx, diags);
}

fn check_context_compat_context(
    callee: &str,
    span: &Span,
    callee_context: Context,
    current_fn: &str,
    current_ctx: Context,
    diags: &mut DiagnosticBag,
) {
    match callee_context {
        Context::Any => {
            // Any function can be called from anywhere
        }
        Context::Thread => {
            if current_ctx.is_isr() {
                diags.error(
                    format!(
                        "cannot call `{callee}` (requires `@context(thread)`) from ISR `{current_fn}`"
                    ),
                    "E403",
                    *span,
                );
            }
        }
        Context::Isr(callee_prio) => {
            match current_ctx {
                Context::Thread => {
                    diags.error(
                        format!(
                            "cannot call `{callee}` (ISR at priority {callee_prio}) from thread context `{current_fn}`"
                        ),
                        "E403",
                        *span,
                    );
                }
                Context::Isr(current_prio) => {
                    // Higher-priority (lower number) ISR can call lower-priority ISR
                    // in ARM, but calling ISRs from ISRs is unusual. Allow it.
                    _ = current_prio;
                }
                Context::Any => {}
            }
        }
    }
}

fn alias_item_context(item: &ast::Item) -> Option<Context> {
    match item {
        ast::Item::FnDef(f) => Some(if let Some(isr) = &f.isr {
            Context::Isr(isr.priority)
        } else {
            context_from_ast(&f.context)
        }),
        ast::Item::ExternFnDef(f) => Some(if let Some(isr) = &f.isr {
            Context::Isr(isr.priority)
        } else if let Some(ctx) = &f.context {
            context_from_ast(ctx)
        } else {
            Context::Any
        }),
        _ => None,
    }
}

fn context_from_ast(ctx: &ast::ContextExpr) -> Context {
    match ctx {
        ast::ContextExpr::Thread => Context::Thread,
        ast::ContextExpr::Any => Context::Any,
    }
}

/// Is the given name a local variable with Move type?
fn is_move_typed_local(name: &str, symbols: &SymbolTable) -> bool {
    // Statics with move semantics aren't consumed (they persist)
    if let Some(_sym) = symbols.statics.get(name) {
        return false; // statics are never "moved" away
    }
    // For now, only statics carry storage annotations.
    // Locals of storage-wrapper types would be move-typed.
    false
}

fn is_local_in_scope(name: &str, scope_stack: &[HashSet<String>]) -> bool {
    for scope in scope_stack.iter().rev() {
        if scope.contains(name) {
            return true;
        }
    }
    false
}
