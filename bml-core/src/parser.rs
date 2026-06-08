#[allow(clippy::wildcard_imports)]
use crate::ast::*;
use crate::errors::DiagnosticBag;
use crate::lexer::{Lexer, Token, TokenKind};
use crate::source::{FileId, Span};

pub struct Parser<'a> {
    tokens: Vec<Token>,
    pos: usize,
    diags: &'a mut DiagnosticBag,
    file: FileId,
    trailing_expr: Option<Expr>,
    /// Current recursion depth across the mutually-recursive expression, type,
    /// and block parsers. Bounds stack growth: a deeply nested input would
    /// otherwise overflow the stack and abort the process (recursive descent
    /// has no natural limit). See `MAX_PARSE_DEPTH` / `guarded`.
    depth: u32,
    /// Emit the `E113` nesting diagnostic at most once per parse.
    depth_error_emitted: bool,
}

/// Maximum nesting depth for expressions, types, and blocks combined. Chosen to
/// trip well before the stack overflows (parser frames are large, ~30 KB, so
/// the real overflow is only a few hundred deep) while staying far above
/// anything hand-written code reaches. Hitting it yields `E113`, not a crash.
const MAX_PARSE_DEPTH: u32 = 128;

/// Precedence of the `as` cast operator. Sits below the prefix unary operators
/// (so `&x as T` parses as `(&x) as T`, matching Rust) and above every binary
/// operator (so `x as u32 + 1` is `(x as u32) + 1`). Binary precedences run
/// 1..=9; `as` is one step above the top binary level.
const CAST_PREC: u8 = 10;

/// Precedence at which a prefix unary operator (`-`, `!`, `~`, `*`, `&`) parses
/// its operand. One above `CAST_PREC` so the operand stops *before* a trailing
/// `as` instead of absorbing it (which would regroup `&x as T` into
/// `&(x as T)`); still above all binary operators, so e.g. `-a + b` stays
/// `(-a) + b`.
const PREFIX_OPERAND_PREC: u8 = 11;

/// The shared head of a function declaration, returned by `parse_fn_signature`.
/// `fn` consumes all fields; `extern fn` ignores `naked`/`section`.
struct FnSignature {
    name: Ident,
    params: Vec<Param>,
    ret: Option<TypeExpr>,
    context: ContextExpr,
    isr: Option<IsrAnnotation>,
    naked: bool,
    section: Option<String>,
}

impl<'a> Parser<'a> {
    pub fn new(source: &'a str, file: FileId, diags: &'a mut DiagnosticBag) -> Self {
        let mut lexer = Lexer::new(source, file, diags);
        let mut tokens = Vec::new();
        loop {
            let tok = lexer.next_token();
            let is_eof = tok.kind == TokenKind::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        Parser {
            tokens,
            pos: 0,
            diags,
            file,
            trailing_expr: None,
            depth: 0,
            depth_error_emitted: false,
        }
    }

    /// Run `f` one recursion level deeper, bounding total nesting. When the
    /// limit is exceeded it emits `E113` once and returns `None` instead of
    /// recursing further, so pathological input fails loudly with a diagnostic
    /// rather than overflowing the stack. The depth is decremented on every
    /// path, so sibling constructs are unaffected.
    fn guarded<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            if !self.depth_error_emitted {
                self.depth_error_emitted = true;
                self.diags.error(
                    "nesting too deep (expression, type, or block)",
                    "E113",
                    self.peek_span(),
                );
            }
            self.depth -= 1;
            return None;
        }
        let r = f(self);
        self.depth -= 1;
        r
    }

    pub fn parse_program(&mut self) -> Program {
        let mut items = Vec::new();
        while !self.is_eof() {
            match self.parse_item() {
                Some(item) => items.push(item),
                None => {
                    // Skip to next recoverable point
                    self.skip_to_next_item();
                }
            }
        }
        Program { items }
    }

    // --- helpers ---

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn peek_span(&self) -> Span {
        self.peek().span
    }

    fn is_eof(&self) -> bool {
        self.peek_kind() == &TokenKind::Eof
    }

    fn advance(&mut self) -> &Token {
        let t = &self.tokens[self.pos];
        self.pos += 1;
        t
    }

    fn expect(&mut self, kind: &TokenKind, msg: &str) -> Result<(), ()> {
        if kind == self.peek_kind() {
            self.advance();
            Ok(())
        } else {
            self.diags.error(msg, "E100", self.peek_span());
            Err(())
        }
    }

    fn check(&self, kind: &TokenKind) -> bool {
        self.peek_kind() == kind
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if self.check(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn skip_to_next_item(&mut self) {
        while !self.is_eof() {
            match self.peek_kind() {
                TokenKind::Extern
                | TokenKind::Fn
                | TokenKind::Var
                | TokenKind::Const
                | TokenKind::Peripheral
                | TokenKind::Import
                | TokenKind::Export
                | TokenKind::Owns
                | TokenKind::Struct
                | TokenKind::Enum => return,
                TokenKind::RBrace => return,
                _ => {
                    self.advance();
                }
            }
        }
    }

    fn skip_to_semicolon_or_brace(&mut self) {
        while !self.is_eof() {
            match self.peek_kind() {
                TokenKind::Semicolon => {
                    self.advance();
                    return;
                }
                TokenKind::RBrace => {
                    return;
                }
                _ => {
                    self.advance();
                }
            }
        }
    }

    // --- items ---

    fn parse_item(&mut self) -> Option<Item> {
        match self.peek_kind() {
            TokenKind::Extern => self.parse_extern_fn_def().map(Item::ExternFnDef),
            TokenKind::Fn => self.parse_fn_def().map(Item::FnDef),
            TokenKind::Var => self.parse_static_def().map(Item::StaticDef),
            TokenKind::Const => self.parse_const_def().map(Item::ConstDef),
            TokenKind::Peripheral => self.parse_peripheral_def().map(Item::PeripheralDef),
            TokenKind::Import => self.parse_import().map(Item::Import),
            TokenKind::Export => self.parse_export().map(Item::Export),
            TokenKind::Owns => self.parse_owns().map(Item::Owns),
            TokenKind::Struct => self.parse_struct_def().map(Item::StructDef),
            TokenKind::Enum => self.parse_enum_def().map(Item::EnumDef),
            TokenKind::ComptimeAssert => {
                let (cond, span) = self.parse_paren_cond("comptime_assert")?;
                Some(Item::ComptimeAssert(ComptimeAssert { cond, span }))
            }
            _ => {
                self.diags.error(
                    format!("expected item, found `{:?}`", self.peek_kind()),
                    "E101",
                    self.peek_span(),
                );
                self.advance();
                None
            }
        }
    }

    /// Parse `( expr ) ;` for a keyword form whose keyword has been peeked but
    /// not yet consumed (`assume`, `assert`, `comptime_assert`). Captures the
    /// keyword span, advances past it, and returns `(cond, span)`.
    fn parse_paren_cond(&mut self, kw: &str) -> Option<(Expr, Span)> {
        let span = self.peek_span();
        self.advance();
        self.expect(&TokenKind::LParen, &format!("expected `(` after `{kw}`"))
            .ok()?;
        let cond = self.parse_expr()?;
        self.expect(&TokenKind::RParen, "expected `)`").ok()?;
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some((cond, span))
    }

    /// Parse the common head of a function declaration: name, `(params)`, an
    /// optional `-> ret`, and the trailing annotations. Shared by `fn` and
    /// `extern fn`, which diverge only after this (a body vs. a `;`).
    fn parse_fn_signature(&mut self) -> Option<FnSignature> {
        let name = self.parse_ident()?;

        self.expect(&TokenKind::LParen, "expected `(` after function name")
            .ok()?;

        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            while let Some(p) = self.parse_param() {
                params.push(p);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }

        self.expect(&TokenKind::RParen, "expected `)` after parameters")
            .ok()?;

        let ret = if self.eat(&TokenKind::Arrow) {
            self.parse_type_expr()
        } else {
            None
        };

        let (context, isr, naked, section) = self.parse_fn_annotations()?;

        Some(FnSignature {
            name,
            params,
            ret,
            context,
            isr,
            naked,
            section,
        })
    }

    fn parse_extern_fn_def(&mut self) -> Option<ExternFnDef> {
        use crate::ast::ExternFnDef;
        self.advance(); // extern

        self.expect(&TokenKind::Fn, "expected `fn` after `extern`")
            .ok()?;

        // `naked`/`section` are accepted but meaningless on an extern decl.
        let sig = self.parse_fn_signature()?;

        self.expect(
            &TokenKind::Semicolon,
            "expected `;` after extern fn declaration",
        )
        .ok()?;

        Some(ExternFnDef {
            name: sig.name,
            params: sig.params,
            ret: sig.ret,
            context: Some(sig.context),
            isr: sig.isr,
        })
    }

    fn parse_fn_def(&mut self) -> Option<FnDef> {
        self.advance(); // fn

        let sig = self.parse_fn_signature()?;

        let body = self.parse_block()?;

        Some(FnDef {
            name: sig.name,
            params: sig.params,
            ret: sig.ret,
            context: sig.context,
            isr: sig.isr,
            naked: sig.naked,
            section: sig.section,
            body,
        })
    }

    fn parse_param(&mut self) -> Option<Param> {
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Colon, "expected `:` after parameter name")
            .ok()?;
        let ty = self.parse_type_expr()?;
        Some(Param { name, ty })
    }

    fn parse_fn_annotations(
        &mut self,
    ) -> Option<(ContextExpr, Option<IsrAnnotation>, bool, Option<String>)> {
        use crate::ast::IsrAnnotation;

        let mut context = ContextExpr::Any;
        let mut isr: Option<IsrAnnotation> = None;
        let mut naked = false;
        let mut section: Option<String> = None;

        while self.eat(&TokenKind::AtSign) {
            match self.peek_kind() {
                TokenKind::Context => {
                    if !matches!(context, ContextExpr::Any) {
                        self.diags
                            .error("duplicate @context annotation", "E108", self.peek_span());
                        return None;
                    }
                    self.advance();
                    self.expect(&TokenKind::LParen, "expected `(`").ok()?;
                    self.expect(&TokenKind::Thread, "expected `thread`").ok()?;
                    self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                    context = ContextExpr::Thread;
                }
                TokenKind::Isr => {
                    if isr.is_some() {
                        self.diags
                            .error("duplicate @isr annotation", "E108", self.peek_span());
                        return None;
                    }
                    self.advance();
                    self.expect(&TokenKind::LParen, "expected `(`").ok()?;
                    let mut label: Option<String> = None;
                    let mut priority: u8 = 0;
                    let mut tailchain = false;

                    loop {
                        match self.peek_kind() {
                            TokenKind::StringLiteral(s) => {
                                if label.is_some() {
                                    self.diags.error(
                                        "duplicate label in @isr",
                                        "E108",
                                        self.peek_span(),
                                    );
                                    return None;
                                }
                                label = Some(s.clone());
                                self.advance();
                            }
                            TokenKind::Priority => {
                                priority = self.parse_isr_priority()?;
                            }
                            TokenKind::Tailchain => {
                                if tailchain {
                                    self.diags.error(
                                        "duplicate tailchain in @isr",
                                        "E108",
                                        self.peek_span(),
                                    );
                                    return None;
                                }
                                self.advance();
                                self.expect(&TokenKind::Eq, "expected `=`").ok()?;
                                match self.peek_kind() {
                                    TokenKind::BoolLiteral(true) | TokenKind::IntLiteral(1, _) => {
                                        tailchain = true;
                                    }
                                    TokenKind::BoolLiteral(false) | TokenKind::IntLiteral(0, _) => {
                                        tailchain = false;
                                    }
                                    _ => {
                                        self.diags.error(
                                            "expected `true` or `false` for tailchain",
                                            "E106",
                                            self.peek_span(),
                                        );
                                        return None;
                                    }
                                }
                                self.advance();
                            }
                            _ => break,
                        }
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                    isr = Some(IsrAnnotation {
                        label,
                        priority,
                        tailchain,
                    });
                }
                TokenKind::Naked => {
                    if naked {
                        self.diags
                            .error("duplicate @naked annotation", "E108", self.peek_span());
                        return None;
                    }
                    self.advance();
                    naked = true;
                }
                TokenKind::Section => {
                    if section.is_some() {
                        self.diags
                            .error("duplicate @section annotation", "E108", self.peek_span());
                        return None;
                    }
                    self.advance();
                    self.expect(&TokenKind::LParen, "expected `(`").ok()?;
                    section = if let TokenKind::StringLiteral(s) = self.peek_kind() {
                        let v = s.clone();
                        self.advance();
                        Some(v)
                    } else {
                        self.diags
                            .error("expected section name string", "E108", self.peek_span());
                        return None;
                    };
                    self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                }
                _ => {
                    self.diags.error(
                        "expected `@context(thread)`, `@isr(...)`, `@naked`, or `@section(...)`",
                        "E103",
                        self.peek_span(),
                    );
                    return None;
                }
            }
        }
        Some((context, isr, naked, section))
    }

    fn parse_isr_priority(&mut self) -> Option<u8> {
        self.expect(&TokenKind::Priority, "expected `priority`")
            .ok()?;
        self.expect(&TokenKind::Eq, "expected `=`").ok()?;
        let span = self.peek_span();
        let val = self.parse_int_literal()?;
        if val > u64::from(u8::MAX) {
            self.diags.error(
                format!("@isr priority must be in 0..=255, got {val}"),
                "E103",
                span,
            );
            return None;
        }
        Some(val as u8)
    }

    fn parse_static_def(&mut self) -> Option<StaticDef> {
        self.advance(); // static
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Colon, "expected `:`").ok()?;
        let ty = self.parse_type_expr()?;

        let mut storage = Vec::new();
        while self.eat(&TokenKind::AtSign) {
            storage.push(self.parse_storage_annotation()?);
        }

        // Optional `in <region>` placement clause, after the @-annotations and
        // before the initializer. The region name is resolved against the
        // target file by the region pass, not here.
        let region = if self.eat(&TokenKind::In) {
            Some(self.parse_ident()?)
        } else {
            None
        };

        let init = if self.eat(&TokenKind::Eq) {
            let expr = self.parse_expr()?;
            Some(expr)
        } else {
            None
        };

        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;

        Some(StaticDef {
            name,
            ty,
            storage,
            region,
            init,
        })
    }

    fn parse_storage_annotation(&mut self) -> Option<StorageAnnotation> {
        match self.peek_kind() {
            TokenKind::Exclusive => {
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(`").ok()?;
                let name = self.parse_ident()?;
                self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                Some(StorageAnnotation::Exclusive(name))
            }
            TokenKind::Shared => {
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(`").ok()?;
                self.expect(&TokenKind::Ceiling, "expected `ceiling`")
                    .ok()?;
                self.expect(&TokenKind::Eq, "expected `=`").ok()?;
                let span = self.peek_span();
                let prio = self.parse_int_literal()?;
                if prio > u64::from(u8::MAX) {
                    self.diags.error(
                        format!("@shared ceiling must be in 0..=255, got {prio}"),
                        "E104",
                        span,
                    );
                    return None;
                }
                self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                Some(StorageAnnotation::Shared(prio as u8))
            }
            TokenKind::Dma => {
                self.advance();
                Some(StorageAnnotation::Dma)
            }
            TokenKind::External => {
                self.advance();
                Some(StorageAnnotation::External)
            }
            TokenKind::Section => {
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(`").ok()?;
                let name = if let TokenKind::StringLiteral(s) = self.peek_kind() {
                    let v = s.clone();
                    self.advance();
                    v
                } else {
                    self.diags
                        .error("expected section name string", "E108", self.peek_span());
                    String::new()
                };
                self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                Some(StorageAnnotation::Section(name))
            }
            TokenKind::Align => {
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(`").ok()?;
                let n = self.parse_int_literal()?;
                self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                if n == 0 || n > u64::from(u32::MAX) || (n & (n - 1)) != 0 {
                    self.diags.error(
                        format!("@align value must be a u32 power of two, got {n}"),
                        "E104",
                        span,
                    );
                    return None;
                }
                Some(StorageAnnotation::Align(n as u32))
            }
            _ => {
                self.diags.error(
                    "expected `exclusive`, `shared`, `dma`, `external`, `section`, or `align`",
                    "E104",
                    self.peek_span(),
                );
                None
            }
        }
    }

    fn parse_const_def(&mut self) -> Option<ConstDef> {
        self.advance(); // const
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Colon, "expected `:`").ok()?;
        let ty = self.parse_type_expr()?;
        self.expect(&TokenKind::Eq, "expected `=`").ok()?;
        let value = self.parse_expr()?;
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(ConstDef { name, ty, value })
    }

    fn parse_struct_def(&mut self) -> Option<StructDef> {
        self.advance(); // struct
        let name = self.parse_ident()?;
        let repr = self.parse_struct_repr()?;
        self.expect(&TokenKind::LBrace, "expected `{`").ok()?;

        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            let field_name = self.parse_ident()?;
            self.expect(&TokenKind::Colon, "expected `:` after field name")
                .ok()?;
            let field_ty = self.parse_type_expr()?;
            let endian = self.parse_field_endian()?;
            fields.push(StructFieldDef {
                name: field_name,
                ty: field_ty,
                endian,
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;

        Some(StructDef { name, repr, fields })
    }

    fn parse_struct_repr(&mut self) -> Option<crate::ast::StructRepr> {
        let mut repr = crate::ast::StructRepr::Explicit;
        while self.eat(&TokenKind::AtSign) {
            let span = self.peek_span();
            let TokenKind::Ident(name) = self.peek_kind() else {
                self.diags.error("expected `repr` annotation", "E108", span);
                return None;
            };
            if name != "repr" {
                self.diags
                    .error("expected `@repr(C)` or `@repr(packed)`", "E108", span);
                return None;
            }
            if !matches!(repr, crate::ast::StructRepr::Explicit) {
                self.diags.error("duplicate @repr annotation", "E108", span);
                return None;
            }
            self.advance();
            self.expect(&TokenKind::LParen, "expected `(` after `repr`")
                .ok()?;
            let arg_span = self.peek_span();
            let TokenKind::Ident(arg) = self.peek_kind() else {
                self.diags
                    .error("expected `C` or `packed` in @repr", "E108", arg_span);
                return None;
            };
            repr = match arg.as_str() {
                "C" => crate::ast::StructRepr::C,
                "packed" => crate::ast::StructRepr::Packed,
                _ => {
                    self.diags
                        .error("expected `C` or `packed` in @repr", "E108", arg_span);
                    return None;
                }
            };
            self.advance();
            self.expect(&TokenKind::RParen, "expected `)` after @repr")
                .ok()?;
        }
        Some(repr)
    }

    /// Parse an optional `@be`/`@le` endianness attribute following a struct
    /// field type. Absence yields `Native`.
    fn parse_field_endian(&mut self) -> Option<crate::ast::FieldEndian> {
        use crate::ast::FieldEndian;
        if !self.eat(&TokenKind::AtSign) {
            return Some(FieldEndian::Native);
        }
        let span = self.peek_span();
        let TokenKind::Ident(name) = self.peek_kind() else {
            self.diags.error(
                "expected `be` or `le` after `@` in struct field",
                "E108",
                span,
            );
            return None;
        };
        let endian = match name.as_str() {
            "be" => FieldEndian::Big,
            "le" => FieldEndian::Little,
            _ => {
                self.diags.error(
                    "expected `be` or `le` after `@` in struct field",
                    "E108",
                    span,
                );
                return None;
            }
        };
        self.advance();
        Some(endian)
    }

    fn parse_enum_def(&mut self) -> Option<EnumDef> {
        self.advance(); // enum
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Colon, "expected `:` after enum name")
            .ok()?;
        let ty = self.parse_type_expr()?;
        self.expect(&TokenKind::LBrace, "expected `{`").ok()?;

        let mut variants = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            let var_name = self.parse_ident()?;
            let value = if self.eat(&TokenKind::Eq) {
                Some(self.parse_int_literal()?)
            } else {
                None
            };
            variants.push(EnumVariantDef {
                name: var_name,
                value,
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;

        Some(EnumDef { name, ty, variants })
    }

    fn parse_peripheral_def(&mut self) -> Option<PeripheralDef> {
        self.advance(); // peripheral
        let name = self.parse_ident()?;
        self.expect(&TokenKind::At, "expected `at`").ok()?;
        let addr = self.parse_int_literal()?;
        self.expect(&TokenKind::LBrace, "expected `{`").ok()?;

        let mut regs = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            if let Some(r) = self.parse_reg_def() {
                regs.push(r);
            } else {
                self.skip_to_semicolon_or_brace();
            }
        }

        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;

        Some(PeripheralDef {
            name,
            base_addr: addr,
            regs,
        })
    }

    fn parse_reg_def(&mut self) -> Option<RegDef> {
        self.expect(&TokenKind::Reg, "expected `reg`").ok()?;
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Offset, "expected `offset`").ok()?;
        let offset = self.parse_int_literal()?;
        self.expect(&TokenKind::LBrace, "expected `{`").ok()?;

        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            if let Some(f) = self.parse_field_def() {
                fields.push(f);
            } else {
                self.skip_to_semicolon_or_brace();
            }
        }

        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;

        Some(RegDef {
            name,
            offset,
            fields,
        })
    }

    fn parse_access_modifier(&mut self) -> Option<crate::ast::Access> {
        if self.eat(&TokenKind::Readonly) {
            Some(crate::ast::Access::ReadOnly)
        } else if self.eat(&TokenKind::Writeonly) {
            Some(crate::ast::Access::WriteOnly)
        } else {
            None
        }
    }

    fn parse_field_def(&mut self) -> Option<FieldDef> {
        self.expect(&TokenKind::Field, "expected `field`").ok()?;
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Colon, "expected `:`").ok()?;

        // Required type annotation before the bit spec
        let ty = self.parse_type_expr()?;

        let bit_spec = if self.eat(&TokenKind::Bit) {
            self.expect(&TokenKind::LBracket, "expected `[`").ok()?;
            let n = self.parse_int_literal()? as usize;
            // Check for range: bit[N..M]
            if self.eat(&TokenKind::DotDot) {
                let end = self.parse_int_literal()? as usize;
                self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
                BitSpec::Range(n, end)
            } else {
                // Single bit: bit[N]
                self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
                BitSpec::Single(n)
            }
        } else {
            self.diags.error("expected `bit`", "E105", self.peek_span());
            return None;
        };

        let access = self.parse_access_modifier();

        Some(FieldDef {
            name,
            ty,
            bit_spec,
            access,
        })
    }

    fn parse_import(&mut self) -> Option<ImportStmt> {
        self.advance(); // import

        let mut module = vec![self.parse_ident()?];
        while self.eat(&TokenKind::Dot) {
            module.push(self.parse_ident()?);
        }

        let imports = if self.eat(&TokenKind::LBrace) {
            let mut names = Vec::new();
            loop {
                names.push(self.parse_ident()?);
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
            self.expect(&TokenKind::RBrace, "expected `}`").ok()?;
            ImportKind::Selective(names)
        } else {
            ImportKind::All
        };

        // optional `as alias`
        let alias = if self.eat(&TokenKind::As) {
            Some(self.parse_ident()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(ImportStmt {
            module,
            imports,
            alias,
        })
    }

    /// `owns P, P.R, ...;` -- a module's exclusive register-ownership claims.
    /// Each path is a peripheral name, optionally `.register`. Field-level
    /// paths (`P.R.F`) are rejected: field-granularity ownership is not yet
    /// supported (see doc/regions-agents-plan.md).
    fn parse_owns(&mut self) -> Option<OwnsStmt> {
        self.advance(); // owns
        let mut paths = Vec::new();
        loop {
            let peripheral = self.parse_ident()?;
            let mut span = peripheral.1;
            let register = if self.eat(&TokenKind::Dot) {
                let reg = self.parse_ident()?;
                span = span.merge(reg.1);
                // Reject a field-level third component loudly rather than
                // silently ignoring it.
                if self.eat(&TokenKind::Dot) {
                    let field = self.parse_ident()?;
                    self.diags.error(
                        format!(
                            "field-level ownership (`{}.{}.{}`) is not yet supported; \
                             own the whole register `{}.{}`",
                            peripheral.0, reg.0, field.0, peripheral.0, reg.0
                        ),
                        "E603",
                        span.merge(field.1),
                    );
                    return None;
                }
                Some(reg)
            } else {
                None
            };
            paths.push(OwnsPath {
                peripheral,
                register,
                span,
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(OwnsStmt { paths })
    }

    fn parse_export(&mut self) -> Option<ExportStmt> {
        self.advance(); // export
        let mut names = Vec::new();
        loop {
            match self.peek_kind() {
                TokenKind::Fn => {
                    self.advance();
                    names.push(ExportItem::Fn(self.parse_ident()?));
                }
                TokenKind::Var => {
                    self.advance();
                    names.push(ExportItem::Static(self.parse_ident()?));
                }
                TokenKind::Const => {
                    self.advance();
                    names.push(ExportItem::Const(self.parse_ident()?));
                }
                TokenKind::Peripheral => {
                    self.advance();
                    names.push(ExportItem::Peripheral(self.parse_ident()?));
                }
                TokenKind::Struct => {
                    self.advance();
                    names.push(ExportItem::Struct(self.parse_ident()?));
                }
                TokenKind::Enum => {
                    self.advance();
                    names.push(ExportItem::Enum(self.parse_ident()?));
                }
                _ => break,
            }
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(ExportStmt { names })
    }

    // --- types ---

    fn parse_type_expr(&mut self) -> Option<TypeExpr> {
        self.guarded(Self::parse_type_expr_inner)
    }

    fn parse_type_expr_inner(&mut self) -> Option<TypeExpr> {
        match self.peek_kind() {
            TokenKind::Star => {
                self.advance();
                // *mut T = mutable pointer, *T = const pointer (default)
                if self.eat(&TokenKind::Mut) {
                    let inner = self.parse_type_expr()?;
                    Some(TypeExpr::Ptr(Box::new(inner)))
                } else {
                    let inner = self.parse_type_expr()?;
                    Some(TypeExpr::ConstPtr(Box::new(inner)))
                }
            }
            TokenKind::View => {
                self.advance();
                // `view mut T` is a mutable view; `view T` is readonly.
                let mutable = self.eat(&TokenKind::Mut);
                let inner = self.parse_type_expr()?;
                // `view T stride K`: a strided view. `stride` is a contextual
                // keyword (a plain identifier), so it stays usable elsewhere.
                if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "stride") {
                    self.advance();
                    let stride = self.parse_expr()?;
                    Some(TypeExpr::StridedView(
                        Box::new(inner),
                        mutable,
                        Box::new(stride),
                    ))
                } else {
                    Some(TypeExpr::View(Box::new(inner), mutable))
                }
            }
            TokenKind::Ring => {
                self.advance();
                let mutable = self.eat(&TokenKind::Mut);
                let inner = self.parse_type_expr()?;
                Some(TypeExpr::Ring(Box::new(inner), mutable))
            }
            TokenKind::Bits => {
                self.advance();
                // `bits mut` is a mutable bit view; `bits` is readonly. No
                // element type: the element is always a single bit.
                let mutable = self.eat(&TokenKind::Mut);
                Some(TypeExpr::Bits(mutable))
            }
            TokenKind::LBracket => {
                self.advance();
                let inner = self.parse_type_expr()?;
                self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
                let size = self.parse_expr()?;
                self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
                Some(TypeExpr::Array(Box::new(inner), Box::new(size)))
            }
            TokenKind::Fn => {
                self.advance();
                self.expect(
                    &TokenKind::LParen,
                    "expected `(` after `fn` in function type",
                )
                .ok()?;
                let mut params = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    params.push(self.parse_type_expr()?);
                    while self.eat(&TokenKind::Comma) {
                        params.push(self.parse_type_expr()?);
                    }
                }
                self.expect(&TokenKind::RParen, "expected `)` after fn parameters")
                    .ok()?;
                let ret = if self.eat(&TokenKind::Arrow) {
                    Some(Box::new(self.parse_type_expr()?))
                } else {
                    None
                };
                Some(TypeExpr::Fn(params, ret))
            }
            // `addr in <region>`: an in-memory handoff slot. `addr` is a
            // contextual keyword (a plain identifier elsewhere), like `stride`.
            TokenKind::Ident(s) if s == "addr" => {
                self.advance();
                self.expect(&TokenKind::In, "expected `in <region>` after `addr`")
                    .ok()?;
                let region = self.parse_ident()?;
                Some(TypeExpr::Addr(region))
            }
            _ => {
                let name = self.parse_ident()?;
                Some(TypeExpr::Named(name))
            }
        }
    }

    // --- blocks and statements ---

    fn parse_block(&mut self) -> Option<Block> {
        self.guarded(Self::parse_block_inner)
    }

    fn parse_block_inner(&mut self) -> Option<Block> {
        let start = self.peek_span();
        self.expect(&TokenKind::LBrace, "expected `{`").ok()?;

        let mut stmts = Vec::new();

        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            if let Some(stmt) = self.parse_stmt() {
                stmts.push(stmt);
            } else if self.trailing_expr.is_some() {
                break;
            } else {
                self.skip_to_semicolon_or_brace();
            }
        }

        let trailing = self.trailing_expr.take().map(Box::new);
        let end = self.peek_span();
        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;

        Some(Block {
            stmts,
            trailing,
            span: start.merge(end),
        })
    }

    fn parse_stmt(&mut self) -> Option<Stmt> {
        match self.peek_kind() {
            TokenKind::Var => self.parse_var_decl(true).map(Stmt::VarDecl),
            TokenKind::Const => self.parse_var_decl(false).map(Stmt::VarDecl),
            TokenKind::If => self.parse_if_stmt().map(Stmt::If),
            TokenKind::Loop => self.parse_loop_stmt().map(Stmt::Loop),
            TokenKind::While => self.parse_while_stmt().map(Stmt::While),
            TokenKind::For => self.parse_for_stmt().map(|f| Stmt::For(Box::new(f))),
            TokenKind::Match => self.parse_match_stmt().map(Stmt::Match),
            TokenKind::AsmBody(text) => {
                let span = self.peek_span();
                let asm_text = text.clone();
                self.advance();
                // Optional GCC-style operand sections: `: outputs : inputs : clobbers`.
                // Each section is positional and may be empty.
                let mut outputs = Vec::new();
                let mut inputs = Vec::new();
                let mut clobbers = Vec::new();
                if self.eat(&TokenKind::Colon) {
                    outputs = self.parse_asm_operands()?;
                    if self.eat(&TokenKind::Colon) {
                        inputs = self.parse_asm_operands()?;
                        if self.eat(&TokenKind::Colon) {
                            clobbers = self.parse_asm_clobbers()?;
                        }
                    }
                }
                self.eat(&TokenKind::Semicolon);
                Some(Stmt::Asm(AsmStmt {
                    asm_text,
                    outputs,
                    inputs,
                    clobbers,
                    span,
                }))
            }
            TokenKind::Assume => {
                let (cond, span) = self.parse_paren_cond("assume")?;
                Some(Stmt::Assume(AssumeStmt { cond, span }))
            }
            TokenKind::Assert => {
                let (cond, span) = self.parse_paren_cond("assert")?;
                Some(Stmt::Assert(AssertStmt { cond, span }))
            }
            TokenKind::Return => self.parse_return_stmt().map(Stmt::Return),
            TokenKind::Break => {
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
                Some(Stmt::Break(span))
            }
            TokenKind::Continue => {
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
                Some(Stmt::Continue(span))
            }
            TokenKind::LBrace => self.parse_block().map(Stmt::Block),
            _ => {
                // Try expression statement, assignment, or trailing expression
                let expr = self.parse_expr()?;
                if self.eat(&TokenKind::Eq) {
                    // Assignment
                    let value = self.parse_expr()?;
                    self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
                    let target = expr_to_lvalue(expr)?;
                    Some(Stmt::Assign(AssignStmt { target, value }))
                } else if let Some(op) = compound_assign_op(self.peek_kind()) {
                    // Compound assignment `a OP= b`: kept as its own node so the
                    // IR can lower it as a single-evaluation read-modify-write.
                    let span = expr.span();
                    self.advance();
                    let value = self.parse_expr()?;
                    self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
                    let target = expr_to_lvalue(expr)?;
                    Some(Stmt::CompoundAssign(CompoundAssignStmt {
                        target,
                        op,
                        value,
                        span,
                    }))
                } else if self.check(&TokenKind::RBrace) {
                    // Trailing expression -- no semicolon before `}`
                    self.trailing_expr = Some(expr);
                    None
                } else {
                    self.expect(&TokenKind::Semicolon, "expected `;` after expression")
                        .ok()?;
                    Some(Stmt::Expr(expr))
                }
            }
        }
    }

    fn parse_var_decl(&mut self, mutable: bool) -> Option<VarDecl> {
        self.advance(); // var or val
        let name = self.parse_ident()?;
        let ty_ann = if self.eat(&TokenKind::Colon) {
            self.parse_type_expr()
        } else {
            None
        };
        self.expect(&TokenKind::Eq, "expected `=`").ok()?;
        let init = self.parse_expr()?;
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(VarDecl {
            mutable,
            name,
            ty_ann,
            init,
        })
    }

    fn parse_if_stmt(&mut self) -> Option<IfStmt> {
        self.advance(); // if
        let cond = self.parse_expr_no_struct()?;
        let then_block = self.parse_block()?;
        let else_branch = if self.eat(&TokenKind::Else) {
            if self.check(&TokenKind::If) {
                Some(Box::new(Stmt::If(self.parse_if_stmt()?)))
            } else {
                Some(Box::new(Stmt::Block(self.parse_block()?)))
            }
        } else {
            None
        };
        Some(IfStmt {
            cond,
            then_block,
            else_branch,
        })
    }

    fn parse_loop_stmt(&mut self) -> Option<LoopStmt> {
        self.advance(); // loop
        let body = self.parse_block()?;
        Some(LoopStmt { body })
    }

    fn parse_match_arms(&mut self) -> Option<(Expr, Vec<MatchArm>, Span)> {
        let scrutinee = self.parse_expr_no_struct()?;
        self.expect(&TokenKind::LBrace, "expected `{`").ok()?;

        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) && !self.is_eof() {
            let patterns = self.parse_match_patterns()?;
            let body = self.parse_block()?;
            arms.push(MatchArm { patterns, body });
            self.eat(&TokenKind::Comma);
        }

        let end = self.peek_span();
        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;

        Some((scrutinee, arms, end))
    }

    fn parse_match_stmt(&mut self) -> Option<MatchStmt> {
        let start = self.peek_span();
        self.advance(); // match
        let (scrutinee, arms, end) = self.parse_match_arms()?;
        Some(MatchStmt {
            scrutinee,
            arms,
            span: start.merge(end),
        })
    }

    fn parse_match_expr(&mut self, start_span: Span) -> Option<Expr> {
        let (scrutinee, arms, end) = self.parse_match_arms()?;
        Some(Expr::Match(Box::new(MatchExpr {
            scrutinee,
            arms,
            span: start_span.merge(end),
        })))
    }

    fn parse_match_patterns(&mut self) -> Option<Vec<MatchPattern>> {
        let mut patterns = Vec::new();
        let first = self.parse_match_pattern()?;
        let mut has_wildcard = matches!(&first, MatchPattern::Wildcard(_));
        patterns.push(first);
        while self.eat(&TokenKind::Pipe) {
            let next = self.parse_match_pattern()?;
            if has_wildcard || matches!(&next, MatchPattern::Wildcard(_)) {
                self.diags.error(
                    "wildcard `_` cannot be combined with other patterns",
                    "E326",
                    next.span(),
                );
            }
            has_wildcard |= matches!(&next, MatchPattern::Wildcard(_));
            patterns.push(next);
        }
        Some(patterns)
    }

    fn parse_match_pattern(&mut self) -> Option<MatchPattern> {
        // Integer or inclusive-range pattern: `N`, `-N`, or `lo..hi`.
        if matches!(
            self.peek_kind(),
            TokenKind::IntLiteral(..) | TokenKind::Minus
        ) {
            let span = self.peek_span();
            let lo = self.parse_pattern_int()?;
            if self.eat(&TokenKind::DotDot) {
                let hi = self.parse_pattern_int()?;
                return Some(MatchPattern::Range(lo, hi, span));
            }
            return Some(MatchPattern::Int(lo, span));
        }
        let ident = self.parse_ident()?;
        if ident.0 == "_" && !self.check(&TokenKind::AtSign) {
            return Some(MatchPattern::Wildcard(ident.1));
        }
        if !self.check(&TokenKind::AtSign) {
            self.diags
                .error("expected `@` in match pattern", "E100", ident.1);
            return None;
        }
        self.advance(); // @
        let variant = self.parse_ident()?;
        Some(MatchPattern::Variant(ident, variant))
    }

    /// Parse an integer pattern value: an optional `-` then an int literal.
    /// The suffix (if any) is ignored; the value is held as `i128`.
    fn parse_pattern_int(&mut self) -> Option<i128> {
        let neg = self.eat(&TokenKind::Minus);
        if let TokenKind::IntLiteral(n, _) = self.peek_kind() {
            let v = i128::from(*n);
            self.advance();
            Some(if neg { -v } else { v })
        } else {
            self.diags.error(
                "expected an integer in match pattern",
                "E107",
                self.peek_span(),
            );
            None
        }
    }

    fn parse_while_stmt(&mut self) -> Option<WhileStmt> {
        self.advance(); // while
        let cond = self.parse_expr_no_struct()?;
        let body = self.parse_block()?;
        Some(WhileStmt { cond, body })
    }

    fn parse_for_stmt(&mut self) -> Option<ForStmt> {
        self.advance(); // for
        let var = self.parse_ident()?;
        self.expect(
            &TokenKind::Colon,
            "expected `:` (for loop variable requires a type annotation)",
        )
        .ok()?;
        let ty = self.parse_type_expr()?;
        self.expect(&TokenKind::In, "expected `in`").ok()?;
        let start = self.parse_expr_no_struct()?;
        let direction = match self.peek_kind() {
            TokenKind::Upto => {
                self.advance();
                ForDirection::Upto
            }
            TokenKind::Downto => {
                self.advance();
                ForDirection::Downto
            }
            TokenKind::DotDot => {
                self.diags.error(
                    "for loops use `upto` or `downto`; `..` is only valid in `bit[L..H]`",
                    "E100",
                    self.peek_span(),
                );
                self.advance();
                ForDirection::Upto
            }
            _ => {
                self.diags
                    .error("expected `upto` or `downto`", "E100", self.peek_span());
                return None;
            }
        };
        let end = self.parse_expr_no_struct()?;
        let step = if self.eat(&TokenKind::Step) {
            Some(self.parse_expr_no_struct()?)
        } else {
            None
        };
        let body = self.parse_block()?;
        Some(ForStmt {
            var,
            ty,
            start,
            direction,
            end,
            step,
            body,
        })
    }

    fn parse_return_stmt(&mut self) -> Option<ReturnStmt> {
        self.advance(); // return
        let value = if self.check(&TokenKind::Semicolon) {
            None
        } else {
            let expr = self.parse_expr()?;
            Some(expr)
        };
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(ReturnStmt { value })
    }

    // --- expressions (Pratt parser) ---

    fn parse_expr(&mut self) -> Option<Expr> {
        self.parse_expr_prec(0, true)
    }

    fn parse_expr_no_struct(&mut self) -> Option<Expr> {
        self.parse_expr_prec(0, false)
    }

    fn parse_expr_prec(&mut self, min_prec: u8, allow_struct: bool) -> Option<Expr> {
        self.guarded(|p| p.parse_expr_prec_inner(min_prec, allow_struct))
    }

    fn parse_expr_prec_inner(&mut self, min_prec: u8, allow_struct: bool) -> Option<Expr> {
        let mut left = self.parse_prefix(allow_struct)?;

        // Merged binary + postfix loop: after extending left via a postfix
        // operator (dot, call, index, cast, etc.), continue to check for
        // binary operators that follow (e.g. a.b + c, f() == 0).
        loop {
            // -- binary operators (Pratt precedence climbing) --
            {
                let op = match self.peek_kind() {
                    TokenKind::Plus => Some(BinaryOp::Add),
                    TokenKind::Minus => Some(BinaryOp::Sub),
                    TokenKind::Star => Some(BinaryOp::Mul),
                    TokenKind::Slash => Some(BinaryOp::Div),
                    TokenKind::Percent => Some(BinaryOp::Mod),
                    TokenKind::EqEq => Some(BinaryOp::Eq),
                    TokenKind::NotEq => Some(BinaryOp::NotEq),
                    TokenKind::Lt => Some(BinaryOp::Lt),
                    TokenKind::Gt => Some(BinaryOp::Gt),
                    TokenKind::LtEq => Some(BinaryOp::LtEq),
                    TokenKind::GtEq => Some(BinaryOp::GtEq),
                    TokenKind::And => Some(BinaryOp::And),
                    TokenKind::Or => Some(BinaryOp::Or),
                    TokenKind::Amp => Some(BinaryOp::BitAnd),
                    TokenKind::Pipe => Some(BinaryOp::BitOr),
                    TokenKind::Caret => Some(BinaryOp::BitXor),
                    TokenKind::Shl => Some(BinaryOp::Shl),
                    TokenKind::Shr => Some(BinaryOp::Shr),
                    _ => None,
                };
                if let Some(op) = op {
                    let prec = op.precedence();
                    if prec >= min_prec {
                        self.advance();
                        let right = self.parse_expr_prec(prec + 1, allow_struct)?;
                        left = Expr::Binary(Box::new(left), op, Box::new(right));
                        continue;
                    }
                }
            }

            // -- `as T` cast --
            // Binds looser than the prefix unary operators (so `&x as T` is
            // `(&x) as T`) but tighter than every binary operator. Gating it by
            // precedence here -- rather than lumping it with the `.`/`()`/`[]`
            // postfix operators below -- is what stops a unary operand (parsed
            // at PREFIX_OPERAND_PREC) from swallowing it and regrouping
            // `&x as T` into `&(x as T)`.
            if matches!(self.peek_kind(), TokenKind::As) && CAST_PREC >= min_prec {
                self.advance();
                let ty = self.parse_type_expr()?;
                left = Expr::Cast(Box::new(left), ty);
                continue;
            }

            // -- postfix operators --
            match self.peek_kind() {
                TokenKind::LParen => {
                    self.advance();
                    let mut args = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            args.push(self.parse_expr()?);
                            if !self.eat(&TokenKind::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                    left = Expr::Call(Box::new(left), args);
                }
                TokenKind::Dot => {
                    self.advance();
                    let field = self.parse_ident()?;
                    left = Expr::FieldAccess(Box::new(left), field);
                }
                TokenKind::AtSign if matches!(&left, Expr::Ident(_)) => {
                    self.advance();
                    let variant = self.parse_ident()?;
                    if let Expr::Ident(enum_name) = left {
                        let span = enum_name.1.merge(variant.1);
                        left = Expr::EnumVariant {
                            enum_name,
                            variant,
                            span,
                        };
                    }
                }
                TokenKind::LBracket => {
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
                    left = Expr::Index(Box::new(left), Box::new(index));
                }
                TokenKind::LBrace if allow_struct => {
                    if let Expr::Ident(name) = left {
                        self.advance();
                        let mut fields = Vec::new();
                        if !self.check(&TokenKind::RBrace) {
                            loop {
                                let fname = self.parse_ident()?;
                                self.expect(&TokenKind::Colon, "expected `:` after field name")
                                    .ok()?;
                                let val = self.parse_expr()?;
                                fields.push((fname, val));
                                if !self.eat(&TokenKind::Comma) {
                                    break;
                                }
                            }
                        }
                        let end_span = self.peek_span();
                        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;
                        let span = name.1.merge(end_span);
                        left = Expr::StructInit { name, fields, span };
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }

        Some(left)
    }

    fn parse_prefix(&mut self, allow_struct: bool) -> Option<Expr> {
        let span = self.peek_span();
        match self.peek_kind() {
            TokenKind::Minus => {
                self.advance();
                let expr = self.parse_expr_prec(PREFIX_OPERAND_PREC, allow_struct)?;
                Some(Expr::Unary(UnaryOp::Neg, Box::new(expr)))
            }
            TokenKind::Bang => {
                self.advance();
                let expr = self.parse_expr_prec(PREFIX_OPERAND_PREC, allow_struct)?;
                Some(Expr::Unary(UnaryOp::Not, Box::new(expr)))
            }
            TokenKind::Tilde => {
                self.advance();
                let expr = self.parse_expr_prec(PREFIX_OPERAND_PREC, allow_struct)?;
                Some(Expr::Unary(UnaryOp::BitNot, Box::new(expr)))
            }
            TokenKind::Star => {
                self.advance();
                let expr = self.parse_expr_prec(PREFIX_OPERAND_PREC, allow_struct)?;
                Some(Expr::Unary(UnaryOp::Deref, Box::new(expr)))
            }
            TokenKind::Amp => {
                self.advance();
                let is_mut = self.eat(&TokenKind::Mut);
                let expr = self.parse_expr_prec(PREFIX_OPERAND_PREC, allow_struct)?;
                let op = if is_mut {
                    UnaryOp::AddrOfMut
                } else {
                    UnaryOp::AddrOf
                };
                Some(Expr::Unary(op, Box::new(expr)))
            }
            TokenKind::Null => {
                let span = self.peek_span();
                self.advance();
                Some(Expr::NullLiteral(span))
            }
            TokenKind::Sizeof => {
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(` after `sizeof`")
                    .ok()?;
                let ty = self.parse_type_expr()?;
                self.expect(&TokenKind::RParen, "expected `)` after type in `sizeof`")
                    .ok()?;
                Some(Expr::SizeOf(ty, span))
            }
            TokenKind::View => {
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(` after `view`")
                    .ok()?;
                let base = self.parse_expr()?;
                // Three forms after the base:
                //   `view(arr)`            -> len/stride both None (contiguous)
                //   `view(ptr, len)`       -> len Some (contiguous over pointer)
                //   `view(arr, stride K)`  -> stride Some (strided over array)
                // The contextual `stride` keyword after the comma selects the
                // strided form; otherwise the second argument is a length.
                let (len, stride) = if self.eat(&TokenKind::Comma) {
                    if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "stride") {
                        self.advance();
                        (None, Some(Box::new(self.parse_expr()?)))
                    } else {
                        (Some(Box::new(self.parse_expr()?)), None)
                    }
                } else {
                    (None, None)
                };
                self.expect(&TokenKind::RParen, "expected `)` to close `view(...)`")
                    .ok()?;
                Some(Expr::ViewNew {
                    base: Box::new(base),
                    len,
                    stride,
                    span,
                })
            }
            TokenKind::Ring => {
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(` after `ring`")
                    .ok()?;
                // Collect the comma-separated arguments, then map by count:
                // 3 = ring(arr, head, len), 4 = ring(ptr, capacity, head, len).
                let mut args = vec![self.parse_expr()?];
                while self.eat(&TokenKind::Comma) {
                    args.push(self.parse_expr()?);
                }
                self.expect(&TokenKind::RParen, "expected `)` to close `ring(...)`")
                    .ok()?;
                let mut it = args.into_iter();
                let (base, capacity, head, len) = match it.len() {
                    3 => {
                        let base = it.next().unwrap();
                        let head = it.next().unwrap();
                        let len = it.next().unwrap();
                        (base, None, head, len)
                    }
                    4 => {
                        let base = it.next().unwrap();
                        let capacity = it.next().unwrap();
                        let head = it.next().unwrap();
                        let len = it.next().unwrap();
                        (base, Some(Box::new(capacity)), head, len)
                    }
                    n => {
                        self.diags.error(
                            format!(
                                "`ring(...)` expects 3 (arr, head, len) or 4 (ptr, capacity, head, len) arguments, got {n}"
                            ),
                            "E100",
                            span,
                        );
                        return None;
                    }
                };
                Some(Expr::RingNew {
                    base: Box::new(base),
                    capacity,
                    head: Box::new(head),
                    len: Box::new(len),
                    span,
                })
            }
            TokenKind::Bits => {
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(` after `bits`")
                    .ok()?;
                // Collect the comma-separated arguments, then map by count:
                // 1 = bits(arr), 3 = bits(ptr, bit_offset, len_bits).
                let mut args = vec![self.parse_expr()?];
                while self.eat(&TokenKind::Comma) {
                    args.push(self.parse_expr()?);
                }
                self.expect(&TokenKind::RParen, "expected `)` to close `bits(...)`")
                    .ok()?;
                let mut it = args.into_iter();
                let (base, bit_offset, len_bits) = match it.len() {
                    1 => {
                        let base = it.next().unwrap();
                        (base, None, None)
                    }
                    3 => {
                        let base = it.next().unwrap();
                        let bit_offset = it.next().unwrap();
                        let len_bits = it.next().unwrap();
                        (base, Some(Box::new(bit_offset)), Some(Box::new(len_bits)))
                    }
                    n => {
                        self.diags.error(
                            format!(
                                "`bits(...)` expects 1 (arr) or 3 (ptr, bit_offset, len_bits) arguments, got {n}"
                            ),
                            "E100",
                            span,
                        );
                        return None;
                    }
                };
                Some(Expr::BitNew {
                    base: Box::new(base),
                    bit_offset,
                    len_bits,
                    span,
                })
            }
            TokenKind::Match => {
                let span = self.peek_span();
                self.advance(); // match
                let expr = self.parse_match_expr(span)?;
                Some(expr)
            }
            TokenKind::LParen => {
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "expected `)`").ok()?;
                Some(Expr::Group(Box::new(expr)))
            }
            TokenKind::LBrace => {
                let block = self.parse_block()?;
                let block_span = block.span;
                Some(Expr::Block(BlockExpr {
                    block,
                    span: block_span,
                }))
            }
            TokenKind::If => {
                let span = self.peek_span();
                self.advance(); // if
                let cond = self.parse_expr_no_struct()?;
                let then_block = self.parse_block()?;
                self.expect(&TokenKind::Else, "expected `else` in if expression")
                    .ok()?;
                let else_branch = if self.check(&TokenKind::If) {
                    self.parse_prefix(true)?
                } else {
                    let block = self.parse_block()?;
                    let block_span = block.span;
                    Expr::Block(BlockExpr {
                        block,
                        span: block_span,
                    })
                };
                let else_span = else_branch.span();
                Some(Expr::If(Box::new(IfExpr {
                    cond,
                    then_block,
                    else_branch: Box::new(else_branch),
                    span: span.merge(else_span),
                })))
            }
            TokenKind::LBracket => {
                let span = self.peek_span();
                self.advance();
                let mut elems = Vec::new();
                if !self.check(&TokenKind::RBracket) {
                    loop {
                        elems.push(self.parse_expr()?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                }
                self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
                Some(Expr::ArrayInit(elems, span))
            }
            TokenKind::IntLiteral(n, suffix) => {
                let v = *n;
                let s = *suffix;
                self.advance();
                Some(Expr::IntLiteral(v, s, span))
            }
            TokenKind::FloatLiteral(f, suffix) => {
                let v = *f;
                let s = *suffix;
                self.advance();
                Some(Expr::FloatLiteral(v, s, span))
            }
            TokenKind::BoolLiteral(b) => {
                let v = *b;
                self.advance();
                Some(Expr::BoolLiteral(v, span))
            }
            TokenKind::StringLiteral(s) => {
                let v = s.clone();
                self.advance();
                Some(Expr::StringLiteral(v, span))
            }
            _ => {
                let name = self.parse_ident()?;
                Some(Expr::Ident(name))
            }
        }
    }

    fn parse_ident(&mut self) -> Option<Ident> {
        if let TokenKind::Ident(s) = self.peek_kind() {
            let name = s.clone();
            let span = self.peek_span();
            self.advance();
            Some((name, span))
        } else {
            self.diags.error(
                format!("expected identifier, found `{:?}`", self.peek_kind()),
                "E106",
                self.peek_span(),
            );
            None
        }
    }

    fn parse_int_literal(&mut self) -> Option<u64> {
        if let TokenKind::IntLiteral(n, _) = self.peek_kind() {
            let v = *n;
            self.advance();
            Some(v)
        } else {
            self.diags.error(
                format!("expected integer, found `{:?}`", self.peek_kind()),
                "E107",
                self.peek_span(),
            );
            None
        }
    }
}

impl Parser<'_> {
    /// Parse a (possibly empty) comma-separated list of asm operands, each
    /// `"<constraint>" "(" expr ")"`. Used for the output and input sections.
    fn parse_asm_operands(&mut self) -> Option<Vec<(String, Expr)>> {
        let mut ops = Vec::new();
        // Empty section: next token is `:` (another section) or end of statement.
        if matches!(
            self.peek_kind(),
            TokenKind::Colon | TokenKind::Semicolon | TokenKind::RBrace
        ) {
            return Some(ops);
        }
        loop {
            let constraint = self.parse_asm_constraint_string()?;
            self.expect(&TokenKind::LParen, "expected `(` after asm constraint")
                .ok()?;
            let expr = self.parse_expr()?;
            self.expect(&TokenKind::RParen, "expected `)`").ok()?;
            ops.push((constraint, expr));
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Some(ops)
    }

    /// Parse a (possibly empty) comma-separated list of clobber strings.
    fn parse_asm_clobbers(&mut self) -> Option<Vec<String>> {
        let mut clobbers = Vec::new();
        if matches!(self.peek_kind(), TokenKind::Semicolon | TokenKind::RBrace) {
            return Some(clobbers);
        }
        loop {
            clobbers.push(self.parse_asm_constraint_string()?);
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        Some(clobbers)
    }

    fn parse_asm_constraint_string(&mut self) -> Option<String> {
        if let TokenKind::StringLiteral(s) = self.peek_kind() {
            let v = s.clone();
            self.advance();
            Some(v)
        } else {
            self.diags
                .error("expected a string constraint", "E100", self.peek_span());
            None
        }
    }
}

/// Map a compound-assignment token (`+=`, `<<=`, ...) to the binary operator it
/// desugars to. Returns `None` for any other token.
fn compound_assign_op(kind: &TokenKind) -> Option<BinaryOp> {
    Some(match kind {
        TokenKind::PlusEq => BinaryOp::Add,
        TokenKind::MinusEq => BinaryOp::Sub,
        TokenKind::StarEq => BinaryOp::Mul,
        TokenKind::SlashEq => BinaryOp::Div,
        TokenKind::PercentEq => BinaryOp::Mod,
        TokenKind::AmpEq => BinaryOp::BitAnd,
        TokenKind::PipeEq => BinaryOp::BitOr,
        TokenKind::CaretEq => BinaryOp::BitXor,
        TokenKind::ShlEq => BinaryOp::Shl,
        TokenKind::ShrEq => BinaryOp::Shr,
        _ => return None,
    })
}

pub(crate) fn expr_to_lvalue(expr: Expr) -> Option<LValue> {
    match expr {
        Expr::Ident((name, span)) => Some(LValue::Name((name, span))),
        Expr::FieldAccess(base, field) => {
            let base = expr_to_lvalue(*base)?;
            Some(LValue::Field(Box::new(base), field))
        }
        Expr::Index(base, index) => {
            let base = expr_to_lvalue(*base)?;
            Some(LValue::Index(Box::new(base), index))
        }
        Expr::Unary(UnaryOp::Deref, inner) => Some(LValue::Deref(inner)),
        // Peel parentheses so a parenthesized place stays assignable, e.g. the
        // `(*p)` in `(*p).field = v` parses as `FieldAccess(Group(Deref p), …)`.
        // Without this the conversion returned `None` and the assignment was
        // silently dropped.
        Expr::Group(inner) => expr_to_lvalue(*inner),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Parser;
    use crate::ast::{BinaryOp, Expr, UnaryOp};
    use crate::errors::DiagnosticBag;
    use crate::source::FileId;

    fn parse_expr_str(src: &str) -> Expr {
        let mut diags = DiagnosticBag::new();
        let expr = Parser::new(src, FileId::new(), &mut diags).parse_expr();
        assert!(!diags.has_errors(), "unexpected parse errors for `{src}`");
        expr.expect("expression should parse")
    }

    // Model B precedence: a prefix unary operator binds tighter than `as`, so
    // `&x as u32` is `(&x) as u32` -- an outer Cast wrapping `&x` -- not
    // `&(x as u32)`. This is the case the eth_dma DMA-address use needs.
    #[test]
    fn addr_of_then_cast_is_cast_of_addr() {
        let Expr::Cast(inner, _) = parse_expr_str("&x as u32") else {
            panic!("expected outer Cast");
        };
        assert!(matches!(*inner, Expr::Unary(UnaryOp::AddrOf, _)));
    }

    #[test]
    fn addr_of_mut_then_cast_is_cast_of_addr() {
        let Expr::Cast(inner, _) = parse_expr_str("&mut x as u32") else {
            panic!("expected outer Cast");
        };
        assert!(matches!(*inner, Expr::Unary(UnaryOp::AddrOfMut, _)));
    }

    #[test]
    fn deref_then_cast_is_cast_of_deref() {
        let Expr::Cast(inner, _) = parse_expr_str("*p as u32") else {
            panic!("expected outer Cast");
        };
        assert!(matches!(*inner, Expr::Unary(UnaryOp::Deref, _)));
    }

    // `as` still binds tighter than binary operators: `x as u32 + 1` is
    // `(x as u32) + 1`, with the cast on the left of the add.
    #[test]
    fn cast_binds_tighter_than_binary() {
        let Expr::Binary(left, BinaryOp::Add, _) = parse_expr_str("x as u32 + 1") else {
            panic!("expected outer Add");
        };
        assert!(matches!(*left, Expr::Cast(_, _)));
    }

    // Field/index still bind tighter than the prefix unary (unchanged by the
    // precedence fix): `&a.b` stays `&(a.b)`.
    #[test]
    fn addr_of_field_unchanged() {
        let Expr::Unary(UnaryOp::AddrOf, inner) = parse_expr_str("&a.b") else {
            panic!("expected outer AddrOf");
        };
        assert!(matches!(*inner, Expr::FieldAccess(_, _)));
    }
}
