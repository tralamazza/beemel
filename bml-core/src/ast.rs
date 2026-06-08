use std::fmt;

use crate::source::Span;

pub type Ident = (String, Span);

#[derive(Debug, Clone)]
pub struct Program {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub enum Item {
    FnDef(FnDef),
    ExternFnDef(ExternFnDef),
    StaticDef(StaticDef),
    ConstDef(ConstDef),
    PeripheralDef(PeripheralDef),
    Import(ImportStmt),
    Export(ExportStmt),
    StructDef(StructDef),
    EnumDef(EnumDef),
    /// `comptime_assert(cond);` -- a compile-time assertion on a const
    /// condition (e.g. `sizeof(GPIO) == 0x28`). Produces no runtime code.
    ComptimeAssert(ComptimeAssert),
    /// `owns P, P.R, ...;` -- this module's exclusive claim over peripheral
    /// registers. Checked by the region pass against the peripheral table and
    /// across modules. See `doc/regions-agents-plan.md`.
    Owns(OwnsStmt),
}

#[derive(Debug, Clone)]
pub struct OwnsStmt {
    pub paths: Vec<OwnsPath>,
}

/// One ownership claim: a whole peripheral (`register` is `None`) or a single
/// register (`register` is `Some`). Field-level ownership is not yet supported.
/// `span` covers the path and carries the source `FileId`, which is how the
/// region pass attributes the claim to a module (two files owning the same
/// register is the cross-module conflict).
#[derive(Debug, Clone)]
pub struct OwnsPath {
    pub peripheral: Ident,
    pub register: Option<Ident>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ComptimeAssert {
    pub cond: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FnDef {
    pub name: Ident,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub context: ContextExpr,
    pub isr: Option<IsrAnnotation>,
    pub naked: bool,
    pub section: Option<String>,
    pub body: Block,
}

/// `extern fn` declaration -- no body, optional annotations.
/// Absent annotations means callable from any context.
#[derive(Debug, Clone)]
pub struct ExternFnDef {
    pub name: Ident,
    pub params: Vec<Param>,
    pub ret: Option<TypeExpr>,
    pub context: Option<ContextExpr>,
    pub isr: Option<IsrAnnotation>,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: Ident,
    pub ty: TypeExpr,
}

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub trailing: Option<Box<Expr>>,
    pub span: Span,
}

impl Block {
    /// True if any top-level statement directly terminates the basic block
    /// (`return`, `break`, `continue`, or a nested block that does so).
    /// Mirrors what the IR emitter treats as a terminator inside `emit_block`.
    #[must_use]
    pub fn has_direct_terminator(&self) -> bool {
        self.stmts.iter().any(Stmt::is_direct_terminator)
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    VarDecl(VarDecl),
    Assign(AssignStmt),
    CompoundAssign(CompoundAssignStmt),
    Expr(Expr),
    If(IfStmt),
    Loop(LoopStmt),
    While(WhileStmt),
    For(Box<ForStmt>),
    Return(ReturnStmt),
    Break(Span),
    Continue(Span),
    Block(Block),
    Match(MatchStmt),
    Asm(AsmStmt),
    Assume(AssumeStmt),
    Assert(AssertStmt),
}

impl Stmt {
    /// True if this statement directly terminates its enclosing basic block.
    /// Only counts `return`/`break`/`continue` at this level, or such a
    /// statement inside an unconditional nested block. Does not recurse into
    /// `if`/`loop`/`while`/`for`/`match` arms, since those don't unconditionally
    /// terminate the surrounding block.
    #[must_use]
    pub fn is_direct_terminator(&self) -> bool {
        match self {
            Stmt::Return(_) | Stmt::Break(_) | Stmt::Continue(_) => true,
            Stmt::Block(inner) => inner.has_direct_terminator(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VarDecl {
    pub mutable: bool, // true = var, false = val
    pub name: Ident,
    pub ty_ann: Option<TypeExpr>,
    pub init: Expr,
}

#[derive(Debug, Clone)]
pub struct AssignStmt {
    pub target: LValue,
    pub value: Expr,
}

/// `target OP= value` (e.g. `x += 1`). Lowered as a single-evaluation
/// read-modify-write so a peripheral-field target is read once (volatile) and a
/// side-effecting index/deref in `target` is evaluated once.
#[derive(Debug, Clone)]
pub struct CompoundAssignStmt {
    pub target: LValue,
    pub op: BinaryOp,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum LValue {
    Name(Ident),
    Field(Box<LValue>, Ident),
    Index(Box<LValue>, Box<Expr>),
    Deref(Box<Expr>),
}

impl LValue {
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            LValue::Name((_, s)) => *s,
            LValue::Field(base, (_, s)) => base.span().merge(*s),
            LValue::Index(base, index) => base.span().merge(index.span()),
            LValue::Deref(inner) => inner.span(),
        }
    }

    /// Reconstruct the read expression for this place (the inverse of
    /// `expr_to_lvalue`). Used to type-check and lower compound assignment.
    #[must_use]
    pub fn to_expr(&self) -> Expr {
        match self {
            LValue::Name(id) => Expr::Ident(id.clone()),
            LValue::Field(base, field) => {
                Expr::FieldAccess(Box::new(base.to_expr()), field.clone())
            }
            LValue::Index(base, index) => Expr::Index(Box::new(base.to_expr()), index.clone()),
            LValue::Deref(inner) => Expr::Unary(UnaryOp::Deref, inner.clone()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IfStmt {
    pub cond: Expr,
    pub then_block: Block,
    pub else_branch: Option<Box<Stmt>>,
}

#[derive(Debug, Clone)]
pub struct LoopStmt {
    pub body: Block,
}

#[derive(Debug, Clone)]
pub struct WhileStmt {
    pub cond: Expr,
    pub body: Block,
}

#[derive(Debug, Clone)]
pub struct ReturnStmt {
    pub value: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct ForStmt {
    pub var: Ident,
    pub ty: TypeExpr,
    pub start: Expr,
    pub direction: ForDirection,
    pub end: Expr,
    pub step: Option<Expr>,
    pub body: Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForDirection {
    Upto,
    Downto,
}

#[derive(Debug, Clone)]
pub struct AssumeStmt {
    pub cond: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AssertStmt {
    pub cond: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct AsmStmt {
    pub asm_text: String,
    /// Output operands: `(constraint, target)`, where the constraint starts with
    /// `=` (e.g. `"=r"`) and the target is an lvalue the result is stored into.
    /// In the template, operands are referenced as `$0`, `$1`, ... (outputs
    /// first, then inputs), per LLVM inline-asm numbering.
    pub outputs: Vec<(String, Expr)>,
    /// Input operands: `(constraint, value)` (e.g. `"r"`).
    pub inputs: Vec<(String, Expr)>,
    /// Clobbers, e.g. `"memory"`, `"cc"`, `"r0"` (lowered to `~{...}`).
    pub clobbers: Vec<String>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloatSuffix {
    None,
    H,
    F,
    D,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntSuffix {
    None,
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

#[derive(Debug, Clone)]
pub enum Expr {
    IntLiteral(u64, IntSuffix, Span),
    FloatLiteral(f64, FloatSuffix, Span),
    BoolLiteral(bool, Span),
    StringLiteral(String, Span),
    NullLiteral(Span),
    Ident(Ident),
    Unary(UnaryOp, Box<Expr>),
    Binary(Box<Expr>, BinaryOp, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    FieldAccess(Box<Expr>, Ident),
    Index(Box<Expr>, Box<Expr>),
    Group(Box<Expr>),
    Cast(Box<Expr>, TypeExpr),
    SizeOf(TypeExpr, Span),
    /// Readonly linear view constructor. Two forms:
    /// - `view(ptr, len)`: `base` is a pointer, `len` is `Some`; the element
    ///   type is inferred from the pointee.
    /// - `view(arr)`: `base` is an array, `len` is `None`; both the element
    ///   type and a compile-known length come from the array type.
    /// - `view(arr, stride K)`: `base` is an array, `stride` is `Some(K)` (a
    ///   compile-time element multiplier); the logical length is `N/K`. The
    ///   element type comes from the array. `len` and `stride` are never both
    ///   `Some` in v1 (the dynamic pointer+stride form is deferred).
    ViewNew {
        base: Box<Expr>,
        len: Option<Box<Expr>>,
        stride: Option<Box<Expr>>,
        span: Span,
    },
    /// Ring view constructor. Two forms:
    /// - `ring(arr, head, len)`: `base` is an array, `capacity` is `None` and
    ///   taken from the array type; element type comes from the array.
    /// - `ring(ptr, capacity, head, len)`: `base` is a pointer, `capacity` is
    ///   `Some`; element type comes from the pointee.
    RingNew {
        base: Box<Expr>,
        capacity: Option<Box<Expr>>,
        head: Box<Expr>,
        len: Box<Expr>,
        span: Span,
    },
    /// Bit view constructor. Two forms:
    /// - `bits(arr)`: `base` is a `[u8; N]`/`[b8; N]` array; `bit_offset` and
    ///   `len_bits` are `None` (offset 0, length `N*8` taken from the array).
    /// - `bits(ptr, bit_offset, len_bits)`: `base` is a byte pointer; the other
    ///   two are `Some`. Mutability follows the pointer's constness.
    BitNew {
        base: Box<Expr>,
        bit_offset: Option<Box<Expr>>,
        len_bits: Option<Box<Expr>>,
        span: Span,
    },
    EnumVariant {
        enum_name: Ident,
        variant: Ident,
        span: Span,
    },
    ArrayInit(Vec<Expr>, Span),
    StructInit {
        name: Ident,
        fields: Vec<(Ident, Expr)>,
        span: Span,
    },
    Match(Box<MatchExpr>),
    Block(BlockExpr),
    If(Box<IfExpr>),
}

impl Expr {
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Expr::IntLiteral(_, _, s) => *s,
            Expr::FloatLiteral(_, _, s) => *s,
            Expr::BoolLiteral(_, s) => *s,
            Expr::StringLiteral(_, s) => *s,
            Expr::NullLiteral(s) => *s,
            Expr::Ident((_, s)) => *s,
            Expr::Unary(_, e) => e.span(),
            Expr::Binary(l, _, r) => l.span().merge(r.span()),
            Expr::Call(f, _) => f.span(),
            Expr::FieldAccess(e, (_, s)) => e.span().merge(*s),
            Expr::Index(e, _) => e.span(),
            Expr::Group(e) => e.span(),
            Expr::Cast(e, _) => e.span(),
            Expr::SizeOf(_, s) => *s,
            Expr::ViewNew { span, .. } => *span,
            Expr::RingNew { span, .. } => *span,
            Expr::BitNew { span, .. } => *span,
            Expr::EnumVariant { span, .. } => *span,
            Expr::ArrayInit(_, s) => *s,
            Expr::StructInit { span, .. } => *span,
            Expr::Match(m) => m.span,
            Expr::Block(b) => b.span,
            Expr::If(i) => i.span,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
    BitNot,
    Deref,
    AddrOf,
    AddrOfMut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
}

impl BinaryOp {
    #[must_use]
    pub fn precedence(self) -> u8 {
        match self {
            BinaryOp::Or => 1,
            BinaryOp::And => 2,
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::LtEq
            | BinaryOp::GtEq => 3,
            BinaryOp::BitOr => 4,
            BinaryOp::BitXor => 5,
            BinaryOp::BitAnd => 6,
            BinaryOp::Shl | BinaryOp::Shr => 7,
            BinaryOp::Add | BinaryOp::Sub => 8,
            BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 9,
        }
    }
}

#[derive(Debug, Clone)]
pub enum TypeExpr {
    Named(Ident),
    Ptr(Box<TypeExpr>),
    ConstPtr(Box<TypeExpr>),
    /// Linear view type. The `bool` is `mutable`: `view T` is readonly (Copy),
    /// `view mut T` is mutable (Move) and allows index writes.
    View(Box<TypeExpr>, bool),
    /// Strided linear view type: `view T stride K` / `view mut T stride K`. The
    /// `bool` is `mutable` (like `View`); the `Expr` is the compile-time stride
    /// `K` (in elements: logical element `i` lives at backing element `i*K`).
    /// The stride is part of type identity, not a runtime descriptor field.
    StridedView(Box<TypeExpr>, bool, Box<Expr>),
    /// Ring view type. The `bool` is `mutable`, like `View`.
    Ring(Box<TypeExpr>, bool),
    /// Bit view type. Carries no element type (the element is always `b1`). The
    /// `bool` is `mutable`, like `View`.
    Bits(bool),
    Array(Box<TypeExpr>, Box<Expr>),
    Fn(Vec<TypeExpr>, Option<Box<TypeExpr>>),
    /// `addr in <region>` -- a byte-address slot constrained to a region (an
    /// in-memory handoff field). Layout-identical to `u32`; not a typed pointer.
    /// The `Ident` is the region name, resolved against the target. See
    /// `doc/regions-agents-plan.md`.
    Addr(Ident),
    Void(Span),
}

impl TypeExpr {
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named((_, s)) => *s,
            TypeExpr::Ptr(inner)
            | TypeExpr::ConstPtr(inner)
            | TypeExpr::View(inner, _)
            | TypeExpr::StridedView(inner, _, _)
            | TypeExpr::Ring(inner, _) => inner.span(),
            TypeExpr::Array(inner, _) => inner.span(),
            TypeExpr::Bits(_) | TypeExpr::Fn(_, _) => Span::empty(crate::source::FileId::new(), 0),
            TypeExpr::Addr((_, s)) => *s,
            TypeExpr::Void(s) => *s,
        }
    }
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExpr::Named((name, _)) => write!(f, "{name}"),
            TypeExpr::Ptr(t) => write!(f, "*{t}"),
            TypeExpr::ConstPtr(t) => write!(f, "&{t}"),
            TypeExpr::View(t, true) => write!(f, "view mut {t}"),
            TypeExpr::View(t, false) => write!(f, "view {t}"),
            TypeExpr::StridedView(t, true, _) => write!(f, "view mut {t} stride"),
            TypeExpr::StridedView(t, false, _) => write!(f, "view {t} stride"),
            TypeExpr::Ring(t, true) => write!(f, "ring mut {t}"),
            TypeExpr::Ring(t, false) => write!(f, "ring {t}"),
            TypeExpr::Bits(true) => write!(f, "bits mut"),
            TypeExpr::Bits(false) => write!(f, "bits"),
            TypeExpr::Array(t, _) => write!(f, "[{t}]"),
            TypeExpr::Fn(params, ret) => {
                let p: Vec<String> = params.iter().map(ToString::to_string).collect();
                match ret {
                    Some(r) => write!(f, "fn({}) -> {r}", p.join(", ")),
                    None => write!(f, "fn({})", p.join(", ")),
                }
            }
            TypeExpr::Addr((region, _)) => write!(f, "addr in {region}"),
            TypeExpr::Void(_) => write!(f, "void"),
        }
    }
}

#[derive(Debug, Clone)]
pub enum ContextExpr {
    Thread,
    Any,
}

#[derive(Debug, Clone)]
pub struct IsrAnnotation {
    pub label: Option<String>,
    pub priority: u8,
    pub tailchain: bool,
}

#[derive(Debug, Clone)]
pub struct StaticDef {
    pub name: Ident,
    pub ty: TypeExpr,
    pub storage: Vec<StorageAnnotation>,
    /// `in <region>` placement: the name of a `[region.*]` from the target
    /// file. The static is emitted into the `.region.<name>` section, which the
    /// generated linker script places in that region's mem block. Validated by
    /// the region pass against the target.
    pub region: Option<Ident>,
    pub init: Option<Expr>,
}

#[derive(Debug, Clone)]
pub struct ConstDef {
    pub name: Ident,
    pub ty: TypeExpr,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub struct PeripheralDef {
    pub name: Ident,
    pub base_addr: u64,
    pub regs: Vec<RegDef>,
}

#[derive(Debug, Clone)]
pub struct RegDef {
    pub name: Ident,
    pub offset: u64,
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Clone)]
pub struct FieldDef {
    pub name: Ident,
    pub ty: TypeExpr,
    pub bit_spec: BitSpec,
    pub access: Option<Access>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    ReadWrite,
    ReadOnly,
    WriteOnly,
}

#[derive(Debug, Clone)]
pub enum BitSpec {
    Single(usize),
    Range(usize, usize),
}

#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: Ident,
    pub repr: StructRepr,
    pub fields: Vec<StructFieldDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructRepr {
    Explicit,
    C,
    Packed,
}

#[derive(Debug, Clone)]
pub struct StructFieldDef {
    pub name: Ident,
    pub ty: TypeExpr,
    /// Stored byte order for this field. `Native`/`Little` store in the target's
    /// native order (little-endian); `Big` byte-swaps at every field load/store
    /// so the in-memory bytes are big-endian while the value the program sees
    /// stays native. Endianness is a storage property, not part of the value
    /// type: `s.field` still has the plain integer type.
    pub endian: FieldEndian,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldEndian {
    /// No endianness attribute; stored in native (little-endian) order.
    Native,
    /// `@be`: stored big-endian, byte-swapped on load/store.
    Big,
    /// `@le`: explicitly little-endian. A no-op on the little-endian target,
    /// accepted for documentation and symmetry.
    Little,
}

#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: Ident,
    pub ty: TypeExpr,
    pub variants: Vec<EnumVariantDef>,
}

#[derive(Debug, Clone)]
pub struct EnumVariantDef {
    pub name: Ident,
    pub value: Option<u64>,
}

#[derive(Debug, Clone)]
pub enum StorageAnnotation {
    Exclusive(Ident),
    Shared(u8),
    Dma,
    External,
    Section(String),
    /// `@align(N)` -- minimum byte alignment of the static (a power of two).
    Align(u32),
}

#[derive(Debug, Clone)]
pub struct ImportStmt {
    pub module: Vec<Ident>,
    pub imports: ImportKind,
    pub alias: Option<Ident>,
}

#[derive(Debug, Clone)]
pub enum ImportKind {
    All,
    Selective(Vec<Ident>),
}

#[derive(Debug, Clone)]
pub struct ExportStmt {
    pub names: Vec<ExportItem>,
}

#[derive(Debug, Clone)]
pub enum ExportItem {
    Fn(Ident),
    Static(Ident),
    Const(Ident),
    Peripheral(Ident),
    Struct(Ident),
    Enum(Ident),
}

#[derive(Debug, Clone)]
pub struct MatchStmt {
    pub scrutinee: Expr,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub patterns: Vec<MatchPattern>,
    pub body: Block,
}

#[derive(Debug, Clone)]
pub struct MatchExpr {
    pub scrutinee: Expr,
    pub arms: Vec<MatchArm>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BlockExpr {
    pub block: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IfExpr {
    pub cond: Expr,
    pub then_block: Block,
    pub else_branch: Box<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum MatchPattern {
    /// `Enum@Variant` (enum scrutinee).
    Variant(Ident, Ident),
    /// A single integer value (integer scrutinee), e.g. `5` or `-1`.
    Int(i128, Span),
    /// An inclusive integer range `lo..hi` (integer scrutinee).
    Range(i128, i128, Span),
    Wildcard(Span),
}

impl MatchPattern {
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            MatchPattern::Variant((_, s), (_, _)) => *s,
            MatchPattern::Int(_, s) | MatchPattern::Range(_, _, s) | MatchPattern::Wildcard(s) => {
                *s
            }
        }
    }
}
