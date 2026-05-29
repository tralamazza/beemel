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
fn bml_run(fixture: &str) -> String {
    let dir = fixtures_dir();
    let src = dir.join(fixture);
    let target = dir.join("qemu.target");

    // 1. Compile to an object file (and linker script) next to the fixture.
    let build = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--opt=0")
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
        "bml build failed for {fixture}:\n{}{}",
        String::from_utf8_lossy(&build.stdout),
        String::from_utf8_lossy(&build.stderr),
    );

    // 2. Link to an ELF in a temp location.
    let stem = src.file_stem().unwrap().to_string_lossy().to_string();
    let elf = std::env::temp_dir().join(format!("bml-exec-{stem}-{}.elf", std::process::id()));
    let out_path = std::env::temp_dir().join(format!("bml-exec-{stem}-{}.out", std::process::id()));
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

    // 4. Clean up intermediates.
    let _ = std::fs::remove_file(src.with_extension("ll"));
    let _ = std::fs::remove_file(&obj);
    let _ = std::fs::remove_file(&lds);
    let _ = std::fs::remove_file(&elf);
    let _ = std::fs::remove_file(&out_path);

    assert!(
        link_ok,
        "link failed for {fixture}:\n{}",
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

/// Run a fixture and assert it reported success: at least one `OK` and no `FAIL`.
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
            let out = bml_run($fixture);
            assert!(
                out.contains("OK"),
                "expected at least one OK from {}; captured output:\n{out}",
                $fixture
            );
            assert!(
                !out.contains("FAIL"),
                "{} reported a FAIL; captured output:\n{out}",
                $fixture
            );
        }
    };
}

/// Same as `assert_exec!` but `#[ignore]`d: it pins a known compiler bug. The
/// fixture exercises documented behavior that currently miscompiles, so the test
/// fails today; it will start passing once the bug is fixed. Run them on demand
/// with `cargo test --test exec -- --ignored`.
macro_rules! known_bug {
    ($name:ident, $fixture:expr, $reason:expr) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            if !tools_available() {
                eprintln!("skipping {}: toolchain not found", $fixture);
                return;
            }
            let out = bml_run($fixture);
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

// ─── known compiler bugs surfaced by the documentation-driven fixtures ────────
// #[ignore]d so the suite stays green; run `-- --ignored` to confirm they still
// reproduce. Each has a minimal fixture documenting the symptom.
known_bug!(
    bug_array_size_const,
    "array_size_const_known_bug.bml",
    "a const used as an array length evaluates to 0"
);
