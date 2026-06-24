use std::collections::{HashMap, HashSet};

use crate::ast::{self, Expr, LValue, Program, Stmt, StructRepr};
use crate::consteval::{self, ConstVal};
use crate::errors::DiagnosticBag;
use crate::resolver::SymbolTable;
use crate::source::Span;
use crate::types::{self, Type};

pub struct Checker;

/// The resolved type of every local `var`/`val`, keyed by its name span. The
/// checker fills this as a by-product of type checking so tools (the LSP's
/// hover and inlay hints) can show the *authoritative* type -- including forms
/// no heuristic recovers, like `var d = a + b` or `var x = arr[i]` -- instead
/// of re-inferring. Locals whose initializer failed to type-check (`Type::Error`)
/// are omitted, so a present entry is always a real type.
pub type LocalTypes = HashMap<Span, Type>;

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
    /// Resolved type of each local declared while checking this function, keyed
    /// by name span. Lives on the stack (not per-scope) so it survives `pop`,
    /// `snapshot`/`restore`, and the loop-body fixpoint -- a re-checked `var`
    /// just overwrites its entry with the same type. Drained by `check_fn`.
    local_types: LocalTypes,
}

impl ScopeStack {
    fn new() -> Self {
        ScopeStack {
            scopes: vec![HashMap::new()],
            local_types: LocalTypes::new(),
        }
    }

    /// Record a local's resolved type for tooling. `Type::Error` is dropped so
    /// the map only ever holds real types.
    fn record_local(&mut self, span: Span, ty: &Type) {
        if !matches!(ty, Type::Error(_)) {
            self.local_types.insert(span, ty.clone());
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
    /// Type-check the program, emitting diagnostics. Returns the resolved type
    /// of every function-body local (keyed by name span) as a by-product, for
    /// tooling; callers that only want diagnostics can ignore it.
    pub fn check(
        program: &Program,
        symbols: &SymbolTable,
        diags: &mut DiagnosticBag,
    ) -> LocalTypes {
        let mut local_types = LocalTypes::new();
        validate_type_annotations(program, symbols, diags);
        validate_struct_layouts(program, symbols, diags);
        validate_extern_abi(program, symbols, diags);
        check_global_initializers(program, symbols, diags);
        for item in &program.items {
            // `len` is intercepted as a builtin before function resolution, so a
            // user function of that name would be silently unreachable. Reject it.
            let fn_name = match item {
                ast::Item::FnDef(f) => Some(&f.name),
                ast::Item::ExternFnDef(f) => Some(&f.name),
                _ => None,
            };
            if let Some(name) = fn_name
                && name.0 == "len"
            {
                diags.error(
                    "`len` is a reserved builtin and cannot be defined as a function",
                    "E345",
                    name.1,
                );
            }
            if let ast::Item::FnDef(fn_def) = item {
                check_shadowing(fn_def, symbols, diags);
                local_types.extend(check_fn(fn_def, symbols, diags));
                check_agent_ptr_escape(fn_def, symbols, diags);
            }
        }
        check_comptime_asserts(program, symbols, diags);
        local_types
    }
}

// ---------------------------------------------------------------------------
// Shadowing check (E347)
//
// A `var`/`const`/for-loop variable may not reuse a name already visible in the
// current or any enclosing lexical scope, including the function's parameters.
// Sibling and sequential scopes are independent, so two disjoint blocks -- or two
// sequential `for i ...` loops -- may reuse a name; only hiding a still-visible
// binding is rejected.
//
// This is a standalone lexical pass, deliberately separate from the type
// checker's `ScopeStack`: that scope inserts a for-variable into the *enclosing*
// block (so it persists past the loop) and re-checks loop bodies for the
// move-analysis fixpoint, both of which would make it report false duplicates.
// ---------------------------------------------------------------------------

/// Names declared in the lexical scopes currently open, innermost last.
type ShadowScopes = Vec<HashSet<String>>;

fn check_shadowing(fn_def: &ast::FnDef, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    // Outermost scope: module-level value names. A parameter, local, or for-loop
    // variable may not hide a module `var`, `const`, `fn`, or `peripheral` -- all
    // of which are referenced as bare identifiers in value/access position. Type
    // names (structs/enums) live in a separate namespace and are not seeded.
    let mut module_scope = HashSet::new();
    module_scope.extend(symbols.functions.keys().cloned());
    module_scope.extend(symbols.statics.keys().cloned());
    module_scope.extend(symbols.consts.keys().cloned());
    module_scope.extend(symbols.peripherals.keys().cloned());

    // Parameters nest just below the module scope. Routing them through
    // `shadow_declare` also rejects duplicate parameter names and a parameter
    // that hides a global.
    let mut scopes: ShadowScopes = vec![module_scope, HashSet::new()];
    for p in &fn_def.params {
        shadow_declare(&p.name, &mut scopes, diags);
    }
    shadow_block(&fn_def.body, &mut scopes, diags);
}

/// Declare `name` in the innermost scope, reporting E347 if it is already
/// visible anywhere up the scope stack.
fn shadow_declare(name: &ast::Ident, scopes: &mut ShadowScopes, diags: &mut DiagnosticBag) {
    if scopes.iter().any(|s| s.contains(&name.0)) {
        diags.error(
            format!(
                "`{}` shadows a binding already in scope; shadowing is not allowed -- \
                 rename one of them",
                name.0
            ),
            "E347",
            name.1,
        );
    }
    scopes.last_mut().unwrap().insert(name.0.clone());
}

fn shadow_block(block: &ast::Block, scopes: &mut ShadowScopes, diags: &mut DiagnosticBag) {
    scopes.push(HashSet::new());
    for stmt in &block.stmts {
        shadow_stmt(stmt, scopes, diags);
    }
    if let Some(trailing) = &block.trailing {
        shadow_expr(trailing, scopes, diags);
    }
    scopes.pop();
}

fn shadow_stmt(stmt: &Stmt, scopes: &mut ShadowScopes, diags: &mut DiagnosticBag) {
    match stmt {
        // The initializer is evaluated before the new binding exists, so walk it
        // first, then declare the name.
        Stmt::VarDecl(vd) => {
            shadow_expr(&vd.init, scopes, diags);
            shadow_declare(&vd.name, scopes, diags);
        }
        Stmt::Assign(a) => {
            shadow_lvalue(&a.target, scopes, diags);
            shadow_expr(&a.value, scopes, diags);
        }
        Stmt::CompoundAssign(a) => {
            shadow_lvalue(&a.target, scopes, diags);
            shadow_expr(&a.value, scopes, diags);
        }
        Stmt::Expr(e) => shadow_expr(e, scopes, diags),
        Stmt::If(i) => {
            shadow_expr(&i.cond, scopes, diags);
            shadow_block(&i.then_block, scopes, diags);
            if let Some(else_branch) = &i.else_branch {
                shadow_stmt(else_branch, scopes, diags);
            }
        }
        Stmt::Loop(l) => shadow_block(&l.body, scopes, diags),
        Stmt::While(w) => {
            shadow_expr(&w.cond, scopes, diags);
            shadow_block(&w.body, scopes, diags);
        }
        // The loop variable is scoped to the loop: a dedicated scope holds it and
        // the body nests inside, so after the loop it is gone and a following
        // `for` may reuse the name.
        Stmt::For(f) => {
            shadow_expr(&f.start, scopes, diags);
            shadow_expr(&f.end, scopes, diags);
            if let Some(step) = &f.step {
                shadow_expr(step, scopes, diags);
            }
            scopes.push(HashSet::new());
            shadow_declare(&f.var, scopes, diags);
            shadow_block(&f.body, scopes, diags);
            scopes.pop();
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                shadow_expr(v, scopes, diags);
            }
        }
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Block(b) => shadow_block(b, scopes, diags),
        Stmt::Match(m) => {
            shadow_expr(&m.scrutinee, scopes, diags);
            for arm in &m.arms {
                shadow_block(&arm.body, scopes, diags);
            }
        }
        Stmt::Asm(a) => {
            for (_, e) in &a.outputs {
                shadow_expr(e, scopes, diags);
            }
            for (_, e) in &a.inputs {
                shadow_expr(e, scopes, diags);
            }
        }
        Stmt::Assume(a) => shadow_expr(&a.cond, scopes, diags),
        Stmt::Assert(a) => shadow_expr(&a.cond, scopes, diags),
        Stmt::Claim(c) => shadow_block(&c.body, scopes, diags),
    }
}

fn shadow_lvalue(lv: &LValue, scopes: &mut ShadowScopes, diags: &mut DiagnosticBag) {
    match lv {
        LValue::Name(_) => {}
        LValue::Field(inner, _) => shadow_lvalue(inner, scopes, diags),
        LValue::Index(inner, idx) => {
            shadow_lvalue(inner, scopes, diags);
            shadow_expr(idx, scopes, diags);
        }
        LValue::Deref(e) => shadow_expr(e, scopes, diags),
    }
}

/// Walk an expression for embedded blocks (`{ ... }`, `if`/`match` expressions)
/// that can themselves contain declarations. Exhaustive over `Expr` -- a new
/// variant must be added here so a declaration nested inside it is not missed.
fn shadow_expr(expr: &Expr, scopes: &mut ShadowScopes, diags: &mut DiagnosticBag) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(..)
        | Expr::Ident(..)
        | Expr::SizeOf(..)
        | Expr::EnumVariant { .. } => {}
        Expr::Unary(_, e) => shadow_expr(e, scopes, diags),
        Expr::Binary(l, _, r) => {
            shadow_expr(l, scopes, diags);
            shadow_expr(r, scopes, diags);
        }
        Expr::Call(callee, args) => {
            shadow_expr(callee, scopes, diags);
            for a in args {
                shadow_expr(a, scopes, diags);
            }
        }
        Expr::FieldAccess(e, _) => shadow_expr(e, scopes, diags),
        Expr::Index(base, idx) => {
            shadow_expr(base, scopes, diags);
            shadow_expr(idx, scopes, diags);
        }
        Expr::Group(e) => shadow_expr(e, scopes, diags),
        Expr::Cast(e, _) => shadow_expr(e, scopes, diags),
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            shadow_expr(base, scopes, diags);
            if let Some(l) = len {
                shadow_expr(l, scopes, diags);
            }
            if let Some(s) = stride {
                shadow_expr(s, scopes, diags);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            shadow_expr(base, scopes, diags);
            if let Some(c) = capacity {
                shadow_expr(c, scopes, diags);
            }
            shadow_expr(head, scopes, diags);
            shadow_expr(len, scopes, diags);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            shadow_expr(base, scopes, diags);
            if let Some(o) = bit_offset {
                shadow_expr(o, scopes, diags);
            }
            if let Some(l) = len_bits {
                shadow_expr(l, scopes, diags);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                shadow_expr(e, scopes, diags);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                shadow_expr(e, scopes, diags);
            }
        }
        Expr::Match(m) => {
            shadow_expr(&m.scrutinee, scopes, diags);
            for arm in &m.arms {
                shadow_block(&arm.body, scopes, diags);
            }
        }
        Expr::Block(b) => shadow_block(&b.block, scopes, diags),
        Expr::If(i) => {
            shadow_expr(&i.cond, scopes, diags);
            shadow_block(&i.then_block, scopes, diags);
            shadow_expr(&i.else_branch, scopes, diags);
        }
    }
}

/// Validate literals embedded in declared type annotations that
/// [`types::resolve_type_expr`] would otherwise coerce into a valid-looking
/// `Type`. Currently this is the strided-view stride: `resolve_type_expr`
/// collapses an out-of-range or zero stride to the `0` sentinel, which loses
/// the literal and its span, so we check the `TypeExpr` here instead.
///
/// Covers the *signature and top-level* annotation positions (struct fields,
/// statics, consts, fn/extern params and returns). Body-local `val`/`var`
/// annotations are validated inline where the checker already visits every
/// `VarDecl`, so nesting inside block/if/match expressions is handled too.
fn validate_type_annotations(program: &Program, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    for item in &program.items {
        match item {
            ast::Item::FnDef(f) => validate_sig_anns(&f.params, f.ret.as_ref(), symbols, diags),
            ast::Item::ExternFnDef(f) => {
                validate_sig_anns(&f.params, f.ret.as_ref(), symbols, diags);
            }
            ast::Item::StaticDef(s) => validate_type_ann_resolved(&s.ty, symbols, diags),
            ast::Item::ConstDef(c) => validate_type_ann_resolved(&c.ty, symbols, diags),
            ast::Item::StructDef(s) => {
                for field in &s.fields {
                    validate_type_ann_resolved(&field.ty, symbols, diags);
                }
            }
            ast::Item::PeripheralDef(p) => {
                for reg in &p.regs {
                    for field in &reg.fields {
                        validate_type_ann_resolved(&field.ty, symbols, diags);
                    }
                }
            }
            // Enum underlying types are named integers and cannot carry a
            // strided-view stride. peripheral_type/instance are elaborated away
            // before the checker (only the fuzzer can reach them) -- ignore.
            ast::Item::EnumDef(_)
            | ast::Item::PeripheralType(_)
            | ast::Item::PeripheralInstance(_)
            | ast::Item::Import(_)
            | ast::Item::Owns(_)
            | ast::Item::ComptimeAssert(_) => {}
        }
    }
}

fn validate_struct_layouts(program: &Program, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    for item in &program.items {
        let ast::Item::StructDef(s) = item else {
            continue;
        };
        let Some(info) = symbols.structs.get(&s.name.0) else {
            continue;
        };

        for (field, (_, ty)) in s.fields.iter().zip(info.fields.iter()) {
            if field.name.0 == "_"
                && !matches!(ty, Type::Array(inner, _) if inner.as_ref() == &Type::U8)
            {
                diags.error(
                    "padding field `_` must have type `[u8; N]`",
                    "E351",
                    field.name.1,
                );
            }
            // `@be`/`@le` describe byte order of a multi-byte integer in memory.
            // A single byte has no byte order, and aggregates/floats/views have
            // no well-defined wire-integer swap, so restrict the attribute to
            // multi-byte integer scalars and reject it loudly elsewhere.
            // `@extent(addr_field [, xN])`: the named sibling must exist and
            // be an `addr in R` field (the delivery this length arms), and
            // the annotated field itself must be an integer count.
            if let Some(ext) = &field.extent {
                let sibling = s.fields.iter().find(|f| f.name.0 == ext.addr_field.0);
                match sibling {
                    None => {
                        diags.error(
                            format!(
                                "`@extent({0})` on `{1}.{2}`: no field `{0}` in `{1}`",
                                ext.addr_field.0, s.name.0, field.name.0
                            ),
                            "E617",
                            ext.addr_field.1,
                        );
                    }
                    Some(f) if !matches!(f.ty, ast::TypeExpr::Addr(_)) => {
                        diags.error(
                            format!(
                                "`@extent({0})` on `{1}.{2}`: `{0}` is not an `addr in <region>`                                  field -- the extent must arm a delivered buffer",
                                ext.addr_field.0, s.name.0, field.name.0
                            ),
                            "E617",
                            ext.addr_field.1,
                        );
                    }
                    Some(_) => {}
                }
                // 32-bit only: the verify-mode arming assert multiplies the
                // stored value as an i32 (descriptor length words in
                // practice); widening narrower fields is not wired up.
                if !matches!(ty, Type::U32 | Type::I32) {
                    diags.error(
                        format!(
                            "`@extent` on `{}.{}` requires a `u32`/`i32` field, got `{ty}`",
                            s.name.0, field.name.0
                        ),
                        "E617",
                        field.name.1,
                    );
                }
                // `mask N`: must be nonzero (a zero mask makes the byte count
                // always 0, silently disabling the obligation) and fit the
                // 32-bit field constrained just above.
                if let Some(mask) = ext.mask {
                    if mask == 0 {
                        diags.error(
                            format!(
                                "`@extent(.., mask 0)` on `{}.{}`: mask must be nonzero",
                                s.name.0, field.name.0
                            ),
                            "E617",
                            field.name.1,
                        );
                    } else if mask > u64::from(u32::MAX) {
                        diags.error(
                            format!(
                                "`@extent(.., mask 0x{mask:X})` on `{}.{}`: mask does not fit the 32-bit field",
                                s.name.0, field.name.0
                            ),
                            "E617",
                            field.name.1,
                        );
                    }
                }
            }
            if field.endian != ast::FieldEndian::Native && !is_multibyte_int(ty) {
                let attr = match field.endian {
                    ast::FieldEndian::Big => "be",
                    ast::FieldEndian::Little => "le",
                    ast::FieldEndian::Native => unreachable!(),
                };
                diags.error(
                    format!(
                        "endianness attribute `@{attr}` on field `{}` requires a multi-byte \
                         integer type (u16/u32/u64/i16/i32/i64), found `{ty}`",
                        field.name.0
                    ),
                    "E359",
                    field.name.1,
                );
            }
        }

        if s.repr != StructRepr::Explicit {
            continue;
        }

        let struct_ty = Type::Struct(s.name.0.clone(), s.repr, info.fields.clone());
        if !validate_resolved_type_size(&struct_ty, s.name.1, diags) {
            continue;
        }

        let mut offset = 0;
        let mut max_align = 1;
        for (field, (_, ty)) in s.fields.iter().zip(info.fields.iter()) {
            let align = types::align_of(ty);
            max_align = max_align.max(align);
            if field.name.0 != "_" {
                let rem = offset % align;
                if rem != 0 {
                    let pad = align - rem;
                    diags.error(
                        format!(
                            "field `{}` is at offset {offset} but requires alignment {align}; add `_: [u8; {pad}],` before it or reorder fields",
                            field.name.0
                        ),
                        "E352",
                        field.name.1,
                    );
                }
            }
            offset += types::element_size(ty);
        }

        let rem = offset % max_align;
        if rem != 0 {
            let pad = max_align - rem;
            diags.error(
                format!(
                    "struct `{}` has size {offset} but alignment {max_align}; add tail padding `_: [u8; {pad}],`",
                    s.name.0
                ),
                "E353",
                s.name.1,
            );
        }
    }
}

/// A multi-byte integer scalar: the only field types for which `@be`/`@le`
/// (byte-order) make sense. Single bytes, floats, aggregates, and views are
/// rejected.
fn is_multibyte_int(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I16 | Type::I32 | Type::I64 | Type::U16 | Type::U32 | Type::U64
    )
}

fn validate_extern_abi(program: &Program, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    for item in &program.items {
        let ast::Item::ExternFnDef(f) = item else {
            continue;
        };
        for param in &f.params {
            let ty = types::resolve_type_expr(&param.ty, &symbols.structs, &symbols.enums);
            if let Err(reason) = extern_abi_value_error(&ty, false, &symbols.structs) {
                diags.error(
                    format!(
                        "extern fn `{}` parameter `{}` is not C ABI-safe: {reason}",
                        f.name.0, param.name.0
                    ),
                    "E356",
                    param.name.1,
                );
            }
        }
        if let Some(ret) = &f.ret {
            let ty = types::resolve_type_expr(ret, &symbols.structs, &symbols.enums);
            if let Err(reason) = extern_abi_value_error(&ty, true, &symbols.structs) {
                diags.error(
                    format!(
                        "extern fn `{}` return type is not C ABI-safe: {reason}",
                        f.name.0
                    ),
                    "E356",
                    f.name.1,
                );
            }
        }
    }
}

fn extern_abi_value_error(
    ty: &Type,
    allow_void: bool,
    structs: &HashMap<String, types::StructInfo>,
) -> Result<(), String> {
    let mut visiting = HashSet::new();
    extern_abi_value_error_inner(ty, allow_void, structs, &mut visiting)
}

fn extern_abi_value_error_inner(
    ty: &Type,
    allow_void: bool,
    structs: &HashMap<String, types::StructInfo>,
    visiting: &mut HashSet<String>,
) -> Result<(), String> {
    match ty.inner() {
        Type::I8
        | Type::I16
        | Type::I32
        | Type::I64
        | Type::U8
        | Type::U16
        | Type::U32
        | Type::U64
        | Type::F32
        | Type::F64
        | Type::B8
        // A byte-address slot is a u32 at the ABI; harmless to pass.
        | Type::Addr(_)
        | Type::Enum(..) => Ok(()),
        Type::Void if allow_void => Ok(()),
        Type::Void => Err("use no parameter instead of `void`".to_string()),
        Type::B1 => Err("`b1` lowers to a 1-bit value; use `b8` for C booleans".to_string()),
        Type::F16 => Err("`f16` has no portable C ABI in bml; use `f32` or `f64`".to_string()),
        Type::Ptr(inner) | Type::ConstPtr(inner) => {
            extern_abi_pointee_error(inner, structs, visiting)
        }
        Type::Fn(params, ret) => {
            for (idx, param) in params.iter().enumerate() {
                extern_abi_value_error_inner(param, false, structs, visiting)
                    .map_err(|reason| format!("function pointer parameter {idx}: {reason}"))?;
            }
            extern_abi_value_error_inner(ret, true, structs, visiting)
                .map_err(|reason| format!("function pointer return type: {reason}"))
        }
        Type::Struct(..) => Err(
            "structs are not supported by value across extern boundaries; pass `*@repr(C) Struct` instead"
                .to_string(),
        ),
        Type::Array(..) => Err("arrays are not C ABI values; pass a pointer to the first element".to_string()),
        Type::LinearView(..) | Type::StridedView(..) | Type::RingView(..) | Type::BitView(..) => {
            Err("bml view/ring/bits descriptors are not C ABI types".to_string())
        }
        Type::Exclusive(_)
        | Type::Shared(_, _)
        | Type::Mmio(_)
        | Type::AgentShared(_) => Err("storage-qualified types cannot cross extern boundaries".to_string()),
        Type::PeripheralHandle(_) => {
            Err("a `peripheral_type` handle cannot cross extern boundaries".to_string())
        }
        Type::Unresolved(_) | Type::Null | Type::Error(_) => Ok(()),
    }
}

fn extern_abi_pointee_error(
    ty: &Type,
    structs: &HashMap<String, types::StructInfo>,
    visiting: &mut HashSet<String>,
) -> Result<(), String> {
    match ty.inner() {
        Type::Void
        | Type::I8
        | Type::I16
        | Type::I32
        | Type::I64
        | Type::U8
        | Type::U16
        | Type::U32
        | Type::U64
        | Type::F32
        | Type::F64
        | Type::B8
        | Type::Addr(_)
        | Type::Enum(..) => Ok(()),
        Type::B1 => Err("pointers to `b1` are not C ABI-safe; use `*b8`".to_string()),
        Type::F16 => Err("pointers to `f16` are not C ABI-safe; use `*f32` or `*f64`".to_string()),
        // A pointer reaches the struct's fields, so each field must itself be C
        // ABI-safe; otherwise a `view`/`b1`/non-`@repr(C)` member would smuggle a
        // BML-only layout across the boundary unchecked.
        s @ Type::Struct(_, StructRepr::C, _) => extern_abi_field_error(s, structs, visiting),
        Type::Struct(name, StructRepr::Explicit, _) => Err(format!(
            "pointer to struct `{name}` requires `@repr(C)` at extern boundaries; use `*void` for opaque handles"
        )),
        Type::Struct(name, StructRepr::Packed, _) => Err(format!(
            "pointer to packed struct `{name}` is not C ABI-safe; use `*void` for opaque handles"
        )),
        Type::Array(inner, _) => extern_abi_pointee_error(inner, structs, visiting),
        Type::Ptr(inner) | Type::ConstPtr(inner) => {
            extern_abi_pointee_error(inner, structs, visiting)
        }
        Type::Fn(params, ret) => {
            for (idx, param) in params.iter().enumerate() {
                extern_abi_value_error_inner(param, false, structs, visiting)
                    .map_err(|reason| format!("function pointer parameter {idx}: {reason}"))?;
            }
            extern_abi_value_error_inner(ret, true, structs, visiting)
                .map_err(|reason| format!("function pointer return type: {reason}"))
        }
        Type::LinearView(..) | Type::StridedView(..) | Type::RingView(..) | Type::BitView(..) => {
            Err("pointers to bml view/ring/bits descriptors are not C ABI-safe".to_string())
        }
        Type::Exclusive(_) | Type::Shared(_, _) | Type::Mmio(_) | Type::AgentShared(_) => {
            Err("pointers to storage-qualified types cannot cross extern boundaries".to_string())
        }
        Type::PeripheralHandle(_) => {
            Err("a `peripheral_type` handle cannot cross extern boundaries".to_string())
        }
        Type::Unresolved(_) | Type::Null | Type::Error(_) => Ok(()),
    }
}

/// Validate a single field of a `@repr(C)` struct that an extern pointer reaches.
/// A field is laid out inline in memory, so aggregates differ from the by-value
/// parameter rules: arrays and nested `@repr(C)` structs recurse instead of being
/// rejected, and a bare `void` member is meaningless (the `*void` pointee rule
/// does not apply inside a struct). Everything else (scalars, enums, pointers,
/// function pointers, views, storage-qualified types) is judged exactly as a
/// by-value parameter, so it delegates to [`extern_abi_value_error`]; that match
/// is exhaustive, so a new `Type` variant fails to compile there rather than
/// silently slipping through this delegation.
fn extern_abi_field_error(
    ty: &Type,
    structs: &HashMap<String, types::StructInfo>,
    visiting: &mut HashSet<String>,
) -> Result<(), String> {
    match ty.inner() {
        Type::Struct(name, StructRepr::C, fields) => {
            if !visiting.insert(name.clone()) {
                return Ok(());
            }

            let fields = structs.get(name).map_or(fields, |info| &info.fields);
            for (fname, fty) in fields {
                extern_abi_field_error(fty, structs, visiting)
                    .map_err(|reason| format!("field `{fname}` of struct `{name}`: {reason}"))?;
            }
            visiting.remove(name);
            Ok(())
        }
        Type::Struct(name, StructRepr::Explicit, _) => Err(format!(
            "field of struct `{name}` needs `@repr(C)` for a stable C layout"
        )),
        Type::Struct(name, StructRepr::Packed, _) => Err(format!(
            "field of packed struct `{name}` has no portable C layout"
        )),
        Type::Array(inner, _) => extern_abi_field_error(inner, structs, visiting),
        Type::Void => Err("`void` is not a valid C struct field".to_string()),
        _ => extern_abi_value_error_inner(ty, false, structs, visiting),
    }
}

fn validate_sig_anns(
    params: &[ast::Param],
    ret: Option<&ast::TypeExpr>,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) {
    for p in params {
        validate_type_ann_resolved(&p.ty, symbols, diags);
    }
    if let Some(ret) = ret {
        validate_type_ann_resolved(ret, symbols, diags);
    }
}

fn validate_type_ann_resolved(
    ty: &ast::TypeExpr,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
) {
    validate_type_ann(ty, diags);
    let resolved = types::resolve_type_expr(ty, &symbols.structs, &symbols.enums);
    validate_resolved_type_size(&resolved, ty.span(), diags);
}

fn validate_resolved_type_size(
    ty: &Type,
    span: crate::source::Span,
    diags: &mut DiagnosticBag,
) -> bool {
    if types::checked_element_size(ty).is_some() {
        true
    } else {
        diags.error(
            format!("type `{ty}` is too large; maximum supported size is 4294967295 bytes"),
            "E358",
            span,
        );
        false
    }
}

/// Reject a strided-view annotation whose compile-time stride is not in
/// `1..=u32::MAX`. Recurses through nested view/pointer/array/fn types so a
/// stride buried in, say, `*mut view u32 stride K` is still caught.
fn validate_type_ann(ty: &ast::TypeExpr, diags: &mut DiagnosticBag) {
    match ty {
        ast::TypeExpr::StridedView(inner, _, stride) => {
            let in_range = matches!(
                stride.as_ref(),
                ast::Expr::IntLiteral(n, _, _) if (1..=u64::from(u32::MAX)).contains(n)
            );
            if !in_range {
                diags.error(
                    "`view` stride must be a compile-time integer in 1..=4294967295".to_string(),
                    "E332",
                    stride.span(),
                );
            }
            validate_type_ann(inner, diags);
        }
        ast::TypeExpr::View(inner, _)
        | ast::TypeExpr::Ring(inner, _)
        | ast::TypeExpr::Ptr(inner)
        | ast::TypeExpr::ConstPtr(inner)
        | ast::TypeExpr::Array(inner, _) => validate_type_ann(inner, diags),
        ast::TypeExpr::Fn(params, ret) => {
            for p in params {
                validate_type_ann(p, diags);
            }
            if let Some(ret) = ret {
                validate_type_ann(ret, diags);
            }
        }
        ast::TypeExpr::Named(_)
        | ast::TypeExpr::Bits(_)
        | ast::TypeExpr::Addr(_)
        | ast::TypeExpr::Void(_) => {}
    }
}

fn check_global_initializers(program: &Program, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    let consts = collect_const_values(program, symbols);
    for item in &program.items {
        match item {
            ast::Item::StaticDef(s) => {
                if let Some(init) = &s.init {
                    let expected =
                        types::resolve_type_expr(&s.ty, &symbols.structs, &symbols.enums);
                    let mut scope = ScopeStack::new();
                    let actual = check_expr(init, symbols, &mut scope, "<global>", None, diags);
                    if !types::types_compatible(&expected, &actual)
                        && !unsuffixed_literal_fits(init, &expected)
                    {
                        diags.error(
                            format!(
                                "type mismatch: declared `{expected:?}` but initialized with `{actual:?}`"
                            ),
                            "E300",
                            s.name.1,
                        );
                    }
                }
            }
            ast::Item::ConstDef(c) => {
                let expected = types::resolve_type_expr(&c.ty, &symbols.structs, &symbols.enums);
                let mut scope = ScopeStack::new();
                let actual = check_expr(&c.value, symbols, &mut scope, "<global>", None, diags);
                if !types::types_compatible(&expected, &actual)
                    && !unsuffixed_literal_fits(&c.value, &expected)
                {
                    diags.error(
                        format!(
                            "type mismatch: declared `{expected:?}` but initialized with `{actual:?}`"
                        ),
                        "E300",
                        c.name.1,
                    );
                }
                if !const_init_is_compile_time(&c.value, symbols, &consts) {
                    diags.error(
                        "const initializer must be a compile-time constant expression",
                        "E343",
                        c.name.1,
                    );
                }
            }
            _ => {}
        }
    }
}

/// Constant-evaluation environment for module-level checks: values come from the
/// [`collect_const_values`] fixpoint, names and types from the symbol table. See
/// [`crate::consteval`] for the shared evaluator.
struct CheckEnv<'a> {
    symbols: &'a SymbolTable,
    vals: &'a HashMap<String, ConstVal>,
}

impl consteval::Env for CheckEnv<'_> {
    fn const_int(&self, name: &str) -> Option<i128> {
        match self.vals.get(name) {
            Some(ConstVal::Int(v)) => Some(*v),
            _ => None,
        }
    }
    fn const_bool(&self, name: &str) -> Option<bool> {
        match self.vals.get(name) {
            Some(ConstVal::Bool(b)) => Some(*b),
            _ => None,
        }
    }
    fn array_len(&self, name: &str) -> Option<i128> {
        global_array_len(self.symbols, name)
    }
    fn sizeof(&self, ty: &ast::TypeExpr) -> Option<i128> {
        let t = types::resolve_type_expr(ty, &self.symbols.structs, &self.symbols.enums);
        if matches!(t, Type::Unresolved(_)) {
            return None;
        }
        Some(i128::from(types::element_size(&t)))
    }
}

/// Element count of a named array `const`/`static`, or `None` if it is not a
/// (visible) array. `.inner()` sees through a storage wrapper.
fn global_array_len(symbols: &SymbolTable, name: &str) -> Option<i128> {
    symbols
        .consts
        .get(name)
        .map(|s| &s.ty)
        .or_else(|| symbols.statics.get(name).map(|s| &s.ty))
        .and_then(|ty| match ty.inner() {
            Type::Array(_, n) => Some(*n as i128),
            _ => None,
        })
}

fn const_init_is_compile_time(
    expr: &Expr,
    symbols: &SymbolTable,
    vals: &HashMap<String, ConstVal>,
) -> bool {
    match expr {
        Expr::ArrayInit(elems, _) => elems
            .iter()
            .all(|elem| const_init_is_compile_time(elem, symbols, vals)),
        Expr::StructInit { fields, .. } => fields
            .iter()
            .all(|(_, value)| const_init_is_compile_time(value, symbols, vals)),
        Expr::FloatLiteral(..) | Expr::NullLiteral(_) => true,
        // Naming another `const` is compile-time regardless of its type (the
        // integer path below only tracks ints, so this also covers float/
        // aggregate const references). Type compatibility is checked separately.
        Expr::Ident((name, _)) if symbols.consts.contains_key(name) => true,
        _ => consteval::eval(expr, &CheckEnv { symbols, vals }).is_some(),
    }
}

#[allow(clippy::too_many_arguments)]
fn check_len_builtin(
    args: &[Expr],
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) -> Type {
    if args.len() != 1 {
        let span = args.first().map_or(
            crate::source::Span::empty(crate::source::FileId::new(), 0),
            Expr::span,
        );
        diags.error(
            format!("function `len` expects 1 argument, got {}", args.len()),
            "E307",
            span,
        );
        return Type::U32;
    }

    let arg = &args[0];
    let ty = read_place_type(arg, symbols, scope, fn_name, expected_ret, diags);
    match ty.inner() {
        Type::Array(..)
        | Type::LinearView(..)
        | Type::StridedView(..)
        | Type::RingView(..)
        | Type::BitView(_) => Type::U32,
        other => {
            diags.error(
                format!("len(...) expects an array or view, got `{other}`"),
                "E326",
                arg.span(),
            );
            Type::U32
        }
    }
}

/// Compile-time values of all `const` items (integer and boolean), resolved to a
/// fixpoint so a `const` defined in terms of another resolves regardless of order.
fn collect_const_values(program: &Program, symbols: &SymbolTable) -> HashMap<String, ConstVal> {
    let mut vals: HashMap<String, ConstVal> = HashMap::new();
    loop {
        let mut changed = false;
        for item in &program.items {
            if let ast::Item::ConstDef(c) = item
                && !vals.contains_key(&c.name.0)
                && let Some(v) = consteval::eval(
                    &c.value,
                    &CheckEnv {
                        symbols,
                        vals: &vals,
                    },
                )
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
            match consteval::eval(
                &ca.cond,
                &CheckEnv {
                    symbols,
                    vals: &vals,
                },
            ) {
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

/// Type-check one function, returning the resolved types of its locals (keyed
/// by name span) for tooling.
fn check_fn(fn_def: &ast::FnDef, symbols: &SymbolTable, diags: &mut DiagnosticBag) -> LocalTypes {
    let mut scope = ScopeStack::new();
    let expected_ret = fn_def
        .ret
        .as_ref()
        .map(|ty| types::resolve_type_expr(ty, &symbols.structs, &symbols.enums));

    // Add parameters to the outermost scope. A param naming a `peripheral_type`
    // is upgraded to a `PeripheralHandle` so `u.REG.FIELD` resolves against the
    // template layout (slice 2).
    let periph_type_names: HashSet<String> = symbols.peripheral_types.keys().cloned().collect();
    for param in &fn_def.params {
        let ty = types::upgrade_peripheral_handle(
            types::resolve_type_expr(&param.ty, &symbols.structs, &symbols.enums),
            &periph_type_names,
        );
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

    scope.local_types
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
                    validate_type_ann(ty_ann, diags);
                    let ann_ty = types::resolve_type_expr(ty_ann, &symbols.structs, &symbols.enums);
                    validate_resolved_type_size(&ann_ty, ty_ann.span(), diags);
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

                // Persist the resolved type for tooling before it is moved into
                // the scope entry.
                scope.record_local(vd.name.1, &ty);
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
                if if_stmt.comptime && !is_comptime_shaped(&if_stmt.cond, symbols) {
                    diags.error(
                        "`comptime if` condition must be a compile-time constant (literals, `const`s, and pure operators)",
                        "E411",
                        if_stmt.cond.span(),
                    );
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

            // `claim X { ... }`: a masked ownership window over the `@shared`
            // static X (the CPU-side reclaim). Inside the body X is its inner
            // type -- views and index-reads allowed -- via a patched symbol
            // table; the lowering wraps the block in one mask pair (BASEPRI to
            // the ceiling on v7-M, cpsid/cpsie otherwise).
            // Restrictions (E614): the target must be a `@shared` static, and
            // the body may not contain calls or escape the window (return, or
            // break/continue of an outer loop) -- a call could unmask early
            // through its own critical sections, an escape would skip the
            // unmask.
            Stmt::Claim(c) => {
                let is_shared = symbols
                    .statics
                    .get(&c.name.0)
                    .is_some_and(|sym| matches!(sym.ty, Type::Shared(..)));
                if is_shared {
                    check_claim_restrictions(&c.body, 0, &c.name.1, diags);
                    check_claim_view_escape(&c.body, &c.name.0, diags);
                    let patched = symbols.with_claimed(&c.name.0);
                    check_block(&c.body, &patched, scope, fn_name, expected_ret, diags);
                } else {
                    diags.error(
                        format!(
                            "`claim {0}` requires `{0}` to be a `@shared` static -- the claim \
                             window is the masked counterpart of `reclaim`, and only the \
                             ceiling discipline needs it.",
                            c.name.0
                        ),
                        "E614",
                        c.name.1,
                    );
                    check_block(&c.body, symbols, scope, fn_name, expected_ret, diags);
                }
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
                if match_stmt.comptime && !is_comptime_shaped(&match_stmt.scrutinee, symbols) {
                    diags.error(
                        "`comptime match` scrutinee must be a compile-time integer constant (literals, `const`s, and pure operators)",
                        "E411",
                        match_stmt.scrutinee.span(),
                    );
                }
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

    let (min, max) = if is_int {
        int_range(scrutinee_ty)
    } else {
        (0, 0)
    };
    let mut covered: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_ints: std::collections::HashSet<i128> = std::collections::HashSet::new();
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
                ast::MatchPattern::Int(v, span) => {
                    if enum_info.is_some() {
                        diags.error("integer pattern in an enum match", "E324", *span);
                    } else if *v < min || *v > max {
                        diags.error(
                            format!("pattern value {v} is out of range for `{scrutinee_ty:?}`"),
                            "E344",
                            *span,
                        );
                    } else if !seen_ints.insert(*v) {
                        diags.error(format!("duplicate pattern value {v}"), "E319", *span);
                    }
                }
                ast::MatchPattern::Range(lo, hi, span) => {
                    if enum_info.is_some() {
                        diags.error("integer pattern in an enum match", "E324", *span);
                    } else if lo > hi {
                        diags.error(format!("empty range `{lo}..{hi}` (lo > hi)"), "E344", *span);
                    } else if *lo < min || *hi > max {
                        diags.error(
                            format!("range `{lo}..{hi}` is out of range for `{scrutinee_ty:?}`"),
                            "E344",
                            *span,
                        );
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

/// Enforce the `claim` body restrictions (E614): no function calls, no
/// `return`, and no `break`/`continue` that would exit the claim block
/// (`loop_depth` tracks loops fully inside it, whose break/continue are
/// fine). A call's own per-access critical sections would restore the mask
/// inside the window and open it early; an escape would skip the restore
/// entirely.
/// Exhaustive walk (no catch-all), mirroring the other Stmt/Expr walkers.
fn check_claim_restrictions(
    block: &ast::Block,
    loop_depth: u32,
    claim_span: &crate::source::Span,
    diags: &mut DiagnosticBag,
) {
    for stmt in &block.stmts {
        claim_restrict_stmt(stmt, loop_depth, claim_span, diags);
    }
    if let Some(t) = &block.trailing {
        claim_restrict_expr(t, claim_span, diags);
    }
}

fn claim_restrict_stmt(
    stmt: &Stmt,
    loop_depth: u32,
    claim_span: &crate::source::Span,
    diags: &mut DiagnosticBag,
) {
    match stmt {
        Stmt::VarDecl(vd) => claim_restrict_expr(&vd.init, claim_span, diags),
        Stmt::Assign(a) => claim_restrict_expr(&a.value, claim_span, diags),
        Stmt::CompoundAssign(ca) => claim_restrict_expr(&ca.value, claim_span, diags),
        Stmt::Expr(e) => claim_restrict_expr(e, claim_span, diags),
        Stmt::If(i) => {
            claim_restrict_expr(&i.cond, claim_span, diags);
            check_claim_restrictions(&i.then_block, loop_depth, claim_span, diags);
            if let Some(eb) = &i.else_branch {
                claim_restrict_stmt(eb, loop_depth, claim_span, diags);
            }
        }
        Stmt::Loop(l) => check_claim_restrictions(&l.body, loop_depth + 1, claim_span, diags),
        Stmt::While(w) => {
            claim_restrict_expr(&w.cond, claim_span, diags);
            check_claim_restrictions(&w.body, loop_depth + 1, claim_span, diags);
        }
        Stmt::For(f) => {
            claim_restrict_expr(&f.start, claim_span, diags);
            claim_restrict_expr(&f.end, claim_span, diags);
            if let Some(step) = &f.step {
                claim_restrict_expr(step, claim_span, diags);
            }
            check_claim_restrictions(&f.body, loop_depth + 1, claim_span, diags);
        }
        Stmt::Match(m) => {
            claim_restrict_expr(&m.scrutinee, claim_span, diags);
            for arm in &m.arms {
                check_claim_restrictions(&arm.body, loop_depth, claim_span, diags);
            }
        }
        Stmt::Return(_) => {
            diags.error(
                "`return` inside a `claim` block would leave the window's mask in place (the \
                 restore is at the block end). Move the return outside the claim.",
                "E614",
                *claim_span,
            );
        }
        Stmt::Break(span) | Stmt::Continue(span) => {
            if loop_depth == 0 {
                diags.error(
                    "`break`/`continue` here exits the `claim` block and would skip its mask \
                     restore (the window never closes). Restructure so the claim block runs \
                     to its end.",
                    "E614",
                    *span,
                );
            }
        }
        Stmt::Asm(a) => {
            for (_, target) in &a.outputs {
                claim_restrict_expr(target, claim_span, diags);
            }
            for (_, value) in &a.inputs {
                claim_restrict_expr(value, claim_span, diags);
            }
        }
        Stmt::Assume(a) => claim_restrict_expr(&a.cond, claim_span, diags),
        Stmt::Assert(a) => claim_restrict_expr(&a.cond, claim_span, diags),
        Stmt::Block(b) => check_claim_restrictions(b, loop_depth, claim_span, diags),
        // A nested claim is its own (depth-suppressed) window; its body obeys
        // the same restrictions relative to the same outer window.
        Stmt::Claim(c) => check_claim_restrictions(&c.body, loop_depth, claim_span, diags),
    }
}

fn claim_restrict_expr(expr: &Expr, claim_span: &crate::source::Span, diags: &mut DiagnosticBag) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::Ident(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            claim_restrict_expr(e, claim_span, diags);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            claim_restrict_expr(l, claim_span, diags);
            claim_restrict_expr(r, claim_span, diags);
        }
        Expr::Call(callee, args) => {
            diags.error(
                "function calls inside a `claim` block are not allowed: a callee's own \
                 critical sections would restore the mask inside the window and open it \
                 early. Hoist the call out of the claim.",
                "E614",
                callee.span(),
            );
            claim_restrict_expr(callee, claim_span, diags);
            for a in args {
                claim_restrict_expr(a, claim_span, diags);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            claim_restrict_expr(base, claim_span, diags);
            if let Some(l) = len {
                claim_restrict_expr(l, claim_span, diags);
            }
            if let Some(s) = stride {
                claim_restrict_expr(s, claim_span, diags);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            claim_restrict_expr(base, claim_span, diags);
            if let Some(c) = capacity {
                claim_restrict_expr(c, claim_span, diags);
            }
            claim_restrict_expr(head, claim_span, diags);
            claim_restrict_expr(len, claim_span, diags);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            claim_restrict_expr(base, claim_span, diags);
            if let Some(o) = bit_offset {
                claim_restrict_expr(o, claim_span, diags);
            }
            if let Some(l) = len_bits {
                claim_restrict_expr(l, claim_span, diags);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                claim_restrict_expr(e, claim_span, diags);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                claim_restrict_expr(e, claim_span, diags);
            }
        }
        Expr::Match(m) => {
            claim_restrict_expr(&m.scrutinee, claim_span, diags);
            for arm in &m.arms {
                check_claim_restrictions(&arm.body, 0, claim_span, diags);
            }
        }
        Expr::Block(b) => check_claim_restrictions(&b.block, 0, claim_span, diags),
        Expr::If(i) => {
            claim_restrict_expr(&i.cond, claim_span, diags);
            check_claim_restrictions(&i.then_block, 0, claim_span, diags);
            claim_restrict_expr(&i.else_branch, claim_span, diags);
        }
    }
}

/// E616 (claim half): a view built over the claimed static must not escape the
/// `claim` window. Inside the window `X` is its inner type and views over it
/// are legal, but the descriptor is only trustworthy while the window's mask
/// holds -- assigned to a binding declared OUTSIDE the body it would outlive
/// the window's mask restore. Value copies out (`outer = v[0] as u32`) are the point of the
/// window and stay legal; what may not leave is the CAPABILITY: a
/// view/ring/bits expression whose base is `X`, or a binding holding one
/// (lexical taint through inside-declared `const`s). Addresses cast to
/// integers (`&X as u32`) are the verify/provenance domain, not checked here.
///
/// Lexical and name-based: a sub-block shadowing an outer name with an inside
/// declaration makes same-named outer assignments invisible (false negative
/// only); calls cannot smuggle the view out because claim bodies forbid calls
/// (E614).
fn check_claim_view_escape(block: &ast::Block, claim_name: &str, diags: &mut DiagnosticBag) {
    let mut esc = EscapeState::default();
    esc_block(block, claim_name, &mut esc, diags);
}

#[derive(Default)]
struct EscapeState {
    /// Names declared inside the claim body (any nesting depth).
    declared: std::collections::HashSet<String>,
    /// Inside-declared names currently holding a view over the claimed static.
    tainted: std::collections::HashSet<String>,
}

fn esc_block(block: &ast::Block, name: &str, esc: &mut EscapeState, diags: &mut DiagnosticBag) {
    for stmt in &block.stmts {
        esc_stmt(stmt, name, esc, diags);
    }
    if let Some(t) = &block.trailing {
        esc_expr(t, name, esc, diags);
    }
}

fn esc_stmt(stmt: &Stmt, name: &str, esc: &mut EscapeState, diags: &mut DiagnosticBag) {
    match stmt {
        Stmt::VarDecl(vd) => {
            esc_expr(&vd.init, name, esc, diags);
            if is_capability(&vd.init, name, esc) {
                esc.tainted.insert(vd.name.0.clone());
            }
            esc.declared.insert(vd.name.0.clone());
        }
        Stmt::Assign(a) => {
            esc_expr(&a.value, name, esc, diags);
            let capability = is_capability(&a.value, name, esc);
            match lvalue_base_name(&a.target) {
                Some(base) => {
                    let inside = esc.declared.contains(&base.0);
                    if capability && !inside {
                        diags.error(
                            format!(
                                "a view over the claimed static `{name}` escapes the `claim` \
                                 window here: `{}` is declared outside the window, so the view \
                                 would outlive the mask that makes it safe. Bind the view with \
                                 `const` inside the claim and finish using it before the window \
                                 closes.",
                                base.0
                            ),
                            "E616",
                            a.value.span(),
                        );
                    }
                    // A whole-name rebind tracks what the binding now holds; a
                    // capability stored into an inside aggregate is not
                    // re-tracked (conservative: the aggregate is inside, so it
                    // dies with the window anyway).
                    if let LValue::Name(_) = &a.target
                        && inside
                    {
                        if capability {
                            esc.tainted.insert(base.0.clone());
                        } else {
                            esc.tainted.remove(&base.0);
                        }
                    }
                }
                // Writing the capability through a pointer escapes to an
                // unknowable place; reject like an outside binding.
                None => {
                    if capability {
                        diags.error(
                            format!(
                                "a view over the claimed static `{name}` is written through a \
                                 pointer inside the `claim` window: the view must not outlive \
                                 the window's mask."
                            ),
                            "E616",
                            a.value.span(),
                        );
                    }
                }
            }
            esc_lvalue_exprs(&a.target, name, esc, diags);
        }
        Stmt::CompoundAssign(ca) => {
            esc_expr(&ca.value, name, esc, diags);
            esc_lvalue_exprs(&ca.target, name, esc, diags);
        }
        Stmt::Expr(e) => esc_expr(e, name, esc, diags),
        Stmt::If(i) => {
            esc_expr(&i.cond, name, esc, diags);
            esc_block(&i.then_block, name, esc, diags);
            if let Some(eb) = &i.else_branch {
                esc_stmt(eb, name, esc, diags);
            }
        }
        Stmt::Loop(l) => esc_block(&l.body, name, esc, diags),
        Stmt::While(w) => {
            esc_expr(&w.cond, name, esc, diags);
            esc_block(&w.body, name, esc, diags);
        }
        Stmt::For(f) => {
            esc_expr(&f.start, name, esc, diags);
            esc_expr(&f.end, name, esc, diags);
            if let Some(step) = &f.step {
                esc_expr(step, name, esc, diags);
            }
            esc.declared.insert(f.var.0.clone());
            esc_block(&f.body, name, esc, diags);
        }
        Stmt::Match(m) => {
            esc_expr(&m.scrutinee, name, esc, diags);
            for arm in &m.arms {
                esc_block(&arm.body, name, esc, diags);
            }
        }
        // `return` inside claim is already E614; nothing to track here.
        Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Asm(a) => {
            for (_, target) in &a.outputs {
                esc_expr(target, name, esc, diags);
            }
            for (_, value) in &a.inputs {
                esc_expr(value, name, esc, diags);
            }
        }
        Stmt::Assume(a) => esc_expr(&a.cond, name, esc, diags),
        Stmt::Assert(a) => esc_expr(&a.cond, name, esc, diags),
        Stmt::Block(b) => esc_block(b, name, esc, diags),
        Stmt::Claim(c) => esc_block(&c.body, name, esc, diags),
    }
}

/// Expression recursion only reaches statements embedded in block/if/match
/// expressions; a bare capability in value position that is not stored
/// anywhere cannot escape. Exhaustive (no catch-all).
fn esc_expr(expr: &Expr, name: &str, esc: &mut EscapeState, diags: &mut DiagnosticBag) {
    match expr {
        Expr::IntLiteral(..)
        | Expr::FloatLiteral(..)
        | Expr::BoolLiteral(..)
        | Expr::StringLiteral(..)
        | Expr::NullLiteral(_)
        | Expr::Ident(_)
        | Expr::EnumVariant { .. }
        | Expr::SizeOf(..) => {}
        Expr::Unary(_, e) | Expr::Group(e) | Expr::Cast(e, _) | Expr::FieldAccess(e, _) => {
            esc_expr(e, name, esc, diags);
        }
        Expr::Binary(l, _, r) | Expr::Index(l, r) => {
            esc_expr(l, name, esc, diags);
            esc_expr(r, name, esc, diags);
        }
        Expr::Call(callee, args) => {
            esc_expr(callee, name, esc, diags);
            for a in args {
                esc_expr(a, name, esc, diags);
            }
        }
        Expr::ViewNew {
            base, len, stride, ..
        } => {
            esc_expr(base, name, esc, diags);
            if let Some(l) = len {
                esc_expr(l, name, esc, diags);
            }
            if let Some(s) = stride {
                esc_expr(s, name, esc, diags);
            }
        }
        Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            esc_expr(base, name, esc, diags);
            if let Some(c) = capacity {
                esc_expr(c, name, esc, diags);
            }
            esc_expr(head, name, esc, diags);
            esc_expr(len, name, esc, diags);
        }
        Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            esc_expr(base, name, esc, diags);
            if let Some(o) = bit_offset {
                esc_expr(o, name, esc, diags);
            }
            if let Some(l) = len_bits {
                esc_expr(l, name, esc, diags);
            }
        }
        Expr::ArrayInit(elems, _) => {
            for e in elems {
                esc_expr(e, name, esc, diags);
            }
        }
        Expr::StructInit { fields, .. } => {
            for (_, e) in fields {
                esc_expr(e, name, esc, diags);
            }
        }
        Expr::Match(m) => {
            esc_expr(&m.scrutinee, name, esc, diags);
            for arm in &m.arms {
                esc_block(&arm.body, name, esc, diags);
            }
        }
        Expr::Block(b) => esc_block(&b.block, name, esc, diags),
        Expr::If(i) => {
            esc_expr(&i.cond, name, esc, diags);
            esc_block(&i.then_block, name, esc, diags);
            esc_expr(&i.else_branch, name, esc, diags);
        }
    }
}

/// Walk the expressions embedded in an lvalue (index positions, deref bases)
/// without treating them as escapes.
fn esc_lvalue_exprs(lv: &LValue, name: &str, esc: &mut EscapeState, diags: &mut DiagnosticBag) {
    match lv {
        LValue::Name(_) => {}
        LValue::Field(base, _) => esc_lvalue_exprs(base, name, esc, diags),
        LValue::Index(base, idx) => {
            esc_lvalue_exprs(base, name, esc, diags);
            esc_expr(idx, name, esc, diags);
        }
        LValue::Deref(e) => esc_expr(e, name, esc, diags),
    }
}

/// The root binding of an lvalue (`v[0].f` -> `v`); `None` for a deref target.
fn lvalue_base_name(lv: &LValue) -> Option<&ast::Ident> {
    match lv {
        LValue::Name(id) => Some(id),
        LValue::Field(base, _) | LValue::Index(base, _) => lvalue_base_name(base),
        LValue::Deref(_) => None,
    }
}

/// Whether `e` evaluates to a view/ring/bits descriptor over the claimed
/// static: a `view`/`ring`/`bits`/`reclaim` whose base is the static (or a
/// tainted binding), a tainted identifier, or an embedded block expression
/// whose result is one.
fn is_capability(e: &Expr, name: &str, esc: &EscapeState) -> bool {
    match e {
        Expr::Group(inner) => is_capability(inner, name, esc),
        Expr::Ident((n, _)) => esc.tainted.contains(n),
        Expr::ViewNew { base, .. } | Expr::RingNew { base, .. } | Expr::BitNew { base, .. } => {
            capability_base(base, name, esc)
        }
        Expr::Block(b) => b
            .block
            .trailing
            .as_ref()
            .is_some_and(|t| is_capability(t, name, esc)),
        Expr::If(i) => {
            i.then_block
                .trailing
                .as_ref()
                .is_some_and(|t| is_capability(t, name, esc))
                || is_capability(&i.else_branch, name, esc)
        }
        Expr::Match(m) => m.arms.iter().any(|arm| {
            arm.body
                .trailing
                .as_ref()
                .is_some_and(|t| is_capability(t, name, esc))
        }),
        _ => false,
    }
}

fn capability_base(base: &Expr, name: &str, esc: &EscapeState) -> bool {
    match base {
        Expr::Group(inner) => capability_base(inner, name, esc),
        Expr::Ident((n, _)) => n == name || esc.tainted.contains(n),
        _ => false,
    }
}

fn block_definitely_returns(block: &ast::Block) -> bool {
    block.stmts.iter().any(stmt_definitely_returns)
}

fn stmt_definitely_returns(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Return(_) => true,
        Stmt::Block(block) => block_definitely_returns(block),
        Stmt::Claim(c) => block_definitely_returns(&c.body),
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
        Stmt::Claim(c) => block_may_break(&c.body),
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

/// A `peripheral_type` argument must be a bare identifier naming a compile-time
/// peripheral instance of the expected type (`type_name` matches), or another
/// handle parameter of that type (pass-through). Anything else is `E308`:
/// monomorphization needs the instance statically.
fn check_peripheral_handle_arg(
    arg: &Expr,
    expected: &str,
    param_name: &str,
    callee: &str,
    symbols: &SymbolTable,
    scope: &ScopeStack,
    diags: &mut DiagnosticBag,
) {
    if let Expr::Ident((arg_name, span)) = arg {
        if let Some(p) = symbols.peripherals.get(arg_name) {
            if p.type_name.as_deref() == Some(expected) {
                return;
            }
            diags.error(
                format!(
                    "argument `{param_name}` of `{callee}` expects a `{expected}` instance, \
                     but `{arg_name}` is not one"
                ),
                "E308",
                *span,
            );
            return;
        }
        if let Some(info) = scope.lookup(arg_name)
            && let Type::PeripheralHandle(t) = &info.ty
        {
            if t == expected {
                return;
            }
            diags.error(
                format!(
                    "argument `{param_name}` of `{callee}` expects a `{expected}` instance, \
                     got a `{t}` handle"
                ),
                "E308",
                *span,
            );
            return;
        }
    }
    diags.error(
        format!(
            "argument `{param_name}` of `{callee}` must be a compile-time peripheral instance \
             of type `{expected}`"
        ),
        "E308",
        arg.span(),
    );
}

/// Whether `arg` is acceptable for a `comptime` value parameter: an integer
/// literal or a named `const`. Const-expression arguments are a follow-up (see
/// `doc/comptime.md`); the IR evaluates a superset of this, so it stays sound.
fn is_comptime_const_arg(arg: &Expr, symbols: &SymbolTable) -> bool {
    match arg {
        Expr::IntLiteral(..) => true,
        Expr::Ident((name, _)) => symbols.consts.contains_key(name),
        _ => false,
    }
}

/// Structural test that `expr` will const-fold: literals, named `const`s, and
/// pure operators over such operands. Validates a `comptime if` condition up
/// front (E411) so codegen can always fold it. (Comptime params are not yet
/// accepted here -- that arrives with the param-driven slice; see doc/comptime.md.)
fn is_comptime_shaped(expr: &Expr, symbols: &SymbolTable) -> bool {
    match expr {
        Expr::IntLiteral(..) | Expr::BoolLiteral(..) => true,
        Expr::Ident((name, _)) => symbols.consts.contains_key(name),
        Expr::Group(inner) | Expr::Unary(_, inner) => is_comptime_shaped(inner, symbols),
        Expr::Binary(l, _, r) => is_comptime_shaped(l, symbols) && is_comptime_shaped(r, symbols),
        _ => false,
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
        for (i, (arg, (param_name, param_ty))) in args.iter().zip(fn_sym.params.iter()).enumerate()
        {
            // A `peripheral_type` parameter (slice 2) is monomorphized, so its
            // argument must be a statically-known peripheral instance of the
            // matching type (or another handle param of that type, for
            // pass-through) -- not an arbitrary value.
            if let Type::PeripheralHandle(t) = param_ty {
                check_peripheral_handle_arg(arg, t, param_name, name, symbols, scope, diags);
                continue;
            }
            let arg_ty = check_expr(arg, symbols, scope, fn_name, expected_ret, diags);
            // A `comptime` value parameter is monomorphized per value, so its
            // argument must be a compile-time constant.
            if fn_sym.comptime.get(i).copied().unwrap_or(false)
                && !is_comptime_const_arg(arg, symbols)
            {
                diags.error(
                    format!(
                        "argument `{param_name}` of `{name}` is a `comptime` parameter and requires a compile-time constant"
                    ),
                    "E410",
                    arg.span(),
                );
            }
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
    structs: &HashMap<String, types::StructInfo>,
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

/// The register map a base identifier denotes when used as `NAME.REG[.FIELD]`:
/// a global peripheral instance, or a `peripheral_type` handle parameter in
/// scope (slice 2). `None` for anything else (the caller then falls through to
/// struct field access). Lets the peripheral-access checks serve both forms with
/// one code path.
fn peripheral_reg_map<'a>(
    name: &str,
    scope: &ScopeStack,
    symbols: &'a SymbolTable,
) -> Option<&'a std::collections::HashMap<String, crate::resolver::RegSymbol>> {
    if let Some(p) = symbols.peripherals.get(name) {
        return Some(&p.regs);
    }
    if let Some(info) = scope.lookup(name)
        && let Type::PeripheralHandle(t) = &info.ty
    {
        return symbols.peripheral_types.get(t).map(|s| &s.regs);
    }
    None
}

/// `P.REG` where REG is an *array* register (declared `reg REG[N] ... stride S`).
/// Returns the register symbol so `P.REG[i]` can route to the indexed-register
/// path. `None` if P isn't a peripheral/handle, REG isn't one of its registers,
/// or REG is a scalar register.
fn array_reg<'a>(
    periph: &str,
    reg: &str,
    scope: &ScopeStack,
    symbols: &'a SymbolTable,
) -> Option<&'a crate::resolver::RegSymbol> {
    let r = peripheral_reg_map(periph, scope, symbols)?.get(reg)?;
    r.array.map(|_| r)
}

/// Compile-time bounds check for a *constant* register-array index. A runtime
/// index is left to the surrounding loop guard / verifier (matching how the
/// SDK's load loop and all other MMIO address math are trusted).
fn check_reg_index_bounds(
    index: &Expr,
    len: u64,
    periph: &str,
    reg: &str,
    diags: &mut DiagnosticBag,
) {
    // Peer through value-preserving parens so `P.REG[(9)]` is caught too. `Cast`
    // and `Unary` are NOT unwrapped: a cast can truncate (`300 as u8` == 44) and
    // negation changes the value, so the inner literal would not be the real
    // index. Named-const and computed indices are trusted like runtime indices
    // (the checker has no const values threaded here) -- only literal indices
    // get the static bound.
    let inner = match index {
        Expr::Group(e) => e.as_ref(),
        other => other,
    };
    if let Expr::IntLiteral(n, _, span) = inner
        && *n >= len
    {
        diags.error(
            format!("register-array index {n} out of range for `{periph}.{reg}` (len {len})"),
            "E338",
            *span,
        );
    }
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
        // Unsuffixed literals type-check as `u32`; fitting into a wider
        // expected type is checked at the assignment/coercion site.
        Expr::IntLiteral(_, suffix, _) => types::int_suffix_type(*suffix).unwrap_or(Type::U32),
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
                // A `peripheral_type` handle reaches this generic value path only
                // by misuse: `NAME.REG[.FIELD]` access and call pass-through are
                // handled before here. A handle has no runtime value (it is
                // monomorphized away), so reject using it as one (slice 2).
                if let Type::PeripheralHandle(t) = &ty {
                    let guard = diags.error(
                        format!(
                            "`{name}` is a `{t}` handle; use it as `{name}.REG` / \
                             `{name}.REG.FIELD` or pass it to a driver, not as a value"
                        ),
                        "E309",
                        *span,
                    );
                    return Type::Error(guard);
                }
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
                // A driver with a `peripheral_type` parameter is monomorphized
                // per instance and has no single address: it cannot be used as a
                // value / function pointer, only called directly (slice 2).
                if fn_sym
                    .params
                    .iter()
                    .any(|(_, t)| matches!(t, Type::PeripheralHandle(_)))
                {
                    let guard = diags.error(
                        format!(
                            "cannot take the address of `{name}`: it has a `peripheral_type` \
                             parameter and is monomorphized per instance -- call it directly"
                        ),
                        "E309",
                        *span,
                    );
                    return Type::Error(guard);
                }
                // Declared core entries are exempt from E408: the launch
                // handshake takes their address for HARDWARE (another
                // core's boot), not for a bml pointer call, so a concrete
                // @context on the entry stays meaningful. Trusted: nothing
                // stops the address being reused as a callback afterwards.
                if fn_sym.context != crate::context::Context::Any
                    && !symbols.entry_fns.contains(name)
                {
                    diags.error(
                        format!("cannot take address of non-any-context function `{name}` -- only functions without @context restriction can be used as function pointers"),
                        "E408",
                        *span,
                    );
                }
                return fn_sym.fn_pointer_type();
            }

            diags.error(format!("undefined name: `{name}`"), "E305", *span);
            Type::Unresolved(name.clone())
        }

        Expr::Unary(op, inner) => {
            use crate::ast::UnaryOp;
            // Taking the address of a local borrows it; it does not consume a
            // Move-typed value. So read the operand's type without consuming.
            let place_info = if matches!(op, UnaryOp::AddrOf | UnaryOp::AddrOfMut) {
                Some(read_place_info(
                    inner,
                    symbols,
                    scope,
                    fn_name,
                    expected_ret,
                    diags,
                ))
            } else {
                None
            };
            let inner_ty = if let Some(info) = &place_info {
                info.ty.clone()
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
                    // Taking the address of a function produces a function
                    // pointer. The non-any-context rejection (E408) is already
                    // emitted while reading the operand above, so here we only
                    // compute the function-pointer type.
                    if let Expr::Ident((name, _)) = inner.as_ref()
                        && let Some(fn_sym) = symbols.functions.get(name)
                    {
                        return fn_sym.fn_pointer_type();
                    }
                    if let Some(error) = place_info
                        .as_ref()
                        .and_then(|info| info.addr_borrow_error.as_ref())
                    {
                        error.emit(diags);
                    }
                    if inner_ty.is_move() {
                        Type::ConstPtr(Box::new(inner_ty.inner().clone()))
                    } else {
                        Type::ConstPtr(Box::new(inner_ty))
                    }
                }
                UnaryOp::AddrOfMut => {
                    if let Some(error) = place_info
                        .as_ref()
                        .and_then(|info| info.addr_borrow_error.as_ref())
                    {
                        error.emit(diags);
                    }
                    if let Some(error) = place_info.and_then(|info| info.mut_borrow_error) {
                        error.emit(diags);
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
                BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap => {
                    // Wrapping arithmetic declares intent to the verifier, so
                    // it must only appear where wrap is meaningful: plain
                    // integer operands. No pointers (wrap on an address is
                    // never intent), no floats, no b1.
                    if !types::is_int(&left_ty) {
                        diags.error(
                            format!(
                                "wrapping operator requires integer operands, got `{left_ty:?}`"
                            ),
                            "E336",
                            left.span(),
                        );
                    }
                    if !types::is_int(&right_ty) {
                        diags.error(
                            format!(
                                "wrapping operator requires integer operands, got `{right_ty:?}`"
                            ),
                            "E336",
                            right.span(),
                        );
                    } else if types::is_int(&left_ty) && left_ty != right_ty {
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
            if consteval::is_len_call(func_expr) {
                return check_len_builtin(args, symbols, scope, fn_name, expected_ret, diags);
            }

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
                    // GPIOA.REG (or `u.REG` for a peripheral_type param) -- register read
                    if let Some(regs) = peripheral_reg_map(periph_name, scope, symbols) {
                        if let Some(reg) = regs.get(&field.0) {
                            if reg.array.is_some() {
                                diags.error(
                                    format!(
                                        "register `{periph_name}.{0}` is a register array; \
                                         index it: `{0}[i]`",
                                        field.0
                                    ),
                                    "E337",
                                    field.1,
                                );
                            } else if reg.access == crate::ast::Access::WriteOnly {
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
                        // GPIOA.REG.FIELD (or `u.REG.FIELD` for a param) -- field read
                        if let Some(regs) = peripheral_reg_map(periph_name, scope, symbols) {
                            if let Some(reg) = regs.get(&reg_field.0) {
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
                // `P.REG[i].FIELD` -- field of an indexed array register.
                Expr::Index(arr, idx) => {
                    if let Expr::FieldAccess(p, reg) = arr.as_ref()
                        && let Expr::Ident((pname, _)) = p.as_ref()
                        && let Some(r) = array_reg(pname, &reg.0, scope, symbols)
                    {
                        check_expr(idx, symbols, scope, fn_name, expected_ret, diags);
                        check_reg_index_bounds(idx, r.array.unwrap().0, pname, &reg.0, diags);
                        if let Some(fs) = r.fields.get(&field.0) {
                            if fs.access == crate::ast::Access::WriteOnly {
                                diags.error(
                                    format!(
                                        "cannot read from writeonly field `{pname}.{}.{}`",
                                        reg.0, field.0
                                    ),
                                    "E330",
                                    field.1,
                                );
                            }
                            return fs.ty.clone();
                        }
                        diags.error(
                            format!("register `{}` has no field `{}`", reg.0, field.0),
                            "E322",
                            field.1,
                        );
                        return Type::U32;
                    }
                }
                _ => {}
            }

            let base_ty = check_expr(base, symbols, scope, fn_name, expected_ret, diags);
            // Check if it's a struct field access
            if field.0 == "_" {
                let guard = diags.error("padding field `_` is not addressable", "E355", field.1);
                return Type::Error(guard);
            }
            if let Type::Struct(name, _, fields) = &base_ty {
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
                && let Type::Struct(name, _, fields) = inner.as_ref()
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
            // Indexed register read: `P.REG[i]` where REG is an array register
            // (`reg REG[N] ... stride S`). Stays on the volatile MMIO path; the
            // value is the register (u32), not an element of some container.
            if let Expr::FieldAccess(p, reg) = base.as_ref()
                && let Expr::Ident((pname, _)) = p.as_ref()
                && let Some(r) = array_reg(pname, &reg.0, scope, symbols)
            {
                check_expr(index, symbols, scope, fn_name, expected_ret, diags);
                if r.access == crate::ast::Access::WriteOnly {
                    diags.error(
                        format!("cannot read from writeonly register `{pname}.{}`", reg.0),
                        "E330",
                        reg.1,
                    );
                }
                check_reg_index_bounds(index, r.array.unwrap().0, pname, &reg.0, diags);
                return Type::U32;
            }
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
            let source_ty = check_expr(inner, symbols, scope, fn_name, expected_ret, diags);
            let target_ty = types::resolve_type_expr(ty_expr, &symbols.structs, &symbols.enums);
            // A cast to `b1` has no valid lowering for a non-`b1` source: there is
            // no correct one-bit width conversion. Const eval would silently yield
            // 0 and codegen emits an invalid `bitcast ... to i1`, so reject it and
            // point the user at an explicit comparison.
            if matches!(target_ty, Type::B1) && !matches!(source_ty.inner(), Type::B1) {
                diags.error(
                    format!("cannot cast `{source_ty:?}` to `b1`; compare instead (e.g. `x != 0`)"),
                    "E346",
                    ty_expr.span(),
                );
            }
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
            validate_resolved_type_size(&resolved, ty_expr.span(), diags);
            Type::U32
        }
        Expr::ViewNew {
            base,
            len,
            stride,
            reclaim,
            span,
        } => {
            let base_ty = check_expr(base, symbols, scope, fn_name, expected_ret, diags);
            if let Some(stride) = stride {
                // `view(arr, stride K)`: strided view over an array. The stride
                // is a compile-time element multiplier (>= 1) carried in the
                // type, not a runtime descriptor field. Element type comes from
                // the array; the logical length `N/K` is computed at lowering.
                let k = match stride.as_ref() {
                    crate::ast::Expr::IntLiteral(n, _, _)
                        if (1..=u64::from(u32::MAX)).contains(n) =>
                    {
                        u32::try_from(*n).expect("stride was range-checked")
                    }
                    _ => {
                        let guard = diags.error(
                            "`view` stride must be a compile-time integer in 1..=4294967295"
                                .to_string(),
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
                if *reclaim {
                    // `reclaim(arr)`: the explicit, handshake-acknowledged view
                    // over agent-shared memory. Requires an `AgentShared` base
                    // (a plain array needs no reclaiming); bypasses the
                    // agent-shared rejection that `view` now applies.
                    if matches!(&base_ty, Type::Shared(inner, _) if matches!(**inner, Type::AgentShared(..)))
                    {
                        // The mixed-sharer composition: agent-shared AND
                        // CPU-shared. The reclaim handshake alone is not
                        // enough -- another CPU context could race it; the
                        // masked window supplies that half.
                        let guard = diags.error(
                            "`reclaim` of a `@shared` region static requires the masked window: \
                             wrap it in `claim X { ... }` (inside the claim the static is plain \
                             agent-shared and the completion-guarded reclaim applies)."
                                .to_string(),
                            "E335",
                            *span,
                        );
                        return Type::Error(guard);
                    }
                    if !matches!(base_ty, Type::AgentShared(..)) {
                        let guard = diags.error(
                            format!(
                                "`reclaim(x)` applies only to agent-shared memory (an array in a \
                                 region a DMA/external agent touches); `{base_ty}` is not \
                                 agent-shared -- use `view(x)`"
                            ),
                            "E335",
                            *span,
                        );
                        Type::Error(guard)
                    } else if let Type::Array(inner, _) = base_ty.inner() {
                        Type::LinearView(Box::new((**inner).clone()), mutable)
                    } else {
                        let guard = diags.error(
                            format!("`reclaim(x)` argument must be an agent-shared array, got `{base_ty}`"),
                            "E335",
                            *span,
                        );
                        Type::Error(guard)
                    }
                } else if matches!(base_ty, Type::AgentShared(..)) {
                    // Tighten the contiguous `view(arr)`: viewing agent-shared
                    // memory directly is the same aliasing the index-read
                    // protection (E326) blocks -- the agent may still own it. The
                    // explicit `reclaim(x)` is the handshake-acknowledged escape.
                    // (ring/strided/bits over agent-shared are not yet tightened;
                    // they have no reclaim form -- a follow-up.)
                    let guard = diags.error(
                        "cannot build a view over agent-shared memory directly: the agent may \
                         still own it (that is what the index-read protection guards). Use \
                         `reclaim(x)` once the agent's transfer has completed -- same \
                         bounds-checked view, but it marks that the ownership handshake happened."
                            .to_string(),
                        "E335",
                        *span,
                    );
                    Type::Error(guard)
                } else if let Some(guard) = reject_shared_view_backing(&base_ty, *span, diags) {
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
                    let cap_hint = u32::try_from(*n).ok().filter(|_| n.is_power_of_two());
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
            if match_expr.comptime && !is_comptime_shaped(&match_expr.scrutinee, symbols) {
                diags.error(
                    "`comptime match` scrutinee must be a compile-time integer constant (literals, `const`s, and pure operators)",
                    "E411",
                    match_expr.scrutinee.span(),
                );
            }
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
            if let Some(struct_info) = symbols.structs.get(struct_name) {
                let struct_fields = &struct_info.fields;
                // Check all required fields are provided
                for (fname, ftype) in struct_fields {
                    if fname == "_" {
                        continue;
                    }
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
                    if fname.0 == "_" {
                        diags.error(
                            "padding field `_` cannot be initialized by name",
                            "E354",
                            fname.1,
                        );
                        continue;
                    }
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
                Type::Struct(struct_name.clone(), struct_info.repr, struct_fields.clone())
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

struct PlaceInfo {
    ty: Type,
    mut_borrow_error: Option<MutBorrowError>,
    addr_borrow_error: Option<AddrBorrowError>,
}

#[derive(Clone)]
enum AddrBorrowError {
    Packed {
        struct_name: String,
        field_name: String,
        field_offset: u32,
        field_align: u32,
        span: crate::source::Span,
    },
    Endian {
        struct_name: String,
        field_name: String,
        span: crate::source::Span,
    },
    /// Taking the address of a `@shared` static (or a field/element of one)
    /// outside a `claim` window. Access through the resulting `*T` carries no
    /// static name, so it receives no ceiling critical section -- the exact
    /// hazard E405 rejects for views. Set at the root static in
    /// `read_place_info` and propagated through field/index; inside a `claim`
    /// the static reads as its inner (non-`@shared`) type, so it is not set.
    Shared {
        name: String,
        span: crate::source::Span,
    },
}

impl AddrBorrowError {
    fn emit(&self, diags: &mut DiagnosticBag) {
        match self {
            AddrBorrowError::Packed {
                struct_name,
                field_name,
                field_offset,
                field_align,
                span,
            } => {
                diags.error(
                    format!(
                        "cannot take address of packed field `{struct_name}.{field_name}` at offset {field_offset}; packed structs are byte-aligned but field alignment is {field_align}"
                    ),
                    "E357",
                    *span,
                );
            }
            AddrBorrowError::Endian {
                struct_name,
                field_name,
                span,
            } => {
                diags.error(
                    format!(
                        "cannot take address of field `{struct_name}.{field_name}`: it is stored in a non-native byte order for this target, so the bytes are swapped relative to a plain `*T` read through the pointer. Read or write the field directly, or take a byte view over the struct for raw bytes."
                    ),
                    "E360",
                    *span,
                );
            }
            AddrBorrowError::Shared { name, span } => {
                diags.error(
                    format!(
                        "cannot take the address of `@shared` `{name}`: access through the \
                         resulting pointer carries no static name, so it gets none of the \
                         ceiling critical-section that direct access does -- it would be an \
                         unprotected race. Access `{name}` directly, or take its address \
                         inside a `claim {name} {{ ... }}` window."
                    ),
                    "E405",
                    *span,
                );
            }
        }
    }
}

enum MutBorrowError {
    ImmutableBinding(String, crate::source::Span),
    ConstPtr(crate::source::Span),
    ReadonlyView(crate::source::Span),
    NotMutablePlace(crate::source::Span),
}

impl MutBorrowError {
    fn emit(self, diags: &mut DiagnosticBag) {
        match self {
            MutBorrowError::ImmutableBinding(name, span) => {
                diags.error(
                    format!("cannot take mutable address of immutable `{name}`"),
                    "E309",
                    span,
                );
            }
            MutBorrowError::ConstPtr(span) => {
                diags.error(
                    "cannot write through const pointer (`*T`) -- use `*mut T`",
                    "E314",
                    span,
                );
            }
            MutBorrowError::ReadonlyView(span) => {
                diags.error(
                    "cannot write through a readonly `view`; only reads are allowed".to_string(),
                    "E334",
                    span,
                );
            }
            MutBorrowError::NotMutablePlace(span) => {
                diags.error(
                    "cannot take mutable address of this expression",
                    "E309",
                    span,
                );
            }
        }
    }
}

fn read_place_info(
    expr: &Expr,
    symbols: &SymbolTable,
    scope: &mut ScopeStack,
    fn_name: &str,
    expected_ret: Option<&Type>,
    diags: &mut DiagnosticBag,
) -> PlaceInfo {
    match expr {
        Expr::Ident((name, span)) => {
            if let Some(info) = scope.lookup(name) {
                // A `peripheral_type` handle has no runtime storage (it is
                // monomorphized away), so it has no address and cannot be
                // borrowed as a place -- e.g. `&u` (slice 2).
                if let Type::PeripheralHandle(t) = &info.ty {
                    let guard = diags.error(
                        format!(
                            "`{name}` is a `{t}` handle; it has no address and cannot be borrowed \
                             -- use `{name}.REG` / `{name}.REG.FIELD` or pass it to a driver"
                        ),
                        "E309",
                        *span,
                    );
                    return PlaceInfo {
                        ty: Type::Error(guard),
                        mut_borrow_error: None,
                        addr_borrow_error: None,
                    };
                }
                if info.moved {
                    diags.error(format!("use of moved value: `{name}`"), "E304", *span);
                }
                return PlaceInfo {
                    ty: info.ty.clone(),
                    mut_borrow_error: (!info.mutable)
                        .then(|| MutBorrowError::ImmutableBinding(name.clone(), *span)),
                    addr_borrow_error: None,
                };
            }

            let ty = check_expr(expr, symbols, scope, fn_name, expected_ret, diags);
            // A `@shared` static reads as `Type::Shared(..)` here; inside a
            // `claim` window `with_claimed` has stripped that wrapper, so the
            // address-of is allowed there (the window's mask covers it). The
            // error propagates through `FieldAccess`/`Index` so `&S.f`/`&S[i]`
            // are caught too.
            let addr_borrow_error = symbols
                .statics
                .get(name)
                .filter(|s| matches!(s.ty, Type::Shared(..)))
                .map(|_| AddrBorrowError::Shared {
                    name: name.clone(),
                    span: *span,
                });
            PlaceInfo {
                ty,
                mut_borrow_error: (!symbols.statics.contains_key(name))
                    .then_some(MutBorrowError::NotMutablePlace(*span)),
                addr_borrow_error,
            }
        }
        Expr::Group(inner) => read_place_info(inner, symbols, scope, fn_name, expected_ret, diags),
        Expr::FieldAccess(base, field) => {
            let base_info = read_place_info(base, symbols, scope, fn_name, expected_ret, diags);
            let mut addr_borrow_error = base_info.addr_borrow_error.clone();
            let ty = if field.0 == "_" {
                let guard = diags.error("padding field `_` is not addressable", "E355", field.1);
                Type::Error(guard)
            } else if let Type::Struct(name, repr, fields) = &base_info.ty {
                fields
                    .iter()
                    .position(|(field_name, _)| field_name == &field.0)
                    .map_or_else(
                        || {
                            let guard = diags.error(
                                format!("struct `{name}` has no field `{}`", field.0),
                                "E318",
                                field.1,
                            );
                            Type::Error(guard)
                        },
                        |idx| {
                            let endian = symbols
                                .structs
                                .get(name)
                                .and_then(|si| si.field_endian.get(idx))
                                .copied()
                                .unwrap_or(ast::FieldEndian::Native);
                            // Reject `&field` only when the field is stored in a
                            // non-native order *for this target*: the address
                            // would point at byte-swapped storage a plain `*T`
                            // read would not swap. A field already in native
                            // order (e.g. `@le` on a little-endian target) is
                            // safe, so it falls through to the packed check.
                            if symbols.target_endianness.swaps(endian) {
                                addr_borrow_error = Some(AddrBorrowError::Endian {
                                    struct_name: name.clone(),
                                    field_name: field.0.clone(),
                                    span: field.1,
                                });
                            } else if *repr == StructRepr::Packed {
                                addr_borrow_error =
                                    packed_field_addr_error(name, fields, idx, field);
                            }
                            fields[idx].1.clone()
                        },
                    )
            } else {
                check_expr(expr, symbols, scope, fn_name, expected_ret, diags)
            };
            PlaceInfo {
                ty,
                mut_borrow_error: base_info.mut_borrow_error,
                addr_borrow_error,
            }
        }
        Expr::Index(base, index) => {
            // Indexed array register as a place (`&P.REG[i]`): it is a mutable
            // MMIO register at base+offset+stride*i, not a container element.
            if let Expr::FieldAccess(p, reg) = base.as_ref()
                && let Expr::Ident((pname, _)) = p.as_ref()
                && let Some(r) = array_reg(pname, &reg.0, scope, symbols)
            {
                check_expr(index, symbols, scope, fn_name, expected_ret, diags);
                check_reg_index_bounds(index, r.array.unwrap().0, pname, &reg.0, diags);
                return PlaceInfo {
                    ty: Type::U32,
                    mut_borrow_error: None,
                    addr_borrow_error: None,
                };
            }
            let base_info = read_place_info(base, symbols, scope, fn_name, expected_ret, diags);
            check_expr(index, symbols, scope, fn_name, expected_ret, diags);
            let ty = index_element_type(base_info.ty.clone(), base, diags);
            let mut_borrow_error = match &base_info.ty {
                Type::Array(_, _) => base_info.mut_borrow_error,
                Type::Ptr(_) => None,
                Type::ConstPtr(_) => Some(MutBorrowError::ConstPtr(base.span())),
                Type::LinearView(_, true)
                | Type::StridedView(_, true, _)
                | Type::RingView(_, true, _)
                | Type::BitView(true) => None,
                Type::LinearView(_, false)
                | Type::StridedView(_, false, _)
                | Type::RingView(_, false, _)
                | Type::BitView(false) => Some(MutBorrowError::ReadonlyView(base.span())),
                _ => None,
            };
            PlaceInfo {
                ty,
                mut_borrow_error,
                addr_borrow_error: base_info.addr_borrow_error,
            }
        }
        Expr::Unary(crate::ast::UnaryOp::Deref, inner) => {
            let ptr_ty = check_expr(inner, symbols, scope, fn_name, expected_ret, diags);
            match &ptr_ty {
                Type::Ptr(pointee) => PlaceInfo {
                    ty: pointee.as_ref().clone(),
                    mut_borrow_error: None,
                    addr_borrow_error: None,
                },
                Type::ConstPtr(pointee) => PlaceInfo {
                    ty: pointee.as_ref().clone(),
                    mut_borrow_error: Some(MutBorrowError::ConstPtr(inner.span())),
                    addr_borrow_error: None,
                },
                _ => {
                    let guard =
                        diags.error("dereference requires pointer type", "E315", inner.span());
                    PlaceInfo {
                        ty: Type::Error(guard),
                        mut_borrow_error: None,
                        addr_borrow_error: None,
                    }
                }
            }
        }
        _ => PlaceInfo {
            ty: check_expr(expr, symbols, scope, fn_name, expected_ret, diags),
            mut_borrow_error: Some(MutBorrowError::NotMutablePlace(expr.span())),
            addr_borrow_error: None,
        },
    }
}

fn packed_field_addr_error(
    struct_name: &str,
    fields: &[(String, Type)],
    idx: usize,
    field: &crate::ast::Ident,
) -> Option<AddrBorrowError> {
    let field_ty = &fields[idx].1;
    let field_align = types::align_of(field_ty);
    let mut field_offset = 0u32;
    for (_, ty) in fields.iter().take(idx) {
        field_offset = field_offset.checked_add(types::element_size(ty))?;
    }
    (field_align > 1).then(|| AddrBorrowError::Packed {
        struct_name: struct_name.to_string(),
        field_name: field.0.clone(),
        field_offset,
        field_align,
        span: field.1,
    })
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
                    // GPIOA.REG = val (or `u.REG` for a param) -- register write
                    if let Some(regs) = peripheral_reg_map(periph_name, scope, symbols) {
                        if let Some(reg) = regs.get(&field.0) {
                            if reg.array.is_some() {
                                diags.error(
                                    format!(
                                        "register `{periph_name}.{0}` is a register array; \
                                         index it: `{0}[i] = ...`",
                                        field.0
                                    ),
                                    "E337",
                                    field.1,
                                );
                            } else if reg.access == crate::ast::Access::ReadOnly {
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
                        // GPIOA.REG.FIELD = val (or `u.REG.FIELD` for a param) -- field write
                        if let Some(regs) = peripheral_reg_map(periph_name, scope, symbols) {
                            if let Some(reg) = regs.get(&reg_field.0) {
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
                // `P.REG[i].FIELD = val` -- field of an indexed array register.
                LValue::Index(arr, idx) => {
                    if let LValue::Field(p, reg) = arr.as_ref()
                        && let LValue::Name((pname, _)) = p.as_ref()
                        && let Some(r) = array_reg(pname, &reg.0, scope, symbols)
                    {
                        check_expr(idx, symbols, scope, fn_name, expected_ret, diags);
                        check_reg_index_bounds(idx, r.array.unwrap().0, pname, &reg.0, diags);
                        if let Some(fs) = r.fields.get(&field.0) {
                            if fs.access == crate::ast::Access::ReadOnly {
                                diags.error(
                                    format!(
                                        "cannot write to readonly field `{pname}.{}.{}`",
                                        reg.0, field.0
                                    ),
                                    "E331",
                                    field.1,
                                );
                            }
                            return fs.ty.clone();
                        }
                        diags.error(
                            format!("register `{}` has no field `{}`", reg.0, field.0),
                            "E322",
                            field.1,
                        );
                        return Type::U32;
                    }
                }
                // `(*p).field` and other non-peripheral bases fall through to the
                // struct-field path below.
                LValue::Deref(_) => {}
            }

            let base_ty = check_lvalue(base, symbols, scope, fn_name, expected_ret, diags, false);
            // Check if it's a struct field write
            if field.0 == "_" {
                let guard = diags.error("padding field `_` is not addressable", "E355", field.1);
                return Type::Error(guard);
            }
            if let Type::Struct(name, _, fields) = &base_ty {
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
            // Indexed register write: `P.REG[i] = v` where REG is an array
            // register. Stays on the volatile MMIO path.
            if let LValue::Field(p, reg) = base.as_ref()
                && let LValue::Name((pname, _)) = p.as_ref()
                && let Some(r) = array_reg(pname, &reg.0, scope, symbols)
            {
                check_expr(index, symbols, scope, fn_name, expected_ret, diags);
                if r.access == crate::ast::Access::ReadOnly {
                    diags.error(
                        format!("cannot write to readonly register `{pname}.{}`", reg.0),
                        "E331",
                        reg.1,
                    );
                }
                check_reg_index_bounds(index, r.array.unwrap().0, pname, &reg.0, diags);
                return Type::U32;
            }
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
             unprotected race. Access the static directly, take the view inside a \
             `claim X { ... }` window, or back the view with non-`@shared` storage."
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
            // Indexed array register as a place (`&P.REG[i]`, `P.REG[i]`): it is
            // a register, not an element of a container.
            if let Expr::FieldAccess(p, reg) = base.as_ref()
                && let Expr::Ident((pname, _)) = p.as_ref()
                && let Some(r) = array_reg(pname, &reg.0, scope, symbols)
            {
                check_expr(index, symbols, scope, fn_name, expected_ret, diags);
                check_reg_index_bounds(index, r.array.unwrap().0, pname, &reg.0, diags);
                return Type::U32;
            }
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

/// E620: a raw pointer into agent-shared memory must not escape the function
/// that derived it. Accesses through such a pointer are lowered `volatile`
/// (the agent mutates the pointee concurrently; a hoisted OWN-bit spin became
/// an infinite branch on real hardware), and the taint that drives that
/// lowering is per-function and syntactic -- an escaped pointer would be
/// dereferenced where the taint is invisible, silently losing the volatile.
/// Escapes: call argument, return value, store into a static or through any
/// non-local lvalue, array/struct literal element, asm input operand, and
/// view/ring/bits descriptor capture (agent views go through `reclaim`).
fn check_agent_ptr_escape(fn_def: &ast::FnDef, symbols: &SymbolTable, diags: &mut DiagnosticBag) {
    let taint = crate::region::agent_ptr_locals(&fn_def.body, symbols);
    let ape = |e: &ast::Expr| crate::region::is_agent_ptr_expr(e, &taint, symbols);
    ape_block(&fn_def.body, &ape, symbols, diags, fn_def.ret.is_some());
}

fn ape_err(diags: &mut DiagnosticBag, span: crate::source::Span, what: &str) {
    let msg = format!(
        "raw pointer into agent-shared memory escapes via {what}: outside the deriving \
             function the agent-pointer taint is invisible and accesses lose the volatile \
             lowering that makes concurrent-agent memory sound. Keep the pointer local; pass \
             the static or an index instead.",
    );
    diags.error(&msg, "E620", span);
}

fn ape_block(
    b: &ast::Block,
    ape: &dyn Fn(&ast::Expr) -> bool,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
    fn_returns: bool,
) {
    for stmt in &b.stmts {
        ape_stmt(stmt, ape, symbols, diags, fn_returns);
    }
    if let Some(t) = &b.trailing {
        if fn_returns && ape(t) {
            ape_err(diags, t.span(), "the function's trailing return value");
        }
        ape_expr(t, ape, symbols, diags, fn_returns);
    }
}

// Exhaustive Stmt walker (no catch-all; see hacking.md Code conventions).
fn ape_stmt(
    stmt: &ast::Stmt,
    ape: &dyn Fn(&ast::Expr) -> bool,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
    fn_returns: bool,
) {
    match stmt {
        ast::Stmt::VarDecl(v) => ape_expr(&v.init, ape, symbols, diags, fn_returns),
        ast::Stmt::Assign(a) => {
            if ape(&a.value) {
                match &a.target {
                    ast::LValue::Name((n, _)) => {
                        if symbols.statics.contains_key(n) {
                            ape_err(diags, a.value.span(), "a store into a static");
                        }
                    }
                    ast::LValue::Field(..) | ast::LValue::Index(..) | ast::LValue::Deref(_) => {
                        ape_err(diags, a.value.span(), "a store through memory");
                    }
                }
            }
            ape_expr(&a.value, ape, symbols, diags, fn_returns);
        }
        ast::Stmt::CompoundAssign(c) => ape_expr(&c.value, ape, symbols, diags, fn_returns),
        ast::Stmt::Expr(e) => ape_expr(e, ape, symbols, diags, fn_returns),
        ast::Stmt::If(i) => {
            ape_expr(&i.cond, ape, symbols, diags, fn_returns);
            ape_block(&i.then_block, ape, symbols, diags, fn_returns);
            if let Some(e) = &i.else_branch {
                ape_stmt(e, ape, symbols, diags, fn_returns);
            }
        }
        ast::Stmt::Loop(l) => ape_block(&l.body, ape, symbols, diags, fn_returns),
        ast::Stmt::While(w) => {
            ape_expr(&w.cond, ape, symbols, diags, fn_returns);
            ape_block(&w.body, ape, symbols, diags, fn_returns);
        }
        ast::Stmt::For(f) => {
            ape_expr(&f.start, ape, symbols, diags, fn_returns);
            ape_expr(&f.end, ape, symbols, diags, fn_returns);
            if let Some(st) = &f.step {
                ape_expr(st, ape, symbols, diags, fn_returns);
            }
            ape_block(&f.body, ape, symbols, diags, fn_returns);
        }
        ast::Stmt::Return(r) => {
            if let Some(v) = &r.value {
                if ape(v) {
                    ape_err(diags, v.span(), "`return`");
                }
                ape_expr(v, ape, symbols, diags, fn_returns);
            }
        }
        ast::Stmt::Break(_) | ast::Stmt::Continue(_) => {}
        ast::Stmt::Block(b) => ape_block(b, ape, symbols, diags, fn_returns),
        ast::Stmt::Match(m) => {
            ape_expr(&m.scrutinee, ape, symbols, diags, fn_returns);
            for arm in &m.arms {
                ape_block(&arm.body, ape, symbols, diags, fn_returns);
            }
        }
        ast::Stmt::Asm(a) => {
            for (_, e) in &a.inputs {
                if ape(e) {
                    ape_err(diags, e.span(), "an asm input operand");
                }
                ape_expr(e, ape, symbols, diags, fn_returns);
            }
        }
        ast::Stmt::Assume(a) => ape_expr(&a.cond, ape, symbols, diags, fn_returns),
        ast::Stmt::Assert(a) => ape_expr(&a.cond, ape, symbols, diags, fn_returns),
        ast::Stmt::Claim(c) => ape_block(&c.body, ape, symbols, diags, fn_returns),
    }
}

// Exhaustive Expr walker (no catch-all): flags escapes at call arguments,
// literal elements, and descriptor captures; recurses everywhere else.
fn ape_expr(
    e: &ast::Expr,
    ape: &dyn Fn(&ast::Expr) -> bool,
    symbols: &SymbolTable,
    diags: &mut DiagnosticBag,
    fn_returns: bool,
) {
    match e {
        ast::Expr::IntLiteral(..)
        | ast::Expr::FloatLiteral(..)
        | ast::Expr::BoolLiteral(..)
        | ast::Expr::StringLiteral(..)
        | ast::Expr::NullLiteral(..)
        | ast::Expr::Ident(_)
        | ast::Expr::SizeOf(..)
        | ast::Expr::EnumVariant { .. } => {}
        ast::Expr::Unary(_, i) | ast::Expr::Group(i) | ast::Expr::Cast(i, _) => {
            ape_expr(i, ape, symbols, diags, fn_returns);
        }
        ast::Expr::Binary(l, _, r) => {
            ape_expr(l, ape, symbols, diags, fn_returns);
            ape_expr(r, ape, symbols, diags, fn_returns);
        }
        ast::Expr::Call(callee, args) => {
            for a in args {
                if ape(a) {
                    ape_err(diags, a.span(), "a call argument");
                }
                ape_expr(a, ape, symbols, diags, fn_returns);
            }
            ape_expr(callee, ape, symbols, diags, fn_returns);
        }
        ast::Expr::FieldAccess(b, _) => ape_expr(b, ape, symbols, diags, fn_returns),
        ast::Expr::Index(b, i) => {
            ape_expr(b, ape, symbols, diags, fn_returns);
            ape_expr(i, ape, symbols, diags, fn_returns);
        }
        ast::Expr::ViewNew {
            base, len, stride, ..
        } => {
            if ape(base) {
                ape_err(diags, base.span(), "a view descriptor capture");
            }
            ape_expr(base, ape, symbols, diags, fn_returns);
            if let Some(l) = len {
                ape_expr(l, ape, symbols, diags, fn_returns);
            }
            if let Some(st) = stride {
                ape_expr(st, ape, symbols, diags, fn_returns);
            }
        }
        ast::Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => {
            if ape(base) {
                ape_err(diags, base.span(), "a ring descriptor capture");
            }
            ape_expr(base, ape, symbols, diags, fn_returns);
            if let Some(c) = capacity {
                ape_expr(c, ape, symbols, diags, fn_returns);
            }
            ape_expr(head, ape, symbols, diags, fn_returns);
            ape_expr(len, ape, symbols, diags, fn_returns);
        }
        ast::Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => {
            if ape(base) {
                ape_err(diags, base.span(), "a bits descriptor capture");
            }
            ape_expr(base, ape, symbols, diags, fn_returns);
            if let Some(o) = bit_offset {
                ape_expr(o, ape, symbols, diags, fn_returns);
            }
            if let Some(l) = len_bits {
                ape_expr(l, ape, symbols, diags, fn_returns);
            }
        }
        ast::Expr::ArrayInit(elems, _) => {
            for el in elems {
                if ape(el) {
                    ape_err(diags, el.span(), "an array literal element");
                }
                ape_expr(el, ape, symbols, diags, fn_returns);
            }
        }
        ast::Expr::StructInit { fields, .. } => {
            for (_, v) in fields {
                if ape(v) {
                    ape_err(diags, v.span(), "a struct literal field");
                }
                ape_expr(v, ape, symbols, diags, fn_returns);
            }
        }
        ast::Expr::Match(m) => {
            ape_expr(&m.scrutinee, ape, symbols, diags, fn_returns);
            for arm in &m.arms {
                ape_block(&arm.body, ape, symbols, diags, fn_returns);
            }
        }
        ast::Expr::Block(b) => ape_block(&b.block, ape, symbols, diags, fn_returns),
        ast::Expr::If(i) => {
            ape_expr(&i.cond, ape, symbols, diags, fn_returns);
            ape_block(&i.then_block, ape, symbols, diags, fn_returns);
            ape_expr(&i.else_branch, ape, symbols, diags, fn_returns);
        }
    }
}
