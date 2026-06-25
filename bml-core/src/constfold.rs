//! Fold constant array lengths into integer literals.
//!
//! Array types may be written with a `const`, `sizeof`, or comptime-function
//! length, e.g. `var buf: [u8; N]` (`const N: u32 = 4`), `var hdr: [u8;
//! sizeof(Foo)]`, or `const N = round_up(40, 16); [u8; N]`. Type resolution
//! (`types::resolve_type_expr`) only understands integer-literal lengths, so this
//! pass rewrites each non-literal `[T; expr]` length (and `[v; expr]` repeat-init
//! count) into an integer literal once its value is known.
//!
//! It runs TWICE. The first pass runs after import resolution but BEFORE type
//! resolution (`symbols = None`), folding literal / `const` / comptime-function
//! lengths. `sizeof` cannot be evaluated yet (no struct/enum layouts), so a
//! `sizeof` length is left for the second pass, which runs AFTER resolution
//! (`symbols = Some`), where `resolve_type_expr` + `element_size` give real sizes.
//! A length that still cannot be folded (a runtime expression, a `comptime`
//! parameter) is left as-is and the checker rejects it (E414).

use std::collections::HashMap;

use crate::ast::{Block, Expr, FnDef, IntSuffix, Item, Program, Stmt, TypeExpr};
use crate::consteval::{self, Env};
use crate::resolver::SymbolTable;
use crate::types;

/// Upper bound on a repeat-init count we expand inline, to bound AST growth. A
/// larger (or non-constant) count is left as an `ArrayRepeat` for the checker to
/// reject (E348) rather than ballooning the program.
const MAX_ARRAY_REPEAT: i128 = 65536;

/// Whether `expr` is safe to duplicate when expanding `[expr; N]` -- i.e. it has
/// no side effects, so N copies behave like a single evaluation. Conservative:
/// only literals, names, enum variants, `sizeof`, and pure arithmetic over those.
/// Anything that could read state or call code (`Call`, `Index`, `FieldAccess`,
/// ...) is rejected, so `[f(); N]` is never silently turned into N calls.
fn is_duplicable(expr: &Expr) -> bool {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::Ident(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => true,
        Expr::Group(inner) | Expr::Unary(_, inner) | Expr::Cast(inner, _) => is_duplicable(inner),
        Expr::Binary(l, _, r) => is_duplicable(l) && is_duplicable(r),
        _ => false,
    }
}

/// Rewrite const-valued array lengths in `program` into integer literals. Pass
/// `symbols = None` pre-resolution (folds literal/`const`/comptime-function
/// lengths) and `Some(table)` post-resolution (also folds `sizeof`).
pub fn fold_array_lengths(program: &mut Program, symbols: Option<&SymbolTable>) {
    // A module function may compute a `const` used as an array length
    // (`const N = f(); [u8; N]`); collect the functions so `const_int_values` can
    // run them at compile time.
    let fns: HashMap<String, &FnDef> = program
        .items
        .iter()
        .filter_map(|it| match it {
            Item::FnDef(f) => Some((f.name.0.clone(), f)),
            _ => None,
        })
        .collect();
    let consts = const_int_values(&program.items, &fns, symbols);
    let array_lens = array_len_values(&program.items, &consts, symbols);
    for item in &mut program.items {
        fold_item(item, &consts, &array_lens, symbols);
    }
}

/// Evaluate every const whose initializer is a constant integer expression.
/// Iterates to a fixpoint so a const may reference an earlier-or-later const.
/// A call-bearing initializer (`const N = f()`) that `consteval` cannot fold is
/// handed to the comptime interpreter (pre-resolution only). With `symbols`, a
/// `sizeof`-valued const (`const N = sizeof(Foo)`) folds through `consteval`.
fn const_int_values(
    items: &[Item],
    fns: &HashMap<String, &FnDef>,
    symbols: Option<&SymbolTable>,
) -> HashMap<String, i128> {
    let mut map = HashMap::new();
    loop {
        let mut changed = false;
        let array_lens = array_len_values(items, &map, symbols);
        for item in items {
            if let Item::ConstDef(c) = item
                && !map.contains_key(&c.name.0)
                && let Some(v) =
                    fold_const_int(&c.value, &map, &array_lens, symbols).or_else(|| {
                        // Skip the comptime interpreter for an array-typed const: it
                        // can only yield a scalar, so for an array-returning builder it
                        // would construct the whole array and then discard it
                        // (`fold_const_calls` builds it for real). Scalar-typed consts
                        // -- the ones usable as a length -- still fold here.
                        (!matches!(c.ty, TypeExpr::Array(..)))
                            .then(|| crate::comptime::eval_scalar(&c.value, fns, &map))
                            .flatten()
                    })
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

fn array_len_values(
    items: &[Item],
    consts: &HashMap<String, i128>,
    symbols: Option<&SymbolTable>,
) -> HashMap<String, usize> {
    let mut map = HashMap::new();
    for item in items {
        match item {
            Item::ConstDef(c) => {
                if let Some(n) = type_array_len(&c.ty, consts, &map, symbols) {
                    map.insert(c.name.0.clone(), n);
                }
            }
            Item::StaticDef(s) => {
                if let Some(n) = type_array_len(&s.ty, consts, &map, symbols) {
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
    symbols: Option<&SymbolTable>,
) -> Option<usize> {
    match ty {
        TypeExpr::Array(_, size) => {
            let n = fold_const_int(size, consts, array_lens, symbols)?;
            usize::try_from(n).ok()
        }
        _ => None,
    }
}

/// Resolves names from the const/array-length maps. `sizeof` is evaluated only
/// when `symbols` is present (post-resolution); pre-resolution it stays unfolded.
struct FoldEnv<'a> {
    consts: &'a HashMap<String, i128>,
    array_lens: &'a HashMap<String, usize>,
    symbols: Option<&'a SymbolTable>,
}

impl Env for FoldEnv<'_> {
    fn const_int(&self, name: &str) -> Option<i128> {
        self.consts.get(name).copied()
    }
    fn array_len(&self, name: &str) -> Option<i128> {
        self.array_lens.get(name).map(|n| *n as i128)
    }
    fn sizeof(&self, ty: &TypeExpr) -> Option<i128> {
        let symbols = self.symbols?;
        let t = types::resolve_type_expr(ty, &symbols.structs, &symbols.enums);
        // A fully-resolved type sizes correctly post-resolution. If any component
        // is still unresolved (a typo, e.g. `sizeof([Nonexistent; 4])`), refuse:
        // `element_size`'s catch-all would otherwise guess a size and silently
        // mis-size the array (top-level-only checks miss `Array(Unresolved, ..)`).
        if types::type_has_unresolved(&t) {
            return None;
        }
        Some(i128::from(types::element_size(&t)))
    }
    fn enum_variant(&self, enum_name: &str, variant: &str) -> Option<i128> {
        self.symbols?.enum_variant_discriminant(enum_name, variant)
    }
}

/// Try to evaluate `expr` as a constant integer using already-known consts,
/// array lengths, and (post-resolution) struct/enum layouts for `sizeof`.
fn fold_const_int(
    expr: &Expr,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
    symbols: Option<&SymbolTable>,
) -> Option<i128> {
    consteval::eval_int(
        expr,
        &FoldEnv {
            consts,
            array_lens,
            symbols,
        },
    )
}

fn fold_type(
    ty: &mut TypeExpr,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
    symbols: Option<&SymbolTable>,
) {
    match ty {
        TypeExpr::Array(inner, size) => {
            fold_type(inner, consts, array_lens, symbols);
            if !matches!(size.as_ref(), Expr::IntLiteral(..))
                && let Some(v) = fold_const_int(size, consts, array_lens, symbols)
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
            fold_type(inner, consts, array_lens, symbols);
        }
        TypeExpr::StridedView(inner, _, stride) => {
            fold_type(inner, consts, array_lens, symbols);
            // Fold a const stride (`view T stride N`) down to a literal, the
            // same way array sizes are folded above.
            if !matches!(stride.as_ref(), Expr::IntLiteral(..))
                && let Some(v) = fold_const_int(stride, consts, array_lens, symbols)
                && let Ok(n) = u64::try_from(v)
            {
                let span = stride.span();
                **stride = Expr::IntLiteral(n, IntSuffix::None, span);
            }
        }
        TypeExpr::Fn(params, ret) => {
            for p in params.iter_mut() {
                fold_type(p, consts, array_lens, symbols);
            }
            if let Some(r) = ret {
                fold_type(r, consts, array_lens, symbols);
            }
        }
        TypeExpr::Named(_) | TypeExpr::Void(_) | TypeExpr::Bits(_) | TypeExpr::Addr(_) => {}
    }
}

fn fold_item(
    item: &mut Item,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
    symbols: Option<&SymbolTable>,
) {
    match item {
        Item::FnDef(f) => {
            for p in &mut f.params {
                fold_type(&mut p.ty, consts, array_lens, symbols);
            }
            if let Some(r) = &mut f.ret {
                fold_type(r, consts, array_lens, symbols);
            }
            fold_block(&mut f.body, consts, array_lens, symbols);
        }
        Item::ExternFnDef(f) => {
            for p in &mut f.params {
                fold_type(&mut p.ty, consts, array_lens, symbols);
            }
            if let Some(r) = &mut f.ret {
                fold_type(r, consts, array_lens, symbols);
            }
        }
        Item::StaticDef(s) => {
            fold_type(&mut s.ty, consts, array_lens, symbols);
            if let Some(init) = &mut s.init {
                fold_expr(init, consts, array_lens, symbols);
            }
        }
        Item::ConstDef(c) => {
            fold_type(&mut c.ty, consts, array_lens, symbols);
            fold_expr(&mut c.value, consts, array_lens, symbols);
        }
        Item::StructDef(s) => {
            for field in &mut s.fields {
                fold_type(&mut field.ty, consts, array_lens, symbols);
            }
        }
        _ => {}
    }
}

fn fold_block(
    block: &mut Block,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
    symbols: Option<&SymbolTable>,
) {
    // A local `const` with a compile-time initializer participates in folding
    // within its scope, so `const N = 4; var buf: [u8; N];` resolves the length
    // just like a module const would. Clone-on-entry gives lexical scoping: a
    // const declared in this block (or a nested one) never leaks outward, and an
    // initializer cannot see its own binding (it is inserted only after folding).
    // Only immutable (`const`) locals qualify; a `var` is not a constant.
    let mut local = consts.clone();
    for stmt in &mut block.stmts {
        fold_stmt(stmt, &local, array_lens, symbols);
        if let Stmt::VarDecl(vd) = &*stmt
            && !vd.mutable
            && let Some(v) = fold_const_int(&vd.init, &local, array_lens, symbols)
        {
            local.insert(vd.name.0.clone(), v);
        }
    }
    if let Some(trailing) = &mut block.trailing {
        fold_expr(trailing, &local, array_lens, symbols);
    }
}

fn fold_stmt(
    stmt: &mut Stmt,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
    symbols: Option<&SymbolTable>,
) {
    match stmt {
        Stmt::VarDecl(vd) => {
            if let Some(ty) = &mut vd.ty_ann {
                fold_type(ty, consts, array_lens, symbols);
            }
            fold_expr(&mut vd.init, consts, array_lens, symbols);
        }
        Stmt::Assign(a) => fold_expr(&mut a.value, consts, array_lens, symbols),
        Stmt::CompoundAssign(a) => fold_expr(&mut a.value, consts, array_lens, symbols),
        Stmt::Expr(e) => fold_expr(e, consts, array_lens, symbols),
        Stmt::If(s) => {
            fold_expr(&mut s.cond, consts, array_lens, symbols);
            fold_block(&mut s.then_block, consts, array_lens, symbols);
            if let Some(alt) = &mut s.else_branch {
                fold_stmt(alt, consts, array_lens, symbols);
            }
        }
        Stmt::Loop(s) => fold_block(&mut s.body, consts, array_lens, symbols),
        Stmt::Claim(c) => fold_block(&mut c.body, consts, array_lens, symbols),
        Stmt::While(s) => {
            fold_expr(&mut s.cond, consts, array_lens, symbols);
            fold_block(&mut s.body, consts, array_lens, symbols);
        }
        Stmt::For(s) => {
            fold_type(&mut s.ty, consts, array_lens, symbols);
            fold_expr(&mut s.start, consts, array_lens, symbols);
            fold_expr(&mut s.end, consts, array_lens, symbols);
            if let Some(step) = &mut s.step {
                fold_expr(step, consts, array_lens, symbols);
            }
            fold_block(&mut s.body, consts, array_lens, symbols);
        }
        Stmt::Return(r) => {
            if let Some(v) = &mut r.value {
                fold_expr(v, consts, array_lens, symbols);
            }
        }
        Stmt::Block(b) => fold_block(b, consts, array_lens, symbols),
        Stmt::Match(m) => {
            fold_expr(&mut m.scrutinee, consts, array_lens, symbols);
            for arm in &mut m.arms {
                fold_block(&mut arm.body, consts, array_lens, symbols);
            }
        }
        Stmt::Assume(a) => fold_expr(&mut a.cond, consts, array_lens, symbols),
        Stmt::Assert(a) => fold_expr(&mut a.cond, consts, array_lens, symbols),
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Asm(_) => {}
    }
}

fn fold_expr(
    expr: &mut Expr,
    consts: &HashMap<String, i128>,
    array_lens: &HashMap<String, usize>,
    symbols: Option<&SymbolTable>,
) {
    match expr {
        Expr::Cast(inner, ty) => {
            fold_expr(inner, consts, array_lens, symbols);
            fold_type(ty, consts, array_lens, symbols);
        }
        Expr::SizeOf(ty, _) => fold_type(ty, consts, array_lens, symbols),
        Expr::Unary(_, inner) | Expr::Group(inner) | Expr::FieldAccess(inner, _) => {
            fold_expr(inner, consts, array_lens, symbols);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            fold_expr(l, consts, array_lens, symbols);
            fold_expr(r, consts, array_lens, symbols);
        }
        Expr::Call(callee, args) => {
            fold_expr(callee, consts, array_lens, symbols);
            for a in args {
                fold_expr(a, consts, array_lens, symbols);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                fold_expr(e, consts, array_lens, symbols);
            }
        }
        Expr::ArrayRepeat(value, count, span) => {
            fold_expr(value, consts, array_lens, symbols);
            fold_expr(count, consts, array_lens, symbols);
            // Desugar `[v; N]` to `[v, v, ..., v]` once N folds to an in-range
            // constant and `v` is safe to duplicate. A non-constant / oversized
            // count or a side-effecting value is left as an `ArrayRepeat` for the
            // checker to reject (E348) -- never silently mis-expanded.
            let replacement = if is_duplicable(value)
                && let Some(n) = fold_const_int(count, consts, array_lens, symbols)
                && (0..=MAX_ARRAY_REPEAT).contains(&n)
                && let Ok(n) = usize::try_from(n)
            {
                Some(Expr::ArrayInit(vec![(**value).clone(); n], *span))
            } else {
                None
            };
            if let Some(r) = replacement {
                *expr = r;
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                fold_expr(e, consts, array_lens, symbols);
            }
        }
        Expr::Block(b) => fold_block(&mut b.block, consts, array_lens, symbols),
        Expr::If(i) => {
            fold_expr(&mut i.cond, consts, array_lens, symbols);
            fold_block(&mut i.then_block, consts, array_lens, symbols);
            fold_expr(&mut i.else_branch, consts, array_lens, symbols);
        }
        Expr::Match(m) => {
            fold_expr(&mut m.scrutinee, consts, array_lens, symbols);
            for arm in &mut m.arms {
                fold_block(&mut arm.body, consts, array_lens, symbols);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            fold_expr(base, consts, array_lens, symbols);
            if let Some(len) = len {
                fold_expr(len, consts, array_lens, symbols);
            }
            // Fold a const stride (`view(arr, stride N)`) to a literal so the
            // checker and IR can read `K` directly.
            if let Some(stride) = stride
                && !matches!(stride.as_ref(), Expr::IntLiteral(..))
                && let Some(v) = fold_const_int(stride, consts, array_lens, symbols)
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
        fold_array_lengths(&mut program, None);
        // N -> 4, M = N * 2 -> 8 (the latter exercises the const-of-const fixpoint)
        assert_eq!(var_array_lens(&program), vec![Some(4), Some(8)]);
    }

    #[test]
    fn folds_local_const_into_array_length() {
        // A local `const` with a compile-time initializer drives a later array
        // length within its scope, including a const-of-const (`M = N * 2`).
        let mut program = parse(concat!(
            "fn f() @context(thread) {\n",
            "    const N: u32 = 4;\n",
            "    var a: [u8; N] = [0u8, 0u8, 0u8, 0u8];\n",
            "    const M: u32 = N * 2;\n",
            "    var b: [u16; M] = [0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 0u16, 0u16];\n",
            "}\n",
        ));
        fold_array_lengths(&mut program, None);
        assert_eq!(var_array_lens(&program), vec![Some(4), Some(8)]);
    }

    #[test]
    fn mutable_local_does_not_fold_array_length() {
        // A `var` is not a constant, so it must not be folded into a length; the
        // type is left unresolved and surfaces as a normal error later.
        let mut program = parse(concat!(
            "fn f() @context(thread) {\n",
            "    var n: u32 = 4;\n",
            "    var a: [u8; n] = [0u8];\n",
            "}\n",
        ));
        fold_array_lengths(&mut program, None);
        assert_eq!(var_array_lens(&program), vec![None]);
    }

    #[test]
    fn leaves_non_constant_length_unfolded() {
        // An unknown identifier is not a known const; the length is left as-is
        // (resolution then reports a normal error rather than miscompiling).
        let mut program = parse("fn f() @context(thread) { var a: [u8; unknown] = [0u8]; }");
        fold_array_lengths(&mut program, None);
        assert_eq!(var_array_lens(&program), vec![None]);
    }

    #[test]
    fn literal_length_is_unchanged() {
        let mut program = parse("fn f() @context(thread) { var a: [u8; 3] = [0u8, 0u8, 0u8]; }");
        fold_array_lengths(&mut program, None);
        assert_eq!(var_array_lens(&program), vec![Some(3)]);
    }

    /// Collect the initializer expression of every `var` in the first fn.
    fn var_inits(program: &Program) -> Vec<Expr> {
        let mut inits = Vec::new();
        for item in &program.items {
            if let Item::FnDef(f) = item {
                for stmt in &f.body.stmts {
                    if let Stmt::VarDecl(vd) = stmt {
                        inits.push(vd.init.clone());
                    }
                }
            }
        }
        inits
    }

    #[test]
    fn repeat_init_desugars_only_for_const_count_and_safe_value() {
        let mut program = parse(concat!(
            "const N: u32 = 3;\n",
            "fn f() @context(thread) {\n",
            "    var a: [u8; 3] = [7u8; N];\n", // const count, literal value -> desugars
            "    var b: [u8; 3] = [0u8; unknown];\n", // unknown count -> left as ArrayRepeat
            "}\n",
        ));
        fold_array_lengths(&mut program, None);
        let inits = var_inits(&program);
        match &inits[0] {
            Expr::ArrayInit(elems, _) => assert_eq!(elems.len(), 3, "N=3 copies expected"),
            other => panic!("expected desugared ArrayInit, got {other:?}"),
        }
        assert!(
            matches!(inits[1], Expr::ArrayRepeat(..)),
            "a non-constant count must be left as ArrayRepeat for the checker to reject"
        );
    }
}
