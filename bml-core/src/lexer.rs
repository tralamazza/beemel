use crate::ast::{FloatSuffix, IntSuffix};
use crate::errors::DiagnosticBag;
use crate::source::{FileId, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Extern,
    Fn,
    Var,
    Const,
    Peripheral,
    Reg,
    Field,
    At,
    Offset,
    Bit,
    If,
    Else,
    Loop,
    While,
    For,
    In,
    Upto,
    Downto,
    Step,
    Return,
    Break,
    Continue,
    Import,
    Export,
    As,
    Context,
    Thread,
    Isr,
    Priority,
    Any,
    Exclusive,
    Shared,
    Ceiling,
    Dma,
    External,
    Section,
    Align,
    Mut,
    Null,
    Struct,
    Sizeof,
    View,
    Ring,
    Bits,
    Enum,
    Match,
    Naked,
    Tailchain,
    Readonly,
    Writeonly,
    Assume,
    Assert,
    ComptimeAssert,
    Asm,
    AsmBody(String),

    // Literals
    IntLiteral(u64, IntSuffix),
    FloatLiteral(f64, FloatSuffix),
    BoolLiteral(bool),
    StringLiteral(String),
    Ident(String),

    // Symbols
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Semicolon,
    Dot,
    DotDot,
    Arrow,
    AtSign,

    // Operators
    Eq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Tilde,
    EqEq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
    Bang,
    Shl,
    Shr,

    // Compound assignment: `+=`, `-=`, `*=`, `/=`, `%=`, `&=`, `|=`, `^=`,
    // `<<=`, `>>=`. Distinct tokens so the expression parser does not consume
    // the operator; the statement parser desugars `a OP= b` to `a = a OP b`.
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,

    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub struct Lexer<'a> {
    source: &'a str,
    file: FileId,
    pos: usize,
    diags: &'a mut DiagnosticBag,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str, file: FileId, diags: &'a mut DiagnosticBag) -> Self {
        Lexer {
            source,
            file,
            pos: 0,
            diags,
        }
    }

    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start, end)
    }

    fn current(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.source[self.pos..].chars().nth(offset)
    }

    fn advance(&mut self) {
        if let Some(c) = self.current() {
            self.pos += c.len_utf8();
        }
    }

    fn advance_by(&mut self, n: usize) {
        for _ in 0..n {
            self.advance();
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match self.peek(0) {
                Some(' ' | '\t' | '\n' | '\r') => {
                    self.advance();
                }
                Some('/') if self.peek(1) == Some('/') => {
                    self.advance_by(2);
                    while let Some(c) = self.current() {
                        if c == '\n' {
                            break;
                        }
                        self.advance();
                    }
                }
                Some('/') if self.peek(1) == Some('*') => {
                    let start = self.pos;
                    self.advance_by(2);
                    let mut depth = 1;
                    while depth > 0 {
                        match (self.current(), self.peek(1)) {
                            (Some('/'), Some('*')) => {
                                self.advance_by(2);
                                depth += 1;
                            }
                            (Some('*'), Some('/')) => {
                                self.advance_by(2);
                                depth -= 1;
                            }
                            (Some(_), _) => {
                                self.advance();
                            }
                            _ => {
                                self.diags.error(
                                    "unterminated block comment",
                                    "E001",
                                    self.span(start, self.pos),
                                );
                                return;
                            }
                        }
                    }
                }
                _ => break,
            }
        }
    }

    fn read_ident_range(&mut self) -> (usize, usize) {
        let start = self.pos;
        while let Some(c) = self.current() {
            if c.is_alphanumeric() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }
        (start, self.pos)
    }

    fn read_number(&mut self) -> TokenKind {
        let start = self.pos;
        let mut is_hex = false;
        let mut is_float = false;

        if self.current() == Some('0') && self.peek(1).is_some_and(|c| c == 'x' || c == 'X') {
            self.advance_by(2);
            is_hex = true;
        }

        while let Some(c) = self.current() {
            if c.is_ascii_digit() || (is_hex && c.is_ascii_hexdigit()) || c == '_' {
                self.advance();
            } else {
                break;
            }
        }

        // Float literal: digits.digits  (only for non-hex)
        if !is_hex
            && self.current() == Some('.')
            && self.peek(1).is_some_and(|c| c.is_ascii_digit())
        {
            is_float = true;
            self.advance(); // consume '.'
            while let Some(c) = self.current() {
                if c.is_ascii_digit() || c == '_' {
                    self.advance();
                } else {
                    break;
                }
            }
        }

        // Scientific notation: optional e/E with optional sign and exponent digits
        if !is_hex && self.current().is_some_and(|c| c == 'e' || c == 'E') {
            is_float = true;
            self.advance(); // consume 'e'/'E'
            if self.current().is_some_and(|c| c == '+' || c == '-') {
                self.advance(); // consume optional sign
            }
            if self.current().is_some_and(|c| c.is_ascii_digit()) {
                while let Some(c) = self.current() {
                    if c.is_ascii_digit() || c == '_' {
                        self.advance();
                    } else {
                        break;
                    }
                }
            } else {
                self.diags.error(
                    "invalid float literal: missing exponent digits",
                    "E002",
                    self.span(start, self.pos),
                );
                return TokenKind::FloatLiteral(0.0, FloatSuffix::None);
            }
        }

        // Parse float suffix: h (f16), f (f32), d (f64) -- optional
        let suffix = if is_float {
            match self.current() {
                Some('h') => FloatSuffix::H,
                Some('f') => FloatSuffix::F,
                Some('d') => FloatSuffix::D,
                _ => FloatSuffix::None,
            }
        } else {
            FloatSuffix::None
        };

        let num_str: String = self.source[start..self.pos]
            .chars()
            .filter(|c| *c != '_')
            .collect();

        // Advance past float suffix character after building num_str
        if suffix != FloatSuffix::None {
            self.advance();
        }

        if is_float {
            if let Ok(v) = num_str.parse::<f64>() {
                TokenKind::FloatLiteral(v, suffix)
            } else {
                self.diags.error(
                    format!("invalid float literal: `{num_str}`"),
                    "E002",
                    self.span(start, self.pos),
                );
                TokenKind::FloatLiteral(0.0, FloatSuffix::None)
            }
        } else {
            // Parse integer suffix: i8/i16/i32/i64/u8/u16/u32/u64
            let int_suffix = parse_int_suffix(self);
            let value = if is_hex {
                u64::from_str_radix(&num_str[2..], 16)
            } else {
                num_str.parse()
            };

            if let Ok(v) = value {
                TokenKind::IntLiteral(v, int_suffix)
            } else {
                self.diags.error(
                    format!("invalid number literal: `{num_str}`"),
                    "E002",
                    self.span(start, self.pos),
                );
                TokenKind::IntLiteral(0, IntSuffix::None)
            }
        }
    }

    fn read_string(&mut self) -> TokenKind {
        let start = self.pos;
        self.advance(); // skip opening "
        let mut s = String::new();
        loop {
            match self.current() {
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.current() {
                        Some('n') => {
                            s.push('\n');
                            self.advance();
                        }
                        Some('t') => {
                            s.push('\t');
                            self.advance();
                        }
                        Some('r') => {
                            s.push('\r');
                            self.advance();
                        }
                        Some('\\') => {
                            s.push('\\');
                            self.advance();
                        }
                        Some('"') => {
                            s.push('"');
                            self.advance();
                        }
                        Some('0') => {
                            s.push('\0');
                            self.advance();
                        }
                        Some(c) => {
                            self.diags.error(
                                format!("unknown escape sequence: \\{c}"),
                                "E003",
                                self.span(self.pos, self.pos + c.len_utf8()),
                            );
                            self.advance();
                        }
                        None => {
                            self.diags.error(
                                "unterminated string literal",
                                "E004",
                                self.span(start, self.pos),
                            );
                            break;
                        }
                    }
                }
                Some(_) => {
                    s.push(self.current().unwrap());
                    self.advance();
                }
                None => {
                    self.diags.error(
                        "unterminated string literal",
                        "E004",
                        self.span(start, self.pos),
                    );
                    break;
                }
            }
        }
        TokenKind::StringLiteral(s)
    }

    pub fn next_token(&mut self) -> Token {
        loop {
            self.skip_whitespace_and_comments();

            let start = self.pos;
            let Some(c) = self.current() else {
                return Token {
                    kind: TokenKind::Eof,
                    span: self.span(start, start),
                };
            };

            let kind = match c {
                '(' => {
                    self.advance();
                    TokenKind::LParen
                }
                ')' => {
                    self.advance();
                    TokenKind::RParen
                }
                '{' => {
                    self.advance();
                    TokenKind::LBrace
                }
                '}' => {
                    self.advance();
                    TokenKind::RBrace
                }
                '[' => {
                    self.advance();
                    TokenKind::LBracket
                }
                ']' => {
                    self.advance();
                    TokenKind::RBracket
                }
                ',' => {
                    self.advance();
                    TokenKind::Comma
                }
                ':' => {
                    self.advance();
                    TokenKind::Colon
                }
                ';' => {
                    self.advance();
                    TokenKind::Semicolon
                }
                '.' => {
                    if self.peek(1) == Some('.') {
                        self.advance_by(2);
                        TokenKind::DotDot
                    } else {
                        self.advance();
                        TokenKind::Dot
                    }
                }
                '+' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::PlusEq
                    } else {
                        TokenKind::Plus
                    }
                }
                '-' => {
                    self.advance();
                    if self.current() == Some('>') {
                        self.advance();
                        TokenKind::Arrow
                    } else if self.current() == Some('=') {
                        self.advance();
                        TokenKind::MinusEq
                    } else {
                        TokenKind::Minus
                    }
                }
                '*' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::StarEq
                    } else {
                        TokenKind::Star
                    }
                }
                '/' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::SlashEq
                    } else {
                        TokenKind::Slash
                    }
                }
                '%' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::PercentEq
                    } else {
                        TokenKind::Percent
                    }
                }
                '~' => {
                    self.advance();
                    TokenKind::Tilde
                }
                '@' => {
                    self.advance();
                    TokenKind::AtSign
                }
                '!' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::NotEq
                    } else {
                        TokenKind::Bang
                    }
                }
                '=' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::EqEq
                    } else {
                        TokenKind::Eq
                    }
                }
                '<' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::LtEq
                    } else if self.current() == Some('<') {
                        self.advance();
                        if self.current() == Some('=') {
                            self.advance();
                            TokenKind::ShlEq
                        } else {
                            TokenKind::Shl
                        }
                    } else {
                        TokenKind::Lt
                    }
                }
                '>' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::GtEq
                    } else if self.current() == Some('>') {
                        self.advance();
                        if self.current() == Some('=') {
                            self.advance();
                            TokenKind::ShrEq
                        } else {
                            TokenKind::Shr
                        }
                    } else {
                        TokenKind::Gt
                    }
                }
                '&' => {
                    self.advance();
                    if self.current() == Some('&') {
                        self.advance();
                        TokenKind::And
                    } else if self.current() == Some('=') {
                        self.advance();
                        TokenKind::AmpEq
                    } else {
                        TokenKind::Amp
                    }
                }
                '|' => {
                    self.advance();
                    if self.current() == Some('|') {
                        self.advance();
                        TokenKind::Or
                    } else if self.current() == Some('=') {
                        self.advance();
                        TokenKind::PipeEq
                    } else {
                        TokenKind::Pipe
                    }
                }
                '^' => {
                    self.advance();
                    if self.current() == Some('=') {
                        self.advance();
                        TokenKind::CaretEq
                    } else {
                        TokenKind::Caret
                    }
                }
                '"' => self.read_string(),
                '0'..='9' => self.read_number(),
                'A'..='Z' | 'a'..='z' | '_' => {
                    let (ident_start, ident_end) = self.read_ident_range();
                    let ident_str = &self.source[ident_start..ident_end];
                    let kind = keyword_or_ident(ident_str);
                    if kind == TokenKind::Asm {
                        // Look ahead for { -- if found, consume raw asm body
                        let mut look = self.pos;
                        while let Some(c) = self.source[look..].chars().next() {
                            if c.is_whitespace() {
                                look += c.len_utf8();
                            } else {
                                break;
                            }
                        }
                        if self.source[look..].starts_with('{') {
                            self.pos = look + 1; // skip past {
                            let body_start = self.pos;
                            let mut depth: u32 = 1;
                            while depth > 0 {
                                match self.current() {
                                    Some('{') => {
                                        depth += 1;
                                        self.advance();
                                    }
                                    Some('}') => {
                                        depth -= 1;
                                        if depth == 0 {
                                            break;
                                        }
                                        self.advance();
                                    }
                                    Some(_) => {
                                        self.advance();
                                    }
                                    None => {
                                        self.diags.error(
                                            "unterminated asm block".to_string(),
                                            "E006",
                                            self.span(body_start, self.pos),
                                        );
                                        return Token {
                                            kind: TokenKind::Eof,
                                            span: self.span(start, self.pos),
                                        };
                                    }
                                }
                            }
                            let body_end = self.pos;
                            self.advance(); // consume closing }
                            let body = self.source[body_start..body_end].trim().to_string();
                            return Token {
                                kind: TokenKind::AsmBody(body),
                                span: self.span(start, self.pos),
                            };
                        }
                    }
                    kind
                }
                _ => {
                    self.advance();
                    self.diags.error(
                        format!("unexpected character: `{c}`"),
                        "E005",
                        self.span(start, self.pos),
                    );
                    continue;
                }
            };

            return Token {
                kind,
                span: self.span(start, self.pos),
            };
        }
    }
}

fn keyword_or_ident(s: &str) -> TokenKind {
    match s {
        "extern" => TokenKind::Extern,
        "fn" => TokenKind::Fn,
        "var" => TokenKind::Var,
        "const" => TokenKind::Const,
        "peripheral" => TokenKind::Peripheral,
        "reg" => TokenKind::Reg,
        "field" => TokenKind::Field,
        "at" => TokenKind::At,
        "offset" => TokenKind::Offset,
        "bit" => TokenKind::Bit,
        "if" => TokenKind::If,
        "else" => TokenKind::Else,
        "loop" => TokenKind::Loop,
        "while" => TokenKind::While,
        "for" => TokenKind::For,
        "in" => TokenKind::In,
        "upto" => TokenKind::Upto,
        "downto" => TokenKind::Downto,
        "step" => TokenKind::Step,
        "return" => TokenKind::Return,
        "break" => TokenKind::Break,
        "continue" => TokenKind::Continue,
        "import" => TokenKind::Import,
        "export" => TokenKind::Export,
        "as" => TokenKind::As,
        "context" => TokenKind::Context,
        "thread" => TokenKind::Thread,
        "isr" => TokenKind::Isr,
        "priority" => TokenKind::Priority,
        "any" => TokenKind::Any,
        "exclusive" => TokenKind::Exclusive,
        "shared" => TokenKind::Shared,
        "ceiling" => TokenKind::Ceiling,
        "dma" => TokenKind::Dma,
        "section" => TokenKind::Section,
        "external" => TokenKind::External,
        "align" => TokenKind::Align,
        "mut" => TokenKind::Mut,
        "null" => TokenKind::Null,
        "struct" => TokenKind::Struct,
        "sizeof" => TokenKind::Sizeof,
        "view" => TokenKind::View,
        "ring" => TokenKind::Ring,
        "bits" => TokenKind::Bits,
        "enum" => TokenKind::Enum,
        "match" => TokenKind::Match,
        "naked" => TokenKind::Naked,
        "tailchain" => TokenKind::Tailchain,
        "readonly" => TokenKind::Readonly,
        "writeonly" => TokenKind::Writeonly,
        "assume" => TokenKind::Assume,
        "assert" => TokenKind::Assert,
        "comptime_assert" => TokenKind::ComptimeAssert,
        "asm" => TokenKind::Asm,
        "true" => TokenKind::BoolLiteral(true),
        "false" => TokenKind::BoolLiteral(false),
        _ => TokenKind::Ident(s.to_string()),
    }
}

fn parse_int_suffix(lex: &mut Lexer) -> IntSuffix {
    let pos = lex.pos;
    let rest = lex.source.get(pos..).unwrap_or("");
    let suffix = match rest {
        s if s.starts_with("u8") => Some((2, IntSuffix::U8)),
        s if s.starts_with("u16") => Some((3, IntSuffix::U16)),
        s if s.starts_with("u32") => Some((3, IntSuffix::U32)),
        s if s.starts_with("u64") => Some((3, IntSuffix::U64)),
        s if s.starts_with("i8") => Some((2, IntSuffix::I8)),
        s if s.starts_with("i16") => Some((3, IntSuffix::I16)),
        s if s.starts_with("i32") => Some((3, IntSuffix::I32)),
        s if s.starts_with("i64") => Some((3, IntSuffix::I64)),
        _ => None,
    };
    if let Some((len, suffix)) = suffix {
        // Verify the suffix is followed by a non-alphanumeric boundary
        let after = rest[len..].chars().next();
        if after.is_none_or(|c| !c.is_alphanumeric() && c != '_') {
            lex.advance_by(len);
            return suffix;
        }
    }
    IntSuffix::None
}
