//! Compile-time interpreter for `comptime` function calls in `const` initializers.
//!
//! [`crate::consteval`] evaluates *expressions* but cannot execute a function
//! *body* (locals, loops, recursion). This interpreter does: it runs an ordinary
//! bml function at compile time, so a `const` can be computed -- a scalar
//! (`const N = factorial(5);`) or a whole array/table built in a loop
//! (`const CRC = build_crc();`) -- instead of hand-written. A function that is not
//! comptime-evaluable (`asm`, runtime features, an unsupported construct, or past
//! the step/recursion budget) yields `None`, leaving the const unresolved exactly
//! as before.
//!
//! Integration is a single pre-codegen pass, [`fold_const_calls`], which rewrites
//! a const initializer that needed the interpreter into a literal (an `IntLiteral`
//! / `BoolLiteral`, or an `ArrayInit` of those), so every downstream pass sees a
//! plain literal and is unchanged.

use crate::ast::{self, Expr, IntSuffix, LValue, Program, Stmt};
use crate::consteval::{self, ConstVal};
use crate::resolver::SymbolTable;
use crate::types::{self, Type};
use std::collections::HashMap;

/// Step budget for one top-level comptime evaluation. A function that loops or
/// recurses past this is rejected (the const stays unresolved) rather than hanging.
const STEP_LIMIT: u64 = 10_000_000;

/// Max comptime call-stack depth. Bounds NATIVE (Rust) recursion so a deeply
/// recursive comptime function fails cleanly (the const stays unresolved -> E343)
/// instead of overflowing the compiler's own stack. `STEP_LIMIT` bounds total
/// work but not depth: a linear-but-deep recursion stays well under it while
/// still blowing the native stack. Generous vs. real comptime use (factorial /
/// fib recurse only as deep as their argument).
const RECURSION_LIMIT: u32 = 256;

/// A comptime value: a scalar ([`ConstVal`]) or an array of values. Arrays let a
/// comptime function build and return a table.
#[derive(Clone)]
enum Val {
    Scalar(ConstVal),
    Array(Vec<Val>),
}

impl Val {
    fn int(&self) -> Option<i128> {
        match self {
            Val::Scalar(ConstVal::Int(n)) => Some(*n),
            _ => None,
        }
    }
    fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Scalar(ConstVal::Bool(b)) => Some(*b),
            _ => None,
        }
    }
    fn scalar(&self) -> Option<ConstVal> {
        match self {
            Val::Scalar(c) => Some(*c),
            Val::Array(_) => None,
        }
    }
}

enum Flow {
    Normal,
    Break,
    Continue,
    Return(Option<Val>),
}

struct Interp<'a> {
    fns: &'a HashMap<String, &'a ast::FnDef>,
    consts: &'a HashMap<String, Val>,
    symbols: &'a SymbolTable,
    steps: u64,
    depth: u32,
    /// Whether `sizeof` is trustworthy. False pre-resolution (`eval_scalar`),
    /// where the empty symbol table would mis-size composite types.
    sizeof_ok: bool,
}

/// Evaluate a `const` initializer that may call ordinary functions, to a comptime
/// value. `fns` maps a function name to its definition; `consts` holds the values
/// of already-resolved module consts. `None` if it is not comptime-evaluable.
fn eval_to_val(
    expr: &Expr,
    fns: &HashMap<String, &ast::FnDef>,
    consts: &HashMap<String, Val>,
    symbols: &SymbolTable,
) -> Option<Val> {
    let mut interp = Interp {
        fns,
        consts,
        symbols,
        steps: 0,
        depth: 0,
        sizeof_ok: true,
    };
    interp.eval(expr, &HashMap::new())
}

/// Evaluate `expr` to a scalar integer at compile time, PRE-RESOLUTION. The
/// interpreter runs with no symbol table, so a function that needs `sizeof`,
/// enum discriminants, or `len` of a named global yields `None` -- only
/// literal/const/arithmetic comptime functions fold. `consts` supplies the values
/// of already-known module consts (as `i128`). `constfold` uses this so a comptime
/// function can compute an array length / repeat count (`const N = f(); [u8; N]`).
/// Returns `None` for a call-free `expr` (consteval already handles those).
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn eval_scalar(
    expr: &Expr,
    fns: &HashMap<String, &ast::FnDef>,
    consts: &HashMap<String, i128>,
) -> Option<i128> {
    if !expr_has_fn_call(expr) {
        return None;
    }
    let symbols = SymbolTable::default();
    let const_vals: HashMap<String, Val> = consts
        .iter()
        .map(|(k, v)| (k.clone(), Val::Scalar(ConstVal::Int(*v))))
        .collect();
    let mut interp = Interp {
        fns,
        consts: &const_vals,
        symbols: &symbols,
        steps: 0,
        depth: 0,
        sizeof_ok: false,
    };
    interp.eval(expr, &HashMap::new())?.int()
}

/// Rewrite every `const` whose initializer required the comptime interpreter (an
/// ordinary function call) into a literal of its computed value, in place. Runs
/// after resolution and before the checker, so downstream passes see literals.
pub fn fold_const_calls(program: &mut Program, symbols: &SymbolTable) {
    // Phase 1 (immutable borrow): resolve every const's value to a fixpoint, and
    // record which ones used a function call (so only those are rewritten).
    let rewrites: HashMap<String, Val> = {
        let fns: HashMap<String, &ast::FnDef> = program
            .items
            .iter()
            .filter_map(|it| match it {
                ast::Item::FnDef(f) => Some((f.name.0.clone(), f)),
                _ => None,
            })
            .collect();
        let mut vals: HashMap<String, Val> = HashMap::new();
        loop {
            let mut changed = false;
            for item in &program.items {
                if let ast::Item::ConstDef(c) = item
                    && !vals.contains_key(&c.name.0)
                    && let Some(v) = eval_to_val(&c.value, &fns, &vals, symbols)
                {
                    vals.insert(c.name.0.clone(), v);
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        program
            .items
            .iter()
            .filter_map(|it| match it {
                ast::Item::ConstDef(c) if expr_has_fn_call(&c.value) => {
                    vals.remove(&c.name.0).map(|v| (c.name.0.clone(), v))
                }
                _ => None,
            })
            .collect()
    };

    // Phase 2 (mutable borrow): replace those initializers with their literal.
    for item in &mut program.items {
        if let ast::Item::ConstDef(c) = item
            && let Some(v) = rewrites.get(&c.name.0)
        {
            let span = c.value.span();
            c.value = val_to_expr(v, span);
        }
    }
}

/// Materialize a computed comptime value as a literal AST node. A negative
/// integer is emitted as `-(magnitude)` -- the shape a user writes -- not the u64
/// two's-complement bit pattern, which would default to u32 and fail to coerce to
/// a signed const (a false E300). An array becomes an `ArrayInit` of its elements.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
fn val_to_expr(val: &Val, span: crate::source::Span) -> Expr {
    match val {
        Val::Scalar(ConstVal::Int(n)) if *n < 0 => Expr::Unary(
            ast::UnaryOp::Neg,
            Box::new(Expr::IntLiteral(
                n.unsigned_abs() as u64,
                IntSuffix::None,
                span,
            )),
        ),
        Val::Scalar(ConstVal::Int(n)) => Expr::IntLiteral(*n as u64, IntSuffix::None, span),
        Val::Scalar(ConstVal::Bool(b)) => Expr::BoolLiteral(*b, span),
        Val::Array(elems) => {
            Expr::ArrayInit(elems.iter().map(|e| val_to_expr(e, span)).collect(), span)
        }
    }
}

/// Whether `expr` contains a call to an ordinary function (not the `len` builtin).
fn expr_has_fn_call(expr: &Expr) -> bool {
    match expr {
        Expr::Call(callee, args) => {
            !consteval::is_len_call(callee) || args.iter().any(expr_has_fn_call)
        }
        Expr::Group(inner) | Expr::Cast(inner, _) | Expr::Unary(_, inner) => {
            expr_has_fn_call(inner)
        }
        Expr::Binary(l, _, r) => expr_has_fn_call(l) || expr_has_fn_call(r),
        Expr::ArrayInit(elems, _) => elems.iter().any(expr_has_fn_call),
        Expr::Index(base, idx) => expr_has_fn_call(base) || expr_has_fn_call(idx),
        _ => false,
    }
}

impl Interp<'_> {
    fn tick(&mut self) -> Option<()> {
        self.steps += 1;
        (self.steps <= STEP_LIMIT).then_some(())
    }

    /// Execute a function body with `args` bound to its parameters, returning the
    /// result (`None` if not comptime-evaluable / returns no value). Bounds native
    /// recursion depth so a deeply recursive comptime function fails cleanly
    /// instead of overflowing the compiler's stack.
    fn call(&mut self, fn_def: &ast::FnDef, args: &[Val]) -> Option<Val> {
        self.tick()?;
        self.depth += 1;
        let result = if self.depth > RECURSION_LIMIT {
            None
        } else {
            self.call_body(fn_def, args)
        };
        self.depth -= 1;
        result
    }

    fn call_body(&mut self, fn_def: &ast::FnDef, args: &[Val]) -> Option<Val> {
        if fn_def.params.len() != args.len() {
            return None;
        }
        let mut locals: HashMap<String, Val> = HashMap::new();
        for (p, v) in fn_def.params.iter().zip(args) {
            locals.insert(p.name.0.clone(), v.clone());
        }
        match self.exec_block(&fn_def.body, &mut locals)? {
            Flow::Return(v) => v,
            // Fell through: the body's trailing expression is the implicit return.
            Flow::Normal => match &fn_def.body.trailing {
                Some(e) => Some(self.eval(e, &locals)?),
                None => None,
            },
            Flow::Break | Flow::Continue => None,
        }
    }

    fn exec_block(
        &mut self,
        block: &ast::Block,
        locals: &mut HashMap<String, Val>,
    ) -> Option<Flow> {
        for stmt in &block.stmts {
            match self.exec_stmt(stmt, locals)? {
                Flow::Normal => {}
                other => return Some(other),
            }
        }
        Some(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, locals: &mut HashMap<String, Val>) -> Option<Flow> {
        self.tick()?;
        Some(match stmt {
            Stmt::VarDecl(v) => {
                let val = self.eval(&v.init, locals)?;
                locals.insert(v.name.0.clone(), val);
                Flow::Normal
            }
            Stmt::Assign(a) => {
                let val = self.eval(&a.value, locals)?;
                self.assign(&a.target, val, locals)?;
                Flow::Normal
            }
            Stmt::CompoundAssign(a) => {
                let cur = self.read_lvalue(&a.target, locals)?.scalar()?;
                let rhs = self.eval(&a.value, locals)?.scalar()?;
                let new = consteval::binop(a.op, cur, rhs)?;
                self.assign(&a.target, Val::Scalar(new), locals)?;
                Flow::Normal
            }
            Stmt::If(i) => {
                if self.eval(&i.cond, locals)?.as_bool()? {
                    self.exec_block(&i.then_block, locals)?
                } else if let Some(e) = &i.else_branch {
                    self.exec_stmt(e, locals)?
                } else {
                    Flow::Normal
                }
            }
            Stmt::While(w) => {
                loop {
                    self.tick()?;
                    if !self.eval(&w.cond, locals)?.as_bool()? {
                        break;
                    }
                    match self.exec_block(&w.body, locals)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Some(ret),
                    }
                }
                Flow::Normal
            }
            Stmt::Loop(l) => loop {
                self.tick()?;
                match self.exec_block(&l.body, locals)? {
                    Flow::Normal | Flow::Continue => {}
                    Flow::Break => break Flow::Normal,
                    ret @ Flow::Return(_) => return Some(ret),
                }
            },
            Stmt::For(f) => {
                let mut i = self.eval(&f.start, locals)?.int()?;
                let end = self.eval(&f.end, locals)?.int()?;
                let step = match &f.step {
                    Some(e) => self.eval(e, locals)?.int()?,
                    None => 1,
                };
                loop {
                    self.tick()?;
                    let cont = match f.direction {
                        ast::ForDirection::Upto => i < end,
                        ast::ForDirection::Downto => i > end,
                    };
                    if !cont {
                        break;
                    }
                    locals.insert(f.var.0.clone(), Val::Scalar(ConstVal::Int(i)));
                    match self.exec_block(&f.body, locals)? {
                        Flow::Normal | Flow::Continue => {}
                        Flow::Break => break,
                        ret @ Flow::Return(_) => return Some(ret),
                    }
                    i = match f.direction {
                        ast::ForDirection::Upto => i.checked_add(step)?,
                        ast::ForDirection::Downto => i.checked_sub(step)?,
                    };
                }
                Flow::Normal
            }
            Stmt::Return(r) => Flow::Return(match &r.value {
                Some(e) => Some(self.eval(e, locals)?),
                None => None,
            }),
            Stmt::Break(_) => Flow::Break,
            Stmt::Continue(_) => Flow::Continue,
            Stmt::Block(b) => self.exec_block(b, locals)?,
            // A bare expression statement has no comptime side effect we model.
            Stmt::Expr(_) => Flow::Normal,
            // match / asm / assume / assert / claim: not comptime-evaluable here.
            _ => return None,
        })
    }

    /// Store `val` at `target` (a local name or an array element). `Field`/`Deref`
    /// targets are not comptime-evaluable.
    fn assign(
        &mut self,
        target: &LValue,
        val: Val,
        locals: &mut HashMap<String, Val>,
    ) -> Option<()> {
        match target {
            LValue::Name((name, _)) => {
                locals.insert(name.clone(), val);
                Some(())
            }
            LValue::Index(..) => {
                // Evaluate every index with the locals read-only first, then walk
                // the array path mutably and store (avoids a borrow conflict).
                let (name, indices) = self.flatten_index_path(target, locals)?;
                let mut slot = locals.get_mut(&name)?;
                for i in indices {
                    match slot {
                        Val::Array(v) => slot = v.get_mut(i)?,
                        Val::Scalar(_) => return None,
                    }
                }
                *slot = val;
                Some(())
            }
            LValue::Field(..) | LValue::Deref(_) => None,
        }
    }

    /// Flatten `arr[i][j]...` into the base local name and the outer-to-inner
    /// index list, evaluating each index expression against `locals`.
    fn flatten_index_path(
        &mut self,
        lv: &LValue,
        locals: &HashMap<String, Val>,
    ) -> Option<(String, Vec<usize>)> {
        let mut indices = Vec::new();
        let mut cur = lv;
        loop {
            match cur {
                LValue::Name((n, _)) => {
                    indices.reverse();
                    return Some((n.clone(), indices));
                }
                LValue::Index(base, idx) => {
                    let i = usize::try_from(self.eval(idx, locals)?.int()?).ok()?;
                    indices.push(i);
                    cur = base;
                }
                LValue::Field(..) | LValue::Deref(_) => return None,
            }
        }
    }

    /// Read the current value at `lv` (for a compound assignment).
    fn read_lvalue(&mut self, lv: &LValue, locals: &HashMap<String, Val>) -> Option<Val> {
        match lv {
            LValue::Name((n, _)) => locals.get(n).cloned(),
            LValue::Index(base, idx) => {
                let i = usize::try_from(self.eval(idx, locals)?.int()?).ok()?;
                match self.read_lvalue(base, locals)? {
                    Val::Array(v) => v.into_iter().nth(i),
                    Val::Scalar(_) => None,
                }
            }
            LValue::Field(..) | LValue::Deref(_) => None,
        }
    }

    fn eval(&mut self, expr: &Expr, locals: &HashMap<String, Val>) -> Option<Val> {
        self.tick()?;
        Some(match expr {
            Expr::IntLiteral(n, _, _) => Val::Scalar(ConstVal::Int(i128::from(*n))),
            Expr::BoolLiteral(b, _) => Val::Scalar(ConstVal::Bool(*b)),
            Expr::Group(inner) => self.eval(inner, locals)?,
            Expr::Cast(inner, ty) => {
                Val::Scalar(consteval::cast(self.eval(inner, locals)?.scalar()?, ty))
            }
            Expr::Ident((name, _)) => {
                if let Some(v) = locals.get(name) {
                    v.clone()
                } else {
                    self.consts.get(name)?.clone()
                }
            }
            Expr::EnumVariant {
                enum_name, variant, ..
            } => Val::Scalar(ConstVal::Int(
                self.symbols
                    .enum_variant_discriminant(&enum_name.0, &variant.0)?,
            )),
            Expr::SizeOf(ty, _) => {
                // Pre-resolution (eval_scalar) the symbol table is empty, so a
                // composite like `[Foo; 4]` resolves to `Array(Unresolved, ..)`,
                // which `element_size`'s catch-all mis-sizes (4 bytes per
                // unresolved component) -- a silent array-sizing miscompile. Only
                // trust `sizeof` once types are resolved.
                if !self.sizeof_ok {
                    return None;
                }
                let t = types::resolve_type_expr(ty, &self.symbols.structs, &self.symbols.enums);
                // Any unresolved component (e.g. `sizeof([Nonexistent; 4])`) would
                // be mis-sized by `element_size`'s catch-all -- refuse it.
                if types::type_has_unresolved(&t) {
                    return None;
                }
                Val::Scalar(ConstVal::Int(i128::from(types::element_size(&t))))
            }
            Expr::Unary(op, inner) => {
                use ast::UnaryOp as U;
                let v = self.eval(inner, locals)?.scalar()?;
                Val::Scalar(match (op, v) {
                    (U::Neg, ConstVal::Int(x)) => ConstVal::Int(x.checked_neg()?),
                    (U::BitNot, ConstVal::Int(x)) => ConstVal::Int(!x),
                    (U::Not, ConstVal::Bool(b)) => ConstVal::Bool(!b),
                    _ => return None,
                })
            }
            Expr::Binary(l, op, r) => {
                let lv = self.eval(l, locals)?.scalar()?;
                let rv = self.eval(r, locals)?.scalar()?;
                Val::Scalar(consteval::binop(*op, lv, rv)?)
            }
            Expr::Call(callee, args) => {
                if consteval::is_len_call(callee) && args.len() == 1 {
                    return self.eval_len(&args[0], locals);
                }
                let Expr::Ident((fname, _)) = callee.as_ref() else {
                    return None;
                };
                let fn_def = *self.fns.get(fname)?;
                let mut arg_vals = Vec::with_capacity(args.len());
                for a in args {
                    arg_vals.push(self.eval(a, locals)?);
                }
                self.call(fn_def, &arg_vals)?
            }
            Expr::ArrayInit(elems, _) => Val::Array(
                elems
                    .iter()
                    .map(|e| self.eval(e, locals))
                    .collect::<Option<Vec<_>>>()?,
            ),
            Expr::Index(base, idx) => {
                let arr = self.eval(base, locals)?;
                let i = usize::try_from(self.eval(idx, locals)?.int()?).ok()?;
                match arr {
                    Val::Array(v) => v.into_iter().nth(i)?,
                    Val::Scalar(_) => return None,
                }
            }
            // ArrayRepeat (desugared by constfold) and everything else: not evaluable.
            _ => return None,
        })
    }

    /// `len(arg)` for a named global array/static (via the symbol table) or any
    /// expression that evaluates to an array value.
    fn eval_len(&mut self, arg: &Expr, locals: &HashMap<String, Val>) -> Option<Val> {
        if let Expr::Ident((name, _)) = arg
            && !locals.contains_key(name)
            && !self.consts.contains_key(name)
        {
            let ty = self
                .symbols
                .consts
                .get(name)
                .map(|s| &s.ty)
                .or_else(|| self.symbols.statics.get(name).map(|s| &s.ty))?;
            return match ty.inner() {
                Type::Array(_, n) => Some(Val::Scalar(ConstVal::Int(i128::try_from(*n).ok()?))),
                _ => None,
            };
        }
        match self.eval(arg, locals)? {
            Val::Array(v) => Some(Val::Scalar(ConstVal::Int(i128::try_from(v.len()).ok()?))),
            Val::Scalar(_) => None,
        }
    }
}
