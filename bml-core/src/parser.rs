#[allow(clippy::wildcard_imports)]
use crate::ast::*;
use crate::errors::DiagnosticBag;
use crate::lexer::{Lexer, Token, TokenKind};
use crate::source::{FileId, Span};

/// A bare ident (`Color`) or a single-level `module.Name` field access, returned
/// as a `(dotted_name, span)` pair. Lets the postfix parser accept qualified
/// type/struct/enum names (`m.Color`, `m.State@V`, `m.Color { ... }`). Returns
/// `None` for any other expression, so deeper chains (`a.b.c`) and arbitrary
/// values never become struct/enum names.
fn qualified_name(e: &Expr) -> Option<Ident> {
    match e {
        Expr::Ident(n) => Some(n.clone()),
        Expr::FieldAccess(base, field) => match base.as_ref() {
            Expr::Ident(m) => Some((format!("{}.{}", m.0, field.0), m.1.merge(field.1))),
            _ => None,
        },
        _ => None,
    }
}

/// Set the `exported` flag on a definition item (the `export` modifier). Items
/// that carry no visibility (`import`/`owns`/`comptime_assert`) pass through; the
/// caller has already reported `E108` for those.
fn set_exported(item: Item) -> Item {
    match item {
        Item::FnDef(mut f) => {
            f.exported = true;
            Item::FnDef(f)
        }
        Item::ExternFnDef(mut f) => {
            f.exported = true;
            Item::ExternFnDef(f)
        }
        Item::StaticDef(mut s) => {
            s.exported = true;
            Item::StaticDef(s)
        }
        Item::ConstDef(mut c) => {
            c.exported = true;
            Item::ConstDef(c)
        }
        Item::PeripheralDef(mut p) => {
            p.exported = true;
            Item::PeripheralDef(p)
        }
        Item::PeripheralInstance(mut p) => {
            p.exported = true;
            Item::PeripheralInstance(p)
        }
        Item::StructDef(mut s) => {
            s.exported = true;
            Item::StructDef(s)
        }
        Item::EnumDef(mut e) => {
            e.exported = true;
            Item::EnumDef(e)
        }
        other => other,
    }
}

/// Backing integer type for a synthesized inline field enum: the smallest
/// unsigned that holds the largest resolved discriminant. Mirrors the
/// resolver's positional rule (an explicit `= n` sets the value and the next
/// auto-increment, an omitted value is previous+1, starting at 0), so the
/// chosen type never under-sizes the enum (`collect_enum` would otherwise raise
/// `E323`). Values past `u32::MAX` get `u32` and fail in the resolver, where
/// the proper diagnostic lives.
fn enum_backing_ty(variants: &[EnumVariantDef], span: Span) -> TypeExpr {
    let mut next: u64 = 0;
    let mut max: u64 = 0;
    for v in variants {
        let val = v.value.unwrap_or(next);
        max = max.max(val);
        next = val.wrapping_add(1);
    }
    let name = if u8::try_from(max).is_ok() {
        "u8"
    } else if u16::try_from(max).is_ok() {
        "u16"
    } else {
        "u32"
    };
    TypeExpr::Named((name.to_string(), span))
}

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
    /// Spans of wrapping-arithmetic expressions (`+%`, `-%`, `*%` and their
    /// compound forms). Collected here -- the parser is the one place that
    /// sees every wrap operator by construction -- and carried on `Program`
    /// so the verifier can suppress V130 (unsigned-int-overflow) exactly
    /// where wrap was declared. See `BinaryOp::AddWrap`.
    wrap_spans: Vec<Span>,
    /// Top-level enums synthesized from inline field-enum declarations
    /// (`field F bit[..] enum Name { .. }`). The parser is the one place that
    /// elaborates them; they are drained into `Program.items` in
    /// `parse_program` so they flow through the normal top-level enum pipeline
    /// (resolver/checker/codegen unchanged).
    synth_enums: Vec<EnumDef>,
    /// Top-level consts synthesized from `pio NAME { .. }` blocks: the encoded
    /// `NAME_PROGRAM: [u16; N]` array and its metadata (`NAME_WRAP`, etc.).
    /// Drained into `Program.items` in `parse_program`, exactly like
    /// `synth_enums`, so the block never reaches the checker or codegen.
    synth_consts: Vec<ConstDef>,
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
            wrap_spans: Vec::new(),
            synth_enums: Vec::new(),
            synth_consts: Vec::new(),
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
        // Enums synthesized from inline field enums, appended as ordinary
        // top-level items. Order is irrelevant: the resolver collects every
        // enum before resolving peripheral field types (pass 2e).
        items.extend(
            std::mem::take(&mut self.synth_enums)
                .into_iter()
                .map(Item::EnumDef),
        );
        // Consts synthesized from `pio { }` blocks, appended as ordinary
        // top-level items (same rationale as the enums above).
        items.extend(
            std::mem::take(&mut self.synth_consts)
                .into_iter()
                .map(Item::ConstDef),
        );
        Program {
            items,
            wrap_spans: std::mem::take(&mut self.wrap_spans),
        }
    }

    /// True when the upcoming tokens are `peripheral IDENT :` -- i.e. an instance
    /// of a `peripheral_type`, as opposed to an anonymous `peripheral IDENT at`.
    /// Assumes `peek_kind() == Peripheral`.
    fn peripheral_instance_ahead(&self) -> bool {
        self.tokens.get(self.pos + 2).map(|t| &t.kind) == Some(&TokenKind::Colon)
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
        // Never step past the trailing `Eof` token: a recovery `advance()` at
        // EOF (e.g. a stray `export` at end of input) must leave `peek()` valid.
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
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
        // `export` is a declaration-site modifier: it marks the following
        // definition public (reachable from importers as `module.name`).
        let export_span = self.peek_span();
        let exported = self.eat(&TokenKind::Export);

        let item = match self.peek_kind() {
            TokenKind::Extern => self.parse_extern_fn_def().map(Item::ExternFnDef),
            TokenKind::Fn => self.parse_fn_def().map(Item::FnDef),
            TokenKind::Var => self.parse_static_def().map(Item::StaticDef),
            TokenKind::Const => self.parse_const_def().map(Item::ConstDef),
            TokenKind::Peripheral if self.peripheral_instance_ahead() => self
                .parse_peripheral_instance()
                .map(Item::PeripheralInstance),
            TokenKind::Peripheral => self.parse_peripheral_def().map(Item::PeripheralDef),
            TokenKind::PeripheralType => self.parse_peripheral_type_def().map(Item::PeripheralType),
            TokenKind::Import => self.parse_import().map(Item::Import),
            TokenKind::Owns => self.parse_owns().map(Item::Owns),
            TokenKind::Struct => self.parse_struct_def().map(Item::StructDef),
            TokenKind::Enum => self.parse_enum_def().map(Item::EnumDef),
            TokenKind::ComptimeAssert => {
                let (cond, span) = self.parse_paren_cond("comptime_assert")?;
                Some(Item::ComptimeAssert(ComptimeAssert { cond, span }))
            }
            TokenKind::PioBody(..) => self.parse_pio_block(exported),
            _ => {
                self.diags.error(
                    format!("expected item, found `{:?}`", self.peek_kind()),
                    "E101",
                    self.peek_span(),
                );
                self.advance();
                None
            }
        }?;

        if exported {
            match &item {
                Item::FnDef(_)
                | Item::ExternFnDef(_)
                | Item::StaticDef(_)
                | Item::ConstDef(_)
                | Item::PeripheralDef(_)
                | Item::PeripheralInstance(_)
                | Item::StructDef(_)
                | Item::EnumDef(_) => {}
                _ => {
                    self.diags.error(
                        "`export` can only modify a `fn`/`extern fn`/`var`/`const`/\
                         `struct`/`enum`/`peripheral` definition",
                        "E108",
                        export_span,
                    );
                }
            }
            return Some(set_exported(item));
        }
        Some(item)
    }

    /// `pio NAME { ...PIO asm... }` -- assemble the body to instruction words
    /// and desugar to top-level consts. Returns `NAME_PROGRAM: [u16; N]` as the
    /// item; the metadata consts (`NAME_WRAP_TARGET`, `NAME_WRAP`,
    /// `NAME_SIDESET_COUNT`, `NAME_SIDESET_OPT`, `NAME_ORIGIN`) are pushed onto
    /// `synth_consts`. Everything flows through the ordinary const pipeline, so
    /// nothing past the parser knows PIO exists (same discipline as inline
    /// field enums and `peripheral_type`).
    fn parse_pio_block(&mut self, exported: bool) -> Option<Item> {
        let span = self.peek_span();
        let (name, body) = match self.peek_kind() {
            TokenKind::PioBody(n, b) => (n.clone(), b.clone()),
            _ => return None,
        };
        self.advance();

        let asm = match pio_encode::assemble(&name, &body) {
            Ok(a) => a,
            Err(errors) => {
                for e in errors {
                    self.diags.error(
                        format!("in pio program `{name}`: {}", e.msg),
                        "E114",
                        span,
                    );
                }
                return None;
            }
        };

        let u16_ty = TypeExpr::Named(("u16".to_string(), span));
        let u32_ty = || TypeExpr::Named(("u32".to_string(), span));

        // Each synthesized const needs a DISTINCT name span: the import-merge
        // dedups items by their definition span (`push_unique`/`item_def_span`,
        // for diamond imports), so consts sharing the block span would collapse
        // to one. Distinct 1-byte spans within the block keep all of them.
        let name_span =
            |k: usize| Span::new(span.file, span.start + k, span.start + k + 1);

        // NAME_PROGRAM: [u16; N] = [w0, w1, ...] -- the instruction words.
        let elems: Vec<Expr> = asm
            .words
            .iter()
            .map(|&w| Expr::IntLiteral(u64::from(w), IntSuffix::U16, span))
            .collect();
        let n = elems.len() as u64;
        let program = ConstDef {
            // the `export` modifier (if any) is applied to this returned item by
            // the caller via `set_exported`.
            exported: false,
            name: (format!("{name}_PROGRAM"), name_span(0)),
            ty: TypeExpr::Array(
                Box::new(u16_ty),
                Box::new(Expr::IntLiteral(n, IntSuffix::None, span)),
            ),
            value: Expr::ArrayInit(elems, span),
        };

        // Metadata consts (u32, friction-free with the u32 register fields the
        // loader writes). ORIGIN is 0xFFFF_FFFF when the program is relocatable
        // (no `.origin`).
        let origin = asm.origin.map_or(0xFFFF_FFFFu64, u64::from);
        let meta: [(&str, u64); 5] = [
            ("WRAP_TARGET", u64::from(asm.wrap_target)),
            ("WRAP", u64::from(asm.wrap)),
            ("SIDESET_COUNT", u64::from(asm.side_set_count)),
            ("SIDESET_OPT", u64::from(asm.side_set_opt)),
            ("ORIGIN", origin),
        ];
        for (k, (suffix, val)) in meta.into_iter().enumerate() {
            self.synth_consts.push(ConstDef {
                exported,
                name: (format!("{name}_{suffix}"), name_span(k + 1)),
                ty: u32_ty(),
                value: Expr::IntLiteral(val, IntSuffix::None, span),
            });
        }

        Some(Item::ConstDef(program))
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
            exported: false,
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
            exported: false,
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
            exported: false,
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
                // Bare `@shared`: the ceiling is derived from the accessor
                // contexts (ceiling.rs). `@shared(ceiling=N)` pins it.
                if !matches!(self.peek_kind(), TokenKind::LParen) {
                    return Some(StorageAnnotation::Shared(None));
                }
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
                Some(StorageAnnotation::Shared(Some(prio as u8)))
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
        Some(ConstDef {
            exported: false,
            name,
            ty,
            value,
        })
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
            let (endian, extent) = self.parse_field_attrs()?;
            fields.push(StructFieldDef {
                name: field_name,
                ty: field_ty,
                endian,
                extent,
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }

        self.expect(&TokenKind::RBrace, "expected `}`").ok()?;

        Some(StructDef {
            exported: false,
            name,
            repr,
            fields,
        })
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

    /// Parse the optional attributes following a struct field type:
    /// `@be`/`@le` (byte order) and `@extent(addr_field [, xN])` (transfer
    /// length for the buffer delivered through the named sibling). Each may
    /// appear at most once, in either order.
    fn parse_field_attrs(
        &mut self,
    ) -> Option<(crate::ast::FieldEndian, Option<crate::ast::FieldExtent>)> {
        use crate::ast::FieldEndian;
        let mut endian = FieldEndian::Native;
        let mut extent: Option<crate::ast::FieldExtent> = None;
        while self.eat(&TokenKind::AtSign) {
            let span = self.peek_span();
            let TokenKind::Ident(name) = self.peek_kind() else {
                self.diags.error(
                    "expected `be`, `le`, or `extent` after `@` in struct field",
                    "E108",
                    span,
                );
                return None;
            };
            match name.as_str() {
                "be" => {
                    endian = FieldEndian::Big;
                    self.advance();
                }
                "le" => {
                    endian = FieldEndian::Little;
                    self.advance();
                }
                "extent" => {
                    self.advance();
                    self.expect(&TokenKind::LParen, "expected `(` after `@extent`")
                        .ok()?;
                    let addr_field = self.parse_ident()?;
                    let mut scale = 1u32;
                    let mut mask: Option<u64> = None;
                    // Optional, in fixed order: `, xN` then `, mask N`.
                    if self.eat(&TokenKind::Comma) {
                        let mspan = self.peek_span();
                        let TokenKind::Ident(tok) = self.peek_kind() else {
                            self.diags.error(
                                "expected `xN` or `mask N` in `@extent(field, ...)`",
                                "E108",
                                mspan,
                            );
                            return None;
                        };
                        if tok.starts_with('x') {
                            match tok.strip_prefix('x').and_then(|n| n.parse::<u32>().ok()) {
                                Some(n) if n > 0 => scale = n,
                                _ => {
                                    self.diags.error(
                                        "expected `xN` multiplier (N > 0) in `@extent(field, xN)`",
                                        "E108",
                                        mspan,
                                    );
                                    return None;
                                }
                            }
                            self.advance();
                            // An optional `, mask N` may follow the multiplier.
                            if self.eat(&TokenKind::Comma) {
                                mask = Some(self.parse_extent_mask()?);
                            }
                        } else {
                            // No multiplier; this first comma arg must be `mask N`.
                            mask = Some(self.parse_extent_mask()?);
                        }
                    }
                    self.expect(&TokenKind::RParen, "expected `)` to close `@extent`")
                        .ok()?;
                    extent = Some(crate::ast::FieldExtent {
                        addr_field,
                        scale,
                        mask,
                    });
                }
                _ => {
                    self.diags.error(
                        "expected `be`, `le`, or `extent` after `@` in struct field",
                        "E108",
                        span,
                    );
                    return None;
                }
            }
        }
        Some((endian, extent))
    }

    /// Parse the `mask N` tail of `@extent(field, [xN,] mask N)`: the `mask`
    /// keyword (an ident) followed by an integer literal.
    fn parse_extent_mask(&mut self) -> Option<u64> {
        let span = self.peek_span();
        let TokenKind::Ident(kw) = self.peek_kind() else {
            self.diags
                .error("expected `mask N` in `@extent`", "E108", span);
            return None;
        };
        if kw.as_str() != "mask" {
            self.diags.error(
                "expected `xN` or `mask N` in `@extent(field, ...)`",
                "E108",
                span,
            );
            return None;
        }
        self.advance(); // `mask`
        self.parse_int_literal()
    }

    fn parse_enum_def(&mut self) -> Option<EnumDef> {
        self.advance(); // enum
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Colon, "expected `:` after enum name")
            .ok()?;
        let ty = self.parse_type_expr()?;
        let variants = self.parse_enum_body()?;

        Some(EnumDef {
            exported: false,
            name,
            ty,
            variants,
        })
    }

    /// Parse the `{ V0 [= n0], V1 [= n1], ... }` variant body of an enum,
    /// shared by top-level `enum` declarations and inline field enums. Assumes
    /// the opening `{` is next.
    fn parse_enum_body(&mut self) -> Option<Vec<EnumVariantDef>> {
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
        Some(variants)
    }

    fn parse_peripheral_def(&mut self) -> Option<PeripheralDef> {
        self.advance(); // peripheral
        let name = self.parse_ident()?;
        self.expect(&TokenKind::At, "expected `at`").ok()?;
        let addr = self.parse_int_literal()?;
        let regs = self.parse_reg_body()?;
        Some(PeripheralDef {
            exported: false,
            name,
            of_type: None,
            base_addr: addr,
            regs,
        })
    }

    /// `peripheral_type NAME { reg ... }` -- a register-layout template (no
    /// address). Same body grammar as a peripheral; collected and elaborated
    /// away after the import merge. Assumes `peek_kind() == PeripheralType`.
    fn parse_peripheral_type_def(&mut self) -> Option<PeripheralTypeDef> {
        self.advance(); // peripheral_type
        let name = self.parse_ident()?;
        let regs = self.parse_reg_body()?;
        Some(PeripheralTypeDef { name, regs })
    }

    /// Parse the `{ reg ... }` register list shared by `peripheral` and
    /// `peripheral_type`. Assumes the opening `{` is next.
    fn parse_reg_body(&mut self) -> Option<Vec<RegDef>> {
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
        Some(regs)
    }

    /// `peripheral NAME: TYPE at ADDR;` -- an instance of a `peripheral_type`.
    /// Assumes `peek_kind() == Peripheral` and `peripheral_instance_ahead()`.
    fn parse_peripheral_instance(&mut self) -> Option<PeripheralInstanceDef> {
        self.advance(); // peripheral
        let name = self.parse_ident()?;
        self.expect(&TokenKind::Colon, "expected `:`").ok()?;
        let type_name = self.parse_ident()?;
        self.expect(&TokenKind::At, "expected `at`").ok()?;
        let base_addr = self.parse_int_literal()?;
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(PeripheralInstanceDef {
            exported: false,
            name,
            type_name,
            base_addr,
        })
    }

    fn parse_reg_def(&mut self) -> Option<RegDef> {
        self.expect(&TokenKind::Reg, "expected `reg`").ok()?;
        let name = self.parse_ident()?;

        // Optional register-array length: `reg NAME[len] ...`. The matching
        // `stride S` after the offset is then required (a register array with no
        // stride is meaningless).
        let array_len = if self.eat(&TokenKind::LBracket) {
            let n = self.parse_int_literal()?;
            self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
            Some(n)
        } else {
            None
        };

        self.expect(&TokenKind::Offset, "expected `offset`").ok()?;
        let offset = self.parse_int_literal()?;

        // Optional `stride S` (contextual keyword, like `view T stride K`).
        let stride = if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "stride") {
            self.advance();
            Some(self.parse_int_literal()?)
        } else {
            None
        };

        let array = match (array_len, stride) {
            (Some(len), Some(stride)) => Some(crate::ast::RegArray { len, stride }),
            (Some(_), None) => {
                self.diags.error(
                    "register array `reg NAME[N]` needs a `stride S` after the offset",
                    "E116",
                    name.1,
                );
                None
            }
            (None, Some(_)) => {
                self.diags.error(
                    "`stride` is only valid on a register array `reg NAME[N]`",
                    "E116",
                    name.1,
                );
                None
            }
            (None, None) => None,
        };

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
            array,
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

        // Optional explicit type before the bit spec. Omitted when an inline
        // `enum { .. }` follows the bit spec (which then supplies the type).
        let explicit_ty = if self.eat(&TokenKind::Colon) {
            Some(self.parse_type_expr()?)
        } else {
            None
        };

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

        // Optional inline enum: `enum Name { V = n, ... }`. Desugars to a
        // synthesized `export enum Name` (collected in `synth_enums`, emitted as
        // a top-level item) plus a field whose type names it -- reusing the
        // enum-typed-field path end to end.
        let inline_enum_ty = if self.check(&TokenKind::Enum) {
            self.advance(); // enum
            let enum_name = self.parse_ident()?;
            let variants = self.parse_enum_body()?;
            let backing = enum_backing_ty(&variants, enum_name.1);
            self.synth_enums.push(EnumDef {
                exported: true,
                name: enum_name.clone(),
                ty: backing,
                variants,
            });
            Some(TypeExpr::Named(enum_name))
        } else {
            None
        };

        let access = self.parse_access_modifier();

        // Exactly one of {explicit type, inline enum} supplies the field type.
        let ty = match (explicit_ty, inline_enum_ty) {
            (Some(_), Some(_)) => {
                self.diags.error(
                    "field has both an explicit type and an inline enum; use one",
                    "E110",
                    name.1,
                );
                return None;
            }
            (Some(t), None) | (None, Some(t)) => t,
            (None, None) => {
                self.diags.error(
                    "field needs a type: `field NAME: TYPE bit[..]` or `field NAME bit[..] enum N { .. }`",
                    "E111",
                    name.1,
                );
                return None;
            }
        };

        Some(FieldDef {
            name,
            ty,
            bit_spec,
            access,
        })
    }

    fn parse_import(&mut self) -> Option<ImportStmt> {
        self.advance(); // import

        let mut module = vec![self.parse_path_segment()?];
        while self.eat(&TokenKind::Dot) {
            module.push(self.parse_path_segment()?);
        }

        // Selective import (`import m { a, b };`) was removed. Reject the `{`
        // form with a clear message, and recover by consuming the brace group so
        // the rest of the file still parses.
        if self.check(&TokenKind::LBrace) {
            let brace = self.peek().span;
            self.advance(); // {
            self.diags.error(
                "selective import `import m { ... }` is no longer supported; use \
                 `import m;` (brings the module's items into scope) or \
                 `import m as alias;`",
                "E109",
                brace,
            );
            while !self.check(&TokenKind::RBrace) && !self.is_eof() {
                self.advance();
            }
            self.eat(&TokenKind::RBrace);
        }

        // optional `as alias`
        let alias = if self.eat(&TokenKind::As) {
            Some(self.parse_ident()?)
        } else {
            None
        };

        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(ImportStmt { module, alias })
    }

    /// `owns P, P.R, gpio[a..b], ...;` -- a module's exclusive claims. Each path
    /// is a peripheral (optionally `.register`), or `gpio[lo..hi]` for an
    /// exclusive GPIO-pin range. Field-level register paths (`P.R.F`) are
    /// rejected (see doc/regions-agents.md).
    fn parse_owns(&mut self) -> Option<OwnsStmt> {
        self.advance(); // owns
        let mut paths = Vec::new();
        loop {
            // `gpio[lo..hi]` -- a GPIO-pin claim (`gpio` is a contextual keyword).
            if matches!(self.peek_kind(), TokenKind::Ident(s) if s == "gpio") {
                let start = self.peek_span();
                self.advance(); // gpio
                self.expect(&TokenKind::LBracket, "expected `[` after `gpio`")
                    .ok()?;
                let lo = self.parse_int_literal()?;
                self.expect(&TokenKind::DotDot, "expected `..` in `gpio[lo..hi]`")
                    .ok()?;
                let hi = self.parse_int_literal()?;
                let end = self.peek_span();
                self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
                if lo > hi {
                    self.diags.error(
                        format!("invalid gpio range `[{lo}..{hi}]` (low must be <= high)"),
                        "E116",
                        start.merge(end),
                    );
                    return None;
                }
                paths.push(OwnsPath {
                    target: crate::ast::OwnsTarget::Gpio { lo, hi },
                    span: start.merge(end),
                });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                continue;
            }

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
                target: crate::ast::OwnsTarget::Reg {
                    peripheral,
                    register,
                },
                span,
            });
            if !self.eat(&TokenKind::Comma) {
                break;
            }
        }
        self.expect(&TokenKind::Semicolon, "expected `;`").ok()?;
        Some(OwnsStmt { paths })
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
                // Qualified type name `module.Type` (an imported type via its
                // import name or alias), stored as the dotted string
                // `"module.Type"`; the import resolver rewrites/keys on it.
                if self.check(&TokenKind::Dot) {
                    self.advance(); // .
                    let ty = self.parse_ident()?;
                    let span = name.1.merge(ty.1);
                    return Some(TypeExpr::Named((format!("{}.{}", name.0, ty.0), span)));
                }
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
            TokenKind::Claim => self.parse_claim_stmt().map(Stmt::Claim),
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
                    if matches!(
                        op,
                        BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                    ) {
                        self.wrap_spans.push(span.merge(value.span()));
                    }
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

    /// `claim X { ... }` -- a masked ownership window over the `@shared`
    /// static `X` (the CPU-side counterpart of `reclaim`).
    fn parse_claim_stmt(&mut self) -> Option<ClaimStmt> {
        self.advance(); // claim
        let name = self.parse_ident()?;
        let body = self.parse_block()?;
        Some(ClaimStmt { name, body })
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
                    TokenKind::PlusPercent => Some(BinaryOp::AddWrap),
                    TokenKind::MinusPercent => Some(BinaryOp::SubWrap),
                    TokenKind::StarPercent => Some(BinaryOp::MulWrap),
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
                        if matches!(
                            op,
                            BinaryOp::AddWrap | BinaryOp::SubWrap | BinaryOp::MulWrap
                        ) {
                            self.wrap_spans.push(left.span().merge(right.span()));
                        }
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
                // Enum variant `Enum@V` or qualified `module.Enum@V`. The base is
                // a bare ident or a `module.Enum` field access; the qualified form
                // is stored as the dotted enum name `"module.Enum"`.
                TokenKind::AtSign if qualified_name(&left).is_some() => {
                    self.advance();
                    let variant = self.parse_ident()?;
                    let enum_name = qualified_name(&left).unwrap();
                    let span = enum_name.1.merge(variant.1);
                    left = Expr::EnumVariant {
                        enum_name,
                        variant,
                        span,
                    };
                }
                TokenKind::LBracket => {
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&TokenKind::RBracket, "expected `]`").ok()?;
                    left = Expr::Index(Box::new(left), Box::new(index));
                }
                // Struct init `Struct { ... }` or qualified `module.Struct { ... }`,
                // stored as the dotted name `"module.Struct"`.
                TokenKind::LBrace if allow_struct && qualified_name(&left).is_some() => {
                    let name = qualified_name(&left).unwrap();
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
                            // Tolerate a trailing comma (`T { a: 1, b: 2, }`),
                            // matching `parse_struct_def`'s field list.
                            if self.check(&TokenKind::RBrace) {
                                break;
                            }
                        }
                    }
                    let end_span = self.peek_span();
                    self.expect(&TokenKind::RBrace, "expected `}`").ok()?;
                    let span = name.1.merge(end_span);
                    left = Expr::StructInit { name, fields, span };
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
                    reclaim: false,
                    span,
                })
            }
            TokenKind::Reclaim => {
                // `reclaim(arr)`: the explicit, handshake-acknowledged view over
                // agent-shared memory. Contiguous form only -- no len/stride.
                let span = self.peek_span();
                self.advance();
                self.expect(&TokenKind::LParen, "expected `(` after `reclaim`")
                    .ok()?;
                let base = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "expected `)` to close `reclaim(...)`")
                    .ok()?;
                Some(Expr::ViewNew {
                    base: Box::new(base),
                    len: None,
                    stride: None,
                    reclaim: true,
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

    /// Parse one module-path segment: an identifier, or a keyword used as a
    /// name. A module file may be named after a peripheral that collides with a
    /// BML keyword (e.g. `dma.bml`); the import path is only a file locator, so
    /// the keyword's spelling is recovered and accepted here. This is scoped to
    /// import paths -- ordinary identifiers (vars, fns, ...) stay keyword-free.
    fn parse_path_segment(&mut self) -> Option<Ident> {
        if let TokenKind::Ident(s) = self.peek_kind() {
            let name = s.clone();
            let span = self.peek_span();
            self.advance();
            Some((name, span))
        } else if let Some(kw) = crate::lexer::keyword_text(self.peek_kind()) {
            let span = self.peek_span();
            self.advance();
            Some((kw.to_string(), span))
        } else {
            self.diags.error(
                format!("expected module name, found `{:?}`", self.peek_kind()),
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
        TokenKind::PlusPercentEq => BinaryOp::AddWrap,
        TokenKind::MinusPercentEq => BinaryOp::SubWrap,
        TokenKind::StarPercentEq => BinaryOp::MulWrap,
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
    use crate::ast::{BinaryOp, ConstDef, Expr, Item, UnaryOp};
    use crate::errors::DiagnosticBag;
    use crate::source::FileId;

    fn parse_expr_str(src: &str) -> Expr {
        let mut diags = DiagnosticBag::new();
        let expr = Parser::new(src, FileId::new(), &mut diags).parse_expr();
        assert!(!diags.has_errors(), "unexpected parse errors for `{src}`");
        expr.expect("expression should parse")
    }

    // A module file may be named after a peripheral that collides with a BML
    // keyword (e.g. RP2350's `dma.bml`, where `dma` is the `@dma` keyword). The
    // import path is a locator, so a keyword is accepted as a segment.
    #[test]
    fn import_path_segment_may_be_a_keyword() {
        let mut diags = DiagnosticBag::new();
        let program =
            Parser::new("import rp2350.svd.dma;", FileId::new(), &mut diags).parse_program();
        assert!(!diags.has_errors(), "keyword module segment should parse");
        let import = program
            .items
            .iter()
            .find_map(|it| match it {
                crate::ast::Item::Import(i) => Some(i),
                _ => None,
            })
            .expect("an import item");
        let segs: Vec<&str> = import.module.iter().map(|(s, _)| s.as_str()).collect();
        assert_eq!(segs, ["rp2350", "svd", "dma"]);
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

    // `+%` has additive precedence: `a +% b * c` is `a +% (b * c)`.
    #[test]
    fn wrap_add_precedence_matches_add() {
        let Expr::Binary(_, BinaryOp::AddWrap, right) = parse_expr_str("a +% b * c") else {
            panic!("expected outer AddWrap");
        };
        assert!(matches!(*right, Expr::Binary(_, BinaryOp::Mul, _)));
    }

    // `a + %b` is not a wrap op and must not lex as one: binary `%` has no
    // left operand there, so the parse errors instead of silently regrouping.
    #[test]
    fn plus_then_percent_is_not_wrap() {
        let mut diags = DiagnosticBag::new();
        let _ = Parser::new("a + % b", FileId::new(), &mut diags).parse_expr();
        assert!(diags.has_errors(), "`a + % b` should not parse");
    }

    // `x +%= 1;` parses as a CompoundAssign carrying AddWrap, and the parser
    // records a wrap span for the verifier.
    #[test]
    fn wrap_compound_assign_parses_and_records_span() {
        let mut diags = DiagnosticBag::new();
        let src = "fn f() { var x: u32 = 0; x +%= 1; x = x -% 2; }";
        let program = Parser::new(src, FileId::new(), &mut diags).parse_program();
        assert!(!diags.has_errors(), "unexpected parse errors");
        assert_eq!(
            program.wrap_spans.len(),
            2,
            "one span per wrap operation (compound + binary)"
        );
    }

    // A `pio { }` block desugars to `NAME_PROGRAM: [u16; N]` plus metadata
    // consts; the encoded words match the M1 hand-encoded blink exactly.
    #[test]
    fn pio_block_desugars_to_program_and_metadata() {
        let mut diags = DiagnosticBag::new();
        let src = "
            pio blink {
            .wrap_target
                set pins, 1
                set x, 31
            on:
                jmp x--, on [31]
                set pins, 0
                set x, 31
            off:
                jmp x--, off [31]
            .wrap
            }
        ";
        let program = Parser::new(src, FileId::new(), &mut diags).parse_program();
        assert!(!diags.has_errors(), "unexpected parse errors");

        let consts: std::collections::HashMap<String, &ConstDef> = program
            .items
            .iter()
            .filter_map(|i| match i {
                Item::ConstDef(c) => Some((c.name.0.clone(), c)),
                _ => None,
            })
            .collect();

        let program_const = consts.get("blink_PROGRAM").expect("blink_PROGRAM exists");
        let Expr::ArrayInit(elems, _) = &program_const.value else {
            panic!("blink_PROGRAM is not an array init");
        };
        let words: Vec<u64> = elems
            .iter()
            .map(|e| match e {
                Expr::IntLiteral(v, _, _) => *v,
                _ => panic!("non-literal program word"),
            })
            .collect();
        assert_eq!(words, vec![0xE001, 0xE03F, 0x1F42, 0xE000, 0xE03F, 0x1F45]);

        let meta = |name: &str| match &consts.get(name).expect("metadata const").value {
            Expr::IntLiteral(v, _, _) => *v,
            _ => panic!("metadata is not a literal"),
        };
        assert_eq!(meta("blink_WRAP_TARGET"), 0);
        assert_eq!(meta("blink_WRAP"), 5);
        assert_eq!(meta("blink_SIDESET_COUNT"), 0);
        assert_eq!(meta("blink_ORIGIN"), 0xFFFF_FFFF); // relocatable
    }

    // A PIO assembly error inside the block surfaces as a bml diagnostic rather
    // than silently producing a wrong program.
    #[test]
    fn pio_block_syntax_error_is_reported() {
        let mut diags = DiagnosticBag::new();
        let src = "pio bad { set pins, 99 }"; // SET value out of range 0..31
        let _ = Parser::new(src, FileId::new(), &mut diags).parse_program();
        assert!(diags.has_errors(), "an invalid pio program must report an error");
    }

    // A struct LITERAL may carry a trailing comma (`T { a: 1, b: 2, }`), matching
    // the struct DECLARATION field list. Previously this errored on the `}`.
    #[test]
    fn struct_literal_allows_trailing_comma() {
        let mut diags = DiagnosticBag::new();
        let src = "struct T { a: u32, b: u32 }\n\
                   fn f() -> T { return T { a: 1, b: 2, }; }";
        let _ = Parser::new(src, FileId::new(), &mut diags).parse_program();
        assert!(!diags.has_errors(), "trailing comma in a struct literal must parse");
    }

    // The tolerance is exactly one trailing comma: a doubled comma (an empty
    // field slot) is still an error.
    #[test]
    fn struct_literal_rejects_double_comma() {
        let mut diags = DiagnosticBag::new();
        let src = "struct T { a: u32, b: u32 }\n\
                   fn f() -> T { return T { a: 1, , b: 2 }; }";
        let _ = Parser::new(src, FileId::new(), &mut diags).parse_program();
        assert!(diags.has_errors(), "a doubled comma must still be rejected");
    }
}
