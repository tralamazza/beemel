//! Front-end no-panic fuzzer (in-tree, stable Rust).
//!
//! A compiler must never *crash* on input: malformed source should produce
//! diagnostics, never a panic, hang, or stack overflow. Nothing else in the
//! suite enforces that. This drives the real front end (lexer -> parser ->
//! resolver -> type checker -> borrow checker) in-process on a large, mutated,
//! and grammar-generated input space and asserts it always returns.
//!
//! Why in-tree/stable rather than cargo-fuzz: it needs no nightly and no
//! embedded toolchain, so it runs in plain `cargo test` on every commit as a
//! per-commit gate. It is not coverage-guided, so the generator is made
//! grammar-aware (real fixtures as a corpus, plus a token-stream generator) to
//! reach past shallow byte noise. A future cargo-fuzz target could share the
//! same corpus for deep, continuous runs.
//!
//! Determinism: the seed is fixed (override with `BML_FUZZ_SEED`) and iteration
//! count with `BML_FUZZ_ITERS`. On a found crash the seed and the exact input
//! are printed so it reproduces. A watchdog turns a single hanging input into a
//! loud failure instead of a stuck test.

use std::collections::HashMap;
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use bml_core::borrow::BorrowChecker;
use bml_core::checker::Checker;
use bml_core::errors::DiagnosticBag;
use bml_core::imports::AliasInfo;
use bml_core::parser::Parser;
use bml_core::resolver::Resolver;
use bml_core::source::SourceMap;

// ─── PRNG (xorshift64*, same convention as the exec.rs generators) ───────────
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn pick<'b, T>(&mut self, xs: &'b [T]) -> &'b T {
        &xs[self.below(xs.len() as u64) as usize]
    }
}

// ─── progress/crash reporting shared with the watchdog ───────────────────────
static CURRENT: Mutex<String> = Mutex::new(String::new());
static ITER: AtomicU64 = AtomicU64::new(0);
static DONE: AtomicBool = AtomicBool::new(false);
static LAST_PANIC: Mutex<Option<String>> = Mutex::new(None);

/// Run the whole front end on `source`, discarding diagnostics. Import
/// resolution is intentionally skipped: it touches the filesystem, which a
/// fuzzer must not do. Every other phase runs. We only care that it returns
/// (no panic / overflow / hang), not what it concludes.
fn drive_frontend(source: &str) {
    let mut source_map = SourceMap::new();
    let file_id = source_map.add_file_with_source(PathBuf::from("fuzz.bml"), source.to_string());
    let text = source_map.source(file_id);
    let mut diags = DiagnosticBag::new();

    let mut parser = Parser::new(text, file_id, &mut diags);
    let program = parser.parse_program();
    if diags.has_errors() {
        return;
    }

    let resolver = Resolver::new();
    let aliases: HashMap<String, AliasInfo> = HashMap::new();
    let symbols = resolver.resolve(&program, &mut diags, aliases);
    if diags.has_errors() {
        return;
    }

    Checker::check(&program, &symbols, &mut diags);
    if diags.has_errors() {
        return;
    }
    BorrowChecker::check(&program, &symbols, &mut diags);
}

/// Feed one input to the front end under `catch_unwind`. Returns the captured
/// panic message on crash, `None` on a clean return. Updates the watchdog state.
fn try_input(source: &str) -> Option<String> {
    *CURRENT.lock().unwrap() = source.to_string();
    ITER.fetch_add(1, Ordering::Relaxed);
    *LAST_PANIC.lock().unwrap() = None;
    let result = panic::catch_unwind(|| drive_frontend(source));
    match result {
        Ok(()) => None,
        Err(payload) => {
            // Prefer the hook-captured "message at file:line"; fall back to the
            // raw payload string.
            let hook_msg = LAST_PANIC.lock().unwrap().take();
            Some(hook_msg.unwrap_or_else(|| {
                payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".to_string())
            }))
        }
    }
}

// ─── corpus: every committed .bml fixture is a seed ──────────────────────────
fn collect_bml(dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_bml(&path, out);
        } else if path.extension().is_some_and(|e| e == "bml")
            && let Ok(src) = std::fs::read_to_string(&path)
        {
            out.push(src);
        }
    }
}

fn corpus() -> Vec<String> {
    let mut out = Vec::new();
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("bml")
        .join("tests")
        .join("fixtures");
    collect_bml(&fixtures, &mut out);
    // A few hand-written seeds so the fuzzer is meaningful even if fixtures move.
    for s in [
        "fn main() @context(thread) { var x: u32 = 1 + 2; }",
        "struct S { a: u32, b: u32 } enum E { A, B }",
        "peripheral P at 0x4000 { reg R offset 0 { field F: u32 bit[0..3] } }",
        "fn f(v: view mut u32) { v[0u32] = 1u32; }",
    ] {
        out.push(s.to_string());
    }
    out
}

// ─── byte-level mutators ─────────────────────────────────────────────────────
/// Structural punctuation, biased into inserts so mutations stay near the grammar.
const PUNCT: &[u8] = b"(){}[]<>;,:.@=+-*/%&|^~!";
/// Characters a recursive-descent parser nests on; repeated to probe depth/loops.
const NEST: &[u8] = b"({[<*!-";

fn mutate(rng: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut b = base.to_vec();
    let rounds = 1 + rng.below(6);
    for _ in 0..rounds {
        if b.is_empty() {
            b.push(rng.below(128) as u8);
            continue;
        }
        match rng.below(7) {
            0 => {
                // flip a byte
                let i = rng.below(b.len() as u64) as usize;
                b[i] = rng.below(256) as u8;
            }
            1 => {
                // insert a byte (bias toward structural punctuation)
                let i = rng.below(b.len() as u64 + 1) as usize;
                let c = if rng.below(2) == 0 {
                    *rng.pick(PUNCT)
                } else {
                    rng.below(128) as u8
                };
                b.insert(i, c);
            }
            2 => {
                // delete a byte
                let i = rng.below(b.len() as u64) as usize;
                b.remove(i);
            }
            3 => {
                // truncate
                let i = rng.below(b.len() as u64) as usize;
                b.truncate(i);
            }
            4 => {
                // duplicate a span (bounded so inputs stay small)
                let len = b.len() as u64;
                let start = rng.below(len) as usize;
                let span = 1 + rng.below((len - start as u64).min(64)) as usize;
                let chunk = b[start..start + span].to_vec();
                let at = rng.below(b.len() as u64 + 1) as usize;
                b.splice(at..at, chunk);
            }
            5 => {
                // repeat one structural char a lot (probe nesting / loops)
                let c = *rng.pick(NEST);
                let n = 1 + rng.below(300) as usize;
                let at = rng.below(b.len() as u64 + 1) as usize;
                b.splice(at..at, std::iter::repeat_n(c, n));
            }
            _ => {
                // zero byte / unicode lead, to stress lexer boundaries
                let i = rng.below(b.len() as u64) as usize;
                b[i] = *rng.pick(&[0u8, 0x80, 0xFF, b'"', b'\'', b'\n']);
            }
        }
    }
    // Keep inputs small so each iteration is fast.
    b.truncate(32 * 1024);
    b
}

// ─── grammar-aware token-stream generator ────────────────────────────────────
const TOKENS: &[&str] = &[
    "fn",
    "var",
    "val",
    "const",
    "struct",
    "enum",
    "import",
    "export",
    "return",
    "if",
    "else",
    "while",
    "loop",
    "for",
    "break",
    "continue",
    "match",
    "upto",
    "downto",
    "in",
    "peripheral",
    "reg",
    "field",
    "bit",
    "at",
    "offset",
    "asm",
    "view",
    "ring",
    "bits",
    "mut",
    "stride",
    "as",
    "u8",
    "u16",
    "u32",
    "u64",
    "i8",
    "i16",
    "i32",
    "i64",
    "f32",
    "f64",
    "b1",
    "b8",
    "bool",
    "@context",
    "@isr",
    "@naked",
    "thread",
    "any",
    "main",
    "a",
    "b",
    "foo",
    "(",
    ")",
    "{",
    "}",
    "[",
    "]",
    "<",
    ">",
    ",",
    ";",
    ":",
    ".",
    "@",
    "=",
    "==",
    "!=",
    "+",
    "-",
    "*",
    "/",
    "%",
    "&",
    "|",
    "^",
    "~",
    "<<",
    ">>",
    "&&",
    "||",
    "!",
    "->",
    "..",
    "0",
    "1",
    "42",
    "0xFF",
    "1u8",
    "1u32",
    "1i32",
    "1.5f",
    "true",
    "false",
    "0x4000",
    "\"s\"",
];

fn gen_tokens(rng: &mut Rng) -> String {
    let n = 1 + rng.below(60);
    let mut s = String::new();
    for _ in 0..n {
        let idx = rng.below(TOKENS.len() as u64) as usize;
        s.push_str(TOKENS[idx]);
        s.push(' ');
    }
    s
}

// ─── deterministic recursion-depth probe ─────────────────────────────────────
//
// Each form drives one recursive parser entry point (expression, type, block)
// far past `MAX_PARSE_DEPTH`. The guard must turn these into a diagnostic, not a
// stack overflow (which `catch_unwind` cannot catch), so a clean return here is
// the whole point.
fn recursion_probes() -> Vec<String> {
    let mut v = Vec::new();
    for d in [130usize, 1000, 50_000] {
        v.push(format!(
            "fn main() @context(thread) {{ var x: u32 = {}1{}; }}",
            "(".repeat(d),
            ")".repeat(d)
        ));
        v.push(format!(
            "fn main() @context(thread) {{ {} {} }}",
            "{".repeat(d),
            "}".repeat(d)
        ));
        v.push(format!("fn f(p: {}u32) {{ }}", "*".repeat(d)));
        v.push(format!(
            "fn main() @context(thread) {{ var b: b1 = {}true; }}",
            "!".repeat(d)
        ));
    }
    v
}

#[test]
fn frontend_never_panics() {
    let seed = std::env::var("BML_FUZZ_SEED")
        .ok()
        .and_then(|s| match s.strip_prefix("0x") {
            Some(h) => u64::from_str_radix(h, 16).ok(),
            None => s.parse().ok(),
        })
        .unwrap_or(0xF1ED_BEEF_0BAD_5EEDu64);
    let iters: u64 = std::env::var("BML_FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000);

    // A big stack so the depth guard (not the OS) is what stops recursion, and a
    // hook that records the panic site quietly instead of spamming backtraces.
    panic::set_hook(Box::new(|info| {
        *LAST_PANIC.lock().unwrap() = Some(format!("{info}"));
    }));

    // Watchdog: if no iteration completes for a while, one input is stuck. Print
    // it and fail the whole process loudly rather than hang CI.
    std::thread::spawn(move || {
        let mut last = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(15));
            if DONE.load(Ordering::Acquire) {
                return;
            }
            let now = ITER.load(Ordering::Relaxed);
            if now == last {
                let cur = CURRENT.lock().unwrap().clone();
                eprintln!("FUZZ HANG (seed {seed:#018x}): no progress in 15s on input:\n{cur:?}");
                std::process::exit(101);
            }
            last = now;
        }
    });

    let worker = std::thread::Builder::new()
        .name("fuzz".into())
        .stack_size(64 * 1024 * 1024)
        .spawn(move || run(seed, iters))
        .expect("spawn fuzz worker");
    let failures = worker
        .join()
        .unwrap_or_else(|_| panic!("fuzz worker thread itself crashed (seed {seed:#018x})"));
    DONE.store(true, Ordering::Release);

    if !failures.is_empty() {
        for (input, msg) in &failures {
            eprintln!(
                "--- front-end CRASH ---\npanic: {msg}\ninput ({} bytes): {:?}\n",
                input.len(),
                input
            );
        }
        panic!(
            "front-end fuzz found {} crashing input(s) (seed {seed:#018x}); reproduce by \
             feeding the input above, or rerun with BML_FUZZ_SEED={seed:#018x}",
            failures.len()
        );
    }
}

/// The actual loop. Returns up to a handful of distinct crashing inputs.
fn run(seed: u64, iters: u64) -> Vec<(String, String)> {
    let mut rng = Rng(seed);
    let corpus = corpus();
    let mut failures: Vec<(String, String)> = Vec::new();

    let record = |input: String, msg: String, failures: &mut Vec<(String, String)>| {
        // Dedupe by panic message so one bug does not flood the report.
        if failures.len() < 8 && !failures.iter().any(|(_, m)| *m == msg) {
            failures.push((input, msg));
        }
    };

    // Tier 0: the committed corpus must parse-check without panicking as-is.
    for src in &corpus {
        if let Some(msg) = try_input(src) {
            record(src.clone(), msg, &mut failures);
        }
    }

    // Tier 1: deterministic recursion probes (the depth guard must hold).
    for src in recursion_probes() {
        if let Some(msg) = try_input(&src) {
            record(src, msg, &mut failures);
        }
    }

    // Tier 2: mutated corpus + generated token streams.
    for i in 0..iters {
        let input = if i % 4 == 0 || corpus.is_empty() {
            gen_tokens(&mut rng)
        } else {
            let base = rng.pick(&corpus).clone();
            String::from_utf8_lossy(&mutate(&mut rng, base.as_bytes())).into_owned()
        };
        if let Some(msg) = try_input(&input) {
            record(input, msg, &mut failures);
        }
    }

    failures
}
