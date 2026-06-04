//! Shared compile-time constant evaluation.
//!
//! Three passes evaluate the same constant expressions:
//!
//! - `constfold` folds const-valued array lengths into literals *before* types
//!   are resolved, so it has no [`SymbolTable`](crate::resolver::SymbolTable).
//! - the `checker` validates `const` initializers and `comptime_assert`.
//! - the IR emitter lowers const initializers and array-length expressions.
//!
//! They only differ in how a name resolves to a value or array length, so the
//! expression walk lives here once and each caller supplies an [`Env`]. The walk
//! itself depends on nothing but the AST, which is what lets `constfold` reuse it
//! before resolution.

use crate::ast::{BinaryOp as B, Expr, TypeExpr, UnaryOp as U};

/// A compile-time constant value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConstVal {
    Int(i128),
    Bool(bool),
}

/// Name and type resolution for constant evaluation. Implementations decide
/// where values come from (a pre-resolution const map, or the symbol table).
pub trait Env {
    /// Integer value of a named `const`, if known.
    fn const_int(&self, name: &str) -> Option<i128>;

    /// Boolean value of a named `const`, if known. Defaults to `None` for passes
    /// that only track integers (e.g. `constfold`, where a `bool` can never be a
    /// valid array length).
    fn const_bool(&self, _name: &str) -> Option<bool> {
        None
    }

    /// Element count of a named array `const`/`static`, or `None` if the name is
    /// not a (visible) array. Used by `len(...)` on a named global.
    fn array_len(&self, name: &str) -> Option<i128>;

    /// Size in bytes of a `sizeof` operand. Returns `None` when types are not yet
    /// resolved (the `constfold` pass), which simply leaves `sizeof` unevaluated.
    fn sizeof(&self, ty: &TypeExpr) -> Option<i128>;
}

/// `true` if `expr` names the `len` builtin (its only spelling is a bare ident).
#[must_use]
pub fn is_len_call(expr: &Expr) -> bool {
    matches!(expr, Expr::Ident((name, _)) if name == "len")
}

/// Evaluate `expr` to a [`ConstVal`], or `None` if it is not a compile-time
/// constant. All integer arithmetic is checked, so an overflowing or
/// divide-by-zero expression yields `None` rather than panicking the compiler.
pub fn eval(expr: &Expr, env: &dyn Env) -> Option<ConstVal> {
    Some(match expr {
        Expr::IntLiteral(n, _, _) => ConstVal::Int(i128::from(*n)),
        Expr::BoolLiteral(b, _) => ConstVal::Bool(*b),
        Expr::Group(inner) => eval(inner, env)?,
        Expr::Cast(inner, ty) => apply_cast(eval(inner, env)?, ty),
        Expr::Ident((name, _)) => match env.const_int(name) {
            Some(v) => ConstVal::Int(v),
            None => ConstVal::Bool(env.const_bool(name)?),
        },
        Expr::Call(callee, args) if is_len_call(callee) && args.len() == 1 => {
            ConstVal::Int(eval_len(&args[0], env)?)
        }
        Expr::SizeOf(ty, _) => ConstVal::Int(env.sizeof(ty)?),
        Expr::Unary(U::Neg, inner) => match eval(inner, env)? {
            ConstVal::Int(v) => ConstVal::Int(v.checked_neg()?),
            ConstVal::Bool(_) => return None,
        },
        Expr::Unary(U::BitNot, inner) => match eval(inner, env)? {
            ConstVal::Int(v) => ConstVal::Int(!v),
            ConstVal::Bool(_) => return None,
        },
        Expr::Unary(U::Not, inner) => match eval(inner, env)? {
            ConstVal::Bool(b) => ConstVal::Bool(!b),
            ConstVal::Int(_) => return None,
        },
        Expr::Binary(lhs, op, rhs) => {
            let lv = eval(lhs, env)?;
            let rv = eval(rhs, env)?;
            match (lv, rv) {
                (ConstVal::Int(x), ConstVal::Int(y)) => eval_int_binop(*op, x, y)?,
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

/// Evaluate `expr` as a constant integer (a `bool` result yields `None`).
pub fn eval_int(expr: &Expr, env: &dyn Env) -> Option<i128> {
    match eval(expr, env)? {
        ConstVal::Int(v) => Some(v),
        ConstVal::Bool(_) => None,
    }
}

/// Evaluate `expr` as a constant boolean (an integer result yields `None`).
pub fn eval_bool(expr: &Expr, env: &dyn Env) -> Option<bool> {
    match eval(expr, env)? {
        ConstVal::Bool(v) => Some(v),
        ConstVal::Int(_) => None,
    }
}

fn eval_int_binop(op: B, x: i128, y: i128) -> Option<ConstVal> {
    Some(match op {
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
    })
}

/// Apply a cast to a constant value. A cast to a primitive integer type
/// truncates/sign-extends to that type's width (so `300 as u8` is `44`, matching
/// run-time cast semantics); a `bool` source becomes `0`/`1` first. Any other
/// target (float, pointer, `b1`, named aggregate) passes the value through
/// unchanged, preserving the representation for the surrounding context.
fn apply_cast(val: ConstVal, ty: &TypeExpr) -> ConstVal {
    match int_type_bits(ty) {
        Some((bits, signed)) => {
            let n = match val {
                ConstVal::Int(n) => n,
                ConstVal::Bool(b) => i128::from(b),
            };
            ConstVal::Int(truncate_int(n, bits, signed))
        }
        None => val,
    }
}

/// Width in bits and signedness of a primitive integer `TypeExpr`, or `None` for
/// non-integer / non-primitive targets. Matches on the spelling so it works
/// before type resolution (in `constfold`).
fn int_type_bits(ty: &TypeExpr) -> Option<(u32, bool)> {
    let TypeExpr::Named((name, _)) = ty else {
        return None;
    };
    Some(match name.as_str() {
        "i8" => (8, true),
        "i16" => (16, true),
        "i32" => (32, true),
        "i64" => (64, true),
        "u8" | "b8" => (8, false),
        "u16" => (16, false),
        "u32" => (32, false),
        "u64" => (64, false),
        _ => return None,
    })
}

/// Truncate `n` to `bits` (<= 64), interpreting the result as signed or unsigned.
fn truncate_int(n: i128, bits: u32, signed: bool) -> i128 {
    let mask = (1i128 << bits) - 1;
    let low = n & mask;
    if signed && (low >> (bits - 1)) & 1 == 1 {
        low - (1i128 << bits)
    } else {
        low
    }
}

/// Evaluate `len(arg)` for its constant forms: a named array, a (possibly
/// strided) `view` of one, or a `bits` view of one. The strided length floors
/// (`n / k`), matching the descriptor length built at run time.
fn eval_len(expr: &Expr, env: &dyn Env) -> Option<i128> {
    match expr {
        Expr::Ident((name, _)) => env.array_len(name),
        Expr::ViewNew { base, stride, .. } => {
            let n = eval_len(base, env)?;
            if let Some(stride) = stride {
                let k = eval_int(stride, env)?;
                if k <= 0 {
                    return None;
                }
                Some(n / k)
            } else {
                Some(n)
            }
        }
        Expr::BitNew { base, .. } => Some(eval_len(base, env)? * 8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{FileId, Span};

    fn named(ty: &str) -> TypeExpr {
        TypeExpr::Named((ty.to_string(), Span::empty(FileId::new(), 0)))
    }

    #[test]
    fn cast_to_integer_truncates_to_target_width() {
        // Unsigned narrowing wraps; `300 as u8` is 44, not 300.
        assert_eq!(
            apply_cast(ConstVal::Int(300), &named("u8")),
            ConstVal::Int(44)
        );
        assert_eq!(
            apply_cast(ConstVal::Int(-1), &named("u8")),
            ConstVal::Int(255)
        );
        // Signed narrowing reinterprets the low bits: `200 as i8` is -56.
        assert_eq!(
            apply_cast(ConstVal::Int(200), &named("i8")),
            ConstVal::Int(-56)
        );
        // Widening preserves the value.
        assert_eq!(
            apply_cast(ConstVal::Int(300), &named("u16")),
            ConstVal::Int(300)
        );
        // `b8` is an 8-bit unsigned target.
        assert_eq!(
            apply_cast(ConstVal::Int(256), &named("b8")),
            ConstVal::Int(0)
        );
        // A bool source becomes 0/1 before truncation.
        assert_eq!(
            apply_cast(ConstVal::Bool(true), &named("u32")),
            ConstVal::Int(1)
        );
    }

    #[test]
    fn cast_to_non_integer_passes_value_through() {
        // Float / bool / named-aggregate targets keep the value unchanged.
        assert_eq!(
            apply_cast(ConstVal::Int(300), &named("f32")),
            ConstVal::Int(300)
        );
        assert_eq!(
            apply_cast(ConstVal::Bool(true), &named("b1")),
            ConstVal::Bool(true)
        );
    }
}
