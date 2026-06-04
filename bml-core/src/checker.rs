use std::collections::HashMap;

use crate::ast::{self, Expr, Item, LValue, Program, Stmt};
use crate::errors::DiagnosticBag;
use crate::resolver::SymbolTable;
use crate::types::{self, Type};

pub struct Checker;

/// Local variable information tracked during type checking.
#[derive(Debug, Clone)]
struct VarInfo {
    ty: Type,
    mutable: bool,
    moved: bool,
}

/// A stack of variable scopes.
struct ScopeStack {
    scopes: Vec<HashMap<String, VarInfo>>,
}

impl ScopeStack {
    fn new() -> Self {
        ScopeStack {
            scopes: vec![HashMap::new()],
        }
    }

    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.scopes.pop();
    }

    fn insert(&mut self, name: String, info: VarInfo) {
        self.scopes.last_mut().unwrap().insert(name, info);
    }

    fn lookup(&self, name: &str) -> Option<&VarInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.get(name) {
                return Some(info);
            }
        }
        None
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut VarInfo> {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.get_mut(name) {
                return Some(info);
            }
        }
        None
    }

    /// Capture the current move-state so it can be restored or merged. Used to
    /// analyze branches independently and to drive the loop-body fixpoint.
    fn snapshot(&self) -> Vec<HashMap<String, VarInfo>> {
        self.scopes.clone()
    }

    /// Restore a previously captured move-state. The snapshot must have been
    /// taken at the same program point (same scope nesting), which holds for
    /// every call site here: branch bodies and loop bodies push and pop their
    /// own inner scopes, leaving the outer scopes structurally unchanged.
    fn restore(&mut self, snap: Vec<HashMap<String, VarInfo>>) {
        self.scopes = snap;
    }

    /// OR the moved flags from `other` into self. A local is considered moved
    /// after a branch if it was moved on *any* path reaching this point
    /// (maybe-moved == moved), which is the sound direction for rejecting
    /// use-after-move.
    fn merge_moved(&mut self, other: &[HashMap<String, VarInfo>]) {
        for (scope, oscope) in self.scopes.iter_mut().zip(other.iter()) {
            for (name, info) in scope.iter_mut() {
                if let Some(oinfo) = oscope.get(name) {
                    info.moved |= oinfo.moved;
                }
            }
        }
    }

    /// Total number of currently-moved locals. Used to detect fixpoint
    /// convergence of the loop-body analysis.
    fn moved_count(&self) -> usize {
        self.scopes
            .iter()
            .flat_map(HashMap::values)
            .filter(|info| info.moved)
            .count()
    }
}

impl Checker {
    pub fn check(program: &Program, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
        for item in &program.items {
            if let ast::Item::FnDef(fn_def) = item {
                check_fn(fn_def, symbols, diags);
            }
        }
        check_comptime_asserts(program, symbols, diags);
    }
}

/// A compile-time constant value.
#[derive(Clone, Copy)]
enum ConstVal {
    Int(i128),
    Bool(bool),
}

/// Evaluate a const expression. Returns `None` for anything not compile-time
/// constant. All arithmetic is checked so an overflowing user expression yields
/// `None` rather than panicking the compiler.
fn const_eval(
    expr: &Expr,
    symbols: &SymbolTable,
    vals: &HashMap<String, i128>,
) -> Option<ConstVal> {
    use crate::ast::{BinaryOp as B, UnaryOp as U};
    Some(match expr {
        Expr::IntLiteral(n, _, _) => ConstVal::Int(i128::from(*n)),
        Expr::BoolLiteral(b, _) => ConstVal::Bool(*b),
        Expr::Group(inner) | Expr::Cast(inner, _) => const_eval(inner, symbols, vals)?,
        Expr::Ident((name, _)) => ConstVal::Int(*vals.get(name)?),
        Expr::SizeOf(ty_expr, _) => {
            let ty = types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
            if matches!(ty, Type::Unresolved(_)) {
                return None;
            }
            ConstVal::Int(i128::from(types::element_size(&ty)))
        }
        Expr::Unary(U::Neg, inner) => match const_eval(inner, symbols, vals)? {
            ConstVal::Int(v) => ConstVal::Int(v.checked_neg()?),
            ConstVal::Bool(_) => return None,
        },
        Expr::Unary(U::BitNot, inner) => match const_eval(inner, symbols, vals)? {
            ConstVal::Int(v) => ConstVal::Int(!v),
            ConstVal::Bool(_) => return None,
        },
        Expr::Unary(U::Not, inner) => match const_eval(inner, symbols, vals)? {
            ConstVal::Bool(b) => ConstVal::Bool(!b),
            ConstVal::Int(_) => return None,
        },
        Expr::Binary(lhs, op, rhs) => {
            let lv = const_eval(lhs, symbols, vals)?;
            let rv = const_eval(rhs, symbols, vals)?;
            match (lv, rv) {
                (ConstVal::Int(x), ConstVal::Int(y)) => match op {
                    B::Add => ConstVal::Int(x.checked_add(y)?),
                    B::Sub => ConstVal::Int(x.checked_sub(y)?),
                    B::Mul => ConstVal::Int(x.checked_mul(y)?),
                    B::Div => ConstVal::Int(x.checked_div(y)?),
                    B::Mod => ConstVal::Int(x.checked_rem(y)?),
                    B::BitAnd => ConstVal::Int(x & y),
                    B::BitOr => ConstVal::Int(x | y),
                    B::BitXor => ConstVal::Int(x ^ y),
                    B::Shl => ConstVal::Int(u32::try_from(y).ok().and_then(|s| x.checked_shl(s))?),
                    B::Shr => ConstVal::Int(u32::try_from(y).ok().and_then(|s| x.checked_shr(s))?),
                    B::Eq => ConstVal::Bool(x == y),
                    B::NotEq => ConstVal::Bool(x != y),
                    B::Lt => ConstVal::Bool(x < y),
                    B::Gt => ConstVal::Bool(x > y),
                    B::LtEq => ConstVal::Bool(x <= y),
                    B::GtEq => ConstVal::Bool(x >= y),
                    _ => return None,
                },
                (ConstVal::Bool(x), ConstVal::Bool(y)) => match op {
                    B::And => ConstVal::Bool(x && y),
                    B::Or => ConstVal::Bool(x || y),
                    B::Eq => ConstVal::Bool(x == y),
                    B::NotEq => ConstVal::Bool(x != y),
                    _ => return None,
                },
                _ => return None,
            }
        }
        _ => return None,
    })
}

/// Compile-time values of all `const` items, resolved to a fixpoint so a
/// `const` defined in terms of another resolves regardless of order.
fn collect_const_values(program: &Program, symbols: &SymbolTable) -> HashMap<String, i128> {
    let mut vals: HashMap<String, i128> = HashMap::new();
    loop {
        let mut changed = false;
        for item in &program.items {
            if let ast::Item::ConstDef(c) = item
                && !vals.contains_key(&c.name.0)
                && let Some(ConstVal::Int(v)) = const_eval(&c.value, symbols, &vals)
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

/// Evaluate every `comptime_assert(cond);` at compile time.
fn check_comptime_asserts(program: &Program, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    if !program
        .items
        .iter()
        .any(|i| matches!(i, ast::Item::ComptimeAssert(_)))
    {
        return;
    }
    let vals = collect_const_values(program, symbols);
    for item in &program.items {
        if let ast::Item::ComptimeAssert(ca) = item {
            match const_eval(&ca.cond, symbols, &vals) {
                Some(ConstVal::Bool(true)) => {}
                Some(ConstVal::Bool(false)) => {
                    diags.error(
                        "comptime_assert failed: condition is false",
                        "E342",
                        ca.span,
                    );
                }
                Some(ConstVal::Int(_)) | None => {
                    diags.error(
                        "comptime_assert condition must be a compile-time-constant `b1` expression",
                        "E343",
                        ca.span,
                    );
                }
            }
        }
    }
}

fn check_fn(fn_def: &ast::FnDef, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    let mut scope = ScopeStack::new();
    let expected_ret = fn_def
        .ret
        .as_ref()
        .map(|ty| types::resolve_type_expr(ty, &symbols.structs, &symbols.enums));

    // Add parameters to the outermost scope
    for param in &fn_def.params {
        let ty = types::resolve_type_expr(&param.ty, &symbols.structs, &symbols.enums);
        scope.insert(
            param.name.0.clone(),
            VarInfo {
                ty,
                mutable: false, // parameters are immutable
                moved: false,
            },
        );
    }

    check_block(
        &fn_def.body,
        symbols,
        &mut scope,
        &fn_def.name.0,
        expected_ret.as_ref(),
        diags,
    );

    if let Some(expected_ret) = &expected_ret
        && *expected_ret != Type::Void
        && !block_definitely_returns(&fn_def.body)
    {
        diags.error(
            format!(
                "function `{}` may exit without returning `{expected_ret:?}`",
                fn_def.name.0
            ),
            "E329",
            fn_def.name.1,
        );
    }
}

fn check_block(
    block: &ast::Block,
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) -> Option<Type> {
    scope.push();
    let mut last_type: Option<Type> = None;

    for stmt in &block.stmts {
        match stmt {
            Stmt::VarDecl(vd) => {
                let init_ty = check_expr(&vd.init, symbols, scope, fn_name, expected_ret, diags);
                let ty = if let Some(ty_ann) = &vd.ty_ann {
                    let ann_ty = types::resolve_type_expr(ty_ann, &symbols.structs, &symbols.enums);
                    // Check that init type is compatible with annotation
                    // (unsuffixed literals are allowed if their value fits)
                    if !types::types_compatible(&ann_ty, &init_ty)
                        && !unsuffixed_literal_fits(&vd.init, &ann_ty)
                    {
                        diags.error(
                        format!(
                            "type mismatch: declared `{ann_ty:?}` but initialized with `{init_ty:?}`"
                        ),
                            "E300",
                            vd.name.1,
                        );
                    }
                    ann_ty
                } else {
                    init_ty
                };

                scope.insert(
                    vd.name.0.clone(),
                    VarInfo {
                        ty: ty.clone(),
                        mutable: vd.mutable,
                        moved: false,
                    },
                );
                last_type = Some(ty);
            }

            Stmt::Assign(assign) => {
                let val_ty =
                    check_expr(&assign.value, symbols, scope, fn_name, expected_ret, diags);
                let target_ty = check_lvalue(
                    &assign.target,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                    true,
                );

                // Type compatibility check
                // (unsuffixed literals are allowed if their value fits)
                if !types::types_compatible(&target_ty, &val_ty)
                    && !unsuffixed_literal_fits(&assign.value, &target_ty)
                {
                    diags.error(
                        format!(
                            "type mismatch in assignment: cannot assign `{val_ty:?}` to `{target_ty:?}`"
                        ),
                        "E301",
                        assign.value.span(),
                    );
                }

                // Mark target as assigned (for move tracking)
                mark_assigned(&assign.target, scope, val_ty, diags);

                last_type = Some(target_ty);
            }

            Stmt::CompoundAssign(ca) => {
                // Type-check as `target = target OP value`: the synthesized
                // binary surfaces operator-type errors (E310/E317) and its
                // result type must be assignable to the target. The target is
                // also checked as an assignable place.
                let value_expr = Expr::Binary(
                    Box::new(ca.target.to_expr()),
                    ca.op,
                    Box::new(ca.value.clone()),
                );
                let val_ty = check_expr(&value_expr, symbols, scope, fn_name, expected_ret, diags);
                let target_ty = check_lvalue(
                    &ca.target,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                    true,
                );
                if !types::types_compatible(&target_ty, &val_ty)
                    && !unsuffixed_literal_fits(&value_expr, &target_ty)
                {
                    diags.error(
                        format!(
                            "type mismatch in compound assignment: `{target_ty:?}` and `{val_ty:?}`"
                        ),
                        "E301",
                        ca.span,
                    );
                }
                mark_assigned(&ca.target, scope, val_ty, diags);
                last_type = Some(target_ty);
            }

            Stmt::Expr(expr) => {
                last_type = Some(check_expr(
                    expr,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                ));
            }

            Stmt::If(if_stmt) => {
                let cond_ty =
                    check_expr(&if_stmt.cond, symbols, scope, fn_name, expected_ret, diags);
                if cond_ty != Type::B1 {
                    diags.error("if condition must be b1", "E302", if_stmt.cond.span());
                }
                // Analyze each branch from the same pre-branch move-state, then
                // union: a local moved on either path is moved afterward.
                let before = scope.snapshot();
                check_block(
                    &if_stmt.then_block,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                );
                let after_then = scope.snapshot();
                scope.restore(before);
                if let Some(else_branch) = &if_stmt.else_branch {
                    match else_branch.as_ref() {
                        Stmt::Block(block) => {
                            check_block(block, symbols, scope, fn_name, expected_ret, diags);
                        }
                        Stmt::If(if_stmt) => {
                            // else if -- recurse
                            check_block(
                                &ast::Block {
                                    stmts: vec![Stmt::If(if_stmt.clone())],
                                    trailing: None,
                                    span: if_stmt.cond.span(),
                                },
                                symbols,
                                scope,
                                fn_name,
                                expected_ret,
                                diags,
                            );
                        }
                        _ => {}
                    }
                }
                // scope now holds the else-path state (or the pre-branch state
                // when there is no else); fold in the then-path moves.
                scope.merge_moved(&after_then);
                last_type = None;
            }

            Stmt::For(for_stmt) => {
                let decl_ty =
                    types::resolve_type_expr(&for_stmt.ty, &symbols.structs, &symbols.enums);
                if !types::is_int(&decl_ty) {
                    diags.error(
                        format!("for loop variable must be an integer type, got `{decl_ty:?}`"),
                        "E312",
                        for_stmt.var.1,
                    );
                }
                for (bound, label) in [(&for_stmt.start, "start"), (&for_stmt.end, "end")] {
                    let bound_ty = check_expr(bound, symbols, scope, fn_name, expected_ret, diags);
                    if !types::types_compatible(&decl_ty, &bound_ty)
                        && !unsuffixed_literal_fits(bound, &decl_ty)
                    {
                        diags.error(
                            format!(
                                "for loop {label} bound type `{bound_ty:?}` does not match \
                                 declared `{decl_ty:?}`"
                            ),
                            "E312",
                            bound.span(),
                        );
                    }
                }
                if let Some(step) = &for_stmt.step {
                    let step_ty = check_expr(step, symbols, scope, fn_name, expected_ret, diags);
                    if !types::types_compatible(&decl_ty, &step_ty)
                        && !unsuffixed_literal_fits(step, &decl_ty)
                    {
                        diags.error(
                            format!(
                                "for loop step type `{step_ty:?}` does not match declared \
                                 `{decl_ty:?}`"
                            ),
                            "E312",
                            step.span(),
                        );
                    }
                    if let Expr::IntLiteral(0, _, _) = step {
                        diags.error("for loop step must not be zero", "E312", step.span());
                    }
                }
                scope.insert(
                    for_stmt.var.0.clone(),
                    VarInfo {
                        ty: decl_ty,
                        mutable: false,
                        moved: false,
                    },
                );
                check_loop_body(&for_stmt.body, symbols, scope, fn_name, expected_ret, diags);
                last_type = None;
            }

            Stmt::Loop(loop_stmt) => {
                check_loop_body(
                    &loop_stmt.body,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                );
                last_type = None;
            }

            Stmt::While(while_stmt) => {
                let cond_ty = check_expr(
                    &while_stmt.cond,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                );
                if cond_ty != Type::B1 {
                    diags.error("while condition must be b1", "E303", while_stmt.cond.span());
                }
                check_loop_body(
                    &while_stmt.body,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                );
                last_type = None;
            }

            Stmt::Return(ret) => {
                if let Some(val) = &ret.value {
                    let actual = check_expr(val, symbols, scope, fn_name, expected_ret, diags);
                    match expected_ret {
                        Some(expected_ret)
                            if !types::types_compatible(expected_ret, &actual)
                                && !unsuffixed_literal_fits(val, expected_ret) =>
                        {
                            diags.error(
                                format!(
                                    "return type mismatch: expected `{expected_ret:?}`, got `{actual:?}`"
                                ),
                                "E300",
                                val.span(),
                            );
                        }
                        None => {
                            diags.error(
                                format!(
                                    "return type mismatch: function `{fn_name}` does not declare a return type"
                                ),
                                "E300",
                                val.span(),
                            );
                        }
                        Some(_) => {}
                    }
                    last_type = Some(actual);
                } else {
                    if let Some(expected_ret) = expected_ret
                        && *expected_ret != Type::Void
                    {
                        diags.error(
                            format!(
                                "return type mismatch: expected `{expected_ret:?}`, got `Void`"
                            ),
                            "E300",
                            block.span,
                        );
                    }
                    last_type = Some(Type::Void);
                }
            }

            Stmt::Assume(assume) => {
                let cond_ty =
                    check_expr(&assume.cond, symbols, scope, fn_name, expected_ret, diags);
                if cond_ty != Type::B1 {
                    diags.error("`assume` condition must be b1", "E340", assume.cond.span());
                }
                last_type = None;
            }

            Stmt::Assert(assert) => {
                let cond_ty =
                    check_expr(&assert.cond, symbols, scope, fn_name, expected_ret, diags);
                if cond_ty != Type::B1 {
                    diags.error("`assert` condition must be b1", "E341", assert.cond.span());
                }
                last_type = None;
            }

            Stmt::Break(_) | Stmt::Continue(_) => {}

            Stmt::Asm(asm_stmt) => {
                // Resolve and type-check operand expressions so undefined names
                // and type errors surface before IR. Output targets must be
                // assignable places and their constraint must start with `=`.
                for (constraint, target) in &asm_stmt.outputs {
                    check_expr(target, symbols, scope, fn_name, expected_ret, diags);
                    if crate::parser::expr_to_lvalue(target.clone()).is_none() {
                        diags.error(
                            "asm output operand must be an assignable place",
                            "E314",
                            target.span(),
                        );
                    }
                    if !constraint.starts_with('=') {
                        diags.error(
                            format!(
                                "asm output constraint must start with `=`, got `{constraint}`"
                            ),
                            "E108",
                            asm_stmt.span,
                        );
                    }
                }
                for (_constraint, value) in &asm_stmt.inputs {
                    check_expr(value, symbols, scope, fn_name, expected_ret, diags);
                }
                last_type = None;
            }

            Stmt::Match(match_stmt) => {
                let scrutinee_ty = check_expr(
                    &match_stmt.scrutinee,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                );
                check_match_coverage(&scrutinee_ty, &match_stmt.arms, match_stmt.span, diags);

                // Each arm runs from the same pre-match move-state; the
                // post-match state is the union of all arm exits (a local moved
                // in any arm is moved afterward).
                let before = scope.snapshot();
                let mut merged: Option<Vec<HashMap<String, VarInfo>>> = None;
                for arm in &match_stmt.arms {
                    scope.restore(before.clone());
                    check_block(&arm.body, symbols, scope, fn_name, expected_ret, diags);
                    let after = scope.snapshot();
                    merged = Some(match merged {
                        None => after,
                        Some(mut acc) => {
                            or_moved(&mut acc, &after);
                            acc
                        }
                    });
                }
                if let Some(merged) = merged {
                    scope.restore(merged);
                }
                last_type = None;
            }

            Stmt::Block(inner_block) => {
                last_type = check_block(inner_block, symbols, scope, fn_name, expected_ret, diags);
            }
        }
    }

    // Check trailing expression while scope is still active
    if let Some(ref trailing) = block.trailing {
        last_type = Some(check_expr(
            trailing,
            symbols,
            scope,
            fn_name,
            expected_ret,
            diags,
        ));
    }

    scope.pop();
    last_type
}

/// Validate a match's patterns against its scrutinee type and check
/// exhaustiveness. Shared by the statement and expression forms. The scrutinee
/// must be an enum (variant patterns) or an integer (int/range patterns); an
/// integer match must include a `_` arm since the value space can't be
/// enumerated.
fn check_match_coverage(
    scrutinee_ty: &Type,
    arms: &[ast::MatchArm],
    match_span: crate::source::Span,
    diags: &mut DiagnosticBag,
) {
    let enum_info = if let Type::Enum(name, _, variants) = scrutinee_ty {
        Some((name.clone(), variants.clone()))
    } else {
        None
    };
    let is_int = types::is_int(scrutinee_ty);
    if enum_info.is_none() && !is_int {
        diags.error(
            "match scrutinee must be an enum or integer type",
            "E324",
            match_span,
        );
        return;
    }

    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut has_wildcard = false;
    for arm in arms {
        for pat in &arm.patterns {
            match pat {
                ast::MatchPattern::Variant((_, _), (v_name, v_span)) => {
                    if let Some((ename, variants)) = &enum_info {
                        if !variants.iter().any(|(n, _)| n == v_name) {
                            diags.error(
                                format!("no variant `{v_name}` in enum `{ename}`"),
                                "E322",
                                *v_span,
                            );
                        }
                        if !covered.insert(v_name.clone()) {
                            diags.error(
                                format!("duplicate variant `{v_name}` in match"),
                                "E319",
                                *v_span,
                            );
                        }
                    } else {
                        diags.error("enum-variant pattern in an integer match", "E324", *v_span);
                    }
                }
                ast::MatchPattern::Int(_, span) | ast::MatchPattern::Range(_, _, span) => {
                    if enum_info.is_some() {
                        diags.error("integer pattern in an enum match", "E324", *span);
                    }
                }
                ast::MatchPattern::Wildcard(span) => {
                    if has_wildcard {
                        diags.error("duplicate wildcard arm", "E319", *span);
                    }
                    has_wildcard = true;
                }
            }
        }
    }

    if let Some((_, variants)) = &enum_info {
        if !has_wildcard && covered.len() < variants.len() {
            let missing: Vec<&str> = variants
                .iter()
                .filter(|(n, _)| !covered.contains(n))
                .map(|(n, _)| n.as_str())
                .collect();
            diags.error(
                format!(
                    "non-exhaustive match: missing variants {}",
                    missing.join(", ")
                ),
                "E325",
                match_span,
            );
        }
    } else if !has_wildcard {
        diags.error(
            "non-exhaustive match: an integer match must have a `_` arm",
            "E325",
            match_span,
        );
    }
}

fn block_definitely_returns(block: &ast::Block) -> bool {
    block.stmts.iter().any(stmt_definitely_returns)
}

fn stmt_definitely_returns(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::Block(block) => block_definitely_returns(block),
        Stmt::If(if_stmt) => {
            let then_returns = block_definitely_returns(&if_stmt.then_block);
            let else_returns = if_stmt
                .else_branch
                .as_ref()
                .is_some_and(|else_branch| stmt_definitely_returns(else_branch));
            then_returns && else_returns
        }
        Stmt::Match(match_stmt) => match_stmt
            .arms
            .iter()
            .all(|arm| block_definitely_returns(&arm.body)),
        Stmt::Loop(loop_stmt) => {
            block_definitely_returns(&loop_stmt.body) && !block_may_break(&loop_stmt.body)
        }
        Stmt::VarDecl(_)
        | Stmt::Assign(_)
        | Stmt::CompoundAssign(_)
        | Stmt::Expr(_)
        | Stmt::While(_)
        | Stmt::For(_)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::Asm(_)
        | Stmt::Assume(_)
        | Stmt::Assert(_) => false,
    }
}

fn block_may_break(block: &ast::Block) -> bool {
    block.stmts.iter().any(stmt_may_break)
}

fn stmt_may_break(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Break(_) => true,
        Stmt::Block(block) => block_may_break(block),
        Stmt::If(if_stmt) => {
            block_may_break(&if_stmt.then_block)
                || if_stmt
                    .else_branch
                    .as_ref()
                    .is_some_and(|else_branch| stmt_may_break(else_branch))
        }
        Stmt::Match(match_stmt) => match_stmt.arms.iter().any(|arm| block_may_break(&arm.body)),
        Stmt::Loop(_) | Stmt::While(_) | Stmt::For(_) => false,
        Stmt::VarDecl(_)
        | Stmt::Assign(_)
        | Stmt::CompoundAssign(_)
        | Stmt::Expr(_)
        | Stmt::Return(_)
        | Stmt::Continue(_)
        | Stmt::Asm(_)
        | Stmt::Assume(_)
        | Stmt::Assert(_) => false,
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::only_used_in_recursion)]
fn check_call_args(
    fn_sym: &crate::resolver::FnSymbol,
    name: &str,
    callee_span: &crate::source::Span,
    args: &[Expr],
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) {
    if args.len() == fn_sym.params.len() {
        for (arg, (param_name, param_ty)) in args.iter().zip(fn_sym.params.iter()) {
            let arg_ty = check_expr(arg, symbols, scope, fn_name, expected_ret, diags);
            if !types::types_compatible(param_ty, &arg_ty)
                && !unsuffixed_literal_fits(arg, param_ty)
            {
                diags.error(
                    format!(
                        "argument `{param_name}` of `{name}` expects `{param_ty:?}`, got `{arg_ty:?}`"
                    ),
                    "E308",
                    arg.span(),
                );
            }
        }
    } else {
        diags.error(
            format!(
                "function `{name}` expects {} arguments, got {}",
                fn_sym.params.len(),
                args.len()
            ),
            "E307",
            *callee_span,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn check_ast_call_args(
    params: &[ast::Param],
    ret: Option<&ast::TypeExpr>,
    name: &str,
    callee_span: crate::source::Span,
    args: &[Expr],
    symbols: &SymbolTable,
    structs: &HashMap<String, Vec<(String, Type)>>,
    enums: &types::EnumDefs,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) -> Type {
    if args.len() == params.len() {
        for (arg, param) in args.iter().zip(params.iter()) {
            let param_ty = types::resolve_type_expr(&param.ty, structs, enums);
            let arg_ty = check_expr(arg, symbols, scope, fn_name, expected_ret, diags);
            if !types::types_compatible(&param_ty, &arg_ty)
                && !unsuffixed_literal_fits(arg, &param_ty)
            {
                diags.error(
                    format!(
                        "argument `{}` of `{name}` expects `{param_ty:?}`, got `{arg_ty:?}`",
                        param.name.0
                    ),
                    "E308",
                    arg.span(),
                );
            }
        }
    } else {
        diags.error(
            format!(
                "function `{name}` expects {} arguments, got {}",
                params.len(),
                args.len()
            ),
            "E307",
            callee_span,
        );
    }

    ret.map_or(Type::Void, |ty| {
        types::resolve_type_expr(ty, structs, enums)
    })
}

#[allow(clippy::only_used_in_recursion)]
fn check_expr(
    expr: &Expr,
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) -> Type {
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
        Expr::FloatLiteral(n, suffix, span) => {
            let ty = match suffix {
                crate::ast::FloatSuffix::H => Type::F16,
                crate::ast::FloatSuffix::F => Type::F32,
                crate::ast::FloatSuffix::D => Type::F64,
                crate::ast::FloatSuffix::None => Type::F32,
            };
            if *suffix == crate::ast::FloatSuffix::None
                && types::is_float(&ty)
                && (!n.is_finite() || *n < f64::from(f32::MIN) || *n > f64::from(f32::MAX))
            {
                diags.error(
                    format!("unsuffixed float literal `{n}` out of range for f32"),
                    "E300",
                    *span,
                );
            }
            ty
        }
        Expr::BoolLiteral(_, _) => Type::B1,
        Expr::NullLiteral(_) => Type::Null,
        Expr::StringLiteral(_, _) => Type::ConstPtr(Box::new(Type::U8)),
        Expr::Ident((name, span)) => {
            // Check local scope first
            if let Some(info) = scope.lookup(name) {
                let ty = info.ty.clone();
                if info.moved {
                    diags.error(format!("use of moved value: `{name}`"), "E304", *span);
                    return ty;
                }
                // Reading a Move-typed local consumes it: any later read is a
                // use-after-move until the local is reassigned (see
                // `mark_assigned`). Copy-typed locals are unaffected.
                if ty.is_move()
                    && let Some(info) = scope.lookup_mut(name)
                {
                    info.moved = true;
                }
                return ty;
            }
            // Check static symbols
            if let Some(sym) = symbols.statics.get(name) {
                return sym.ty.clone();
            }
            // Check const symbols
            if let Some(sym) = symbols.consts.get(name) {
                return sym.ty.clone();
            }
            // Check peripheral symbols
            if symbols.peripherals.contains_key(name) {
                // Peripherals are MMIO types
                return Type::Mmio(Box::new(Type::Unresolved(name.clone())));
            }
            // Check functions
            if let Some(fn_sym) = symbols.functions.get(name) {
                let params: Vec<Type> = fn_sym.params.iter().map(|(_, t)| t.clone()).collect();
                let ret = fn_sym.ret.clone().unwrap_or(Type::Void);
                return Type::Fn(params, Box::new(ret));
            }

            diags.error(format!("undefined name: `{name}`"), "E305", *span);
            Type::Unresolved(name.clone())
        }

        Expr::Unary(op, inner) => {
            use crate::ast::UnaryOp;
            // Taking the address of a local borrows it; it does not consume a
            // Move-typed value. So read the operand's type without consuming.
            let inner_ty = if matches!(op, UnaryOp::AddrOf | UnaryOp::AddrOfMut) {
                read_place_type(inner, symbols, scope, fn_name, expected_ret, diags)
            } else {
                check_expr(inner, symbols, scope, fn_name, expected_ret, diags)
            };
            match op {
                UnaryOp::Neg => {
                    if inner_ty != Type::U32 && inner_ty != Type::U64 && inner_ty != Type::U16 {
                        // Don't error on Unresolved types
                    }
                    inner_ty
                }
                UnaryOp::Not => {
                    if inner_ty != Type::B1 {
                        diags.error("logical not requires b1", "E306", inner.span());
                    }
                    Type::B1
                }
                UnaryOp::BitNot => inner_ty,
                UnaryOp::Deref => match &inner_ty {
                    Type::Ptr(pointee) | Type::ConstPtr(pointee) => {
                        if *pointee.as_ref() == Type::Void {
                            diags.error(
                                "cannot dereference opaque pointer (`*void`)",
                                "E315",
                                inner.span(),
                            );
                        }
                        pointee.as_ref().clone()
                    }
                    Type::Unresolved(_) => inner_ty,
                    _ => {
                        diags.error("dereference requires pointer type", "E315", inner.span());
                        inner_ty
                    }
                },
                UnaryOp::AddrOf => {
                    // Taking address of a function produces a function pointer
                    if let Expr::Ident((name, span)) = inner.as_ref()
                        && let Some(fn_sym) = symbols.functions.get(name)
                    {
                        if fn_sym.context != crate::context::Context::Any {
                            diags.error(
                                format!("cannot take address of non-any-context function `{name}` -- only functions without @context restriction can be used as function pointers"),
                                "E408",
                                *span,
                            );
                        }
                        let params: Vec<Type> =
                            fn_sym.params.iter().map(|(_, t)| t.clone()).collect();
                        let ret = fn_sym.ret.clone().unwrap_or(Type::Void);
                        return Type::Fn(params, Box::new(ret));
                    }
                    if inner_ty.is_move() {
                        Type::ConstPtr(Box::new(inner_ty.inner().clone()))
                    } else {
                        Type::ConstPtr(Box::new(inner_ty))
                    }
                }
                UnaryOp::AddrOfMut => {
                    // Can only take &mut of mutable variables or statics
                    if let Expr::Ident((name, span)) = inner.as_ref()
                        && let Some(info) = scope.lookup(name)
                        && !info.mutable
                    {
                        diags.error(
                            format!("cannot take mutable address of immutable `{name}`"),
                            "E309",
                            *span,
                        );
                    }
                    let ty = if inner_ty.is_move() {
                        inner_ty.inner().clone()
                    } else {
                        inner_ty
                    };
                    Type::Ptr(Box::new(ty))
                }
            }
        }

        Expr::Binary(left, op, right) => {
            use crate::ast::BinaryOp;
            let left_ty = check_expr(left, symbols, scope, fn_name, expected_ret, diags);
            let right_ty = check_expr(right, symbols, scope, fn_name, expected_ret, diags);

            match op {
                BinaryOp::Add | BinaryOp::Sub => {
                    // Pointer arithmetic: p + n, p - n, p - q
                    if types::is_ptr(&left_ty) && types::is_int(&right_ty) {
                        // pointer + int → pointer (GEP)
                        left_ty.clone()
                    } else if types::is_ptr(&left_ty)
                        && types::is_ptr(&right_ty)
                        && *op == BinaryOp::Sub
                    {
                        // pointer - pointer → signed int (element diff)
                        Type::I32
                    } else if left_ty != right_ty {
                        diags.error(
                            format!(
                                "arithmetic between different types `{left_ty:?}` and `{right_ty:?}` -- use `as` to cast"
                            ),
                            "E310",
                            left.span(),
                        );
                        left_ty.clone()
                    } else {
                        left_ty.clone()
                    }
                }
                BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
                    if left_ty != right_ty {
                        diags.error(
                            format!(
                                "arithmetic between different types `{left_ty:?}` and `{right_ty:?}` -- use `as` to cast"
                            ),
                            "E310",
                            left.span(),
                        );
                    }
                    left_ty.clone()
                }
                BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::Gt
                | BinaryOp::LtEq
                | BinaryOp::GtEq => {
                    // null is only comparable with pointer-shaped types (and
                    // itself). Without this guard, `null == 5` would slip
                    // through the bare `left_ty != right_ty` check.
                    if !types::types_compatible(&left_ty, &right_ty) {
                        diags.error(
                            format!(
                                "comparison between different types `{left_ty:?}` and `{right_ty:?}` -- use `as` to cast"
                            ),
                            "E311",
                            left.span(),
                        );
                    }
                    Type::B1
                }
                BinaryOp::And | BinaryOp::Or => {
                    if left_ty != Type::B1 {
                        diags.error(
                            format!("logical operator expects b1, got `{left_ty:?}`"),
                            "E316",
                            left.span(),
                        );
                    }
                    if right_ty != Type::B1 {
                        diags.error(
                            format!("logical operator expects b1, got `{right_ty:?}`"),
                            "E316",
                            right.span(),
                        );
                    }
                    Type::B1
                }
                BinaryOp::BitAnd
                | BinaryOp::BitOr
                | BinaryOp::BitXor
                | BinaryOp::Shl
                | BinaryOp::Shr => {
                    if !types::is_int(&left_ty) && !matches!(left_ty, Type::B1 | Type::B8) {
                        diags.error(
                            format!("bitwise operator expects integer type, got `{left_ty:?}`"),
                            "E317",
                            left.span(),
                        );
                    }
                    if !types::is_int(&right_ty) && !matches!(right_ty, Type::B1 | Type::B8) {
                        diags.error(
                            format!("bitwise operator expects integer type, got `{right_ty:?}`"),
                            "E317",
                            right.span(),
                        );
                    }
                    left_ty.clone()
                }
            }
        }

        Expr::Call(func_expr, args) => {
            if let Expr::Ident((name, span)) = func_expr.as_ref()
                && let Some(fn_sym) = symbols.functions.get(name)
            {
                check_call_args(
                    fn_sym,
                    name,
                    span,
                    args,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                );
                return fn_sym.ret.clone().unwrap_or(Type::Void);
            }

            // Try import alias: L.foo()
            if let Expr::FieldAccess(base, field) = func_expr.as_ref()
                && let Expr::Ident((alias_name, _)) = base.as_ref()
                && let Some(alias_info) = symbols.import_aliases.get(alias_name)
                && let Some(item) = alias_info.exports.get(&field.0)
            {
                let (alias_structs, alias_enums) =
                    types::alias_type_defs(&alias_info.items, &symbols.structs, &symbols.enums);
                return match item {
                    Item::FnDef(f) => check_ast_call_args(
                        &f.params,
                        f.ret.as_ref(),
                        &f.name.0,
                        field.1,
                        args,
                        symbols,
                        &alias_structs,
                        &alias_enums,
                        scope,
                        fn_name,
                        expected_ret,
                        diags,
                    ),
                    Item::ExternFnDef(f) => check_ast_call_args(
                        &f.params,
                        f.ret.as_ref(),
                        &f.name.0,
                        field.1,
                        args,
                        symbols,
                        &alias_structs,
                        &alias_enums,
                        scope,
                        fn_name,
                        expected_ret,
                        diags,
                    ),
                    _ => {
                        let guard = diags.error("cannot call non-function type", "E327", field.1);
                        Type::Error(guard)
                    }
                };
            }

            if let Expr::Ident((name, span)) = func_expr.as_ref()
                && scope.lookup(name).is_none()
                && !symbols.statics.contains_key(name)
                && !symbols.consts.contains_key(name)
            {
                // Genuinely undefined name. Calling a name that resolves through
                // an alias's exports unqualified is also reported here -- the
                // user must write `Alias.name(...)` for those.
                let guard = diags.error(format!("undefined name: `{name}`"), "E305", *span);
                for arg in args {
                    check_expr(arg, symbols, scope, fn_name, expected_ret, diags);
                }
                return Type::Error(guard);
            }

            // Indirect call via function pointer expression
            let callee_ty = check_expr(func_expr, symbols, scope, fn_name, expected_ret, diags);
            if let Type::Fn(params, ret) = &callee_ty {
                if args.len() == params.len() {
                    for (arg, param_ty) in args.iter().zip(params.iter()) {
                        let arg_ty = check_expr(arg, symbols, scope, fn_name, expected_ret, diags);
                        if !types::types_compatible(param_ty, &arg_ty)
                            && !unsuffixed_literal_fits(arg, param_ty)
                        {
                            diags.error(
                                format!(
                                    "argument type mismatch: expected `{param_ty:?}`, got `{arg_ty:?}`"
                                ),
                                "E308",
                                arg.span(),
                            );
                        }
                    }
                } else {
                    diags.error(
                        format!(
                            "indirect call expects {} arguments, got {}",
                            params.len(),
                            args.len()
                        ),
                        "E307",
                        func_expr.span(),
                    );
                }
                return *ret.clone();
            }
            let guard = diags.error("cannot call non-function type", "E327", func_expr.span());
            Type::Error(guard)
        }

        Expr::FieldAccess(base, field) => {
            // Try peripheral register/field read patterns first
            match base.as_ref() {
                Expr::Ident((periph_name, _)) => {
                    // GPIOA.REG -- register read
                    if let Some(p) = symbols.peripherals.get(periph_name) {
                        if let Some(reg) = p.regs.get(&field.0) {
                            if reg.access == crate::ast::Access::WriteOnly {
                                diags.error(
                                    format!(
                                        "cannot read from writeonly register `{periph_name}.{}`",
                                        field.0
                                    ),
                                    "E330",
                                    field.1,
                                );
                            }
                        } else {
                            diags.error(
                                format!("peripheral `{periph_name}` has no register `{}`", field.0),
                                "E322",
                                field.1,
                            );
                        }
                        return Type::U32;
                    }
                }
                Expr::FieldAccess(inner, reg_field) => {
                    if let Expr::Ident((periph_name, _)) = inner.as_ref() {
                        // GPIOA.REG.FIELD -- field read
                        if let Some(p) = symbols.peripherals.get(periph_name) {
                            if let Some(reg) = p.regs.get(&reg_field.0) {
                                if let Some(field_sym) = reg.fields.get(&field.0) {
                                    if field_sym.access == crate::ast::Access::WriteOnly {
                                        diags.error(
                                            format!(
                                                "cannot read from writeonly field `{periph_name}.{}.{}`",
                                                reg_field.0, field.0
                                            ),
                                            "E330",
                                            field.1,
                                        );
                                    }
                                    return field_sym.ty.clone();
                                }
                                diags.error(
                                    format!(
                                        "register `{}` has no field `{}`",
                                        reg_field.0, field.0
                                    ),
                                    "E322",
                                    field.1,
                                );
                                return Type::U32;
                            }
                            diags.error(
                                format!(
                                    "peripheral `{periph_name}` has no register `{}`",
                                    reg_field.0
                                ),
                                "E322",
                                reg_field.1,
                            );
                            return Type::U32;
                        }
                    }
                }
                _ => {}
            }

            let base_ty = check_expr(base, symbols, scope, fn_name, expected_ret, diags);
            // Check if it's a struct field access
            if let Type::Struct(name, fields) = &base_ty {
                if let Some((_, field_ty)) = fields.iter().find(|(n, _)| n == &field.0) {
                    return field_ty.clone();
                }
                let guard = diags.error(
                    format!("struct `{name}` has no field `{}`", field.0),
                    "E318",
                    field.1,
                );
                return Type::Error(guard);
            }
            // Check if it's a pointer to struct (auto-deref for field access)
            if let Type::Ptr(inner) | Type::ConstPtr(inner) = &base_ty
                && let Type::Struct(name, fields) = inner.as_ref()
            {
                if let Some((_, field_ty)) = fields.iter().find(|(n, _)| n == &field.0) {
                    return field_ty.clone();
                }
                let guard = diags.error(
                    format!("struct `{name}` has no field `{}`", field.0),
                    "E318",
                    field.1,
                );
                return Type::Error(guard);
            }
            // Fallback: unknown field access
            Type::U32
        }

        Expr::Index(base, index) => {
            // Indexing addresses a place through `base`; it borrows the
            // container, it does not move it. Read the base non-consuming so a
            // Move-typed view (`view mut`/`ring mut`/`bits mut`) can be indexed
            // repeatedly (e.g. in a loop). Only a binding transfer of the view
            // consumes it.
            let base_ty = read_place_type(base, symbols, scope, fn_name, expected_ret, diags);
            check_expr(index, symbols, scope, fn_name, expected_ret, diags);
            index_element_type(base_ty, base, diags)
        }

        Expr::ArrayInit(elems, _) => {
            let elem_ty = if let Some(first) = elems.first() {
                check_expr(first, symbols, scope, fn_name, expected_ret, diags)
            } else {
                // Empty array literal: element type must come from context
                // (annotation). Not an error on its own — relies on the
                // Unresolved leniency rule for the eventual annotation match.
                Type::Unresolved("empty-array".into())
            };
            for elem in elems.iter().skip(1) {
                let ty = check_expr(elem, symbols, scope, fn_name, expected_ret, diags);
                if ty != elem_ty {
                    diags.error(
                        format!("array element type mismatch: `{elem_ty:?}` vs `{ty:?}`"),
                        "E313",
                        elem.span(),
                    );
                }
            }
            Type::Array(Box::new(elem_ty), elems.len())
        }
        Expr::Cast(inner, ty_expr) => {
            check_expr(inner, symbols, scope, fn_name, expected_ret, diags);
            let target_ty = types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
            // Warn on literal narrowing
            if let Expr::IntLiteral(n, _, _) = inner.as_ref() {
                let (min, max) = int_range(&target_ty);
                let val = i128::from(*n);
                if val < min || val > max {
                    diags.warn(
                        format!(
                            "literal `{n}` may be truncated in cast to `{target_ty:?}` (range {min}..{max})"
                        ),
                        "W301",
                        inner.span(),
                    );
                }
            }
            target_ty
        }
        Expr::SizeOf(ty_expr, _span) => {
            let resolved = types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
            if let Type::Unresolved(name) = &resolved
                && let crate::ast::TypeExpr::Named((_, type_span)) = ty_expr
            {
                diags.error(format!("undefined type: `{name}`"), "E305", *type_span);
            }
            Type::U32
        }
        Expr::ViewNew {
            base,
            len,
            stride,
            span,
        } => {
            let base_ty = check_expr(base, symbols, scope, fn_name, expected_ret, diags);
            if let Some(stride) = stride {
                // `view(arr, stride K)`: strided view over an array. The stride
                // is a compile-time element multiplier (>= 1) carried in the
                // type, not a runtime descriptor field. Element type comes from
                // the array; the logical length `N/K` is computed at lowering.
                let k = match stride.as_ref() {
                    crate::ast::Expr::IntLiteral(n, _, _) if *n >= 1 => *n as u32,
                    _ => {
                        let guard = diags.error(
                            "`view` stride must be a compile-time integer >= 1".to_string(),
                            "E332",
                            stride.span(),
                        );
                        return Type::Error(guard);
                    }
                };
                let mutable = is_mutable_place(base, scope, symbols);
                if let Some(guard) = reject_shared_view_backing(&base_ty, *span, diags) {
                    Type::Error(guard)
                } else if let Type::Array(inner, _) = base_ty.inner() {
                    Type::StridedView(Box::new((**inner).clone()), mutable, k)
                } else {
                    let guard = diags.error(
                        format!("`view(x, stride K)` argument must be an array, got `{base_ty}`"),
                        "E333",
                        *span,
                    );
                    Type::Error(guard)
                }
            } else if let Some(len) = len {
                // `view(ptr, len)`: base must be a pointer, len an integer.
                let len_ty = check_expr(len, symbols, scope, fn_name, expected_ret, diags);
                if !types::is_int(&len_ty) {
                    diags.error(
                        format!("`view` length must be an integer, got `{len_ty}`"),
                        "E332",
                        len.span(),
                    );
                }
                // A view over `*mut T` is mutable; over `*T` it is readonly.
                match base_ty {
                    Type::Ptr(inner) => Type::LinearView(inner, true),
                    Type::ConstPtr(inner) => Type::LinearView(inner, false),
                    other => {
                        let guard = diags.error(
                            format!(
                                "`view(ptr, len)` first argument must be a pointer, got `{other}`"
                            ),
                            "E333",
                            *span,
                        );
                        Type::Error(guard)
                    }
                }
            } else {
                // `view(arr)`: base must be an array; length is taken from it.
                // A view over a mutable place (a `var` array or a static) is
                // mutable; otherwise it is readonly. `.inner()` sees through a
                // storage wrapper (`@shared`/`@dma`/`@external`/`@exclusive`) so a
                // view over a storage-class array is allowed; the storage stays
                // out of the view's type identity.
                let mutable = is_mutable_place(base, scope, symbols);
                if let Some(guard) = reject_shared_view_backing(&base_ty, *span, diags) {
                    Type::Error(guard)
                } else if let Type::Array(inner, _) = base_ty.inner() {
                    Type::LinearView(Box::new((**inner).clone()), mutable)
                } else {
                    let guard = diags.error(
                        format!("`view(x)` argument must be an array (or use `view(ptr, len)`), got `{base_ty}`"),
                        "E333",
                        *span,
                    );
                    Type::Error(guard)
                }
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            span,
        } => {
            let base_ty = check_expr(base, symbols, scope, fn_name, expected_ret, diags);
            // capacity (when explicit), head, and len must be integers.
            for (arg, present) in [
                (capacity.as_deref(), capacity.is_some()),
                (Some(head.as_ref()), true),
                (Some(len.as_ref()), true),
            ] {
                if present && let Some(arg) = arg {
                    let ty = check_expr(arg, symbols, scope, fn_name, expected_ret, diags);
                    if !types::is_int(&ty) {
                        diags.error(
                            format!("`ring` capacity/head/len must be an integer, got `{ty}`"),
                            "E332",
                            arg.span(),
                        );
                    }
                }
            }
            if capacity.is_some() {
                // `ring(ptr, capacity, head, len)`: a view over `*mut T` is
                // mutable, over `*T` readonly.
                // Explicit/runtime capacity: no compile-time hint, so the mask
                // optimization never applies (indexing stays `urem`).
                match base_ty {
                    Type::Ptr(inner) => Type::RingView(inner, true, None),
                    Type::ConstPtr(inner) => Type::RingView(inner, false, None),
                    other => {
                        let guard = diags.error(
                            format!(
                                "`ring(ptr, ...)` first argument must be a pointer, got `{other}`"
                            ),
                            "E333",
                            *span,
                        );
                        Type::Error(guard)
                    }
                }
            } else {
                // `ring(arr, head, len)`: capacity comes from the array; the ring
                // is mutable iff the array is a mutable place. `.inner()` sees
                // through a storage wrapper so a ring over a storage-class array
                // is allowed.
                let mutable = is_mutable_place(base, scope, symbols);
                if let Some(guard) = reject_shared_view_backing(&base_ty, *span, diags) {
                    Type::Error(guard)
                } else if let Type::Array(inner, n) = base_ty.inner() {
                    // Capacity is the array length. Carry it as a compile-time
                    // hint only when it is a power of two, which is what lets
                    // the physical index lower to `& (n - 1)` instead of `urem`.
                    let cap_hint = n.is_power_of_two().then_some(*n as u32);
                    Type::RingView(Box::new((**inner).clone()), mutable, cap_hint)
                } else {
                    let guard = diags.error(
                        format!("`ring(x, head, len)` first argument must be an array (or use `ring(ptr, capacity, head, len)`), got `{base_ty}`"),
                        "E333",
                        *span,
                    );
                    Type::Error(guard)
                }
            }
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            span,
        } => {
            let base_ty = check_expr(base, symbols, scope, fn_name, expected_ret, diags);
            // bit_offset and len_bits (explicit form) must be integers.
            for arg in [bit_offset.as_deref(), len_bits.as_deref()]
                .into_iter()
                .flatten()
            {
                let ty = check_expr(arg, symbols, scope, fn_name, expected_ret, diags);
                if !types::is_int(&ty) {
                    diags.error(
                        format!("`bits` bit_offset/len_bits must be an integer, got `{ty}`"),
                        "E332",
                        arg.span(),
                    );
                }
            }
            if bit_offset.is_some() {
                // `bits(ptr, bit_offset, len_bits)`: byte pointer; a `*mut u8`
                // is mutable, a `&u8` readonly.
                let mutability = match &base_ty {
                    Type::Ptr(inner) if matches!(**inner, Type::U8 | Type::B8) => Some(true),
                    Type::ConstPtr(inner) if matches!(**inner, Type::U8 | Type::B8) => Some(false),
                    _ => None,
                };
                if let Some(mutable) = mutability {
                    Type::BitView(mutable)
                } else {
                    let guard = diags.error(
                        format!("`bits(ptr, ...)` first argument must be a byte pointer (`*u8`/`&u8`), got `{base_ty}`"),
                        "E333",
                        *span,
                    );
                    Type::Error(guard)
                }
            } else {
                // `bits(arr)`: byte array; mutable iff the array is a mutable
                // place. `.inner()` sees through a storage wrapper so a bit view
                // over a storage-class byte array is allowed.
                let is_byte_array = matches!(base_ty.inner(), Type::Array(inner, _) if matches!(**inner, Type::U8 | Type::B8));
                if let Some(guard) = reject_shared_view_backing(&base_ty, *span, diags) {
                    Type::Error(guard)
                } else if is_byte_array {
                    Type::BitView(is_mutable_place(base, scope, symbols))
                } else {
                    let guard = diags.error(
                        format!("`bits(x)` argument must be a byte array (`[u8; N]`/`[b8; N]`), got `{base_ty}`"),
                        "E333",
                        *span,
                    );
                    Type::Error(guard)
                }
            }
        }
        Expr::Group(inner) => check_expr(inner, symbols, scope, fn_name, expected_ret, diags),
        Expr::Match(match_expr) => {
            let scrutinee_ty = check_expr(
                &match_expr.scrutinee,
                symbols,
                scope,
                fn_name,
                expected_ret,
                diags,
            );
            check_match_coverage(&scrutinee_ty, &match_expr.arms, match_expr.span, diags);

            let mut arm_type: Option<Type> = None;

            // Union the move-state across all arms (see the statement-form match).
            let before = scope.snapshot();
            let mut merged: Option<Vec<HashMap<String, VarInfo>>> = None;
            for arm in &match_expr.arms {
                scope.restore(before.clone());
                let arm_result =
                    check_block(&arm.body, symbols, scope, fn_name, expected_ret, diags);
                let after = scope.snapshot();
                merged = Some(match merged.take() {
                    None => after,
                    Some(mut acc) => {
                        or_moved(&mut acc, &after);
                        acc
                    }
                });
                // An arm that directly terminates (e.g. `Pat => { return; }`)
                // contributes no value to the match expression. BML has no
                // never/bottom type, so unlike Rust's `!` such arms cannot
                // unify with a value-producing arm; they trigger E328 below.
                // Lift this restriction once Type::Never is introduced.
                let arm_trailing_ty =
                    if arm.body.trailing.is_some() && !arm.body.has_direct_terminator() {
                        arm_result
                    } else {
                        None
                    };

                match (&arm_type, arm_trailing_ty) {
                    (None, Some(ty)) => arm_type = Some(ty),
                    (Some(expected), Some(ty)) if types::types_compatible(expected, &ty) => {}
                    (Some(expected), Some(ty)) => {
                        diags.error(
                            format!(
                                "match arm type mismatch: expected `{expected:?}`, got `{ty:?}`"
                            ),
                            "E327",
                            arm.body.span,
                        );
                    }
                    (Some(_), None) => {
                        diags.error(
                            "match arm missing trailing expression",
                            "E328",
                            arm.body.span,
                        );
                    }
                    (None, None) => {
                        diags.error(
                            "match arm missing trailing expression",
                            "E328",
                            arm.body.span,
                        );
                    }
                }
            }
            if let Some(merged) = merged {
                scope.restore(merged);
            }

            // Fallback only if no arm provided a type (caused by earlier
            // diagnostics on missing trailing expressions in arms).
            arm_type.unwrap_or_else(|| {
                Type::Error(crate::errors::ErrorGuaranteed::unchecked_claim_error_was_emitted())
            })
        }
        Expr::Block(block_expr) => {
            let result = check_block(
                &block_expr.block,
                symbols,
                scope,
                fn_name,
                expected_ret,
                diags,
            );
            if block_expr.block.trailing.is_none() || block_expr.block.has_direct_terminator() {
                let guard = diags.error("block has no value", "E328", block_expr.span);
                Type::Error(guard)
            } else {
                result.expect("check_block returns Some when trailing.is_some()")
            }
        }
        Expr::If(if_expr) => {
            let cond_ty = check_expr(&if_expr.cond, symbols, scope, fn_name, expected_ret, diags);
            if cond_ty != Type::B1 {
                diags.error(
                    "if expression condition must be b1",
                    "E302",
                    if_expr.cond.span(),
                );
            }
            // Analyze both arms from the same pre-branch move-state, then union.
            let before = scope.snapshot();
            let then_result = check_block(
                &if_expr.then_block,
                symbols,
                scope,
                fn_name,
                expected_ret,
                diags,
            );
            // A then-block that terminates (e.g. `{ return; }`) produces no
            // value. Same restriction as match arms — see comment there;
            // lift once Type::Never exists.
            let then_ty = if if_expr.then_block.trailing.is_none()
                || if_expr.then_block.has_direct_terminator()
            {
                let guard = diags.error("if branch has no value", "E328", if_expr.then_block.span);
                Type::Error(guard)
            } else {
                then_result.expect("check_block returns Some when trailing.is_some()")
            };
            let after_then = scope.snapshot();
            scope.restore(before);
            let else_ty = check_expr(
                &if_expr.else_branch,
                symbols,
                scope,
                fn_name,
                expected_ret,
                diags,
            );
            scope.merge_moved(&after_then);

            if !types::types_compatible(&then_ty, &else_ty) {
                diags.error(
                    format!("if expression arm type mismatch: `{then_ty:?}` vs `{else_ty:?}`"),
                    "E327",
                    if_expr.span,
                );
            }
            then_ty
        }
        Expr::StructInit {
            name: (struct_name, span),
            fields,
            ..
        } => {
            let struct_type = Type::Unresolved(struct_name.clone());
            // Resolve struct definition
            if let Some(struct_fields) = symbols.structs.get(struct_name) {
                // Check all required fields are provided
                for (fname, ftype) in struct_fields {
                    let provided = fields.iter().find(|(n, _)| n.0 == *fname);
                    match provided {
                        Some((_, expr)) => {
                            let expr_ty =
                                check_expr(expr, symbols, scope, fn_name, expected_ret, diags);
                            // (unsuffixed literals are allowed if their value fits)
                            if !types::types_compatible(ftype, &expr_ty)
                                && !unsuffixed_literal_fits(expr, ftype)
                            {
                                diags.error(
                                    format!(
                                        "type mismatch for field `{fname}` of struct `{struct_name}`: expected `{ftype:?}`, got `{expr_ty:?}`"
                                    ),
                                    "E300",
                                    expr.span(),
                                );
                            }
                        }
                        None => {
                            diags.error(
                                format!(
                                    "missing field `{fname}` in struct `{struct_name}` initializer"
                                ),
                                "E320",
                                *span,
                            );
                        }
                    }
                }
                // Check for unknown fields
                for (fname, _) in fields {
                    if !struct_fields.iter().any(|(n, _)| n == &fname.0) {
                        diags.error(
                            format!("unknown field `{}` in struct `{struct_name}`", fname.0),
                            "E318",
                            fname.1,
                        );
                    }
                }
                // Check for duplicate fields in initializer
                let mut seen = std::collections::HashSet::new();
                for (fname, _) in fields {
                    if seen.contains(&fname.0) {
                        diags.error(
                            format!("duplicate field `{}` in struct initializer", fname.0),
                            "E321",
                            fname.1,
                        );
                    }
                    seen.insert(fname.0.clone());
                }
                Type::Struct(struct_name.clone(), struct_fields.clone())
            } else {
                diags.error(
                    format!("unknown struct type: `{struct_name}`"),
                    "E318",
                    *span,
                );
                struct_type
            }
        }

        Expr::EnumVariant {
            enum_name: (name, span),
            variant: (vname, vspan),
            ..
        } => {
            if let Some((inner_ty, variants)) = symbols.enums.get(name) {
                if !variants.iter().any(|(n, _)| n == vname) {
                    diags.error(
                        format!("enum `{name}` has no variant `{vname}`"),
                        "E322",
                        *vspan,
                    );
                }
                return Type::Enum(name.clone(), Box::new(inner_ty.clone()), variants.clone());
            }
            diags.error(format!("undefined enum type: `{name}`"), "E305", *span);
            Type::Unresolved(name.clone())
        }
    }
}

#[allow(clippy::only_used_in_recursion)]
fn check_lvalue(
    lval: &LValue,
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
    // True when this lvalue is the whole assignment target. False when it is a
    // base being projected through (an index/field base). The binding-mutability
    // check only fires for the root or for non-view bases; writing *through* a
    // view does not require a mutable binding (the view's own `mut` flag, checked
    // at the index site, governs that), the same way `*mut T` deref-writes do.
    root: bool,
) -> Type {
    match lval {
        LValue::Name((name, span)) => {
            if let Some(info) = scope.lookup(name) {
                // A whole-name assignment (`root`) *defines* the local, reviving a
                // previously-moved value (see `mark_assigned`), so no use-after-move
                // check there. But projecting into a moved value (`b[i] = x` after
                // `b` was moved away) is a use-after-move: the place no longer
                // exists. Writes do not consume, so this only reports, never moves.
                if !root && info.moved {
                    diags.error(format!("use of moved value: `{name}`"), "E304", *span);
                }
                let is_view = matches!(
                    info.ty,
                    Type::LinearView(..)
                        | Type::StridedView(..)
                        | Type::RingView(..)
                        | Type::BitView(..)
                );
                if !info.mutable && (root || !is_view) {
                    diags.error(
                        format!("cannot assign to immutable variable `{name}`"),
                        "E309",
                        *span,
                    );
                }
                return info.ty.clone();
            }
            // Check if it's a static
            if let Some(sym) = symbols.statics.get(name) {
                // Statics are always assignable (mutable in our model)
                return sym.ty.inner().clone();
            }
            // Check peripheral symbols
            if symbols.peripherals.contains_key(name) {
                return Type::Mmio(Box::new(Type::U32));
            }
            diags.error(format!("undefined name: `{name}`"), "E305", *span);
            Type::Unresolved(name.clone())
        }
        LValue::Field(base, field) => {
            // Try peripheral register/field write patterns first
            match base.as_ref() {
                LValue::Name((periph_name, _)) => {
                    // GPIOA.REG = val -- register write
                    if let Some(p) = symbols.peripherals.get(periph_name) {
                        if let Some(reg) = p.regs.get(&field.0) {
                            if reg.access == crate::ast::Access::ReadOnly {
                                diags.error(
                                    format!(
                                        "cannot write to readonly register `{periph_name}.{}`",
                                        field.0
                                    ),
                                    "E331",
                                    field.1,
                                );
                            }
                        } else {
                            diags.error(
                                format!("peripheral `{periph_name}` has no register `{}`", field.0),
                                "E322",
                                field.1,
                            );
                        }
                        return Type::U32;
                    }
                }
                LValue::Field(inner, reg_field) => {
                    if let LValue::Name((periph_name, _)) = inner.as_ref() {
                        // GPIOA.REG.FIELD = val -- field write
                        if let Some(p) = symbols.peripherals.get(periph_name) {
                            if let Some(reg) = p.regs.get(&reg_field.0) {
                                if let Some(field_sym) = reg.fields.get(&field.0) {
                                    if field_sym.access == crate::ast::Access::ReadOnly {
                                        diags.error(
                                            format!(
                                                "cannot write to readonly field `{periph_name}.{}.{}`",
                                                reg_field.0, field.0
                                            ),
                                            "E331",
                                            field.1,
                                        );
                                    }
                                    return field_sym.ty.clone();
                                }
                                diags.error(
                                    format!(
                                        "register `{}` has no field `{}`",
                                        reg_field.0, field.0
                                    ),
                                    "E322",
                                    field.1,
                                );
                                return Type::U32;
                            }
                            diags.error(
                                format!(
                                    "peripheral `{periph_name}` has no register `{}`",
                                    reg_field.0
                                ),
                                "E322",
                                reg_field.1,
                            );
                            return Type::U32;
                        }
                    }
                }
                _ => {}
            }

            let base_ty = check_lvalue(base, symbols, scope, fn_name, expected_ret, diags, false);
            // Check if it's a struct field write
            if let Type::Struct(name, fields) = &base_ty {
                if let Some((_, field_ty)) = fields.iter().find(|(n, _)| n == &field.0) {
                    return field_ty.clone();
                }
                let guard = diags.error(
                    format!("struct `{name}` has no field `{}`", field.0),
                    "E318",
                    field.1,
                );
                return Type::Error(guard);
            }
            // Fallback: unknown field access
            Type::U32
        }
        LValue::Index(base, index) => {
            let base_ty = check_lvalue(base, symbols, scope, fn_name, expected_ret, diags, false);
            check_expr(index, symbols, scope, fn_name, expected_ret, diags);
            match base_ty {
                Type::Array(inner, _) => *inner,
                Type::Ptr(inner) | Type::ConstPtr(inner) => *inner,
                // A mutable view permits index writes; a readonly view does not.
                Type::LinearView(inner, true)
                | Type::StridedView(inner, true, _)
                | Type::RingView(inner, true, _) => *inner,
                // A mutable bit view permits writes; the assigned value is a bit.
                Type::BitView(true) => Type::B1,
                Type::LinearView(_, false)
                | Type::StridedView(_, false, _)
                | Type::RingView(_, false, _)
                | Type::BitView(false) => {
                    let guard = diags.error(
                        "cannot write through a readonly `view`; only reads are allowed"
                            .to_string(),
                        "E334",
                        base.span(),
                    );
                    Type::Error(guard)
                }
                other => {
                    let guard = diags.error(
                        format!("cannot index value of type `{other:?}`"),
                        "E326",
                        base.span(),
                    );
                    Type::Error(guard)
                }
            }
        }
        LValue::Deref(inner) => {
            let inner_ty = check_expr(inner, symbols, scope, fn_name, expected_ret, diags);
            match &inner_ty {
                Type::Ptr(pointee) => pointee.as_ref().clone(),
                Type::ConstPtr(_) => {
                    let guard = diags.error(
                        "cannot write through const pointer (`*T`) -- use `*mut T`",
                        "E314",
                        inner.span(),
                    );
                    Type::Error(guard)
                }
                _ => {
                    let guard =
                        diags.error("dereference requires pointer type", "E315", inner.span());
                    Type::Error(guard)
                }
            }
        }
    }
}

/// Number of moved locals in a captured move-state snapshot.
fn count_moved(snap: &[HashMap<String, VarInfo>]) -> usize {
    snap.iter()
        .flat_map(HashMap::values)
        .filter(|info| info.moved)
        .count()
}

/// OR the moved flags from `other` into `into` (per local, by name and scope
/// depth). Used to accumulate the maybe-moved state across match arms.
fn or_moved(into: &mut [HashMap<String, VarInfo>], other: &[HashMap<String, VarInfo>]) {
    for (scope, oscope) in into.iter_mut().zip(other.iter()) {
        for (name, info) in scope.iter_mut() {
            if let Some(oinfo) = oscope.get(name) {
                info.moved |= oinfo.moved;
            }
        }
    }
}

/// Check a loop body with cross-iteration move awareness.
///
/// A move inside a loop body is a use-after-move on the next iteration. We find
/// the locals that leak as moved by running the body to a fixpoint over the
/// entry move-state (a dry run into a throwaway diagnostic bag), then do one
/// real pass from the converged state so use-after-move is reported once.
/// Reassignment revives a local within a single pass, so a local reassigned
/// before use each iteration does not leak and is not flagged.
fn check_loop_body(
    body: &ast::Block,
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) {
    let before = scope.snapshot();
    let mut entry = before;

    loop {
        scope.restore(entry.clone());
        let mut throwaway = DiagnosticBag::new();
        check_block(body, symbols, scope, fn_name, expected_ret, &mut throwaway);
        let exit = scope.snapshot();
        let prev = count_moved(&entry);
        or_moved(&mut entry, &exit);
        if count_moved(&entry) == prev {
            break;
        }
    }

    // Real pass from the converged entry state, with diagnostics. The loop may
    // run zero times (while/for) or many, so `entry` (pre-loop state unioned
    // with everything moved across iterations) is the sound after-loop state.
    scope.restore(entry.clone());
    check_block(body, symbols, scope, fn_name, expected_ret, diags);
    scope.restore(entry);
}

/// Is `expr` a mutable place? Used to decide whether `view(arr)` yields a
/// mutable or readonly view: a view over a `var` array (or a static, which is
/// assignable in this model) is mutable; over a `val` binding it is readonly.
/// Conservative: anything not recognized as a place is treated as immutable.
fn is_mutable_place(expr: &Expr, scope: &ScopeStack, symbols: &SymbolTable) -> bool {
    match expr {
        Expr::Ident((name, _)) => {
            if let Some(info) = scope.lookup(name) {
                info.mutable
            } else {
                symbols.statics.contains_key(name)
            }
        }
        Expr::Index(base, _) | Expr::FieldAccess(base, _) => is_mutable_place(base, scope, symbols),
        Expr::Group(inner) => is_mutable_place(inner, scope, symbols),
        // `*mut T` deref is a mutable place; `*T` is not. Other forms are not
        // places we can prove mutable.
        Expr::Unary(crate::ast::UnaryOp::Deref, _) => false,
        _ => false,
    }
}

/// A view over `@shared` memory is rejected: the `@shared` ceiling protocol is
/// enforced by a critical section emitted around *direct* static access, but a
/// view loads/stores through the descriptor pointer with no static name, so it
/// receives no critical section. Allowing it would silently produce an
/// unprotected race. The other storage classes (`@dma`/`@external`/`@exclusive`)
/// carry no ceiling protocol, so views over them are unaffected. Returns the
/// error guard when the backing is `@shared`, otherwise `None`.
fn reject_shared_view_backing(
    base_ty: &Type,
    span: crate::source::Span,
    diags: &mut DiagnosticBag,
) -> Option<crate::errors::ErrorGuaranteed> {
    if matches!(base_ty, Type::Shared(..)) {
        Some(
            diags.error(
                "cannot build a view over `@shared` memory: view access does not carry \
             the ceiling critical-section that direct access does, so it would be an \
             unprotected race. Access the static directly, or back the view with \
             non-`@shared` storage."
                    .to_string(),
                "E405",
                span,
            ),
        )
    } else {
        None
    }
}

/// The element type produced by indexing a value of type `base_ty`. Shared by
/// the `Expr::Index` read path and `read_place_type` so both agree on what
/// `base[i]` yields (and on the E326 "cannot index" diagnostic).
fn index_element_type(base_ty: Type, base: &Expr, diags: &mut DiagnosticBag) -> Type {
    match base_ty {
        Type::Array(inner, _) => *inner,
        Type::Ptr(inner) | Type::ConstPtr(inner) => *inner,
        Type::LinearView(inner, _)
        | Type::StridedView(inner, _, _)
        | Type::RingView(inner, _, _) => *inner,
        // A bit view yields a single bit regardless of mutability.
        Type::BitView(_) => Type::B1,
        other => {
            let guard = diags.error(
                format!("cannot index value of type `{other:?}`"),
                "E326",
                base.span(),
            );
            Type::Error(guard)
        }
    }
}

/// Read the type of a place expression without consuming it. Used for the
/// operand of `&`/`&mut` and for the base of an index: both borrow the place
/// rather than transferring it, so a Move-typed local must not be flipped to
/// moved. Recurses through `(p)`, `p[i]`, and `&`-style chains so the *root*
/// local of a place is never consumed; a non-place operand falls back to the
/// normal consuming read (a temporary, where consuming is correct).
fn read_place_type(
    expr: &Expr,
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) -> Type {
    match expr {
        Expr::Ident((name, span)) => {
            if let Some(info) = scope.lookup(name) {
                if info.moved {
                    diags.error(format!("use of moved value: `{name}`"), "E304", *span);
                }
                return info.ty.clone();
            }
            // Not a local (static/const/peripheral/fn): no move state to guard.
            check_expr(expr, symbols, scope, fn_name, expected_ret, diags)
        }
        Expr::Group(inner) => read_place_type(inner, symbols, scope, fn_name, expected_ret, diags),
        Expr::Index(base, index) => {
            let base_ty = read_place_type(base, symbols, scope, fn_name, expected_ret, diags);
            check_expr(index, symbols, scope, fn_name, expected_ret, diags);
            index_element_type(base_ty, base, diags)
        }
        _ => check_expr(expr, symbols, scope, fn_name, expected_ret, diags),
    }
}

/// Reassigning a whole local fully overwrites it, so it revives a previously
/// moved Move-typed local. Only plain `name = ...` targets revive; assignments
/// through a field/index/deref project into an existing value and do not.
fn mark_assigned(lval: &LValue, scope: &mut ScopeStack, _val_ty: Type, _diags: &mut DiagnosticBag) {
    if let LValue::Name((name, _)) = lval
        && let Some(info) = scope.lookup_mut(name)
    {
        info.moved = false;
    }
}

/// Returns (min, max) range for an integer type as i128.
fn int_range(ty: &Type) -> (i128, i128) {
    match ty {
        Type::I8 => (-128, 127),
        Type::I16 => (-32768, 32767),
        Type::I32 => (-2_147_483_648, 2_147_483_647),
        Type::I64 => (-9_223_372_036_854_775_808, 9_223_372_036_854_775_807),
        Type::U8 => (0, 255),
        Type::U16 => (0, 65535),
        Type::U32 => (0, 4_294_967_295),
        Type::U64 => (0, i128::MAX),
        Type::B1 => (0, 1),
        Type::B8 => (0, 255),
        _ => (i128::MIN, i128::MAX),
    }
}

/// Check if an unsuffixed integer/float literal fits within the target type's range.
/// This allows `var x: u8 = 0;` without requiring a `u8` suffix on the literal.
fn unsuffixed_literal_fits(expr: &Expr, target_ty: &Type) -> bool {
    match (expr, target_ty) {
        // 0 and 1 int literals coerce to b1
        (Expr::IntLiteral(n, ast::IntSuffix::None, _), Type::B1) => *n == 0 || *n == 1,
        (Expr::IntLiteral(n, ast::IntSuffix::None, _), ty) if types::is_int(ty) => {
            let (min, max) = int_range(ty);
            i128::from(*n) >= min && i128::from(*n) <= max
        }
        (Expr::FloatLiteral(n, ast::FloatSuffix::None, _), ty) if types::is_float(ty) => {
            n.is_finite()
                && match ty {
                    Type::F16 => *n >= -65504.0f64 && *n <= 65504.0f64,
                    Type::F32 => *n >= f64::from(f32::MIN) && *n <= f64::from(f32::MAX),
                    Type::F64 => true,
                    _ => unreachable!(),
                }
        }
        (Expr::Unary(ast::UnaryOp::Neg, inner), ty) => match (inner.as_ref(), ty) {
            (Expr::IntLiteral(n, ast::IntSuffix::None, _), ty) if types::is_int(ty) => {
                let (min, max) = int_range(ty);
                let val = -i128::from(*n);
                val >= min && val <= max
            }
            (Expr::FloatLiteral(n, ast::FloatSuffix::None, _), ty) if types::is_float(ty) => {
                n.is_finite()
                    && match ty {
                        Type::F16 => -*n >= -65504.0f64 && -*n <= 65504.0f64,
                        Type::F32 => -*n >= f64::from(f32::MIN) && -*n <= f64::from(f32::MAX),
                        Type::F64 => true,
                        _ => unreachable!(),
                    }
            }
            _ => false,
        },
        // An array literal of unsuffixed elements adopts the declared element
        // type, e.g. `var b: [u8; 4] = [0, 0, 0, 0]`.
        (Expr::ArrayInit(elems, _), Type::Array(elem, n)) => {
            elems.len() == *n && elems.iter().all(|e| unsuffixed_literal_fits(e, elem))
        }
        _ => false,
    }
}
