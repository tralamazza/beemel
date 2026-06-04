//! Fold constant array lengths before type resolution.
//!
//! Array types may be written with a `const` length, e.g.
//! `var buf: [u8; N]` where `const N: u32 = 4`. Type resolution
//! (`types::resolve_type_expr`) only understands integer-literal lengths, so
//! this pass rewrites each non-literal `[T; expr]` length into an integer
//! literal once the const's value is known. Running it on the flattened program
//! (after import resolution) means every later pass sees the literal length.
//!
//! A length that cannot be folded (references something non-constant) is left
//! as-is; resolution then treats it as length 0, which surfaces as a normal
//! type error rather than a miscompile.

use std::collections::HashMap;

use crate::ast::{BinaryOp, Block, Expr, IntSuffix, Item, Program, Stmt, TypeExpr, UnaryOp};

/// Rewrite const-valued array lengths in `program` into integer literals.
pub fn fold_array_lengths(program: &mut Program) {
    let consts = const_int_values(&program.items);
    let array_lens = array_len_values(&program.items, &consts);
    for item in &mut program.items {
        fold_item(item, &consts, &array_lens);
    }
}

/// Evaluate every const whose initializer is a constant integer expression.
/// Iterates to a fixpoint so a const may reference an earlier-or-later const.
fn const_int_values(items: &[Item]) -> HashMap<String, i128> {
    let mut map = HashMap::new();
    loop {
        let mut changed = false;
        let array_lens = array_len_values(items, &map);
        for item in items {
            if let Item::ConstDef(c) = item
                && !map.contains_key(&c.name.0)
                && let Some(v) = fold_const_int(&c.value, &map, &array_lens)
            {
                map.insert(c.name.0.clone(), v);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    map
}

fn array_len_values(items: &[Item], consts: &HashMap<String, i128>) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for item in items {
        match item {
            Item::ConstDef(c) => {
                if let Some(n) = type_array_len(&c.ty, consts, &map) {
                    map.insert(c.name.0.clone(), n);
                }
            }
            Item::StaticDef(s) => {
                if let Some(n) = type_array_len(&s.ty, consts, &map) {
                    map.insert(s.name.0.clone(), n);
                }
            }
            _ => {}
        }
    }
    map
}

fn type_array_len(
    ty: &TypeExpr,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
) -> Option<usize> {
    match ty {
        TypeExpr::Array(_, size) => {
            let n = fold_const_int(size, consts, array_lens)?;
            usize::try_from(n).ok()
        }
        _ => None,
    }
}

/// Try to evaluate `expr` as a constant integer using already-known consts.
fn fold_const_int(
    expr: &Expr,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
) -> Option<i128> {
    match expr {
        Expr::IntLiteral(n, _, _) => Some(i128::from(*n)),
        Expr::Ident((name, _)) => consts.get(name).copied(),
        Expr::Group(inner) => fold_const_int(inner, consts, array_lens),
        Expr::Unary(UnaryOp::Neg, inner) => fold_const_int(inner, consts, array_lens).map(|v| -v),
        Expr::Call(callee, args)
            if matches!(callee.as_ref(), Expr::Ident((name, _)) if name == "len")
                && args.len() == 1 =>
        {
            fold_len(&args[0], consts, array_lens)
        }
        Expr::Binary(lhs, op, rhs) => {
            let a = fold_const_int(lhs, consts, array_lens)?;
            let b = fold_const_int(rhs, consts, array_lens)?;
            Some(match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div if b != 0 => a / b,
                BinaryOp::Mod if b != 0 => a % b,
                BinaryOp::BitAnd => a & b,
                BinaryOp::BitOr => a | b,
                BinaryOp::BitXor => a ^ b,
                BinaryOp::Shl => a << b,
                BinaryOp::Shr => a >> b,
                _ => return None,
            })
        }
        _ => None,
    }
}

fn fold_len(
    expr: &Expr,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
) -> Option<i128> {
    match expr {
        Expr::Ident((name, _)) => array_lens.get(name).map(|n| *n as i128),
        Expr::ViewNew { base, stride, .. } => {
            let n = fold_len(base, consts, array_lens)?;
            if let Some(stride) = stride {
                let k = fold_const_int(stride, consts, array_lens)?;
                if k <= 0 {
                    return None;
                }
                Some(n / k)
            } else {
                Some(n)
            }
        }
        Expr::BitNew { base, .. } => Some(fold_len(base, consts, array_lens)? * 8),
        _ => None,
    }
}

fn fold_type(
    ty: &mut TypeExpr,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
) {
    match ty {
        TypeExpr::Array(inner, size) => {
            fold_type(inner, consts, array_lens);
            if !matches!(size.as_ref(), Expr::IntLiteral(..))
                && let Some(v) = fold_const_int(size, consts, array_lens)
                && let Ok(n) = u64::try_from(v)
            {
                let span = size.span();
                **size = Expr::IntLiteral(n, IntSuffix::None, span);
            }
        }
        TypeExpr::Ptr(inner)
        | TypeExpr::ConstPtr(inner)
        | TypeExpr::View(inner, _)
        | TypeExpr::Ring(inner, _) => {
            fold_type(inner, consts, array_lens);
        }
        TypeExpr::StridedView(inner, _, stride) => {
            fold_type(inner, consts, array_lens);
            // Fold a const stride (`view T stride N`) down to a literal, the
            // same way array sizes are folded above.
            if !matches!(stride.as_ref(), Expr::IntLiteral(..))
                && let Some(v) = fold_const_int(stride, consts, array_lens)
                && let Ok(n) = u64::try_from(v)
            {
                let span = stride.span();
                **stride = Expr::IntLiteral(n, IntSuffix::None, span);
            }
        }
        TypeExpr::Fn(params, ret) => {
            for p in params.iter_mut() {
                fold_type(p, consts, array_lens);
            }
            if let Some(r) = ret {
                fold_type(r, consts, array_lens);
            }
        }
        TypeExpr::Named(_) | TypeExpr::Void(_) | TypeExpr::Bits(_) => {}
    }
}

fn fold_item(item: &mut Item, consts: &HashMap<String, i128>, array_lens: &HashMap<String, usize>) {
    match item {
        Item::FnDef(f) => {
            for p in &mut f.params {
                fold_type(&mut p.ty, consts, array_lens);
            }
            if let Some(r) = &mut f.ret {
                fold_type(r, consts, array_lens);
            }
            fold_block(&mut f.body, consts, array_lens);
        }
        Item::ExternFnDef(f) => {
            for p in &mut f.params {
                fold_type(&mut p.ty, consts, array_lens);
            }
            if let Some(r) = &mut f.ret {
                fold_type(r, consts, array_lens);
            }
        }
        Item::StaticDef(s) => {
            fold_type(&mut s.ty, consts, array_lens);
            if let Some(init) = &mut s.init {
                fold_expr(init, consts, array_lens);
            }
        }
        Item::ConstDef(c) => {
            fold_type(&mut c.ty, consts, array_lens);
            fold_expr(&mut c.value, consts, array_lens);
        }
        Item::StructDef(s) => {
            for field in &mut s.fields {
                fold_type(&mut field.ty, consts, array_lens);
            }
        }
        _ => {}
    }
}

fn fold_block(
    block: &mut Block,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
) {
    for stmt in &mut block.stmts {
        fold_stmt(stmt, consts, array_lens);
    }
    if let Some(trailing) = &mut block.trailing {
        fold_expr(trailing, consts, array_lens);
    }
}

fn fold_stmt(stmt: &mut Stmt, consts: &HashMap<String, i128>, array_lens: &HashMap<String, usize>) {
    match stmt {
        Stmt::VarDecl(vd) => {
            if let Some(ty) = &mut vd.ty_ann {
                fold_type(ty, consts, array_lens);
            }
            fold_expr(&mut vd.init, consts, array_lens);
        }
        Stmt::Assign(a) => fold_expr(&mut a.value, consts, array_lens),
        Stmt::CompoundAssign(a) => fold_expr(&mut a.value, consts, array_lens),
        Stmt::Expr(e) => fold_expr(e, consts, array_lens),
        Stmt::If(s) => {
            fold_expr(&mut s.cond, consts, array_lens);
            fold_block(&mut s.then_block, consts, array_lens);
            if let Some(alt) = &mut s.else_branch {
                fold_stmt(alt, consts, array_lens);
            }
        }
        Stmt::Loop(s) => fold_block(&mut s.body, consts, array_lens),
        Stmt::While(s) => {
            fold_expr(&mut s.cond, consts, array_lens);
            fold_block(&mut s.body, consts, array_lens);
        }
        Stmt::For(s) => {
            fold_type(&mut s.ty, consts, array_lens);
            fold_expr(&mut s.start, consts, array_lens);
            fold_expr(&mut s.end, consts, array_lens);
            if let Some(step) = &mut s.step {
                fold_expr(step, consts, array_lens);
            }
            fold_block(&mut s.body, consts, array_lens);
        }
        Stmt::Return(r) => {
            if let Some(v) = &mut r.value {
                fold_expr(v, consts, array_lens);
            }
        }
        Stmt::Block(b) => fold_block(b, consts, array_lens),
        Stmt::Match(m) => {
            fold_expr(&mut m.scrutinee, consts, array_lens);
            for arm in &mut m.arms {
                fold_block(&mut arm.body, consts, array_lens);
            }
        }
        Stmt::Assume(a) => fold_expr(&mut a.cond, consts, array_lens),
        Stmt::Assert(a) => fold_expr(&mut a.cond, consts, array_lens),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm(_) => {}
    }
}

fn fold_expr(expr: &mut Expr, consts: &HashMap<String, i128>, array_lens: &HashMap<String, usize>) {
    match expr {
        Expr::Cast(inner, ty) => {
            fold_expr(inner, consts, array_lens);
            fold_type(ty, consts, array_lens);
        }
        Expr::SizeOf(ty, _) => fold_type(ty, consts, array_lens),
        Expr::Unary(_, inner) | Expr::Group(inner) | Expr::FieldAccess(inner, _) => {
            fold_expr(inner, consts, array_lens);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            fold_expr(l, consts, array_lens);
            fold_expr(r, consts, array_lens);
        }
        Expr::Call(callee, args) => {
            fold_expr(callee, consts, array_lens);
            for a in args {
                fold_expr(a, consts, array_lens);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                fold_expr(e, consts, array_lens);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                fold_expr(e, consts, array_lens);
            }
        }
        Expr::Block(b) => fold_block(&mut b.block, consts, array_lens),
        Expr::If(i) => {
            fold_expr(&mut i.cond, consts, array_lens);
            fold_block(&mut i.then_block, consts, array_lens);
            fold_expr(&mut i.else_branch, consts, array_lens);
        }
        Expr::Match(m) => {
            fold_expr(&mut m.scrutinee, consts, array_lens);
            for arm in &mut m.arms {
                fold_block(&mut arm.body, consts, array_lens);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            fold_expr(base, consts, array_lens);
            if let Some(len) = len {
                fold_expr(len, consts, array_lens);
            }
            // Fold a const stride (`view(arr, stride N)`) to a literal so the
            // checker and IR can read `K` directly.
            if let Some(stride) = stride
                && !matches!(stride.as_ref(), Expr::IntLiteral(..))
                && let Some(v) = fold_const_int(stride, consts, array_lens)
                && let Ok(n) = u64::try_from(v)
            {
                let span = stride.span();
                **stride = Expr::IntLiteral(n, IntSuffix::None, span);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::errors::DiagnosticBag;
    use crate::parser::Parser;
    use crate::source::FileId;

    fn parse(src: &str) -> Program {
        let mut diags = DiagnosticBag::new();
        let program = Parser::new(src, FileId::new(), &mut diags).parse_program();
        assert!(!diags.has_errors(), "unexpected parse errors");
        program
    }

    /// Collect the literal array lengths of every typed `var` in the first fn.
    fn var_array_lens(program: &Program) -> Vec<Option<u64>> {
        let mut lens = Vec::new();
        for item in &program.items {
            if let Item::FnDef(f) = item {
                for stmt in &f.body.stmts {
                    if let Stmt::VarDecl(vd) = stmt
                        && let Some(TypeExpr::Array(_, size)) = &vd.ty_ann
                    {
                        lens.push(match size.as_ref() {
                            Expr::IntLiteral(n, _, _) => Some(*n),
                            _ => None,
                        });
                    }
                }
            }
        }
        lens
    }

    #[test]
    fn folds_const_identifier_and_const_of_const() {
        let mut program = parse(concat!(
            "const N: u32 = 4;\n",
            "const M: u32 = N * 2;\n",
            "fn f() @context(thread) {\n",
            "    var a: [u8; N] = [0u8, 0u8, 0u8, 0u8];\n",
            "    var b: [u16; M] = [0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 0u16];\n",
            "}\n",
        ));
        fold_array_lengths(&mut program);
        // N -> 4, M = N * 2 -> 8 (the latter exercises the const-of-const fixpoint)
        assert_eq!(var_array_lens(&program), vec![Some(4), Some(8)]);
    }

    #[test]
    fn leaves_non_constant_length_unfolded() {
        // An unknown identifier is not a known const; the length is left as-is
        // (resolution then reports a normal error rather than miscompiling).
        let mut program = parse("fn f() @context(thread) { var a: [u8; unknown] = [0u8]; }");
        fold_array_lengths(&mut program);
        assert_eq!(var_array_lens(&program), vec![None]);
    }

    #[test]
    fn literal_length_is_unchanged() {
        let mut program = parse("fn f() @context(thread) { var a: [u8; 3] = [0u8, 0u8, 0u8]; }");
        fold_array_lengths(&mut program);
        assert_eq!(var_array_lens(&program), vec![Some(3)]);
    }
}
