use std::fmt;

use std::collections::HashMap;

use crate::ast::{Item, TypeExpr};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F16,
    F32,
    F64,
    B1,
    B8,
    Array(Box<Type>, usize),
    Ptr(Box<Type>),
    ConstPtr(Box<Type>),
    Void,
    // Wrapper types carrying borrow semantics
    Exclusive(Box<Type>),
    Shared(Box<Type>, u8),
    Mmio(Box<Type>),
    Dma(Box<Type>),
    External(Box<Type>),
    // User-defined struct types: name + ordered field list
    Struct(String, Vec<(String, Type)>),
    // User-defined enum types: name + underlying type + (variant_name, discriminant)
    Enum(String, Box<Type>, Vec<(String, i64)>),
    // A named but unresolved type (resolved during name resolution pass)
    Unresolved(String),
    // Function pointer type: fn(params) -> ret
    Fn(Vec<Type>, Box<Type>),
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::I8 => write!(f, "i8"),
            Type::I16 => write!(f, "i16"),
            Type::I32 => write!(f, "i32"),
            Type::I64 => write!(f, "i64"),
            Type::U8 => write!(f, "u8"),
            Type::U16 => write!(f, "u16"),
            Type::U32 => write!(f, "u32"),
            Type::U64 => write!(f, "u64"),
            Type::F16 => write!(f, "f16"),
            Type::F32 => write!(f, "f32"),
            Type::F64 => write!(f, "f64"),
            Type::B1 => write!(f, "b1"),
            Type::B8 => write!(f, "b8"),
            Type::Void => write!(f, "void"),
            Type::Array(t, n) => write!(f, "[{t}; {n}]"),
            Type::Ptr(t) => write!(f, "*{t}"),
            Type::ConstPtr(t) => write!(f, "&{t}"),
            Type::Exclusive(t) => write!(f, "@exclusive({t})"),
            Type::Shared(t, c) => write!(f, "@shared({t}, ceiling={c})"),
            Type::Mmio(t) => write!(f, "@mmio({t})"),
            Type::Dma(t) => write!(f, "@dma({t})"),
            Type::External(t) => write!(f, "@external({t})"),
            Type::Struct(name, _) => write!(f, "struct {name}"),
            Type::Enum(name, _, _) => write!(f, "enum {name}"),
            Type::Unresolved(name) => write!(f, "{name}"),
            Type::Fn(params, ret) => {
                let p: Vec<String> = params.iter().map(ToString::to_string).collect();
                write!(f, "fn({}) -> {ret}", p.join(", "))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Semantics {
    Copy,
    Move,
}

impl Type {
    #[must_use]
    pub fn semantics(&self) -> Semantics {
        match self {
            Type::I8
            | Type::I16
            | Type::I32
            | Type::I64
            | Type::U8
            | Type::U16
            | Type::U32
            | Type::U64
            | Type::F16
            | Type::F32
            | Type::F64
            | Type::B1
            | Type::B8
            | Type::Void
            | Type::Unresolved(_)
            | Type::Fn(..) => Semantics::Copy,
            Type::Array(inner, _) | Type::Ptr(inner) | Type::ConstPtr(inner) => inner.semantics(),
            Type::Struct(_, fields) => {
                if fields.iter().all(|(_, ty)| ty.is_copy()) {
                    Semantics::Copy
                } else {
                    Semantics::Move
                }
            }
            Type::Enum(..) => Semantics::Copy,
            Type::Exclusive(_)
            | Type::Shared(_, _)
            | Type::Mmio(_)
            | Type::Dma(_)
            | Type::External(_) => Semantics::Move,
        }
    }

    /// Unwrap storage wrappers to get the underlying data type
    #[must_use]
    pub fn inner(&self) -> &Type {
        match self {
            Type::Exclusive(inner)
            | Type::Shared(inner, _)
            | Type::Mmio(inner)
            | Type::Dma(inner)
            | Type::External(inner) => inner,
            other => other,
        }
    }

    #[must_use]
    pub fn is_copy(&self) -> bool {
        self.semantics() == Semantics::Copy
    }

    #[must_use]
    pub fn is_move(&self) -> bool {
        self.semantics() == Semantics::Move
    }
}

/// Map from enum name to (underlying type, (variant name, discriminant) list)
pub type EnumDefs = HashMap<String, (Type, Vec<(String, i64)>)>;

/// Resolve a type expression to a concrete Type.
/// Struct names are resolved via the `structs` map (name → fields).
/// Enum names are resolved via the `enums` map (name → (`underlying_type`, variants)).
#[must_use]
pub fn resolve_type_expr<S: ::std::hash::BuildHasher>(
    ty: &TypeExpr,
    structs: &HashMap<String, Vec<(String, Type)>, S>,
    enums: &EnumDefs,
) -> Type {
    match ty {
        TypeExpr::Named((name, _)) => match name.as_str() {
            "i8" => Type::I8,
            "i16" => Type::I16,
            "i32" => Type::I32,
            "i64" => Type::I64,
            "u8" => Type::U8,
            "u16" => Type::U16,
            "u32" => Type::U32,
            "u64" => Type::U64,
            "f16" => Type::F16,
            "f32" => Type::F32,
            "f64" => Type::F64,
            "b1" => Type::B1,
            "b8" => Type::B8,
            "void" => Type::Void,
            _ => {
                if let Some(fields) = structs.get(name.as_str()) {
                    Type::Struct(name.clone(), fields.clone())
                } else if let Some((inner_ty, variants)) = enums.get(name.as_str()) {
                    Type::Enum(name.clone(), Box::new(inner_ty.clone()), variants.clone())
                } else {
                    Type::Unresolved(name.clone())
                }
            }
        },
        TypeExpr::Ptr(inner) => Type::Ptr(Box::new(resolve_type_expr(inner, structs, enums))),
        TypeExpr::ConstPtr(inner) => {
            Type::ConstPtr(Box::new(resolve_type_expr(inner, structs, enums)))
        }
        TypeExpr::Array(inner, size) => {
            let ty = resolve_type_expr(inner, structs, enums);
            let size_val = match size.as_ref() {
                crate::ast::Expr::IntLiteral(n, _, _) => *n as usize,
                _ => 0,
            };
            Type::Array(Box::new(ty), size_val)
        }
        TypeExpr::Fn(params, ret) => {
            let resolved_params: Vec<Type> = params
                .iter()
                .map(|p| resolve_type_expr(p, structs, enums))
                .collect();
            let resolved_ret = ret
                .as_ref()
                .map_or(Type::Void, |r| resolve_type_expr(r, structs, enums));
            Type::Fn(resolved_params, Box::new(resolved_ret))
        }
        TypeExpr::Void(_) => Type::Void,
    }
}

/// Build struct and enum type definitions from alias module items.
///
/// Two-pass approach: first insert structs with unresolved field types,
/// then resolve all fields and enum inner types sequentially.
#[must_use]
pub fn alias_type_defs<S: ::std::hash::BuildHasher>(
    items: &[Item],
    base_structs: &HashMap<String, Vec<(String, Type)>, S>,
    base_enums: &EnumDefs,
) -> (HashMap<String, Vec<(String, Type)>>, EnumDefs) {
    let mut structs: HashMap<String, Vec<(String, Type)>> = base_structs
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let mut enums = base_enums.clone();

    for item in items {
        if let Item::StructDef(s) = item {
            let fields = s
                .fields
                .iter()
                .map(|field| (field.name.0.clone(), Type::Unresolved(field.name.0.clone())))
                .collect();
            structs.insert(s.name.0.clone(), fields);
        }
    }

    for item in items {
        match item {
            Item::StructDef(s) => {
                let fields = s
                    .fields
                    .iter()
                    .map(|field| {
                        (
                            field.name.0.clone(),
                            resolve_type_expr(&field.ty, &structs, &enums),
                        )
                    })
                    .collect();
                structs.insert(s.name.0.clone(), fields);
            }
            Item::EnumDef(e) => {
                let inner_ty = resolve_type_expr(&e.ty, &structs, &enums);
                let mut next_val = 0i64;
                let variants = e
                    .variants
                    .iter()
                    .map(|variant| {
                        let disc = variant.value.map_or(next_val, u64::cast_signed);
                        next_val = disc + 1;
                        (variant.name.0.clone(), disc)
                    })
                    .collect();
                enums.insert(e.name.0.clone(), (inner_ty, variants));
            }
            _ => {}
        }
    }

    (structs, enums)
}

/// Check if two types are compatible (for assignment, arguments, etc.).
/// Strict: exact match only. No implicit coercion -- use `as` for casts.
/// Exception: `*mut T` implicitly coerces to `*T` (mutable → const pointer).
#[must_use]
pub fn types_compatible(expected: &Type, actual: &Type) -> bool {
    if expected == actual {
        return true;
    }
    // Allow storage-wrapped assignment to unwrapped
    // e.g., assigning Exclusive(U32) to U32 variable is fine
    if expected == actual.inner() && actual.is_move() {
        return true;
    }
    if actual == expected.inner() && expected.is_move() {
        return true;
    }
    // *mut T → *T implicit coercion (mutable → const)
    if let (Type::ConstPtr(e_inner), Type::Ptr(a_inner)) = (expected, actual)
        && e_inner == a_inner
    {
        return true;
    }
    // Function pointer types: structural comparison
    if let (Type::Fn(expected_params, expected_ret), Type::Fn(actual_params, actual_ret)) =
        (expected, actual)
    {
        if expected_params.len() != actual_params.len() {
            return false;
        }
        if !expected_params
            .iter()
            .zip(actual_params)
            .all(|(e, a)| types_compatible(e, a))
        {
            return false;
        }
        if !types_compatible(expected_ret, actual_ret) {
            return false;
        }
        return true;
    }
    // Unresolved types are compatible with anything (lenient for now)
    if matches!(expected, Type::Unresolved(_)) || matches!(actual, Type::Unresolved(_)) {
        return true;
    }
    false
}

/// Check if two types belong to the same family for `as` casts.
/// Ints ↔ Ints, Floats ↔ Floats.
#[must_use]
pub fn same_family(a: &Type, b: &Type) -> bool {
    is_int(a) && is_int(b) || is_float(a) && is_float(b)
}

#[must_use]
pub fn is_int(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I8 | Type::I16 | Type::I32 | Type::I64 | Type::U8 | Type::U16 | Type::U32 | Type::U64
    )
}

#[must_use]
pub fn is_float(ty: &Type) -> bool {
    matches!(ty, Type::F16 | Type::F32 | Type::F64)
}

#[must_use]
pub fn is_ptr(ty: &Type) -> bool {
    matches!(ty, Type::Ptr(_) | Type::ConstPtr(_) | Type::Fn(..))
}

/// Size of a type in bytes (for pointer diff arithmetic).
#[must_use]
pub fn element_size(ty: &Type) -> u32 {
    match ty {
        Type::I8 | Type::U8 | Type::B8 => 1,
        Type::I16 | Type::U16 | Type::F16 => 2,
        Type::I32 | Type::U32 | Type::F32 => 4,
        Type::I64 | Type::U64 | Type::F64 => 8,
        Type::B1 | Type::Void => 1,
        Type::Ptr(_) | Type::ConstPtr(_) | Type::Fn(..) => 4,
        Type::Struct(_, fields) => fields.iter().map(|(_, ty)| element_size(ty)).sum(),
        Type::Enum(_, inner_ty, _) => element_size(inner_ty),
        _ => 4,
    }
}
