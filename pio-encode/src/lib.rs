//! RP2040/RP2350 PIO assembler.
//!
//! Turns PIO assembly source into the 16-bit instruction words a host CPU loads
//! into a PIO block's instruction memory, plus the program metadata (wrap
//! points, side-set shape, origin, public defines) a loader needs.
//!
//! This crate is deliberately free of any bml dependency: it is the isolated,
//! golden-testable core that both the in-language `pio { }` block (in bml-core)
//! and a future CLI can call. The block name is supplied by the caller, so a
//! `.program` directive is neither required nor accepted in the body.
//!
//! Scope (v1): the nine PIO instructions (jmp/wait/in/out/push/pull/mov/irq/set)
//! and `nop`, the `side`/`[delay]` modifiers, labels, and the `.side_set`,
//! `.wrap_target`, `.wrap`, `.origin`, `.define`, and `.word` directives. The
//! RP2350-only encodings (IRQ index modes, MOV to/from RX FIFO, WAIT JMPPIN) are
//! not emitted yet -- see the notes on `assemble`.

// Every i64 -> u8/u16 cast in this crate is guarded by an explicit range check
// immediately before it (addresses 0..31, values 0..31, indices 0..7, etc.), so
// sign-loss and wrap cannot actually occur; the casts are the encoding step.
#![allow(
    clippy::missing_errors_doc,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

/// A successfully assembled PIO program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assembled {
    /// Program name (supplied by the caller, e.g. the `pio NAME { }` block name).
    pub name: String,
    /// The instruction words, in address order, ready for instruction memory.
    pub words: Vec<u16>,
    /// `.origin N`, or `None` for a relocatable program.
    pub origin: Option<u8>,
    /// Wrap destination address (`.wrap_target`); defaults to 0.
    pub wrap_target: u8,
    /// Wrap source address (`.wrap`); defaults to the last instruction.
    pub wrap: u8,
    /// Value for `PINCTRL.SIDESET_COUNT`: the `.side_set N` data-bit count plus
    /// one for the enable bit when `opt` (the register field is inclusive of the
    /// enable bit). 0 when no side-set is used.
    pub side_set_count: u8,
    /// `.side_set N opt`: the MSB of the side-set field is an enable bit.
    pub side_set_opt: bool,
    /// `.side_set N pindirs`: side-set targets pin directions, not values.
    pub side_set_pindirs: bool,
    /// `.define PUBLIC SYM value` entries (host-visible symbols only).
    pub defines: Vec<(String, i64)>,
}

/// An assembly error, located by 1-based source line within the program body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsmError {
    pub line: usize,
    pub msg: String,
}

impl std::fmt::Display for AsmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.msg)
    }
}

/// Assemble a PIO program body. `name` is the program name (from the caller).
///
/// Returns every error found rather than just the first, so the caller can
/// surface them all at once.
pub fn assemble(name: &str, src: &str) -> Result<Assembled, Vec<AsmError>> {
    Assembler::new(name).run(src)
}

// ---------------------------------------------------------------------------
// Side-set shape
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
struct SideSet {
    /// Number of side-set DATA bits, exactly as written in `.side_set N`
    /// (matching pioasm: `opt` adds a separate enable bit on top, it is NOT one
    /// of these N).
    count: u8,
    opt: bool,
    pindirs: bool,
}

impl SideSet {
    /// Total bits of the [12:8] field consumed by side-set: the data bits plus
    /// the optional enable bit. This is the value programmed into
    /// `PINCTRL.SIDESET_COUNT` ("inclusive of the enable bit, if present").
    fn total_bits(self) -> u8 {
        self.count + u8::from(self.opt)
    }
    /// Bits of the [12:8] field left for `[delay]` after side-set.
    fn delay_bits(self) -> u8 {
        5 - self.total_bits()
    }
}

// ---------------------------------------------------------------------------
// Assembler
// ---------------------------------------------------------------------------

struct Assembler {
    name: String,
    side_set: SideSet,
    origin: Option<u8>,
    wrap_target: Option<u8>,
    wrap: Option<u8>,
    /// label/define name -> value
    symbols: std::collections::HashMap<String, i64>,
    public_defines: Vec<(String, i64)>,
    errors: Vec<AsmError>,
}

/// One instruction line carried from pass 1 (counting/labels) to pass 2
/// (encoding): the source text and its source line number.
struct InstrLine {
    text: String,
    line: usize,
}

impl Assembler {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            side_set: SideSet::default(),
            origin: None,
            wrap_target: None,
            wrap: None,
            symbols: std::collections::HashMap::new(),
            public_defines: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn err(&mut self, line: usize, msg: impl Into<String>) {
        self.errors.push(AsmError { line, msg: msg.into() });
    }

    fn run(mut self, src: &str) -> Result<Assembled, Vec<AsmError>> {
        let instrs = self.pass1(src);

        let count = instrs.len();
        if count == 0 {
            self.err(1, "pio program is empty (no instructions)");
        }
        if count > 32 {
            self.err(
                instrs.last().map_or(1, |i| i.line),
                format!("program has {count} instructions; PIO instruction memory holds 32"),
            );
        }

        let mut words = Vec::with_capacity(count);
        for (idx, instr) in instrs.iter().enumerate() {
            match self.encode_line(&instr.text, idx) {
                Ok(w) => words.push(w),
                Err(msg) => {
                    self.err(instr.line, msg);
                    words.push(0);
                }
            }
        }

        if !self.errors.is_empty() {
            return Err(self.errors);
        }

        let last = (count.saturating_sub(1)) as u8;
        Ok(Assembled {
            name: self.name,
            words,
            origin: self.origin,
            wrap_target: self.wrap_target.unwrap_or(0),
            wrap: self.wrap.unwrap_or(last),
            side_set_count: self.side_set.total_bits(),
            side_set_opt: self.side_set.opt,
            side_set_pindirs: self.side_set.pindirs,
            defines: self.public_defines,
        })
    }

    /// Pass 1: strip comments, process directives, record labels at their
    /// instruction address, and collect the instruction lines to encode.
    fn pass1(&mut self, src: &str) -> Vec<InstrLine> {
        let mut instrs: Vec<InstrLine> = Vec::new();
        for (i, raw) in src.lines().enumerate() {
            let line = i + 1;
            let mut text = strip_comment(raw).trim().to_string();
            if text.is_empty() {
                continue;
            }

            // Leading labels: `name:` possibly followed by an instruction.
            while let Some(rest) = take_label(&text) {
                let (label, remainder) = rest;
                let addr = instrs.len() as i64;
                if self.symbols.insert(label.clone(), addr).is_some() {
                    self.err(line, format!("duplicate label `{label}`"));
                }
                text = remainder.trim().to_string();
                if text.is_empty() {
                    break;
                }
            }
            if text.is_empty() {
                continue;
            }

            if let Some(stripped) = text.strip_prefix('.') {
                // `.word EXPR` is a raw instruction word, not a config directive:
                // it occupies an instruction slot and is encoded in pass 2. Every
                // other `.`-line is a directive.
                if stripped.split_whitespace().next() == Some("word") {
                    instrs.push(InstrLine { text, line });
                } else {
                    self.directive(stripped, instrs.len(), line);
                }
                continue;
            }

            instrs.push(InstrLine { text, line });
        }
        instrs
    }

    fn directive(&mut self, body: &str, instr_index: usize, line: usize) {
        let toks: Vec<&str> = body.split_whitespace().collect();
        let name = toks.first().copied().unwrap_or("");
        match name {
            "program" => self.err(
                line,
                "`.program` is implied by the block name; drop it from the body",
            ),
            "side_set" => self.dir_side_set(&toks, line),
            "wrap_target" => self.wrap_target = Some(instr_index as u8),
            "wrap" => {
                if instr_index == 0 {
                    self.err(line, "`.wrap` before any instruction");
                } else {
                    self.wrap = Some((instr_index - 1) as u8);
                }
            }
            "origin" => {
                // Eval the whole remainder, so a spaced expression
                // (`.origin 8 + 8`) is not silently truncated to its first token.
                let rest = body.strip_prefix("origin").unwrap_or("").trim();
                if rest.is_empty() {
                    self.err(line, "`.origin` needs an address");
                } else {
                    match self.eval(rest, line) {
                        Ok(v) if (0..32).contains(&v) => self.origin = Some(v as u8),
                        Ok(v) => self.err(line, format!("`.origin {v}` out of range 0..31")),
                        Err(e) => self.err(line, e),
                    }
                }
            }
            "define" => self.dir_define(body, line),
            // `.word` is handled in pass1 as an instruction line, so it never
            // reaches here.
            "lang_opt" | "pio_version" | "clock_div" | "fifo" => {
                // Tooling/host hints with no bearing on instruction encoding.
            }
            other => self.err(line, format!("unknown directive `.{other}`")),
        }
    }

    fn dir_side_set(&mut self, toks: &[&str], line: usize) {
        let count = match toks.get(1).map(|t| self.eval(t, line)) {
            Some(Ok(v)) if (0..=5).contains(&v) => v as u8,
            Some(Ok(v)) => {
                self.err(line, format!("`.side_set {v}` out of range 0..5"));
                return;
            }
            Some(Err(e)) => {
                self.err(line, e);
                return;
            }
            None => {
                self.err(line, "`.side_set` needs a count");
                return;
            }
        };
        let mut opt = false;
        let mut pindirs = false;
        for &t in &toks[2..] {
            match t {
                "opt" => opt = true,
                "pindirs" => pindirs = true,
                other => self.err(line, format!("unexpected `.side_set` qualifier `{other}`")),
            }
        }
        // `count` is data bits; `opt` adds one enable bit. The total must fit the
        // 5-bit [12:8] field, and `opt` needs at least one data bit to be useful.
        if opt && count == 0 {
            self.err(line, "`.side_set 0 opt` has no data bits; drop `opt` or raise the count");
            return;
        }
        if count + u8::from(opt) > 5 {
            self.err(
                line,
                format!(
                    "`.side_set {count}{}` needs {} bits but the side-set/delay field holds 5",
                    if opt { " opt" } else { "" },
                    count + u8::from(opt)
                ),
            );
            return;
        }
        self.side_set = SideSet { count, opt, pindirs };
    }

    fn dir_define(&mut self, body: &str, line: usize) {
        // `.define [PUBLIC] NAME EXPR`
        let rest = body.strip_prefix("define").unwrap_or("").trim_start();
        let (public, rest) = match rest.strip_prefix("PUBLIC ").or_else(|| rest.strip_prefix("public ")) {
            Some(r) => (true, r.trim_start()),
            None => (false, rest),
        };
        let mut it = rest.splitn(2, char::is_whitespace);
        let Some(sym) = it.next().filter(|s| !s.is_empty()) else {
            self.err(line, "`.define` needs a name");
            return;
        };
        let Some(expr) = it.next() else {
            self.err(line, "`.define` needs a value");
            return;
        };
        match self.eval(expr.trim(), line) {
            Ok(v) => {
                if self.symbols.insert(sym.to_string(), v).is_some() {
                    self.err(line, format!("redefinition of `{sym}`"));
                }
                if public {
                    self.public_defines.push((sym.to_string(), v));
                }
            }
            Err(e) => self.err(line, e),
        }
    }

    // -- instruction encoding -------------------------------------------------

    fn encode_line(&mut self, text: &str, instr_index: usize) -> Result<u16, String> {
        // `.word EXPR` emits a literal 16-bit word (delay/side-set still apply
        // to nothing; pioasm treats it as a raw word).
        if let Some(rest) = text.strip_prefix(".word") {
            let v = self.eval(rest.trim(), 0)?;
            // Accept any value representable in 16 bits, signed or unsigned
            // (-1 == 0xFFFF), but reject anything that genuinely does not fit
            // rather than silently masking it.
            if !(-0x8000..=0xFFFF).contains(&v) {
                return Err(format!("`.word` value {v} does not fit 16 bits"));
            }
            return Ok((v & 0xFFFF) as u16);
        }

        // Split modifiers (`side <v>` and `[delay]`) off the operand text.
        let (core, side, delay) = self.split_modifiers(text)?;
        let mut toks: Vec<String> = core
            .replace(',', " ")
            .split_whitespace()
            .map(str::to_string)
            .collect();
        if toks.is_empty() {
            return Err("empty instruction".into());
        }
        let mnemonic = toks.remove(0).to_ascii_lowercase();

        let base = match mnemonic.as_str() {
            "nop" => enc_nop(&toks)?,
            "set" => self.enc_set(&toks)?,
            "jmp" => self.enc_jmp(&toks, instr_index)?,
            "mov" => enc_mov(&toks)?,
            "out" => self.enc_out(&toks)?,
            "in" => self.enc_in(&toks)?,
            "push" => enc_push(&toks)?,
            "pull" => enc_pull(&toks)?,
            "wait" => self.enc_wait(&toks)?,
            "irq" => self.enc_irq(&toks)?,
            other => return Err(format!("unknown instruction `{other}`")),
        };

        Ok(base | self.delay_sideset_field(side, delay)?)
    }

    /// Build the [12:8] field: high `side_set.count` bits side-set, the rest delay.
    fn delay_sideset_field(&self, side: Option<i64>, delay: i64) -> Result<u16, String> {
        let ss = self.side_set;
        let delay_bits = ss.delay_bits();
        let delay_max = (1i64 << delay_bits) - 1;
        if delay < 0 || delay > delay_max {
            return Err(format!("delay {delay} out of range 0..{delay_max}"));
        }

        // No side-set declared at all: `side` is an error, delay uses all 5 bits.
        if ss.total_bits() == 0 {
            if side.is_some() {
                return Err("`side` used but no `.side_set` declared".into());
            }
            return Ok(((delay & 0x1F) as u16) << 8);
        }

        let side_field: i64 = match side {
            Some(v) => {
                let data_max = (1i64 << ss.count) - 1;
                if v < 0 || v > data_max {
                    return Err(format!("side-set value {v} out of range 0..{data_max}"));
                }
                if ss.opt {
                    // The enable bit sits just above the `count` data bits (the
                    // MSB of the side-set portion).
                    (1i64 << ss.count) | v
                } else {
                    v
                }
            }
            None => {
                if ss.opt {
                    0 // optional side-set omitted: enable bit clear, no data
                } else {
                    return Err("`.side_set` is not optional; every instruction needs `side`".into());
                }
            }
        };

        let field = ((side_field << delay_bits) | delay) & 0x1F;
        Ok((field as u16) << 8)
    }

    /// Pull `side <expr>` and `[<expr>]` modifiers out, returning the remaining
    /// operand text plus the parsed side/delay values.
    fn split_modifiers(&mut self, text: &str) -> Result<(String, Option<i64>, i64), String> {
        let mut delay = 0i64;
        let mut rest = text.to_string();

        // Delay: a bracketed expression `[ ... ]`.
        if let Some(open) = rest.find('[') {
            let close = rest[open..]
                .find(']')
                .map(|c| open + c)
                .ok_or("unterminated `[delay]`")?;
            let inner = rest[open + 1..close].trim().to_string();
            delay = self.eval(&inner, 0)?;
            rest.replace_range(open..=close, " ");
        }

        // Side-set: the `side` keyword followed by one expression token.
        let mut side = None;
        let toks: Vec<&str> = rest.split_whitespace().collect();
        if let Some(p) = toks.iter().position(|&t| t.eq_ignore_ascii_case("side")) {
            let val = toks
                .get(p + 1)
                .ok_or("`side` needs a value")?;
            side = Some(self.eval(val, 0)?);
            // rebuild `rest` without the `side <val>` pair
            let mut kept: Vec<&str> = Vec::new();
            let mut skip = 0;
            for (i, &t) in toks.iter().enumerate() {
                if i == p || skip > 0 {
                    if i == p {
                        skip = 1;
                    } else {
                        skip = 0;
                    }
                    continue;
                }
                kept.push(t);
            }
            rest = kept.join(" ");
        }

        Ok((rest, side, delay))
    }

    fn enc_set(&mut self, toks: &[String]) -> Result<u16, String> {
        let [dest, val] = two(toks, "set DEST, VALUE")?;
        let d = set_dest(dest)?;
        let v = self.eval(val, 0)?;
        if !(0..=31).contains(&v) {
            return Err(format!("set value {v} out of range 0..31"));
        }
        Ok(0xE000 | (d << 5) | (v as u16))
    }

    fn enc_jmp(&mut self, toks: &[String], _idx: usize) -> Result<u16, String> {
        let (cond, target) = match toks {
            [t] => (0u16, t.as_str()),
            [c, t] => (jmp_cond(c)?, t.as_str()),
            _ => return Err("jmp [COND,] TARGET".into()),
        };
        let addr = self.eval(target, 0)?;
        if !(0..=31).contains(&addr) {
            return Err(format!("jmp target {addr} out of range 0..31"));
        }
        Ok((cond << 5) | (addr as u16))
    }

    fn enc_out(&mut self, toks: &[String]) -> Result<u16, String> {
        let [dest, cnt] = two(toks, "out DEST, COUNT")?;
        let d = out_dest(dest)?;
        let c = self.bitcount(cnt)?;
        Ok(0x6000 | (d << 5) | c)
    }

    fn enc_in(&mut self, toks: &[String]) -> Result<u16, String> {
        let [src, cnt] = two(toks, "in SRC, COUNT")?;
        let s = in_src(src)?;
        let c = self.bitcount(cnt)?;
        Ok(0x4000 | (s << 5) | c)
    }

    fn enc_wait(&mut self, toks: &[String]) -> Result<u16, String> {
        // wait POLARITY SOURCE INDEX
        let [pol, src, idx] = three(toks, "wait POLARITY SOURCE INDEX")?;
        let p = self.eval(pol, 0)?;
        if !(0..=1).contains(&p) {
            return Err("wait polarity must be 0 or 1".into());
        }
        let s: u16 = match src.to_ascii_lowercase().as_str() {
            "gpio" => 0,
            "pin" => 1,
            "irq" => 2,
            other => return Err(format!("wait source must be gpio|pin|irq, got `{other}`")),
        };
        let i = self.eval(idx, 0)?;
        if !(0..=31).contains(&i) {
            return Err(format!("wait index {i} out of range 0..31"));
        }
        Ok(0x2000 | ((p as u16) << 7) | (s << 5) | (i as u16))
    }

    fn enc_irq(&mut self, toks: &[String]) -> Result<u16, String> {
        // irq [set|nowait|wait|clear] NUM [rel]
        let mut word = 0x0000u16; // opcode 110 added below
        let mut rest: Vec<&str> = toks.iter().map(String::as_str).collect();
        match rest.first().map(|s| s.to_ascii_lowercase()) {
            Some(m) if m == "wait" => {
                word |= 1 << 5;
                rest.remove(0);
            }
            Some(m) if m == "clear" => {
                word |= 1 << 6;
                rest.remove(0);
            }
            Some(m) if m == "set" || m == "nowait" => {
                rest.remove(0);
            }
            _ => {}
        }
        let mut rel = false;
        if rest.last().is_some_and(|s| s.eq_ignore_ascii_case("rel")) {
            rel = true;
            rest.pop();
        }
        let [num] = <[&str; 1]>::try_from(rest.as_slice())
            .map_err(|_| "irq [MODE] NUM [rel]".to_string())?;
        let n = self.eval(num, 0)?;
        if !(0..=7).contains(&n) {
            return Err(format!("irq index {n} out of range 0..7"));
        }
        if rel {
            word |= 1 << 4;
        }
        Ok(0xC000 | word | (n as u16))
    }

    fn bitcount(&mut self, tok: &str) -> Result<u16, String> {
        let c = self.eval(tok, 0)?;
        if !(1..=32).contains(&c) {
            return Err(format!("bit count {c} out of range 1..32"));
        }
        Ok((c as u16) & 0x1F) // 32 encodes as 00000
    }

    // -- expression evaluation ------------------------------------------------

    fn eval(&mut self, expr: &str, line: usize) -> Result<i64, String> {
        let toks = lex_expr(expr)?;
        let mut p = ExprParser { toks: &toks, pos: 0, symbols: &self.symbols };
        let v = p.parse(0)?;
        if p.pos != p.toks.len() {
            return Err(format!("trailing tokens in expression `{expr}`"));
        }
        let _ = line;
        Ok(v)
    }
}

// ---------------------------------------------------------------------------
// Operand tables
// ---------------------------------------------------------------------------

// These four instructions take no symbol/expression operands, so they are
// free functions rather than methods (nothing to borrow from the assembler).

fn enc_nop(toks: &[String]) -> Result<u16, String> {
    if toks.is_empty() {
        Ok(0xA042) // mov y, y
    } else {
        Err("`nop` takes no operands".into())
    }
}

fn enc_mov(toks: &[String]) -> Result<u16, String> {
    let [dest, src] = two(toks, "mov DEST, [op]SRC")?;
    let d = mov_dest(dest)?;
    let (op, s) = mov_src(src)?;
    Ok(0xA000 | (d << 5) | (op << 3) | s)
}

fn enc_push(toks: &[String]) -> Result<u16, String> {
    let mut word = 0x8000u16;
    let mut block = true;
    for t in toks {
        match t.to_ascii_lowercase().as_str() {
            "iffull" => word |= 1 << 6,
            "block" => block = true,
            "noblock" => block = false,
            other => return Err(format!("unexpected `push` operand `{other}`")),
        }
    }
    if block {
        word |= 1 << 5;
    }
    Ok(word)
}

fn enc_pull(toks: &[String]) -> Result<u16, String> {
    let mut word = 0x8080u16; // PULL: bit7 set
    let mut block = true;
    for t in toks {
        match t.to_ascii_lowercase().as_str() {
            "ifempty" => word |= 1 << 6,
            "block" => block = true,
            "noblock" => block = false,
            other => return Err(format!("unexpected `pull` operand `{other}`")),
        }
    }
    if block {
        word |= 1 << 5;
    }
    Ok(word)
}

fn set_dest(d: &str) -> Result<u16, String> {
    Ok(match d.to_ascii_lowercase().as_str() {
        "pins" => 0,
        "x" => 1,
        "y" => 2,
        "pindirs" => 4,
        other => return Err(format!("set destination must be pins|x|y|pindirs, got `{other}`")),
    })
}

fn jmp_cond(c: &str) -> Result<u16, String> {
    Ok(match c.to_ascii_lowercase().as_str() {
        "!x" => 1,
        "x--" => 2,
        "!y" => 3,
        "y--" => 4,
        "x!=y" => 5,
        "pin" => 6,
        "!osre" => 7,
        other => return Err(format!("unknown jmp condition `{other}`")),
    })
}

fn mov_dest(d: &str) -> Result<u16, String> {
    Ok(match d.to_ascii_lowercase().as_str() {
        "pins" => 0,
        "x" => 1,
        "y" => 2,
        "exec" => 4,
        "pc" => 5,
        "isr" => 6,
        "osr" => 7,
        other => return Err(format!("mov destination invalid: `{other}`")),
    })
}

fn mov_src(s: &str) -> Result<(u16, u16), String> {
    let (op, name) = if let Some(rest) = s.strip_prefix('~').or_else(|| s.strip_prefix('!')) {
        (1u16, rest)
    } else if let Some(rest) = s.strip_prefix("::") {
        (2u16, rest)
    } else {
        (0u16, s)
    };
    let src = match name.to_ascii_lowercase().as_str() {
        "pins" => 0,
        "x" => 1,
        "y" => 2,
        "null" => 3,
        "status" => 5,
        "isr" => 6,
        "osr" => 7,
        other => return Err(format!("mov source invalid: `{other}`")),
    };
    Ok((op, src))
}

fn out_dest(d: &str) -> Result<u16, String> {
    Ok(match d.to_ascii_lowercase().as_str() {
        "pins" => 0,
        "x" => 1,
        "y" => 2,
        "null" => 3,
        "pindirs" => 4,
        "pc" => 5,
        "isr" => 6,
        "exec" => 7,
        other => return Err(format!("out destination invalid: `{other}`")),
    })
}

fn in_src(s: &str) -> Result<u16, String> {
    Ok(match s.to_ascii_lowercase().as_str() {
        "pins" => 0,
        "x" => 1,
        "y" => 2,
        "null" => 3,
        "isr" => 6,
        "osr" => 7,
        other => return Err(format!("in source invalid: `{other}`")),
    })
}

fn two<'a>(toks: &'a [String], usage: &str) -> Result<[&'a str; 2], String> {
    match toks {
        [a, b] => Ok([a, b]),
        _ => Err(usage.to_string()),
    }
}

fn three<'a>(toks: &'a [String], usage: &str) -> Result<[&'a str; 3], String> {
    match toks {
        [a, b, c] => Ok([a, b, c]),
        _ => Err(usage.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Lexical helpers
// ---------------------------------------------------------------------------

/// Drop a `;`, `//`, or `#` line comment.
fn strip_comment(line: &str) -> &str {
    let mut cut = line.len();
    if let Some(i) = line.find(';') {
        cut = cut.min(i);
    }
    if let Some(i) = line.find("//") {
        cut = cut.min(i);
    }
    if let Some(i) = line.find('#') {
        cut = cut.min(i);
    }
    &line[..cut]
}

/// If `text` begins with `label:`, return `(label, remainder)`.
fn take_label(text: &str) -> Option<(String, String)> {
    let end = text.find(|c: char| !(c.is_alphanumeric() || c == '_'))?;
    if end == 0 {
        return None;
    }
    let after = text[end..].trim_start();
    let label = &text[..end];
    // `name:` -- but not `x!=y` or a bare colon-less word.
    if let Some(rest) = after.strip_prefix(':') {
        // Avoid swallowing `::` (mov bit-reverse) -- a label colon is single.
        if !rest.starts_with(':') {
            return Some((label.to_string(), rest.to_string()));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Expression grammar: ints, symbols, + - * / % << >> & | ^, unary -, parens.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum ETok {
    Int(i64),
    Sym(String),
    Op(char),
    Shl,
    Shr,
    LParen,
    RParen,
}

fn lex_expr(s: &str) -> Result<Vec<ETok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '<' && chars.get(i + 1) == Some(&'<') {
            out.push(ETok::Shl);
            i += 2;
        } else if c == '>' && chars.get(i + 1) == Some(&'>') {
            out.push(ETok::Shr);
            i += 2;
        } else if matches!(c, '+' | '-' | '*' | '/' | '%' | '&' | '|' | '^') {
            out.push(ETok::Op(c));
            i += 1;
        } else if c == '(' {
            out.push(ETok::LParen);
            i += 1;
        } else if c == ')' {
            out.push(ETok::RParen);
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            let lit: String = chars[start..i].iter().collect();
            out.push(ETok::Int(parse_int(&lit)?));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            out.push(ETok::Sym(chars[start..i].iter().collect()));
        } else {
            return Err(format!("unexpected character `{c}` in expression"));
        }
    }
    Ok(out)
}

fn parse_int(lit: &str) -> Result<i64, String> {
    let r = if let Some(h) = lit.strip_prefix("0x").or_else(|| lit.strip_prefix("0X")) {
        i64::from_str_radix(h, 16)
    } else if let Some(b) = lit.strip_prefix("0b").or_else(|| lit.strip_prefix("0B")) {
        i64::from_str_radix(b, 2)
    } else {
        lit.parse::<i64>()
    };
    r.map_err(|_| format!("invalid integer `{lit}`"))
}

struct ExprParser<'a> {
    toks: &'a [ETok],
    pos: usize,
    symbols: &'a std::collections::HashMap<String, i64>,
}

impl ExprParser<'_> {
    fn peek(&self) -> Option<&ETok> {
        self.toks.get(self.pos)
    }

    /// Precedence-climbing parser. `min_bp` is the minimum binding power.
    fn parse(&mut self, min_bp: u8) -> Result<i64, String> {
        let mut lhs = self.parse_atom()?;
        while let Some(tok) = self.peek() {
            let Some((bp, _)) = binding_power(tok) else {
                break;
            };
            if bp < min_bp {
                break;
            }
            let op = tok.clone();
            self.pos += 1;
            let rhs = self.parse(bp + 1)?;
            lhs = apply(&op, lhs, rhs)?;
        }
        Ok(lhs)
    }

    fn parse_atom(&mut self) -> Result<i64, String> {
        match self.peek().cloned() {
            Some(ETok::Int(n)) => {
                self.pos += 1;
                Ok(n)
            }
            Some(ETok::Sym(name)) => {
                self.pos += 1;
                self.symbols
                    .get(&name)
                    .copied()
                    .ok_or_else(|| format!("unknown symbol `{name}`"))
            }
            Some(ETok::Op('-')) => {
                self.pos += 1;
                Ok(-self.parse_atom()?)
            }
            Some(ETok::Op('+')) => {
                self.pos += 1;
                self.parse_atom()
            }
            Some(ETok::LParen) => {
                self.pos += 1;
                let v = self.parse(0)?;
                match self.peek() {
                    Some(ETok::RParen) => {
                        self.pos += 1;
                        Ok(v)
                    }
                    _ => Err("expected `)`".into()),
                }
            }
            other => Err(format!("expected value, found {other:?}")),
        }
    }
}

fn binding_power(tok: &ETok) -> Option<(u8, u8)> {
    Some(match tok {
        ETok::Op('|') => (1, 1),
        ETok::Op('^') => (2, 2),
        ETok::Op('&') => (3, 3),
        ETok::Shl | ETok::Shr => (4, 4),
        ETok::Op('+' | '-') => (5, 5),
        ETok::Op('*' | '/' | '%') => (6, 6),
        _ => return None,
    })
}

fn apply(op: &ETok, a: i64, b: i64) -> Result<i64, String> {
    // All arithmetic is checked: a malformed expression in user PIO source
    // (`1 << 70`, `i64::MAX + 1`) must surface as an AsmError, never panic the
    // host compiler.
    let overflow = || "integer overflow in expression".to_string();
    Ok(match op {
        ETok::Op('+') => a.checked_add(b).ok_or_else(overflow)?,
        ETok::Op('-') => a.checked_sub(b).ok_or_else(overflow)?,
        ETok::Op('*') => a.checked_mul(b).ok_or_else(overflow)?,
        ETok::Op('/') => a.checked_div(b).ok_or("division by zero")?,
        ETok::Op('%') => a.checked_rem(b).ok_or("modulo by zero")?,
        ETok::Op('&') => a & b,
        ETok::Op('|') => a | b,
        ETok::Op('^') => a ^ b,
        // `checked_shl`/`checked_shr` guard the SHIFT AMOUNT (>= 64); they do not
        // guard value overflow, but a wrapped shift result is acceptable (bit
        // ops), whereas a >= 64 shift is the panic to avoid.
        ETok::Shl => u32::try_from(b)
            .ok()
            .and_then(|s| a.checked_shl(s))
            .ok_or("invalid left-shift amount")?,
        ETok::Shr => u32::try_from(b)
            .ok()
            .and_then(|s| a.checked_shr(s))
            .ok_or("invalid right-shift amount")?,
        _ => return Err("bad operator".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(name: &str, src: &str) -> Vec<u16> {
        match assemble(name, src) {
            Ok(a) => a.words,
            Err(e) => panic!("assemble failed: {e:?}"),
        }
    }

    #[test]
    fn blink_matches_m1_golden() {
        // The exact words hand-encoded in the M1 PoC (pio_blink.bml). This is
        // the primary golden vector tying the encoder to a verified program.
        let src = "
            set pins, 1
            set x, 31
        on:
            jmp x--, on [31]
            set pins, 0
            set x, 31
        off:
            jmp x--, off [31]
        ";
        assert_eq!(
            words("blink", src),
            vec![0xE001, 0xE03F, 0x1F42, 0xE000, 0xE03F, 0x1F45]
        );
    }

    #[test]
    fn single_instruction_ops() {
        assert_eq!(words("t", "nop"), vec![0xA042]);
        assert_eq!(words("t", "set pins, 1"), vec![0xE001]);
        assert_eq!(words("t", "set pindirs, 1"), vec![0xE081]);
        assert_eq!(words("t", "set x, 31"), vec![0xE03F]);
        assert_eq!(words("t", "jmp 0"), vec![0x0000]);
        assert_eq!(words("t", "mov x, y"), vec![0xA022]);
        assert_eq!(words("t", "mov x, ~y"), vec![0xA02A]);
        assert_eq!(words("t", "mov osr, ::isr"), vec![0xA0F6]);
        assert_eq!(words("t", "out pins, 32"), vec![0x6000]);
        assert_eq!(words("t", "out x, 1"), vec![0x6021]);
        assert_eq!(words("t", "in pins, 32"), vec![0x4000]);
        assert_eq!(words("t", "push"), vec![0x8020]);
        assert_eq!(words("t", "push noblock"), vec![0x8000]);
        assert_eq!(words("t", "pull"), vec![0x80A0]);
        assert_eq!(words("t", "pull ifempty noblock"), vec![0x80C0]);
        assert_eq!(words("t", "wait 1 pin 0"), vec![0x20A0]);
        assert_eq!(words("t", "wait 0 gpio 5"), vec![0x2005]);
        assert_eq!(words("t", "irq 0"), vec![0xC000]);
        assert_eq!(words("t", "irq wait 3"), vec![0xC023]);
        assert_eq!(words("t", "irq clear 1"), vec![0xC041]);
    }

    #[test]
    fn squarewave_wrap_and_meta() {
        // The pico-examples squarewave: a tight 2-cycle toggle.
        let src = "
        .wrap_target
            set pins, 1 [1]
            set pins, 0
        .wrap
        ";
        let a = assemble("squarewave", src).unwrap();
        assert_eq!(a.words, vec![0xE101, 0xE000]);
        assert_eq!(a.wrap_target, 0);
        assert_eq!(a.wrap, 1);
        assert_eq!(a.origin, None);
    }

    #[test]
    fn side_set_packs_into_delay_field() {
        // .side_set 1: 1 side bit (MSB of [12:8]), 4 delay bits.
        let src = "
        .side_set 1
            nop side 1 [2]
            nop side 0
        ";
        let a = assemble("ss", src).unwrap();
        // nop = 0xA042; side 1 -> bit12 set (0x1000); delay 2 -> 0x0200.
        assert_eq!(a.words[0], 0xA042 | 0x1000 | 0x0200);
        assert_eq!(a.words[1], 0xA042);
        assert_eq!(a.side_set_count, 1);
    }

    #[test]
    fn defines_and_expressions() {
        let src = "
        .define PUBLIC T1 3
        .define T2 (T1 - 1)
            set x, T1
            nop [T2]
        ";
        let a = assemble("d", src).unwrap();
        assert_eq!(a.words, vec![0xE000 | 0x20 | 3, 0xA042 | (2 << 8)]);
        assert_eq!(a.defines, vec![("T1".to_string(), 3)]);
    }

    #[test]
    fn origin_directive() {
        let a = assemble("o", ".origin 16\n nop").unwrap();
        assert_eq!(a.origin, Some(16));
    }

    #[test]
    fn errors_collect_with_lines() {
        let err = assemble("e", "set pins, 99\n bogus x").unwrap_err();
        assert_eq!(err.len(), 2);
        assert_eq!(err[0].line, 1);
        assert_eq!(err[1].line, 2);
    }

    #[test]
    fn too_many_instructions_is_rejected() {
        let src = "nop\n".repeat(33);
        let err = assemble("big", &src).unwrap_err();
        assert!(err.iter().any(|e| e.msg.contains("instruction memory holds 32")));
    }

    // Overflowing arithmetic in an expression returns an AsmError, never panics.
    #[test]
    fn expr_overflow_is_an_error_not_a_panic() {
        assert!(assemble("t", "nop [1 << 70]").is_err());
        assert!(assemble("t", ".define A (9223372036854775807 + 1)\n nop [A]").is_err());
        assert!(assemble("t", "nop [1 >> 0]").is_ok()); // sane shift still works
    }

    // Optional side-set follows pioasm: `.side_set N opt` is N DATA bits plus a
    // separate enable bit (max N reduced 5->4); SIDESET_COUNT = N + 1.
    #[test]
    fn side_set_opt_matches_pioasm() {
        // .side_set 1 opt; nop side 1 -> enable bit (12) + 1 data bit (11) set.
        let a = assemble("s", ".side_set 1 opt\n nop side 1").unwrap();
        assert_eq!(a.words[0], 0xA042 | 0x1000 | 0x0800);
        assert_eq!(a.side_set_count, 2); // 1 data + 1 enable
        // optional side-set omitted -> enable clear, plain nop.
        let b = assemble("s", ".side_set 1 opt\n nop").unwrap();
        assert_eq!(b.words[0], 0xA042);
        // `.side_set 5 opt` needs 6 bits -> rejected; `.side_set 0 opt` -> rejected.
        assert!(assemble("s", ".side_set 5 opt\n nop side 0").is_err());
        assert!(assemble("s", ".side_set 0 opt\n nop").is_err());
        // non-opt `.side_set 5` (all side-set, no delay) is fine.
        assert!(assemble("s", ".side_set 5\n nop side 31").is_ok());
    }

    // `.word EXPR` emits a raw 16-bit instruction word (now reachable), and an
    // out-of-range value is rejected rather than silently masked.
    #[test]
    fn word_directive_emits_and_range_checks() {
        assert_eq!(assemble("w", ".word 0xC000").unwrap().words, vec![0xC000]);
        assert_eq!(assemble("w", ".word -1").unwrap().words, vec![0xFFFF]);
        assert!(assemble("w", ".word 0x12345").is_err());
    }

    // `.origin` evaluates a full (spaced) expression, not just its first token.
    #[test]
    fn origin_accepts_expression() {
        assert_eq!(assemble("o", ".origin 8 + 8\n nop").unwrap().origin, Some(16));
    }
}
