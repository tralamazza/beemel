//! Compile-time interpreter for `comptime` function calls in `const` initializers
//! (Phase 4 -- scalars only; arrays/tables are a follow-up).
//!
//! [`crate::consteval`] evaluates *expressions* but cannot execute a function
//! *body* (locals, loops, recursion). This interpreter does: it runs an ordinary
//! bml function at compile time to a scalar [`ConstVal`], so a `const` can be
//! computed (`const N = factorial(5);`) instead of hand-written. A function that
//! is not comptime-evaluable -- arrays, `asm`, runtime features, or past the step
//! budget -- yields `None`, leaving the const unresolved exactly as before.
//!
//! Integration is a single pre-codegen pass, [`fold_const_calls`], which rewrites
//! a const initializer that needed the interpreter into a literal, so every
//! downstream pass (checker validation, const-value collection, codegen) sees a
//! plain literal and is unchanged.

use crate::ast::{self, Expr, IntSuffix, Program, Stmt};
use crate::consteval::{self, ConstVal};
use crate::resolver::SymbolTable;
use crate::types::{self, Type};
use std::collections::HashMap;

/// Step budget for one top-level comptime evaluation. A function that loops or
/// recurses past this is rejected (the const stays unresolved) rather than hanging.
const STEP_LIMIT: u64 = 10_000_000;

enum Flow {
    Normal,
    Break,
    Continue,
    Return(Option<ConstVal>),
}

struct Interp<'a> {
    fns: &'a HashMap<String, &'a ast::FnDef>,
    consts: &'a HashMap<String, ConstVal>,
    symbols: &'a SymbolTable,
    steps: u64,
}

/// Evaluate a `const` initializer that may call ordinary functions, to a scalar
/// value. `fns` maps a function name to its definition; `consts` holds the values
/// of already-resolved module consts. `None` if it is not comptime-evaluable.
#[must_use]
#[allow(clippy::implicit_hasher)]
pub fn eval_const(
    expr: &Expr,
    fns: &HashMap<String, &ast::FnDef>,
    consts: &HashMap<String, ConstVal>,
    symbols: &SymbolTable,
) -> Option<ConstVal> {
    let mut interp = Interp {
        fns,
        consts,
        symbols,
        steps: 0,
    };
    interp.eval(expr, &HashMap::new())
}

/// Rewrite every `const` whose initializer required the comptime interpreter (an
/// ordinary function call) into a literal of its computed value, in place. Runs
/// after resolution and before the checker, so downstream passes see literals.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
pub fn fold_const_calls(program: &mut Program, symbols: &SymbolTable) {
    // Phase 1 (immutable borrow): resolve every const's value to a fixpoint, and
    // record which ones used a function call (so only those are rewritten).
    let rewrites: HashMap<String, ConstVal> = {
        let fns: HashMap<String, &ast::FnDef> = program
            .items
            .iter()
            .filter_map(|it| match it {
                ast::Item::FnDef(f) => Some((f.name.0.clone(), f)),
                _ => None,
            })
            .collect();
        let mut vals: HashMap<String, ConstVal> = HashMap::new();
        loop {
            let mut changed = false;
            for item in &program.items {
                if let ast::Item::ConstDef(c) = item
                    && !vals.contains_key(&c.name.0)
                    && let Some(v) = eval_const(&c.value, &fns, &vals, symbols)
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
                    vals.get(&c.name.0).map(|v| (c.name.0.clone(), *v))
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
            c.value = match *v {
                ConstVal::Int(n) => Expr::IntLiteral(n as u64, IntSuffix::None, span),
                ConstVal::Bool(b) => Expr::BoolLiteral(b, span),
            };
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
        _ => false,
    }
}

impl Interp<'_> {
    fn tick(&mut self) -> Option<()> {
        self.steps += 1;
        (self.steps <= STEP_LIMIT).then_some(())
    }

    /// Execute a function body with `args` bound to its parameters, returning the
    /// scalar result (`None` if not comptime-evaluable / returns no value).
    fn call(&mut self, fn_def: &ast::FnDef, args: &[ConstVal]) -> Option<ConstVal> {
        self.tick()?;
        if fn_def.params.len() != args.len() {
            return None;
        }
        let mut locals: HashMap<String, ConstVal> = HashMap::new();
        for (p, v) in fn_def.params.iter().zip(args) {
            locals.insert(p.name.0.clone(), *v);
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
        locals: &mut HashMap<String, ConstVal>,
    ) -> Option<Flow> {
        for stmt in &block.stmts {
            match self.exec_stmt(stmt, locals)? {
                Flow::Normal => {}
                other => return Some(other),
            }
        }
        Some(Flow::Normal)
    }

    fn exec_stmt(&mut self, stmt: &Stmt, locals: &mut HashMap<String, ConstVal>) -> Option<Flow> {
        self.tick()?;
        Some(match stmt {
            Stmt::VarDecl(v) => {
                let val = self.eval(&v.init, locals)?;
                locals.insert(v.name.0.clone(), val);
                Flow::Normal
            }
            Stmt::Assign(a) => {
                let ast::LValue::Name((name, _)) = &a.target else {
                    return None;
                };
                let val = self.eval(&a.value, locals)?;
                locals.insert(name.clone(), val);
                Flow::Normal
            }
            Stmt::CompoundAssign(a) => {
                let ast::LValue::Name((name, _)) = &a.target else {
                    return None;
                };
                let cur = *locals.get(name)?;
                let rhs = self.eval(&a.value, locals)?;
                locals.insert(name.clone(), consteval::binop(a.op, cur, rhs)?);
                Flow::Normal
            }
            Stmt::If(i) => {
                let ConstVal::Bool(c) = self.eval(&i.cond, locals)? else {
                    return None;
                };
                if c {
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
                    let ConstVal::Bool(c) = self.eval(&w.cond, locals)? else {
                        return None;
                    };
                    if !c {
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
                let ConstVal::Int(mut i) = self.eval(&f.start, locals)? else {
                    return None;
                };
                let ConstVal::Int(end) = self.eval(&f.end, locals)? else {
                    return None;
                };
                let step = match &f.step {
                    Some(e) => match self.eval(e, locals)? {
                        ConstVal::Int(s) => s,
                        ConstVal::Bool(_) => return None,
                    },
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
                    locals.insert(f.var.0.clone(), ConstVal::Int(i));
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

    fn eval(&mut self, expr: &Expr, locals: &HashMap<String, ConstVal>) -> Option<ConstVal> {
        self.tick()?;
        Some(match expr {
            Expr::IntLiteral(n, _, _) => ConstVal::Int(i128::from(*n)),
            Expr::BoolLiteral(b, _) => ConstVal::Bool(*b),
            Expr::Group(inner) => self.eval(inner, locals)?,
            Expr::Cast(inner, ty) => consteval::cast(self.eval(inner, locals)?, ty),
            Expr::Ident((name, _)) => {
                if let Some(v) = locals.get(name) {
                    *v
                } else {
                    *self.consts.get(name)?
                }
            }
            Expr::EnumVariant {
                enum_name, variant, ..
            } => ConstVal::Int(
                self.symbols
                    .enum_variant_discriminant(&enum_name.0, &variant.0)?,
            ),
            Expr::SizeOf(ty, _) => {
                let t = types::resolve_type_expr(ty, &self.symbols.structs, &self.symbols.enums);
                if matches!(t, Type::Unresolved(_)) {
                    return None;
                }
                ConstVal::Int(i128::from(types::element_size(&t)))
            }
            Expr::Unary(op, inner) => {
                use ast::UnaryOp as U;
                match (op, self.eval(inner, locals)?) {
                    (U::Neg, ConstVal::Int(x)) => ConstVal::Int(x.checked_neg()?),
                    (U::BitNot, ConstVal::Int(x)) => ConstVal::Int(!x),
                    (U::Not, ConstVal::Bool(b)) => ConstVal::Bool(!b),
                    _ => return None,
                }
            }
            Expr::Binary(l, op, r) => {
                let lv = self.eval(l, locals)?;
                let rv = self.eval(r, locals)?;
                consteval::binop(*op, lv, rv)?
            }
            Expr::Call(callee, args) => {
                if consteval::is_len_call(callee) && args.len() == 1 {
                    return self.eval_len(&args[0]);
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
            // Arrays (Index / ArrayInit) and everything else: not yet evaluable.
            _ => return None,
        })
    }

    /// `len(arr)` for a named array `const`/`static`.
    fn eval_len(&self, arg: &Expr) -> Option<ConstVal> {
        let Expr::Ident((name, _)) = arg else {
            return None;
        };
        let ty = self
            .symbols
            .consts
            .get(name)
            .map(|s| &s.ty)
            .or_else(|| self.symbols.statics.get(name).map(|s| &s.ty))?;
        match ty.inner() {
            Type::Array(_, n) => Some(ConstVal::Int(*n as i128)),
            _ => None,
        }
    }
}
