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
    bml_run_with_target(fixture, opt, "qemu.target")
}

/// As `bml_run`, but with an explicit target file -- lets region/placement
/// fixtures use a mem-block layout instead of the flat default target.
fn bml_run_with_target(fixture: &str, opt: &str, target_file: &str) -> String {
    let dir = fixtures_dir();
    let src = dir.join(fixture);
    let target = dir.join(target_file);

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
    let mut link_cmd = Command::new(ld_bin());
    link_cmd.arg("-T").arg(&lds).arg(&obj);
    // Float fixtures lower to soft-float `__aeabi_*` runtime calls (the cortex-m3
    // has no FPU); pull in libgcc to resolve them. Harmless for integer-only
    // fixtures -- the linker pulls nothing from it.
    if let Some(libgcc) = libgcc_path() {
        link_cmd.arg(&libgcc);
    }
    let link = link_cmd
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

/// xorshift64* — small deterministic PRNG so failures reproduce from a seed.
/// Shared by the generative tests (`property`, `build_validity`).
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

/// Write a generated fixture into the exec fixtures dir (so its imports, if any,
/// resolve) and return its path.
fn write_generated(name: &str, source: &str) -> PathBuf {
    let path = fixtures_dir().join(name);
    std::fs::write(&path, source).expect("write generated fixture");
    path
}

// ─── smoke ──────────────────────────────────────────────────────────────────
assert_exec!(exec_smoke, "smoke.bml");

// ─── integer wrapping (design-decisions.md §8) ───────────────────────────────
assert_exec!(exec_wrapping, "wrapping.bml");

// ─── casts / widths (language.md §1) ─────────────────────────────────────────
assert_exec!(exec_casts, "casts.bml");

// Prefix unary (`&`, `*`) binds tighter than `as`, so the bare `&x as u32` /
// `*p as u32` forms group as `(&x) as u32` / `(*p) as u32`. Pins that precedence
// at runtime; the wrong grouping would not even typecheck.
assert_exec!(exec_addr_of_cast, "addr_of_cast.bml");

// A local `const` with a compile-time initializer drives an array length
// (`const N = 4; var buf: [u32; N];`), and is also read as a runtime value.
assert_exec!(exec_local_const_array_len, "local_const_array_len.bml");

// ─── for loops (language.md §11) ─────────────────────────────────────────────
assert_exec!(exec_for_loops, "for_loops.bml");

// ─── pointer arithmetic (language.md §5) ─────────────────────────────────────
assert_exec!(exec_pointers, "pointers.bml");

// ─── struct semantics (language.md §6) ───────────────────────────────────────
assert_exec!(exec_structs, "structs.bml");
assert_exec!(exec_struct_layout_repr, "struct_layout_repr.bml");
assert_exec!(exec_struct_field_endian, "struct_field_endian.bml");

// ─── enum semantics (language.md §7) ─────────────────────────────────────────
assert_exec!(exec_enums, "enums.bml");

// ─── register read-modify-write (language.md §9) ─────────────────────────────
assert_exec!(exec_register_rmw, "register_rmw.bml");

// ─── bit view (memory-views): set/clear, RMW neighbor preservation, byte ─────
// crossing, and a nonzero bit_offset, checked by behavior under QEMU.
assert_exec!(exec_bit_view, "bit_view.bml");

// A mutable view indexed in write and read loops (index-read does not consume
// the view), with the summed values checked.
assert_exec!(exec_view_mut_loop, "view_mut_loop.bml");

// A view over an array static reads the right elements, which also pins that an
// array static emits its real initializer (not a scalar 0).
assert_exec!(exec_static_array_view, "static_array_view.bml");

// A ring view over a power-of-two array: pins that the `& (cap-1)` mask maps
// logical -> physical correctly, including wraparound (head + i >= cap).
assert_exec!(exec_ring_mask, "ring_mask.bml");

// The symmetry counterpart: a non-power-of-two ring keeps the `urem` physical
// index and must wrap correctly too.
assert_exec!(exec_ring_npot_wrap, "ring_npot_wrap.bml");

// A strided view maps logical index i -> physical i*K for reads and writes, at
// power-of-two and non-power-of-two strides. The IR-substring test for this
// lowering passes regardless of the stride constant, so this pins the value.
assert_exec!(exec_view_strided, "view_strided.bml");

// Compound assignment (`+=` ... `>>=`) desugars to `a = a OP b` and works across
// locals, array elements, struct fields, pointer derefs, and peripheral fields.
assert_exec!(exec_compound_assign, "compound_assign.bml");

// Compound assignment across signedness and width: arithmetic shift / signed
// division, narrow-type wrapping, and 64-bit operands.
assert_exec!(exec_compound_assign_widths, "compound_assign_widths.bml");

// Inline asm with operands (outputs/inputs/clobbers, single + struct-return).
assert_exec!(exec_asm_operands, "asm_operands.bml");

// ─── control flow & expressions (language.md §11) ────────────────────────────
assert_exec!(exec_match_int, "match_int.bml");
assert_exec!(exec_control_flow, "control_flow.bml");

// `loop` is a distinct lowering branch from `while` (control_flow covers the
// latter): checks that `break` exits and `continue` skips the iteration body.
assert_exec!(exec_loop_break_continue, "loop_break_continue.bml");

// `@align(N)` over-aligns a static; the linker script must honor it so the
// runtime address is actually aligned (DMA buffers etc.).
assert_exec!(exec_align_static, "align_static.bml");

// `in <region>` places a static at its region's mem block address. Uses a
// dedicated mem-block target (qemu_regions.target) so the placement is at a
// distinct address from ordinary statics, checked at runtime. See
// doc/regions-agents-plan.md slice 1.
#[test]
fn exec_region_placement() {
    if !tools_available() {
        eprintln!("skipping region_placement.bml: qemu-system-arm / arm-none-eabi-ld not found");
        return;
    }
    for opt in OPT_LEVELS {
        let out = bml_run_with_target("region_placement.bml", opt, "qemu_regions.target");
        assert!(
            out.contains("OK"),
            "expected OK from region_placement at -O{opt}; captured output:\n{out}"
        );
        assert!(
            !out.contains("FAIL"),
            "region_placement reported a FAIL at -O{opt}; captured output:\n{out}"
        );
    }
}

// word_addr handoff encoding: source writes the byte address into a field at
// bit 2; the compiler inserts `>> 2` so the register reads back as the byte
// address. Without the insertion the register would hold the wrong value, so a
// passing run proves the encoding (not just that some IR was emitted). See
// doc/regions-agents-plan.md slice 3.
#[test]
fn exec_handoff_encode() {
    if !tools_available() {
        eprintln!("skipping handoff_encode.bml: qemu-system-arm / arm-none-eabi-ld not found");
        return;
    }
    for opt in OPT_LEVELS {
        let out = bml_run_with_target("handoff_encode.bml", opt, "handoff_encode.target");
        assert!(
            out.contains("OK"),
            "expected OK from handoff_encode at -O{opt}; captured output:\n{out}"
        );
        assert!(
            !out.contains("FAIL"),
            "handoff_encode reported a FAIL at -O{opt}; captured output:\n{out}"
        );
    }
}

// ─── const evaluation (language.md §1) ───────────────────────────────────────
assert_exec!(exec_const_eval, "const_eval.bml");

// ─── unsuffixed literals adopt the narrow context type (language.md §1) ───────
assert_exec!(exec_narrow_literals, "narrow_literals.bml");

// Regression (found by the all-widths property test, fixed): an unsuffixed
// literal above 2^32-1 in a 64-bit context used to be materialized at i32 width
// and truncated. The literal is now emitted at 64 bits in a 64-bit context.
assert_exec!(exec_lit_u64_unsuffixed, "lit_u64_unsuffixed.bml");

// ─── operator / signedness matrix (language.md §1) ───────────────────────────
assert_exec!(exec_div_mod_ops, "div_mod_ops.bml");
assert_exec!(exec_shift_ops, "shift_ops.bml");
assert_exec!(exec_compare_ops, "compare_ops.bml");
assert_exec!(exec_bool_ops, "bool_ops.bml");

// ─── float arithmetic / casts compute correct values (language.md §1) ─────────
// Unlike the build-validity fuzzer (which only checks float IR is *valid*), this
// runs the program and checks the results. Float ops lower to soft-float
// `__aeabi_*` calls, so it additionally needs libgcc; skips if absent.
#[test]
fn exec_float_ops() {
    if !tools_available() || libgcc_path().is_none() {
        eprintln!(
            "skipping exec_float_ops: needs qemu, arm-none-eabi-ld, and \
             arm-none-eabi-gcc (for soft-float libgcc)"
        );
        return;
    }
    for opt in OPT_LEVELS {
        let out = bml_run("float_ops.bml", opt);
        assert!(
            out.contains("OK"),
            "expected OK from float_ops at -O{opt}; got:\n{out}"
        );
        assert!(
            !out.contains("FAIL"),
            "float_ops reported a FAIL at -O{opt}; got:\n{out}"
        );
    }
}

// ─── const-valued array lengths (language.md §1) ─────────────────────────────
assert_exec!(exec_const_array_len, "const_array_len.bml");
assert_exec!(exec_const_aggregate_len, "const_aggregate_len.bml");
assert_exec!(exec_const_ref_init, "const_ref_init.bml");
assert_exec!(exec_const_cast_bool, "const_cast_bool.bml");

// ─── array values: init / index read / index write / var index (language.md §6)
assert_exec!(exec_arrays, "arrays.bml");

// ─── match as an expression (language.md §11, §7) ────────────────────────────
assert_exec!(exec_match_expr, "match_expr.bml");

// ─── if- and block-expressions yield values (language.md §11) ────────────────
assert_exec!(exec_if_block_expr, "if_block_expr.bml");

// ─── function-pointer dispatch (language.md §5) ──────────────────────────────
assert_exec!(exec_fn_ptr, "fn_ptr.bml");

// ─── bool → int cast zero-extends to 0/1 (language.md §1) ────────────────────
assert_exec!(exec_bool_to_int, "bool_to_int.bml");

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
    clippy::cast_lossless,
    clippy::many_single_char_names,
    clippy::doc_markdown,
    clippy::format_push_string
)]
mod property {
    use super::{OPT_LEVELS, Rng, bml_run, libgcc_path, tools_available, write_generated};

    /// Every integer type, not just 32-bit. Each carries its own wrapping /
    /// signedness rules, so width-dependent bugs (u8 mul wrap, i64 shifts,
    /// narrow sign-extension on the way to the u32 check) become observable.
    #[derive(Clone, Copy, PartialEq)]
    enum Ty {
        U8,
        U16,
        U32,
        U64,
        I8,
        I16,
        I32,
        I64,
    }
    use Ty::{I8, I16, I32, I64, U8, U16, U32, U64};
    const TYPES: &[Ty] = &[U8, U16, U32, U64, I8, I16, I32, I64];

    fn width(t: Ty) -> u32 {
        match t {
            U8 | I8 => 8,
            U16 | I16 => 16,
            U32 | I32 => 32,
            U64 | I64 => 64,
        }
    }
    fn is_signed(t: Ty) -> bool {
        matches!(t, I8 | I16 | I32 | I64)
    }
    fn suffix(t: Ty) -> &'static str {
        match t {
            U8 => "u8",
            U16 => "u16",
            U32 => "u32",
            U64 => "u64",
            I8 => "i8",
            I16 => "i16",
            I32 => "i32",
            I64 => "i64",
        }
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

    /// `Lit` holds the `width`-bit value as a zero-extended bit pattern, so it
    /// can represent any unsigned or signed value across all widths uniformly.
    enum Expr {
        Lit(u64),
        Bin(Box<Expr>, Op, Box<Expr>),
    }

    /// Low `w` bits set.
    fn mask(w: u32) -> u64 {
        if w >= 64 { u64::MAX } else { (1u64 << w) - 1 }
    }
    /// Sign-extend a `w`-bit pattern to its true signed value.
    fn sext(bits: u64, w: u32) -> i64 {
        if w >= 64 {
            bits as i64
        } else {
            let shift = 64 - w;
            ((bits << shift) as i64) >> shift
        }
    }

    /// A `width`-bit pattern: half the time a boundary value (0, ±1, min+1,
    /// max, halfway), otherwise uniform random. The signed minimum is avoided:
    /// it has no writable negative literal (its magnitude overflows the type).
    fn gen_lit(rng: &mut Rng, t: Ty) -> u64 {
        let w = width(t);
        let m = mask(w);
        let min_pat = 1u64 << (w - 1); // signed minimum: sign bit only
        if rng.below(100) < 50 {
            if is_signed(t) {
                let max = (m >> 1) as i64; // 2^(w-1) - 1
                let edges = [0i64, 1, -1, 2, -2, max, -max, max / 2, -max / 2];
                let v = edges[rng.below(edges.len() as u64) as usize];
                (v as u64) & m
            } else {
                let edges = [0u64, 1, 2, m, m >> 1, (m >> 1) + 1, 0xFF & m, 0xFFFF & m];
                edges[rng.below(edges.len() as u64) as usize] & m
            }
        } else {
            let bits = rng.next() & m;
            // Avoid the signed minimum pattern (nudge to min+1).
            if is_signed(t) && bits == min_pat {
                min_pat | 1
            } else {
                bits
            }
        }
    }

    /// A nonzero divisor; for signed, also avoid -1 (INT_MIN / -1 is UB).
    fn gen_divisor(rng: &mut Rng, t: Ty) -> u64 {
        let w = width(t);
        loop {
            let bits = gen_lit(rng, t);
            if is_signed(t) {
                let v = sext(bits, w);
                if v != 0 && v != -1 {
                    return bits;
                }
            } else if bits & mask(w) != 0 {
                return bits;
            }
        }
    }

    /// A shift count in `0..width`, where the shift is always well-defined.
    fn gen_shift(rng: &mut Rng, t: Ty) -> u64 {
        u64::from(rng.below(u64::from(width(t))) as u32)
    }

    fn gen_expr(rng: &mut Rng, t: Ty, depth: u32) -> Expr {
        if depth == 0 || rng.below(100) < 30 {
            return Expr::Lit(gen_lit(rng, t));
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
        let l = gen_expr(rng, t, depth - 1);
        let r = match op {
            Op::Div | Op::Rem => Expr::Lit(gen_divisor(rng, t)),
            Op::Shl | Op::Shr => Expr::Lit(gen_shift(rng, t)),
            _ => gen_expr(rng, t, depth - 1),
        };
        Expr::Bin(Box::new(l), op, Box::new(r))
    }

    /// Generate one `eval_*` per Rust integer type. Each interprets `Lit` and
    /// operand patterns as `$t`, evaluates with `$t`'s native wrapping / signed
    /// vs unsigned semantics (the trusted oracle), and returns the result's
    /// zero-extended bit pattern. Using the real Rust type is what makes the
    /// oracle exact for that width.
    macro_rules! eval_ty {
        ($name:ident, $t:ty, $u:ty) => {
            fn $name(e: &Expr) -> u64 {
                match e {
                    Expr::Lit(b) => u64::from(*b as $u),
                    Expr::Bin(l, op, r) => {
                        let a = $name(l) as $u as $t;
                        let b = $name(r) as $u as $t;
                        let v: $t = match op {
                            Op::Add => a.wrapping_add(b),
                            Op::Sub => a.wrapping_sub(b),
                            Op::Mul => a.wrapping_mul(b),
                            Op::Div => a.wrapping_div(b),
                            Op::Rem => a.wrapping_rem(b),
                            Op::And => a & b,
                            Op::Or => a | b,
                            Op::Xor => a ^ b,
                            Op::Shl => a.wrapping_shl(b as u32),
                            Op::Shr => a.wrapping_shr(b as u32),
                        };
                        v as $u as u64
                    }
                }
            }
        };
    }
    eval_ty!(eval_u8, u8, u8);
    eval_ty!(eval_u16, u16, u16);
    eval_ty!(eval_u32, u32, u32);
    eval_ty!(eval_u64, u64, u64);
    eval_ty!(eval_i8, i8, u8);
    eval_ty!(eval_i16, i16, u16);
    eval_ty!(eval_i32, i32, u32);
    eval_ty!(eval_i64, i64, u64);

    fn eval(e: &Expr, t: Ty) -> u64 {
        match t {
            U8 => eval_u8(e),
            U16 => eval_u16(e),
            U32 => eval_u32(e),
            U64 => eval_u64(e),
            I8 => eval_i8(e),
            I16 => eval_i16(e),
            I32 => eval_i32(e),
            I64 => eval_i64(e),
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

    fn render(e: &Expr, t: Ty) -> String {
        match e {
            Expr::Lit(b) => {
                let s = suffix(t);
                if is_signed(t) {
                    let v = sext(*b, width(t));
                    if v < 0 {
                        format!("({v}{s})")
                    } else {
                        format!("{v}{s}")
                    }
                } else {
                    format!("{}{s}", *b & mask(width(t)))
                }
            }
            Expr::Bin(l, op, r) => {
                format!("({} {} {})", render(l, t), op_str(*op), render(r, t))
            }
        }
    }

    #[test]
    fn exec_property_arith() {
        // 32-bit integer division (cortex-m3 has no divide instruction) and all
        // 64-bit multiply/divide lower to `__aeabi_*` runtime calls in libgcc,
        // so this needs the soft-float toolchain like `exec_float_ops` does.
        if !tools_available() || libgcc_path().is_none() {
            eprintln!(
                "skipping property test: needs qemu, arm-none-eabi-ld, and \
                 arm-none-eabi-gcc (libgcc, for __aeabi_* div/mul)"
            );
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
        for _ in 0..64 {
            for &t in TYPES {
                let e = gen_expr(&mut rng, t, 4);
                let bits = eval(&e, t);
                let src = render(&e, t);
                if width(t) <= 32 {
                    // `as u32` zero-extends an unsigned source and sign-extends a
                    // signed one; the expected value must match that widening.
                    let expected = if is_signed(t) {
                        sext(bits, width(t)) as u32
                    } else {
                        bits as u32
                    };
                    body.push_str(&format!("    expect_u32(({src}) as u32, {expected});\n"));
                } else {
                    // 64-bit: compare the full pattern. `as u64` on a signed
                    // value is a same-width reinterpretation (the bit pattern).
                    let lhs = if is_signed(t) {
                        format!("({src}) as u64")
                    } else {
                        src
                    };
                    body.push_str(&format!("    expect_u64({lhs}, {bits}u64);\n"));
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

// ─── float value-differential ────────────────────────────────────────────────
//
// The integer property test value-checks arithmetic; floats only had
// build-validity (does it compile) and a few hand-written values. This generates
// random f32/f64 expressions, evaluates each with a Rust oracle at the matching
// precision, then on-device reinterprets the result's *bits* (via `(&v) as *u32`
// / `*u64`, since bml's `f as u32` is a numeric convert, not a bit cast) and
// compares them to `to_bits()`. Comparing bits, not values, makes the check
// exact and sidesteps re-parsing a computed float literal.
//
// `+ - * /` are IEEE-754 correctly-rounded on both sides (Rust and libgcc
// soft-float), so finite results are bit-identical. Non-finite oracle results
// (inf/NaN, whose bit patterns can differ) are rejected by resampling, and
// divisors are nonzero literals, so no division-by-zero.
#[allow(
    clippy::cast_lossless,
    clippy::many_single_char_names,
    clippy::doc_markdown,
    clippy::format_push_string
)]
mod property_float {
    use super::{OPT_LEVELS, Rng, bml_run, libgcc_path, tools_available, write_generated};

    #[derive(Clone, Copy, PartialEq)]
    enum Ty {
        F32,
        F64,
    }

    #[derive(Clone, Copy)]
    enum Op {
        Add,
        Sub,
        Mul,
        Div,
    }

    /// A literal is an index into `POOL` plus a sign. The pool holds values that
    /// are exactly representable in both f32 and f64 (so the rendered decimal
    /// parses to the identical value on device and in the oracle); the paired
    /// string is the exact bml spelling.
    enum Expr {
        Lit { idx: usize, neg: bool },
        Bin(Box<Expr>, Op, Box<Expr>),
    }

    const POOL: &[(&str, f64)] = &[
        ("0.0", 0.0),
        ("0.125", 0.125),
        ("0.25", 0.25),
        ("0.5", 0.5),
        ("1.0", 1.0),
        ("2.5", 2.5),
        ("3.25", 3.25),
        ("4.0", 4.0),
        ("7.75", 7.75),
        ("8.0", 8.0),
        ("10.0", 10.0),
        ("100.0", 100.0),
    ];

    fn lit_val(idx: usize, neg: bool) -> f64 {
        if neg { -POOL[idx].1 } else { POOL[idx].1 }
    }

    fn gen_lit(rng: &mut Rng) -> Expr {
        Expr::Lit {
            idx: rng.below(POOL.len() as u64) as usize,
            neg: rng.below(2) == 1,
        }
    }

    /// A nonzero literal divisor (index 0 is `0.0`), so no division by zero.
    fn gen_divisor(rng: &mut Rng) -> Expr {
        Expr::Lit {
            idx: 1 + rng.below(POOL.len() as u64 - 1) as usize,
            neg: rng.below(2) == 1,
        }
    }

    fn gen_expr(rng: &mut Rng, depth: u32) -> Expr {
        if depth == 0 || rng.below(100) < 35 {
            return gen_lit(rng);
        }
        let op = [Op::Add, Op::Sub, Op::Mul, Op::Div][rng.below(4) as usize];
        let l = gen_expr(rng, depth - 1);
        let r = match op {
            Op::Div => gen_divisor(rng),
            _ => gen_expr(rng, depth - 1),
        };
        Expr::Bin(Box::new(l), op, Box::new(r))
    }

    fn eval_f32(e: &Expr) -> f32 {
        match e {
            Expr::Lit { idx, neg } => lit_val(*idx, *neg) as f32,
            Expr::Bin(l, op, r) => {
                let (a, b) = (eval_f32(l), eval_f32(r));
                match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div => a / b,
                }
            }
        }
    }

    fn eval_f64(e: &Expr) -> f64 {
        match e {
            Expr::Lit { idx, neg } => lit_val(*idx, *neg),
            Expr::Bin(l, op, r) => {
                let (a, b) = (eval_f64(l), eval_f64(r));
                match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div => a / b,
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
        }
    }

    fn render(e: &Expr, t: Ty) -> String {
        let suf = if t == Ty::F32 { "f" } else { "d" };
        match e {
            Expr::Lit { idx, neg } => {
                let s = POOL[*idx].0;
                if *neg {
                    format!("(-{s}{suf})")
                } else {
                    format!("{s}{suf}")
                }
            }
            Expr::Bin(l, op, r) => {
                format!("({} {} {})", render(l, t), op_str(*op), render(r, t))
            }
        }
    }

    #[test]
    fn exec_property_float() {
        // Soft-float `__aeabi_*` calls live in libgcc (cortex-m3 has no FPU).
        if !tools_available() || libgcc_path().is_none() {
            eprintln!(
                "skipping float property test: needs qemu, arm-none-eabi-ld, and \
                 arm-none-eabi-gcc (libgcc, for soft-float)"
            );
            return;
        }
        let seed = std::env::var("BML_PROP_SEED")
            .ok()
            .and_then(|s| match s.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            })
            .unwrap_or(0x0F10_A732_1234_5678u64);
        let mut rng = Rng(seed);

        let mut body = String::new();
        let mut k = 0u32;
        for _ in 0..60 {
            for t in [Ty::F32, Ty::F64] {
                // Resample until the oracle result is finite: inf/NaN bit
                // patterns can legitimately differ across implementations.
                let (src, bits, is32) = loop {
                    let e = gen_expr(&mut rng, 4);
                    let src = render(&e, t);
                    match t {
                        Ty::F32 => {
                            let v = eval_f32(&e);
                            if v.is_finite() {
                                break (src, u64::from(v.to_bits()), true);
                            }
                        }
                        Ty::F64 => {
                            let v = eval_f64(&e);
                            if v.is_finite() {
                                break (src, v.to_bits(), false);
                            }
                        }
                    }
                };
                // Bind the result, reinterpret its address as int, compare bits.
                if is32 {
                    body.push_str(&format!("    var vf{k}: f32 = {src};\n"));
                    body.push_str(&format!("    var pf{k}: *u32 = (&vf{k}) as *u32;\n"));
                    body.push_str(&format!("    expect_u32(*pf{k}, {bits});\n"));
                } else {
                    body.push_str(&format!("    var vf{k}: f64 = {src};\n"));
                    body.push_str(&format!("    var pf{k}: *u64 = (&vf{k}) as *u64;\n"));
                    body.push_str(&format!("    expect_u64(*pf{k}, {bits}u64);\n"));
                }
                k += 1;
            }
        }
        let program = format!(
            "// GENERATED by exec_property_float (seed {seed:#018x}). Do not edit.\n\
         import harness.semihost;\n\
         fn main() @context(thread) {{\n{body}    done();\n}}\n"
        );

        let name = "_generated_float.bml";
        let path = write_generated(name, &program);
        let mut failures = Vec::new();
        for opt in OPT_LEVELS {
            let out = bml_run(name, opt);
            if !out.contains("OK") || out.contains("FAIL") {
                failures.push(format!(
                    "-O{opt}: {} OK / {} FAIL",
                    out.matches("OK").count(),
                    out.matches("FAIL").count()
                ));
            }
        }
        let _ = std::fs::remove_file(&path);

        assert!(
            failures.is_empty(),
            "float property test failed (seed {seed:#018x}): {}\n--- generated source ---\n{program}",
            failures.join("; ")
        );
    }
}

// ─── Tier-2 build-validity fuzzer ─────────────────────────────────────────────
//
// Generate random *well-typed* scalar expressions across every scalar type
// (and `as` casts between them), compile through the full `bml build` pipeline
// at -O0 and -O2, and link. The oracle is the LLVM/ld toolchain itself: a
// well-typed program that fails to build or link means we emitted IR the
// toolchain rejects -- i.e. a back-end bug -- not a wrong value (that needs an
// execution oracle, which this layer deliberately does not use). This is how
// the bool-cast and int<->float-cast bugs were found.

/// Locate the soft-float libgcc for the cortex-m3 (`thumb/v7-m/nofp`) multilib
/// via `arm-none-eabi-gcc`. Float ops on this FPU-less target lower to
/// `__aeabi_*` runtime calls that live in libgcc, so the link step needs it.
fn libgcc_path() -> Option<String> {
    static PATH: OnceLock<Option<String>> = OnceLock::new();
    PATH.get_or_init(|| {
        let out = Command::new("arm-none-eabi-gcc")
            .args(["-mcpu=cortex-m3", "-mthumb", "-print-libgcc-file-name"])
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!p.is_empty() && std::path::Path::new(&p).exists()).then_some(p)
    })
    .clone()
}

/// Tools for the build+link validity oracle. `llc`/`opt` are discovered by
/// `bml build` itself, so we only gate on the linker and the soft-float libgcc.
fn build_link_tools_available() -> bool {
    runnable(&ld_bin()) && libgcc_path().is_some()
}

/// Build `fixture` with `bml build` at `opt`, then link the object (with the
/// soft-float libgcc) to a throwaway ELF. Returns (success, captured log).
/// Unlike `bml_run` this never panics on failure -- the caller collects it.
fn bml_build_link(fixture: &str, opt: &str) -> (bool, String) {
    let dir = fixtures_dir();
    let src = dir.join(fixture);
    let target = dir.join("qemu.target");

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
    let mut log = format!(
        "[build -O{opt}]\n{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    let mut ok = build.status.success() && obj.exists();
    if ok {
        let stem = src.file_stem().unwrap().to_string_lossy().to_string();
        let pid = std::process::id();
        let elf = std::env::temp_dir().join(format!("bml-build-{stem}-O{opt}-{pid}.elf"));
        let libgcc = libgcc_path().expect("libgcc present (gated)");
        let link = Command::new(ld_bin())
            .arg("-T")
            .arg(&lds)
            .arg(&obj)
            .arg(&libgcc)
            .arg("-o")
            .arg(&elf)
            .output()
            .expect("failed to run linker");
        // The linker emits benign warnings (RWX segment, GNU-stack note) to
        // stderr but still succeeds; only a non-zero status is a real failure.
        if !link.status.success() {
            use std::fmt::Write;
            ok = false;
            let _ = write!(
                log,
                "[link -O{opt}]\n{}",
                String::from_utf8_lossy(&link.stderr)
            );
        }
        let _ = std::fs::remove_file(&elf);
    }

    let _ = std::fs::remove_file(src.with_extension("ll"));
    let _ = std::fs::remove_file(src.with_extension("opt.ll"));
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&lds);

    (ok, log)
}

// Short mathematical names and deliberate width casts in the generator trip the
// usual pedantic lints; they are noise here.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    clippy::many_single_char_names,
    clippy::doc_markdown,
    clippy::format_push_string
)]
mod build_validity {
    use super::{OPT_LEVELS, Rng, bml_build_link, build_link_tools_available, write_generated};

    #[derive(Clone, Copy, PartialEq)]
    enum Sty {
        I8,
        I16,
        I32,
        I64,
        U8,
        U16,
        U32,
        U64,
        F32,
        F64,
        B1,
        B8,
    }
    use Sty::{B1, B8, F32, F64, I8, I16, I32, I64, U8, U16, U32, U64};

    // f16 is "ARM VFP only" (language.md §1) and the cortex-m3 has no FPU, so it
    // is intentionally excluded to avoid ABI false-positives unrelated to the
    // lowering paths under test.
    const NUMERIC: &[Sty] = &[I8, I16, I32, I64, U8, U16, U32, U64, F32, F64];
    const ALL: &[Sty] = &[I8, I16, I32, I64, U8, U16, U32, U64, F32, F64, B1, B8];
    const BINDABLE: &[Sty] = ALL;

    fn name(t: Sty) -> &'static str {
        match t {
            I8 => "i8",
            I16 => "i16",
            I32 => "i32",
            I64 => "i64",
            U8 => "u8",
            U16 => "u16",
            U32 => "u32",
            U64 => "u64",
            F32 => "f32",
            F64 => "f64",
            B1 => "b1",
            B8 => "b8",
        }
    }

    fn is_int(t: Sty) -> bool {
        matches!(t, I8 | I16 | I32 | I64 | U8 | U16 | U32 | U64)
    }
    fn is_signed(t: Sty) -> bool {
        matches!(t, I8 | I16 | I32 | I64)
    }
    fn is_float(t: Sty) -> bool {
        matches!(t, F32 | F64)
    }
    fn is_bool(t: Sty) -> bool {
        matches!(t, B1 | B8)
    }
    fn int_width(t: Sty) -> u32 {
        match t {
            I8 | U8 => 8,
            I16 | U16 => 16,
            I64 | U64 => 64,
            _ => 32,
        }
    }

    fn pick(rng: &mut Rng, pool: &[Sty]) -> Sty {
        pool[rng.below(pool.len() as u64) as usize]
    }

    /// In-range integer literal. Signed values skip the type minimum (which has
    /// no writable negative literal); negatives are parenthesised.
    fn int_lit(rng: &mut Rng, t: Sty) -> String {
        let w = int_width(t);
        let suffix = name(t);
        if is_signed(t) {
            let max: i128 = (1i128 << (w - 1)) - 1;
            let span = (2 * max + 1) as u128;
            let v = (u128::from(rng.next()) % span) as i128 - max;
            if v < 0 {
                format!("({v}{suffix})")
            } else {
                format!("{v}{suffix}")
            }
        } else {
            let v: u128 = if w >= 64 {
                u128::from(rng.next())
            } else {
                u128::from(rng.next()) % (1u128 << w)
            };
            format!("{v}{suffix}")
        }
    }

    fn float_lit(rng: &mut Rng, t: Sty) -> String {
        const POOL: &[&str] = &["0.0", "1.0", "0.5", "2.5", "3.25", "10.0", "100.0", "7.75"];
        let base = POOL[rng.below(POOL.len() as u64) as usize];
        let suffix = if matches!(t, F32) { "f" } else { "d" };
        if rng.below(2) == 1 {
            format!("(-{base}{suffix})")
        } else {
            format!("{base}{suffix}")
        }
    }

    /// A nonzero divisor with small magnitude in range; signed divisors avoid -1
    /// (INT_MIN / -1 overflows).
    fn int_divisor(rng: &mut Rng, t: Sty) -> String {
        let w = int_width(t);
        let suffix = name(t);
        let cap = if is_signed(t) {
            if w >= 64 { 1000 } else { (1u64 << (w - 1)) - 1 }
        } else if w >= 64 {
            1000
        } else {
            (1u64 << w) - 1
        };
        let mag = 1 + rng.below(cap.clamp(1, 1000));
        if is_signed(t) && rng.below(2) == 1 {
            let m = if mag == 1 { 2 } else { mag };
            format!("(-{m}{suffix})")
        } else {
            format!("{mag}{suffix}")
        }
    }

    /// Shift count in `0..width`, typed as the operand type.
    fn shift_amount(rng: &mut Rng, t: Sty) -> String {
        format!("{}{}", rng.below(u64::from(int_width(t))), name(t))
    }

    /// A non-constant leaf derived from the runtime `seed`, so the optimizer
    /// cannot fold the whole expression away (keeping -O2 coverage live).
    fn num_leaf(rng: &mut Rng, t: Sty) -> String {
        if rng.below(100) < 40 {
            if matches!(t, U32) {
                "seed".into()
            } else {
                format!("(seed as {})", name(t))
            }
        } else if is_float(t) {
            float_lit(rng, t)
        } else {
            int_lit(rng, t)
        }
    }

    fn gen_int(rng: &mut Rng, t: Sty, depth: u32) -> String {
        const OPS: &[&str] = &["+", "-", "*", "/", "%", "&", "|", "^", "<<", ">>"];
        let l = gen_expr(rng, t, depth - 1);
        let op = OPS[rng.below(OPS.len() as u64) as usize];
        let r = match op {
            "/" | "%" => int_divisor(rng, t),
            "<<" | ">>" => shift_amount(rng, t),
            _ => gen_expr(rng, t, depth - 1),
        };
        format!("({l} {op} {r})")
    }

    fn gen_float(rng: &mut Rng, t: Sty, depth: u32) -> String {
        const OPS: &[&str] = &["+", "-", "*", "/"];
        let l = gen_expr(rng, t, depth - 1);
        let op = OPS[rng.below(OPS.len() as u64) as usize];
        let r = gen_expr(rng, t, depth - 1);
        format!("({l} {op} {r})")
    }

    /// A b1 expression. Never produced via a cast (int/float -> bool casts are a
    /// separate, untested lowering path); only comparisons and bool ops.
    fn gen_bool(rng: &mut Rng, depth: u32) -> String {
        if depth == 0 || rng.below(100) < 25 {
            return if rng.below(2) == 1 { "true" } else { "false" }.into();
        }
        match rng.below(3) {
            0 => {
                const CMP: &[&str] = &["==", "!=", "<", ">", "<=", ">="];
                let nt = pick(rng, NUMERIC);
                let a = gen_expr(rng, nt, depth - 1);
                let b = gen_expr(rng, nt, depth - 1);
                let c = CMP[rng.below(CMP.len() as u64) as usize];
                format!("({a} {c} {b})")
            }
            1 => {
                let a = gen_bool(rng, depth - 1);
                let b = gen_bool(rng, depth - 1);
                let op = if rng.below(2) == 1 { "&&" } else { "||" };
                format!("({a} {op} {b})")
            }
            _ => format!("(!{})", gen_bool(rng, depth - 1)),
        }
    }

    fn gen_expr(rng: &mut Rng, t: Sty, depth: u32) -> String {
        if is_bool(t) {
            let b = gen_bool(rng, depth);
            return if matches!(t, B8) {
                format!("({b} as b8)")
            } else {
                b
            };
        }
        if depth == 0 {
            return num_leaf(rng, t);
        }
        match rng.below(100) {
            // cast from any source type into this numeric type
            0..=24 => {
                let src = pick(rng, ALL);
                format!("({} as {})", gen_expr(rng, src, depth - 1), name(t))
            }
            25..=44 => num_leaf(rng, t),
            _ if is_int(t) => gen_int(rng, t, depth),
            _ => gen_float(rng, t, depth),
        }
    }

    /// Build one program: N bindings of random typed expressions, each XORed
    /// (via `as u32`) into an accumulator that is finally stored to an
    /// `@external` static so nothing can be optimized out.
    fn build_program(seed: u64, n: usize) -> String {
        let mut rng = Rng(seed);
        let mut body = String::new();
        for k in 0..n {
            let t = pick(&mut rng, BINDABLE);
            let e = gen_expr(&mut rng, t, 3);
            body.push_str(&format!("    const v{k}: {} = {e};\n", name(t)));
            body.push_str(&format!("    acc = acc ^ (v{k} as u32);\n"));
        }
        format!(
            "// GENERATED by build_validity (seed {seed:#018x}). Do not edit.\n\
             var sink: u32 @external = 0;\n\
             fn main() @context(thread) {{\n\
             \x20   var seed: u32 = sink;\n\
             \x20   var acc: u32 = 0u32;\n\
             {body}    sink = acc;\n}}\n"
        )
    }

    #[test]
    fn build_validity_scalars() {
        if !build_link_tools_available() {
            eprintln!(
                "skipping build_validity: need arm-none-eabi-ld and \
                 arm-none-eabi-gcc (for soft-float libgcc)"
            );
            return;
        }
        let base = std::env::var("BML_PROP_SEED")
            .ok()
            .and_then(|s| match s.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => s.parse().ok(),
            })
            .unwrap_or(0x0BAD_C0DE_1234_5678u64);

        // Sweep several deterministic sub-seeds so one CI run explores more
        // structural variety; each is reproducible from the base via BML_PROP_SEED.
        let name = "_generated_build.bml";
        for round in 0..8u64 {
            let seed = base ^ round.wrapping_mul(0x9E37_79B9_7F4A_7C15);
            let program = build_program(seed, 150);
            let path = write_generated(name, &program);

            let mut failures = Vec::new();
            for opt in OPT_LEVELS {
                let (ok, log) = bml_build_link(name, opt);
                if !ok {
                    failures.push(log);
                }
            }
            let _ = std::fs::remove_file(&path);

            assert!(
                failures.is_empty(),
                "build-validity failed (seed {seed:#018x}):\n{}\n--- generated source ---\n{program}",
                failures.join("\n")
            );
        }
    }
}
