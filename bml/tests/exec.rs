//! Black-box execution tests.
//!
//! Each fixture under `tests/fixtures/exec/` is compiled, linked, and run on a
//! Cortex-M3 under QEMU with semihosting. The fixture computes a value and
//! self-checks it against the answer mandated by `doc/language.md` /
//! `doc/design-decisions.md`, printing `OK` or `FAIL` via the `harness.semihost`
//! module. This file only inspects that text -- it never looks at the emitted
//! IR -- so a passing test means the *program behaves correctly*, not merely
//! that the compiler lowered it a particular way.
//!
//! The tests need `qemu-system-arm` and `arm-none-eabi-ld` (override with
//! `BML_QEMU_BIN` / `BML_ARM_LD_BIN`). When either is missing the tests skip
//! with a notice, mirroring how the `bml verify` tests gate on `BML_IKOS_BIN`.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

const QEMU_MACHINE: &str = "stm32vldiscovery";
const RUN_TIMEOUT: Duration = Duration::from_secs(20);

fn qemu_bin() -> String {
    std::env::var("BML_QEMU_BIN").unwrap_or_else(|_| "qemu-system-arm".to_string())
}

fn ld_bin() -> String {
    std::env::var("BML_ARM_LD_BIN").unwrap_or_else(|_| "arm-none-eabi-ld".to_string())
}

fn runnable(bin: &str) -> bool {
    Command::new(bin)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// True when the embedded toolchain needed to build and run fixtures is present.
fn tools_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| runnable(&qemu_bin()) && runnable(&ld_bin()))
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("exec")
}

/// Compile, link, and run `fixture` under QEMU; return the semihosting output.
fn bml_run(fixture: &str, opt: &str) -> String {
    let dir = fixtures_dir();
    let src = dir.join(fixture);
    let target = dir.join("qemu.target");

    // 1. Compile to an object file (and linker script) next to the fixture.
    let build = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg(format!("--opt={opt}"))
        .arg("--save-temps")
        .arg("--target")
        .arg(&target)
        .arg(&src)
        .output()
        .expect("failed to run bml build");
    let obj = src.with_extension("o");
    let lds = src.with_extension("ld");
    assert!(
        build.status.success() && obj.exists(),
        "bml build failed for {fixture} at -O{opt}:\n{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    // 2. Link to an ELF in a temp location.
    let stem = src.file_stem().unwrap().to_string_lossy().to_string();
    let pid = std::process::id();
    let elf = std::env::temp_dir().join(format!("bml-exec-{stem}-O{opt}-{pid}.elf"));
    let out_path = std::env::temp_dir().join(format!("bml-exec-{stem}-O{opt}-{pid}.out"));
    let link = Command::new(ld_bin())
        .arg("-T")
        .arg(&lds)
        .arg(&obj)
        .arg("-o")
        .arg(&elf)
        .output()
        .expect("failed to run linker");
    let link_ok = link.status.success() && elf.exists();

    // 3. Run under QEMU, capturing semihosting stdout to a file (avoids any
    //    pipe-buffer deadlock) and enforcing a wall-clock timeout.
    let output = if link_ok {
        run_qemu(&elf, &out_path)
    } else {
        String::new()
    };

    // 4. Clean up intermediates (incl. the optimized IR from --save-temps).
    let _ = std::fs::remove_file(src.with_extension("ll"));
    let _ = std::fs::remove_file(src.with_extension("opt.ll"));
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&lds);
    let _ = std::fs::remove_file(&elf);
    let _ = std::fs::remove_file(&out_path);

    assert!(
        link_ok,
        "link failed for {fixture} at -O{opt}:\n{}",
        String::from_utf8_lossy(&link.stderr),
    );
    output
}

fn run_qemu(elf: &std::path::Path, out_path: &std::path::Path) -> String {
    // QEMU sends semihosting SYS_WRITE0 output to its stderr, so capture that.
    let out_file = std::fs::File::create(out_path).expect("create qemu output file");
    let mut child = Command::new(qemu_bin())
        .arg("-M")
        .arg(QEMU_MACHINE)
        .arg("-semihosting")
        .arg("-nographic")
        .arg("-kernel")
        .arg(elf)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(out_file)
        .spawn()
        .expect("failed to spawn qemu");

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > RUN_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("error waiting on qemu: {e}"),
        }
    }
    std::fs::read_to_string(out_path).unwrap_or_default()
}

/// Optimization levels every fixture is run at. `-O0` checks the unoptimized
/// lowering; `-O2` checks that the program still behaves correctly through the
/// LLVM optimizer -- e.g. that wrapping arithmetic is preserved (no nsw/nuw is
/// emitted, per design-decisions.md §8) and MMIO/volatile accesses aren't
/// elided.
const OPT_LEVELS: &[&str] = &["0", "2"];

/// Run a fixture at every opt level and assert it reported success at each: at
/// least one `OK` and no `FAIL`.
macro_rules! assert_exec {
    ($name:ident, $fixture:expr) => {
        #[test]
        fn $name() {
            if !tools_available() {
                eprintln!(
                    "skipping {}: qemu-system-arm / arm-none-eabi-ld not found \
                     (set BML_QEMU_BIN / BML_ARM_LD_BIN to enable)",
                    $fixture
                );
                return;
            }
            for opt in OPT_LEVELS {
                let out = bml_run($fixture, opt);
                assert!(
                    out.contains("OK"),
                    "expected at least one OK from {} at -O{opt}; captured output:\n{out}",
                    $fixture
                );
                assert!(
                    !out.contains("FAIL"),
                    "{} reported a FAIL at -O{opt}; captured output:\n{out}",
                    $fixture
                );
            }
        }
    };
}

/// Same as `assert_exec!` but `#[ignore]`d: it pins a known compiler bug. The
/// fixture exercises documented behavior that currently miscompiles, so the test
/// fails today; it will start passing once the bug is fixed. Run them on demand
/// with `cargo test --test exec -- --ignored`.
#[allow(unused_macros)]
macro_rules! known_bug {
    ($name:ident, $fixture:expr, $reason:expr) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            if !tools_available() {
                eprintln!("skipping {}: toolchain not found", $fixture);
                return;
            }
            let out = bml_run($fixture, "0");
            assert!(out.contains("OK") && !out.contains("FAIL"), "{}", $reason);
        }
    };
}

// ─── smoke ──────────────────────────────────────────────────────────────────
assert_exec!(exec_smoke, "smoke.bml");

// ─── integer wrapping (design-decisions.md §8) ───────────────────────────────
assert_exec!(exec_wrapping, "wrapping.bml");

// ─── casts / widths (language.md §1) ─────────────────────────────────────────
assert_exec!(exec_casts, "casts.bml");

// ─── for loops (language.md §11) ─────────────────────────────────────────────
assert_exec!(exec_for_loops, "for_loops.bml");

// ─── pointer arithmetic (language.md §5) ─────────────────────────────────────
assert_exec!(exec_pointers, "pointers.bml");

// ─── struct semantics (language.md §6) ───────────────────────────────────────
assert_exec!(exec_structs, "structs.bml");

// ─── enum semantics (language.md §7) ─────────────────────────────────────────
assert_exec!(exec_enums, "enums.bml");

// ─── register read-modify-write (language.md §9) ─────────────────────────────
assert_exec!(exec_register_rmw, "register_rmw.bml");

// ─── control flow & expressions (language.md §11) ────────────────────────────
assert_exec!(exec_control_flow, "control_flow.bml");

// ─── const evaluation (language.md §1) ───────────────────────────────────────
assert_exec!(exec_const_eval, "const_eval.bml");

// ─── unsuffixed literals adopt the narrow context type (language.md §1) ───────
assert_exec!(exec_narrow_literals, "narrow_literals.bml");

// ─── operator / signedness matrix (language.md §1) ───────────────────────────
assert_exec!(exec_div_mod_ops, "div_mod_ops.bml");
assert_exec!(exec_shift_ops, "shift_ops.bml");
assert_exec!(exec_compare_ops, "compare_ops.bml");
assert_exec!(exec_bool_ops, "bool_ops.bml");

// ─── const-valued array lengths (language.md §1) ─────────────────────────────
assert_exec!(exec_const_array_len, "const_array_len.bml");

// ─── property / differential test for integer arithmetic ─────────────────────
//
// Generate random integer expressions, evaluate each with a Rust oracle that
// mirrors bml's semantics (two's-complement wrapping; signed vs unsigned
// div/rem/shr), then emit one program that self-checks every expression and
// run it under QEMU. This explores far more of the arithmetic space than the
// hand-written fixtures. The seed is fixed for reproducibility (override with
// BML_PROP_SEED); on failure the generated source is printed.

// The generator does deliberate two's-complement bit reinterpretation and uses
// short mathematical names, so the usual pedantic cast/naming lints are noise.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::doc_markdown,
    clippy::format_push_string
)]
mod property {
    use super::{OPT_LEVELS, bml_run, fixtures_dir, tools_available};
    use std::path::PathBuf;

    #[derive(Clone, Copy, PartialEq)]
    enum Ty {
        U32,
        I32,
    }

    #[derive(Clone, Copy)]
    enum Op {
        Add,
        Sub,
        Mul,
        Div,
        Rem,
        And,
        Or,
        Xor,
        Shl,
        Shr,
    }

    enum Expr {
        Lit(i64),
        Bin(Box<Expr>, Op, Box<Expr>),
    }

    /// xorshift64* — small deterministic PRNG so failures reproduce from a seed.
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
            self.next() % n
        }
    }

    const U32_EDGES: &[i64] = &[
        0,
        1,
        2,
        255,
        256,
        65535,
        65536,
        0x7FFF_FFFF,
        0x8000_0000,
        0xFFFF_FFFF,
    ];
    const I32_EDGES: &[i64] = &[
        0,
        1,
        -1,
        2,
        -2,
        255,
        -256,
        32767,
        -32768,
        2_147_483_647,
        -2_147_483_647,
    ];

    fn gen_lit(rng: &mut Rng, ty: Ty) -> i64 {
        if rng.below(100) < 50 {
            let pool = match ty {
                Ty::U32 => U32_EDGES,
                Ty::I32 => I32_EDGES,
            };
            pool[rng.below(pool.len() as u64) as usize]
        } else {
            match ty {
                Ty::U32 => i64::from(rng.next() as u32),
                // full i32 range except MIN (which has no writable negative literal)
                Ty::I32 => (i64::from(rng.next() as u32) % 4_294_967_295) - 2_147_483_647,
            }
        }
    }

    /// A nonzero divisor; for signed, also avoid -1 (INT_MIN / -1 overflows).
    fn gen_divisor(rng: &mut Rng, ty: Ty) -> i64 {
        loop {
            let v = gen_lit(rng, ty);
            match ty {
                Ty::U32 if (v as u32) != 0 => return v,
                Ty::I32 if (v as i32) != 0 && (v as i32) != -1 => return v,
                _ => {}
            }
        }
    }

    fn gen_expr(rng: &mut Rng, ty: Ty, depth: u32) -> Expr {
        if depth == 0 || rng.below(100) < 30 {
            return Expr::Lit(gen_lit(rng, ty));
        }
        let op = [
            Op::Add,
            Op::Sub,
            Op::Mul,
            Op::Div,
            Op::Rem,
            Op::And,
            Op::Or,
            Op::Xor,
            Op::Shl,
            Op::Shr,
        ][rng.below(10) as usize];
        let l = gen_expr(rng, ty, depth - 1);
        let r = match op {
            Op::Div | Op::Rem => Expr::Lit(gen_divisor(rng, ty)),
            // shift count in 0..32 keeps the shift well-defined
            Op::Shl | Op::Shr => Expr::Lit(rng.below(32) as i64),
            _ => gen_expr(rng, ty, depth - 1),
        };
        Expr::Bin(Box::new(l), op, Box::new(r))
    }

    /// Evaluate to the u32 bit pattern, using `ty`'s arithmetic.
    fn eval(e: &Expr, ty: Ty) -> u32 {
        match e {
            Expr::Lit(n) => match ty {
                Ty::U32 => *n as u32,
                Ty::I32 => *n as i32 as u32,
            },
            Expr::Bin(l, op, r) => {
                let a = eval(l, ty);
                let b = eval(r, ty);
                match ty {
                    Ty::U32 => match op {
                        Op::Add => a.wrapping_add(b),
                        Op::Sub => a.wrapping_sub(b),
                        Op::Mul => a.wrapping_mul(b),
                        Op::Div => a / b,
                        Op::Rem => a % b,
                        Op::And => a & b,
                        Op::Or => a | b,
                        Op::Xor => a ^ b,
                        Op::Shl => a.wrapping_shl(b),
                        Op::Shr => a.wrapping_shr(b),
                    },
                    Ty::I32 => {
                        let (x, y) = (a as i32, b as i32);
                        let v = match op {
                            Op::Add => x.wrapping_add(y),
                            Op::Sub => x.wrapping_sub(y),
                            Op::Mul => x.wrapping_mul(y),
                            Op::Div => x.wrapping_div(y),
                            Op::Rem => x.wrapping_rem(y),
                            Op::And => x & y,
                            Op::Or => x | y,
                            Op::Xor => x ^ y,
                            Op::Shl => x.wrapping_shl(b),
                            Op::Shr => x.wrapping_shr(b),
                        };
                        v as u32
                    }
                }
            }
        }
    }

    fn op_str(op: Op) -> &'static str {
        match op {
            Op::Add => "+",
            Op::Sub => "-",
            Op::Mul => "*",
            Op::Div => "/",
            Op::Rem => "%",
            Op::And => "&",
            Op::Or => "|",
            Op::Xor => "^",
            Op::Shl => "<<",
            Op::Shr => ">>",
        }
    }

    fn render(e: &Expr, ty: Ty) -> String {
        match e {
            Expr::Lit(n) => match ty {
                Ty::U32 => format!("{}", *n as u32),
                Ty::I32 => {
                    let v = *n as i32;
                    if v < 0 {
                        format!("({v}i32)")
                    } else {
                        format!("{v}i32")
                    }
                }
            },
            Expr::Bin(l, op, r) => {
                format!("({} {} {})", render(l, ty), op_str(*op), render(r, ty))
            }
        }
    }

    fn write_generated(name: &str, source: &str) -> PathBuf {
        let path = fixtures_dir().join(name);
        std::fs::write(&path, source).expect("write generated fixture");
        path
    }

    #[test]
    fn exec_property_arith() {
        if !tools_available() {
            eprintln!("skipping property test: toolchain not found");
            return;
        }
        let seed = std::env::var("BML_PROP_SEED")
            .ok()
            .and_then(|s| match s.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            })
            .unwrap_or(0x1234_5678_9ABC_DEF0u64);
        let mut rng = Rng(seed);

        let mut body = String::new();
        for _ in 0..120 {
            for ty in [Ty::U32, Ty::I32] {
                let e = gen_expr(&mut rng, ty, 4);
                let expected = eval(&e, ty);
                let src = render(&e, ty);
                match ty {
                    Ty::U32 => body.push_str(&format!("    expect_u32({src}, {expected});\n")),
                    Ty::I32 => {
                        body.push_str(&format!("    expect_u32(({src}) as u32, {expected});\n"));
                    }
                }
            }
        }
        let program = format!(
            "// GENERATED by exec_property_arith (seed {seed:#018x}). Do not edit.\n\
         import harness.semihost;\n\
         fn main() @context(thread) {{\n{body}    done();\n}}\n"
        );

        let name = "_generated_arith.bml";
        let path = write_generated(name, &program);
        let mut failures = Vec::new();
        for opt in OPT_LEVELS {
            let out = bml_run(name, opt);
            if !out.contains("OK") || out.contains("FAIL") {
                failures.push(format!(
                    "-O{opt}: {} OK / {} FAIL",
                    count(&out, "OK"),
                    count(&out, "FAIL")
                ));
            }
        }
        let _ = std::fs::remove_file(&path);

        assert!(
            failures.is_empty(),
            "property test failed (seed {seed:#018x}): {}\n--- generated source ---\n{program}",
            failures.join("; ")
        );
    }

    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }
}
