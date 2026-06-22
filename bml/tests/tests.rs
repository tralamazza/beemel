use std::path::PathBuf;
use std::process::Command;

/// A unique, fresh output directory for one `bml build --out-dir` run. Because
/// `bml build` derives its artifact paths (`<stem>.ll`/`.o`/`.ld`) from the
/// output directory, giving every build its own dir means two tests building the
/// SAME fixture in parallel never share a path -- no clobbered `.ll`, no "opt
/// failed" race -- and nothing is written into the source-controlled fixtures
/// directory. The caller removes the dir when done.
fn unique_out_dir(fixture: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "bml_build_{}_{}_{seq}",
        std::process::id(),
        fixture.replace('.', "_")
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// The `.ll` (or other artifact) path inside an `--out-dir`: the fixture's name
/// with its extension swapped, mirroring how `bml build` names outputs.
fn out_artifact(out_dir: &std::path::Path, fixture: &str, ext: &str) -> PathBuf {
    out_dir.join(PathBuf::from(fixture).with_extension(ext))
}

fn bml_check(fixture: &str) -> (bool, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture);

    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("check")
        .arg(&path)
        .output()
        .expect("failed to run bml");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    (output.status.success(), combined)
}

/// Extract the definition of a single function from LLVM IR text.
/// Finds lines from `define ... @fn_name(` through the closing `}`.
fn extract_fn_body(ir: &str, fn_name: &str) -> String {
    if let Some(start) = ir
        .lines()
        .position(|l| l.trim().starts_with("define") && l.contains(fn_name))
    {
        let lines: Vec<&str> = ir.lines().collect();
        let mut depth = 0;
        let mut end = start;
        for (i, line) in lines.iter().enumerate().skip(start) {
            let trimmed = line.trim();
            if trimmed.starts_with("define ") {
                depth = 0;
            }
            for ch in trimmed.chars() {
                if ch == '{' {
                    depth += 1;
                }
                if ch == '}' {
                    depth -= 1;
                }
            }
            if depth == 0 && i > start && trimmed == "}" {
                end = i;
                break;
            }
        }
        lines[start..=end].join("\n")
    } else {
        String::new()
    }
}

fn assert_alloca_before_first_label(body: &str, var_name: &str) {
    let alloca = format!("%__{var_name}.");
    let alloca_pos = body.find(&alloca).unwrap_or_else(|| {
        panic!("missing alloca for `{var_name}`\n--- IR ---\n{body}\n-----------")
    });
    let first_label_pos = body
        .lines()
        .scan(0usize, |offset, line| {
            let start = *offset;
            *offset += line.len() + 1;
            Some((start, line))
        })
        .find_map(|(offset, line)| {
            let label = line.trim();
            (label.ends_with(':') && label != "entry:").then_some(offset)
        })
        .unwrap_or(body.len());

    assert!(
        alloca_pos < first_label_pos,
        "expected `{var_name}` alloca in entry block before first label\n--- IR ---\n{body}\n-----------"
    );
}

fn bml_ir(fixture: &str) -> String {
    bml_ir_with_target(fixture, None)
}

/// Build a fixture with debug info (`-g`) and return the emitted `.ll`. Used to
/// assert DWARF metadata (e.g. view/ring descriptor composite types).
fn bml_ir_debug(fixture: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture);
    let out = unique_out_dir(fixture);
    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--opt=0")
        .arg("-g")
        .arg("--save-temps")
        .arg("--out-dir")
        .arg(&out)
        .arg(&path)
        .output()
        .expect("failed to run bml build -g");

    let ir = std::fs::read_to_string(out_artifact(&out, fixture, "ll")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&out);

    assert!(
        output.status.success() || !ir.is_empty(),
        "build -g failed before IR emission:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    ir
}

fn bml_ir_with_target(fixture: &str, target: Option<&str>) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture);
    let out = unique_out_dir(fixture);

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bml"));
    cmd.arg("build")
        .arg("--opt=0")
        .arg("--save-temps")
        .arg("--out-dir")
        .arg(&out);
    if let Some(t) = target {
        let tpath = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(t);
        cmd.arg("--target").arg(&tpath);
    }
    cmd.arg(&path);
    let output = cmd.output().expect("failed to run bml build");

    let ir = std::fs::read_to_string(out_artifact(&out, fixture, "ll")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&out);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() || !ir.is_empty(),
        "build failed before IR emission:\n{stdout}{stderr}"
    );

    ir
}

/// Compare `actual` against a committed snapshot under `tests/snapshots/`.
///
/// Snapshots capture the emitted IR for a fixture so that changes to lowering
/// show up as a reviewable diff instead of silently breaking a substring
/// assertion (or passing when behavior is wrong). Regenerate after an
/// intentional change with `UPDATE_SNAPSHOTS=1 cargo test`, then review the
/// diff before committing.
fn check_snapshot(name: &str, actual: &str) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("snapshots")
        .join(format!("{name}.snap"));
    let actual = actual.trim_end();
    if std::env::var_os("UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{actual}\n")).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!("missing snapshot `{name}`; run `UPDATE_SNAPSHOTS=1 cargo test` to create it")
    });
    assert_eq!(
        actual,
        expected.trim_end(),
        "snapshot `{name}` mismatch; if intended, run `UPDATE_SNAPSHOTS=1 cargo test` and review the diff"
    );
}

/// Snapshot a single function's emitted IR from a fixture.
fn snapshot_fn(snapshot: &str, fixture: &str, fn_name: &str) {
    check_snapshot(snapshot, &extract_fn_body(&bml_ir(fixture), fn_name));
}

macro_rules! assert_pass {
    ($name:ident, $fixture:expr) => {
        #[test]
        fn $name() {
            let (ok, output) = bml_check($fixture);
            assert!(ok, "expected pass, got failure:\n{output}");
        }
    };
}

macro_rules! assert_error {
    ($name:ident, $fixture:expr, $code:expr) => {
        #[test]
        fn $name() {
            let (ok, output) = bml_check($fixture);
            let code = $code;
            assert!(!ok, "expected error {code}, got success");
            assert!(
                output.contains(&format!("error[{code}]")),
                "expected error {code} in output:\n{output}"
            );
        }
    };
}

macro_rules! assert_warn {
    ($name:ident, $fixture:expr, $code:expr) => {
        #[test]
        fn $name() {
            let (ok, output) = bml_check($fixture);
            let code = $code;
            assert!(ok, "expected pass with warning, got failure:\n{output}");
            assert!(
                output.contains(&format!("warning[{code}]")),
                "expected warning {code} in output:\n{output}"
            );
        }
    };
}

macro_rules! assert_ir_contains {
    ($name:ident, $fixture:expr, $pattern:expr) => {
        #[test]
        fn $name() {
            let ir = bml_ir($fixture);
            let pattern = $pattern;
            assert!(
                ir.contains(pattern),
                "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
            );
        }
    };
}

macro_rules! assert_ir_contains_target {
    ($name:ident, $fixture:expr, $target:expr, $pattern:expr) => {
        #[test]
        fn $name() {
            let ir = bml_ir_with_target($fixture, Some($target));
            let pattern = $pattern;
            assert!(
                ir.contains(pattern),
                "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
            );
        }
    };
}

macro_rules! assert_ir_not_contains {
    ($name:ident, $fixture:expr, $pattern:expr) => {
        #[test]
        fn $name() {
            let ir = bml_ir($fixture);
            let pattern = $pattern;
            assert!(
                !ir.contains(pattern),
                "expected IR to NOT contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
            );
        }
    };
}

assert_pass!(test_uart, "uart.bml");
assert_pass!(test_floats, "floats.bml");
assert_pass!(test_for_loop, "for_loop.bml");
assert_pass!(test_const_aggregate_len, "const_aggregate_len.bml");
assert_error!(test_len_bad_arg, "len_bad_arg_error.bml", "E326");
assert_error!(test_len_redefine, "len_redefine_error.bml", "E345");
assert_error!(test_cast_to_b1, "cast_to_b1_error.bml", "E346");
assert_error!(test_shadowing, "shadowing_error.bml", "E347");
assert_error!(test_shadowing_global, "shadowing_global_error.bml", "E347");

// Inline named field enums: `field F bit[..] enum Name { .. }`. The OK fixture
// writes, reads, and matches an inline-enum field; the error fixtures cover the
// "both an explicit type and an inline enum" (E110) and "no type at all" (E111)
// cases.
assert_pass!(test_inline_field_enum_ok, "inline_field_enum_ok.bml");
assert_error!(
    test_inline_field_enum_both,
    "inline_field_enum_both_error.bml",
    "E110"
);
assert_error!(
    test_inline_field_enum_missing_type,
    "inline_field_enum_missing_type_error.bml",
    "E111"
);

// An inline field enum is pure parser desugar: it must lower to byte-identical
// IR to the hand-written top-level `enum` + `field F: Enum` equivalent.
#[test]
fn test_inline_field_enum_desugars_identically() {
    let inline = extract_fn_body(&bml_ir("inline_field_enum_ok.bml"), "@main");
    let explicit = extract_fn_body(&bml_ir("inline_field_enum_explicit.bml"), "@main");
    assert!(!inline.is_empty(), "inline `main` body was empty");
    assert_eq!(
        inline, explicit,
        "inline field enum must lower identically to a top-level enum + typed field"
    );
}

// peripheral_type (register-layout template) + instances. The OK fixtures cover
// plain templates and the inline-enum-in-template composition; the error
// fixtures cover an instance of an unknown type (E112) and a duplicate template
// name (E115).
assert_pass!(test_peripheral_type_ok, "peripheral_type_ok.bml");
assert_pass!(test_peripheral_type_enum_ok, "peripheral_type_enum_ok.bml");
// Cross-file: template in peripheral_type_ip.bml, instances here. Elaboration
// runs after the import merge, so this resolves.
assert_pass!(
    test_peripheral_type_crossfile,
    "peripheral_type_crossfile_ok.bml"
);
// Cross-file template whose inline enum is qualified by the template's module
// (variant referenced via the import alias) -- exercises the qualify interaction.
assert_pass!(
    test_peripheral_type_crossfile_enum,
    "peripheral_type_enum_crossfile_ok.bml"
);
// A template name collides with another global (a struct) -> E200, the same
// global uniqueness peripherals get (templates are stripped before the resolver).
assert_error!(
    test_peripheral_type_name_collision,
    "peripheral_type_name_collision_error.bml",
    "E200"
);

// peripheral_type as a function parameter (slice 2): one driver, many instances.
assert_pass!(
    test_peripheral_type_param_ok,
    "peripheral_type_param_ok.bml"
);
assert_pass!(
    test_peripheral_type_param_transitive,
    "peripheral_type_param_transitive_ok.bml"
);
// A handle argument must be a compile-time instance (E308); a handle cannot be
// used as a value (E309).
assert_error!(
    test_peripheral_type_param_badarg,
    "peripheral_type_param_badarg_error.bml",
    "E308"
);
assert_error!(
    test_peripheral_type_param_value,
    "peripheral_type_param_value_error.bml",
    "E309"
);
// A handle has no address: `&u` and taking the address of a handle driver are
// both rejected (E309) -- monomorphization leaves nothing to point at.
assert_error!(
    test_peripheral_type_param_addr,
    "peripheral_type_param_addr_error.bml",
    "E309"
);
assert_error!(
    test_peripheral_type_param_fnaddr,
    "peripheral_type_param_fnaddr_error.bml",
    "E309"
);

// A peripheral_type parameter is monomorphized: one specialized function per
// instance argument (writing that instance's base address), the generic
// function is not emitted, and calls go to the mangled names.
#[test]
fn test_peripheral_type_param_monomorphizes() {
    let ir = bml_ir("peripheral_type_param_ok.bml");
    for spec in ["@usart_init$USART1(", "@usart_init$USART2("] {
        assert!(
            ir.contains(&format!("define internal void {spec}")),
            "expected an internal-linkage specialization `{spec}`\n--- IR ---\n{ir}"
        );
    }
    assert!(
        !ir.contains("define void @usart_init("),
        "the generic driver must not be emitted\n--- IR ---\n{ir}"
    );
    // USART1 base 0x40011000, USART2 base 0x40004400 -- each specialization
    // touches its own peripheral.
    assert!(
        ir.contains("1073811456") || ir.contains("1073811464"),
        "USART1 base missing"
    );
    assert!(
        ir.contains("1073759232") || ir.contains("1073759240"),
        "USART2 base missing"
    );
}

// Register arrays (M6): `reg NAME[N] offset O stride S` reached as `P.NAME[i]`.
assert_pass!(test_reg_array_ok, "reg_array_ok.bml");
// Accessing the array register without an index (E337), a constant index past
// the length (E338), and a missing `stride` (E116) are all rejected.
assert_error!(test_reg_array_bare, "reg_array_bare_error.bml", "E337");
assert_error!(test_reg_array_oob, "reg_array_oob_error.bml", "E338");
assert_error!(
    test_reg_array_stride_missing,
    "reg_array_stride_missing_error.bml",
    "E116"
);
// `P.MEM[i]` lowers to a volatile MMIO store at `base + offset + stride*i`:
// FIFO0 base 0x40000000 + MEM offset 0x10 = 0x40000010 = 1073741840, stride 4.
#[test]
fn test_reg_array_lowering() {
    let ir = bml_ir("reg_array_ok.bml");
    assert!(
        ir.contains("mul i32") && ir.contains("1073741840"),
        "expected `base + stride*i` address math for the register array\n--- IR ---\n{ir}"
    );
    assert!(
        ir.contains("store volatile i32"),
        "register-array writes must stay on the volatile MMIO path\n--- IR ---\n{ir}"
    );
}

// Address-of an indexed register `&P.REG[i]` lowers to the MMIO pointer
// base+offset+stride*i (FIFO0 base 0x40000000 + TXF offset 0x10 = 1073741840).
assert_pass!(test_reg_array_addr_ok, "reg_array_addr_ok.bml");
assert_ir_contains!(
    test_reg_array_addr_lowering,
    "reg_array_addr_ok.bml",
    "1073741840"
);

// Regression (review #1/#2): a non-u32 register-array index and a sub-u32
// indexed-field read must lower to valid IR. These only fail at llc, so the
// fixture is BUILT (not just checked) -- a type-mismatched `mul i32`/`store i32`
// would make llc reject it and the build fail.
#[test]
fn test_reg_array_index_types_build() {
    let (ok, stderr) = bml_build_with_target(
        "reg_array_index_types_ok.bml",
        Some("reg_index_types.target"),
    );
    assert!(
        ok,
        "non-u32 index / sub-u32 indexed field must build; stderr:\n{stderr}"
    );
}

// Field access on an indexed array register `P.REG[i].FIELD` (the per-SM PIO
// register shape). The RMW happens at the runtime address with the field shift.
assert_pass!(test_reg_array_field_ok, "reg_array_field_ok.bml");
#[test]
fn test_reg_array_field_lowering() {
    let ir = bml_ir("reg_array_field_ok.bml");
    // CLK0 base 0x50200000 + DIV offset 0xC8 = 0x502000C8 = 1344274632, stride
    // 0x18 = 24; INT is bits[16..31] so the write shifts the value left by 16.
    assert!(
        ir.contains("mul i32") && ir.contains("1344274632") && ir.contains("shl i32"),
        "expected indexed-field RMW with the field shift\n--- IR ---\n{ir}"
    );
}

// Transitive monomorphization: `setup$USART1` calls `enable$USART1`.
#[test]
fn test_peripheral_type_param_transitive_ir() {
    let ir = bml_ir("peripheral_type_param_transitive_ok.bml");
    assert!(
        ir.contains("define internal void @setup$USART1(")
            && ir.contains("define internal void @enable$USART1("),
        "both specializations should be emitted (with internal linkage)\n--- IR ---\n{ir}"
    );
    assert!(
        ir.contains("call void @enable$USART1("),
        "setup$USART1 should call enable$USART1\n--- IR ---\n{ir}"
    );
}
assert_error!(
    test_peripheral_type_unknown,
    "peripheral_type_unknown_error.bml",
    "E112"
);
assert_error!(
    test_peripheral_type_dup,
    "peripheral_type_dup_error.bml",
    "E115"
);

// A peripheral_type instance is pure parser desugar: it must lower to byte-
// identical IR to two hand-written peripherals with the same register layout.
#[test]
fn test_peripheral_type_desugars_identically() {
    let templ = bml_ir("peripheral_type_ok.bml");
    let explicit = bml_ir("peripheral_type_explicit.bml");
    let templ_main = extract_fn_body(&templ, "@main");
    assert!(!templ_main.is_empty(), "template `main` body was empty");
    assert_eq!(
        templ_main,
        extract_fn_body(&explicit, "@main"),
        "peripheral_type instances must lower identically to hand-written peripherals"
    );
}
assert_error!(
    test_const_nonconst_init,
    "const_nonconst_init_error.bml",
    "E343"
);
// An initializer that names another `const` is inlined to that const's value:
// a static aggregate must not collapse to an invalid `[4 x i32] 0`, and a float
// const must inherit the referenced const's bits (3.5 here). Both checks share
// one compile (each fixture's `.ll` is written next to it, so two parallel
// `bml_ir` tests on the same fixture would race on that file).
#[test]
fn test_const_ref_init_inlined() {
    let ir = bml_ir("const_ref_init.bml");
    for pattern in [
        "@FROM_CONST = global [4 x i32] [i32 11, i32 22, i32 33, i32 44]",
        "@PI_ALIAS = constant float 0x400C000000000000",
    ] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}

// Const eval respects cast truncation (`300 as u8` -> 44) and resolves bool
// const aliases (`ALIAS` keeps `OK`'s value, not a silent 0). Compiling at all
// also proves `comptime_assert` accepts a boolean `const`. One compile (the
// fixture's `.ll` is written next to it, so parallel `bml_ir` tests would race).
#[test]
fn test_const_cast_and_bool() {
    let ir = bml_ir("const_cast_bool.bml");
    for pattern in [
        "@X = constant i8 44",
        "@Y8 = constant i8 44",
        "@ALIAS = constant i1 1",
    ] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}

// Readonly linear views
assert_pass!(test_view_read, "view_read.bml");
assert_pass!(test_view_helper, "view_helper.bml");
assert_error!(test_view_readonly_write, "view_readonly_write.bml", "E334");
assert_error!(test_view_bad_len, "view_bad_len.bml", "E332");
assert_error!(test_view_bad_ptr, "view_bad_ptr.bml", "E333");
assert_pass!(test_view_from_array, "view_from_array.bml");
assert_error!(test_view_from_nonarray, "view_from_nonarray.bml", "E333");
// View kinds can be built over a storage-wrapped array whose storage carries no
// ceiling protocol (@dma/@external/@exclusive): the storage class is unwrapped at
// construction and kept out of the view's type.
assert_pass!(test_view_over_dma, "view_over_dma.bml");
// But a view over @shared is rejected (E405): view access would bypass the
// ceiling critical-section that protects direct @shared access.
assert_error!(test_view_over_shared, "view_over_shared.bml", "E405");
// Same hazard, same code via raw address-of: `&`/`&mut` of a @shared static
// (or a field/element of one) outside a `claim` is E405 -- the aliased pointer
// would write past the ceiling mask. Inside a `claim` window it is allowed.
assert_error!(test_addr_of_shared, "addr_of_shared.bml", "E405");
assert_pass!(test_addr_of_shared_in_claim, "addr_of_shared_in_claim.bml");
// Mutable linear views (contiguous): write through index, coerce to readonly,
// and the move gate (a mutable view is Move, so reuse after a call is E304).
assert_pass!(test_view_mut_write, "view_mut_write.bml");
assert_pass!(test_view_mut_coerce, "view_mut_coerce.bml");
assert_error!(test_view_mut_move, "view_mut_move_error.bml", "E304");
// Indexing borrows the view, it does not move it: two index reads of a `view
// mut` are valid (so read/RMW loops type-check). But indexing a view that was
// moved away is still a use-after-move, on both the read and the write side.
assert_pass!(
    test_view_mut_index_read_twice,
    "view_mut_index_read_twice.bml"
);
assert_error!(
    test_view_mut_moved_index_read,
    "view_mut_moved_index_read_error.bml",
    "E304"
);
assert_error!(
    test_view_mut_moved_index_write,
    "view_mut_moved_index_write_error.bml",
    "E304"
);
// The place-chain read is non-consuming through addr-of and nested indices too:
// `&mut v[i]` borrows the view (no move), but addr-of an element of a moved view
// is still E304; and a two-level index place type-checks.
assert_pass!(test_addrof_index_no_consume, "addrof_index_no_consume.bml");
assert_error!(
    test_addrof_index_moved,
    "addrof_index_moved_error.bml",
    "E304"
);
assert_pass!(test_nested_index_read_twice, "nested_index_read_twice.bml");
// The index-borrow fix is shared across view kinds, not just `view mut`.
assert_pass!(
    test_ring_mut_index_read_twice,
    "ring_mut_index_read_twice.bml"
);
assert_pass!(
    test_bit_mut_index_read_twice,
    "bit_mut_index_read_twice.bml"
);
// Writing through a `view mut` *parameter* (immutable binding) is allowed, like
// a `*mut T` param. Also covers the `view(arr)` mutable array-form derivation.
assert_pass!(test_view_mut_param_write, "view_mut_param_write.bml");
// The reverse coercion (readonly view -> mutable view) is rejected.
assert_error!(
    test_view_readonly_to_mut,
    "view_readonly_to_mut_error.bml",
    "E300"
);
#[test]
fn test_view_mut_write_ir_lowering() {
    let ir = bml_ir("view_mut_write.bml");
    // The write path extracts { ptr, len }, asserts the index is in range via
    // the same branch-to-unreachable assume as reads, then stores.
    for pattern in ["extractvalue { ptr, i32 }", "icmp ult i32", "store i32"] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}
// Strided linear views (compile-time stride): backing index is `i * K`, K a
// type-level constant, descriptor unchanged from the contiguous view. Reads,
// mutable writes, mut->readonly coercion, plus the readonly-write / bad-stride /
// move / non-array rejections.
assert_pass!(test_view_strided_read, "view_strided_read.bml");
assert_pass!(test_view_strided_helper, "view_strided_helper.bml");
assert_pass!(test_view_strided_mut_write, "view_strided_mut_write.bml");
assert_pass!(test_view_strided_coerce, "view_strided_coerce.bml");
assert_error!(
    test_view_strided_readonly_write,
    "view_strided_readonly_write.bml",
    "E334"
);
assert_error!(
    test_view_strided_bad_stride,
    "view_strided_bad_stride.bml",
    "E332"
);
assert_error!(
    test_view_strided_large_stride,
    "view_strided_large_stride_error.bml",
    "E332"
);
assert_error!(
    test_view_strided_anno_stride_zero,
    "view_strided_anno_stride_zero_error.bml",
    "E332"
);
assert_error!(
    test_view_strided_anno_stride_large,
    "view_strided_anno_stride_large_error.bml",
    "E332"
);
assert_error!(
    test_view_strided_move,
    "view_strided_move_error.bml",
    "E304"
);
assert_error!(
    test_view_strided_nonarray,
    "view_strided_nonarray.bml",
    "E333"
);
#[test]
fn test_view_strided_ir_lowering() {
    let ir = bml_ir("view_strided_read.bml");
    // The strided index scales the logical index by the constant stride and
    // keeps the GEP typed (so the verifier bounds it), and the constructor
    // emits the logical length N/K = 8/2 = 4. The descriptor is the same
    // { ptr, i32 } as the contiguous view (no runtime stride field).
    for pattern in [
        "mul i32",
        "getelementptr i32",
        "add i32 0, 4",
        "extractvalue { ptr, i32 }",
    ] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}
// Ring views (contiguous): logical-to-physical indexing via (head+i) % capacity,
// readonly read + mutable write, readonly write rejected, and the move gate.
assert_pass!(test_ring_read, "ring_read.bml");
assert_pass!(test_ring_npot_read, "ring_npot_read.bml");
// A ring with a power-of-two capacity hint stays compatible with a hint-less
// `ring u32` parameter: the hint is not part of type identity.
assert_pass!(test_ring_cap_compat, "ring_cap_compat.bml");
assert_pass!(test_ring_mut_write, "ring_mut_write.bml");
assert_error!(
    test_ring_readonly_write,
    "ring_readonly_write_error.bml",
    "E334"
);
assert_error!(test_ring_mut_move, "ring_mut_move_error.bml", "E304");
// Writing through a `ring mut` parameter (immutable binding). Regression for the
// RingView case of the binding-mutability exemption.
assert_pass!(test_ring_mut_param_write, "ring_mut_param_write.bml");
// The runtime-capacity ring form ring(ptr, capacity, head, len) type-checks.
assert_pass!(test_ring_runtime, "ring_runtime.bml");
// `ring(...)` accepts only 3 or 4 arguments.
assert_error!(
    test_ring_bad_argcount,
    "ring_bad_argcount_error.bml",
    "E100"
);
#[test]
fn test_ring_debug_type() {
    let ir = bml_ir_debug("ring_debug.bml");
    // The ring descriptor emits a 4-field DWARF composite type so IKOS (and
    // debuggers) see { data, capacity, head, len } rather than an opaque blob.
    assert!(
        ir.contains(r#"name: "ring""#) && ir.contains(r#"name: "capacity""#),
        "expected ring DICompositeType with a capacity member\n--- IR ---\n{ir}\n---"
    );
}
#[test]
fn test_fn_ptr_debug_type() {
    let ir = bml_ir_debug("fn_ptr_debug.bml");
    // A fn-pointer lowers to `ptr`, so its DWARF type must be a pointer type
    // over a subroutine type -- an integer DIBasicType here makes the LLVM
    // verifier / IKOS reject the module. This fixture has no data pointers, so
    // the only DW_TAG_pointer_type present is the function pointer.
    assert!(
        ir.contains("tag: DW_TAG_pointer_type, baseType:") && ir.contains("DISubroutineType"),
        "expected fn-pointer DIDerivedType(DW_TAG_pointer_type) over a DISubroutineType\n--- IR ---\n{ir}\n---"
    );
}
#[test]
fn test_internal_linkage_specialization() {
    let ir = bml_ir("internal_linkage.bml");
    // Monomorphized specializations are `internal` (E309 => no address escape =>
    // globaldce can strip them once inlined away).
    assert!(
        ir.contains("define internal void @uart_putc$UART0")
            && ir.contains("define internal void @uart_putc$UART1"),
        "expected specializations to have internal linkage\n--- IR ---\n{ir}\n---"
    );
    // Entry points and ordinary functions keep external linkage.
    assert!(
        !ir.contains("define internal void @main")
            && !ir.contains("define internal void @reset_handler"),
        "main / reset_handler must NOT be internal\n--- IR ---\n{ir}\n---"
    );
}
#[test]
fn test_ring_ir_lowering() {
    let ir = bml_ir("ring_read.bml");
    // 4-field descriptor; capacity 8 is a power of two, so the physical index
    // lowers to the constant mask `& 7` rather than `urem`.
    for pattern in ["extractvalue { ptr, i32, i32, i32 }", "and i32"] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
    assert!(
        !ir.contains("urem"),
        "power-of-two ring should not emit urem\n--- IR ---\n{ir}\n-----------"
    );
}
#[test]
fn test_ring_npot_ir_lowering() {
    let ir = bml_ir("ring_npot_read.bml");
    // Capacity 6 is not a power of two, so the physical index keeps `urem`.
    for pattern in ["extractvalue { ptr, i32, i32, i32 }", "urem i32"] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}
// Bit views (contiguous): one bit per index over a byte array. Read extracts a
// bit, write is a read-modify-write of one byte. Readonly write rejected and the
// move gate, mirroring the linear/ring views.
assert_pass!(test_bit_read, "bit_read.bml");
assert_pass!(test_bit_mut_write, "bit_mut_write.bml");
assert_error!(
    test_bit_readonly_write,
    "bit_readonly_write_error.bml",
    "E334"
);
assert_error!(test_bit_mut_move, "bit_mut_move_error.bml", "E304");
// Writing through a `bits mut` parameter (immutable binding). Regression for the
// BitView case of the binding-mutability exemption.
assert_pass!(test_bit_mut_param_write, "bit_mut_param_write.bml");
// The runtime pointer form bits(ptr, bit_offset, len_bits) type-checks.
assert_pass!(test_bit_runtime, "bit_runtime.bml");
// `bits(...)` accepts only 1 or 3 arguments; bit_offset/len_bits must be ints.
assert_error!(test_bit_bad_argcount, "bit_bad_argcount_error.bml", "E100");
assert_error!(test_bit_non_int, "bit_non_int_error.bml", "E332");
// The backing must be a byte array / byte pointer (the byte-type restriction):
// a word array and a non-pointer base are both rejected (E333).
assert_error!(test_bit_nonbyte, "bit_nonbyte_error.bml", "E333");
assert_error!(test_bit_bad_base, "bit_bad_base_error.bml", "E333");
// A `[b8; N]` array is also a valid backing.
assert_pass!(test_bit_b8, "bit_b8.bml");
#[test]
fn test_bit_debug_type() {
    let ir = bml_ir_debug("bit_debug.bml");
    // The bit-view descriptor emits a 3-field DWARF composite so IKOS (and
    // debuggers) see { data, bit_offset, len_bits } rather than an opaque blob.
    assert!(
        ir.contains(r#"name: "bits""#) && ir.contains(r#"name: "bit_offset""#),
        "expected bits DICompositeType with a bit_offset member\n--- IR ---\n{ir}\n---"
    );
}
#[test]
fn test_bit_ir_lowering() {
    let ir = bml_ir("bit_read.bml");
    // 3-field descriptor and the byte/bit address math (byte = bit/8 via lshr 3,
    // bit-in-byte via and 7), ending in a single-bit extract.
    for pattern in ["extractvalue { ptr, i32, i32 }", "lshr i32", "trunc i8"] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}
#[test]
fn test_bit_write_ir_lowering() {
    let ir = bml_ir("bit_mut_write.bml");
    // The mutable write lowers to a read-modify-write of one byte: clear the
    // target bit (xor mask, and) then set it (zext value, shl, or, store).
    for pattern in ["xor i8", "zext i1", "or i8"] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}
// Both checks share one build to avoid racing on the fixture's `.ll` file
// (two `bml_ir` calls on the same fixture would write/read/delete it
// concurrently under the parallel test runner).
#[test]
fn test_view_ir_lowering() {
    let ir = bml_ir("view_read.bml");
    for pattern in ["extractvalue { ptr, i32 }", "icmp ult i32"] {
        assert!(
            ir.contains(pattern),
            "expected IR to contain `{pattern}`\n--- IR ---\n{ir}\n-----------"
        );
    }
}
assert_pass!(test_for_continue_advances, "for_continue_advances.bml");
assert_pass!(test_for_bounds_runtime, "for_bounds_runtime.bml");
assert_pass!(test_for_empty_range, "for_empty_range.bml");
assert_pass!(test_for_downto, "for_downto.bml");
assert_pass!(test_for_downto_unsigned, "for_downto_unsigned.bml");
assert_pass!(test_for_step_default, "for_step_default.bml");

#[test]
fn test_for_continue_branches_to_step() {
    let ir = bml_ir("for_continue_advances.bml");
    assert!(
        ir.contains("for_step"),
        "expected `for_step` block in emitted IR; got:\n{ir}"
    );
    assert!(
        ir.matches("for_step").count() >= 2,
        "expected both a `for_step:` label and a `br label %for_step` from `continue`; got:\n{ir}"
    );
}
assert_pass!(test_extern_fn, "extern_fn.bml");
assert_pass!(test_booleans, "booleans.bml");
assert_pass!(test_break_continue, "break_continue.bml");
assert_pass!(test_as_cast, "as_cast.bml");
assert_pass!(test_comparisons, "comparisons.bml");
assert_pass!(test_unary_ops, "unary_ops.bml");
assert_pass!(test_if_else, "if_else.bml");
assert_pass!(test_pointers, "pointers.bml");
assert_pass!(test_pointer_casts, "pointer_casts.bml");
assert_pass!(test_pointer_void, "pointer_void.bml");
assert_pass!(test_extern_ptr, "extern_ptr.bml");
assert_pass!(test_extern_abi_repr_c_ptr, "extern_abi_repr_c_ptr.bml");
// AAPCS narrow-int extension at the extern (C ABI) boundary: declares and
// calls carry zeroext/signext on sub-word ints; full-width stays bare.
// Return attribute precedes the type; parameter/argument attribute follows it.
assert_ir_contains!(
    test_extern_ext_ret,
    "extern_abi_ext.bml",
    "declare zeroext i8 @getchar()"
);
assert_ir_contains!(
    test_extern_ext_param_u8,
    "extern_abi_ext.bml",
    "declare void @putchar(i8 zeroext)"
);
assert_ir_contains!(
    test_extern_ext_param_i8,
    "extern_abi_ext.bml",
    "declare void @put_signed(i8 signext)"
);
assert_ir_contains!(
    test_extern_ext_param_u16,
    "extern_abi_ext.bml",
    "declare void @put_wide(i16 zeroext)"
);
// The load-bearing case: a dynamic narrow arg gets the caller-side attr.
assert_ir_contains!(
    test_extern_ext_call_arg,
    "extern_abi_ext.bml",
    "@putchar(i8 zeroext %"
);
assert_ir_contains!(
    test_extern_ext_call_ret,
    "extern_abi_ext.bml",
    "call zeroext i8 @getchar()"
);
// Full-width args never get an extension attribute.
assert_ir_not_contains!(
    test_extern_ext_word_bare,
    "extern_abi_ext.bml",
    "i32 zeroext"
);
// Uniform AAPCS extension: applied to internal signatures and calls, indirect
// calls, and enums lowering to a narrow integer -- not just the extern boundary.
// An internal define and its call site must agree.
assert_ir_contains!(
    test_narrow_internal_def,
    "narrow_abi_uniform.bml",
    "define zeroext i8 @scale(i8 zeroext %x)"
);
assert_ir_contains!(
    test_narrow_internal_call,
    "narrow_abi_uniform.bml",
    "call zeroext i8 @scale(i8 zeroext "
);
assert_ir_contains!(
    test_narrow_indirect_def,
    "narrow_abi_uniform.bml",
    "define zeroext i8 @apply(ptr %f, i8 zeroext %v)"
);
// An indirect call (callee is a register `%`, not `@`) carries the attr too.
assert_ir_contains!(
    test_narrow_indirect_call,
    "narrow_abi_uniform.bml",
    "call zeroext i8 %"
);
// An enum with a u8 repr lowers to i8 and must be extended (the hole a flat
// surface-type match would miss).
assert_ir_contains!(
    test_narrow_enum_extern,
    "narrow_abi_uniform.bml",
    "declare void @set_mode(i8 zeroext)"
);
assert_error!(test_extern_abi_b1, "extern_abi_b1_error.bml", "E356");
assert_error!(test_extern_abi_view, "extern_abi_view_error.bml", "E356");
assert_error!(
    test_extern_abi_struct_value,
    "extern_abi_struct_value_error.bml",
    "E356"
);
assert_error!(
    test_extern_abi_struct_ptr,
    "extern_abi_struct_ptr_error.bml",
    "E356"
);
assert_error!(
    test_extern_abi_packed_ptr,
    "extern_abi_packed_ptr_error.bml",
    "E356"
);
assert_error!(
    test_extern_abi_fn_ptr,
    "extern_abi_fn_ptr_error.bml",
    "E356"
);
assert_error!(
    test_extern_abi_array_value,
    "extern_abi_array_value_error.bml",
    "E356"
);
assert_error!(
    test_extern_abi_struct_field_view,
    "extern_abi_struct_field_view_error.bml",
    "E356"
);
assert_error!(
    test_extern_abi_struct_field_nested,
    "extern_abi_struct_field_nested_error.bml",
    "E356"
);
assert_error!(
    test_extern_abi_struct_field_forward_nested,
    "extern_abi_struct_field_forward_nested_error.bml",
    "E356"
);
assert_pass!(test_ptr_coercion, "ptr_coercion.bml");
assert_pass!(test_struct_basic, "struct_basic.bml");
assert_pass!(test_struct_ptr, "struct_ptr.bml");
assert_pass!(test_sizeof_basic, "sizeof_basic.bml");
assert_pass!(test_peripheral_reg_write, "peripheral_reg_write.bml");
assert_pass!(test_peripheral_reg_read, "peripheral_reg_read.bml");
assert_pass!(test_peripheral_field_read, "peripheral_field_read.bml");
assert_pass!(test_peripheral_field_write, "peripheral_field_write.bml");
assert_pass!(test_peripheral_field_range, "peripheral_field_range.bml");
assert_pass!(
    test_peripheral_field_explicit_ty,
    "peripheral_field_explicit_ty.bml"
);
assert_pass!(test_peripheral_field_32bit, "peripheral_field_32bit.bml");
assert_pass!(test_peripheral_field_access, "peripheral_field_access.bml");
assert_error!(
    test_peripheral_readonly_write,
    "peripheral_readonly_write_error.bml",
    "E331"
);
assert_error!(
    test_peripheral_writeonly_read,
    "peripheral_writeonly_read_error.bml",
    "E330"
);
assert_error!(
    test_peripheral_readonly_reg_write,
    "peripheral_readonly_reg_write_error.bml",
    "E331"
);
assert_error!(
    test_peripheral_writeonly_reg_read,
    "peripheral_writeonly_reg_read_error.bml",
    "E330"
);
assert_pass!(test_enum_basic, "enum_basic.bml");
assert_pass!(test_enum_u8, "enum_u8.bml");
assert_pass!(test_enum_autoincr, "enum_autoincr.bml");
assert_pass!(test_match_basic, "match_basic.bml");
assert_pass!(test_match_wildcard, "match_wildcard.bml");
assert_pass!(test_match_single, "match_single.bml");
assert_pass!(test_match_or_pattern, "match_or_pattern.bml");
assert_pass!(test_match_expr_basic, "match_expr_basic.bml");
assert_pass!(test_match_expr_wildcard, "match_expr_wildcard.bml");
assert_pass!(test_match_expr_infer, "match_expr_infer.bml");
assert_pass!(test_block_expr_basic, "block_expr_basic.bml");
assert_pass!(test_if_expr_basic, "if_expr_basic.bml");
assert_pass!(test_if_expr_elseif, "if_expr_elseif.bml");
assert_pass!(test_import_basic, "import_basic.bml");
assert_pass!(test_import_multi, "import_multi.bml");
assert_pass!(test_import_alias, "import_alias.bml");
assert_ir_contains!(
    test_import_alias_codegen,
    "import_alias.bml",
    "call i32 @L.hello()"
);
assert_ir_contains!(
    test_import_alias_internal_codegen,
    "import_alias_internal_codegen.bml",
    "call i32 @I.helper()"
);
assert_pass!(
    test_import_alias_struct_codegen_check,
    "import_alias_struct_codegen.bml"
);
assert_ir_contains!(
    test_import_alias_struct_codegen,
    "import_alias_struct_codegen.bml",
    "call { i32, i32 } @S.make_point()"
);
assert_ir_contains!(
    test_import_alias_symbol_collision,
    "import_alias_symbol_collision.bml",
    "define i32 @L__hello()"
);
assert_pass!(test_import_transitive, "import_transitive.bml");
// Regression: transitive call through a private dependency must reach IR
// (previously the symbol table missed `quux`, masked by a leniency rule in
// `types_compatible`, and IR emission panicked).
assert_ir_contains!(
    test_import_transitive_ir,
    "import_transitive.bml",
    "call i32 @lib_c.quux()"
);
assert_pass!(test_import_path, "import_path.bml");
assert_pass!(test_import_path_alias, "import_path_alias.bml");
assert_pass!(test_import_path_wildcard, "import_path_wildcard.bml");
assert_pass!(test_import_shared_dependency, "import_shared_root.bml");
assert_pass!(
    test_import_alias_collision_isolated,
    "import_alias_collision_isolated.bml"
);
assert_pass!(test_struct_cross, "struct_cross.bml");
assert_pass!(test_naked_fn_pass, "naked_fn.bml");
assert_pass!(test_naked_isr_pass, "naked_isr.bml");
assert_pass!(test_fn_section_pass, "fn_section.bml");
assert_pass!(test_static_section_pass, "static_section.bml");
assert_pass!(test_tailchain_leaf_pass, "tailchain_leaf.bml");
assert_pass!(test_tailchain_calls_pass, "tailchain_calls.bml");

assert_pass!(test_missing_context, "missing_context.bml");
assert_error!(test_exclusive_violation, "exclusive_violation.bml", "E401");
assert_error!(test_shared_ceiling, "shared_ceiling_violation.bml", "E402");
assert_error!(test_call_context, "call_context_error.bml", "E403");
assert_error!(
    test_borrow_array_init_context,
    "borrow_array_init_context_error.bml",
    "E403"
);
assert_error!(
    test_borrow_asm_input_context,
    "borrow_asm_input_context_error.bml",
    "E403"
);
assert_error!(test_thread_only, "thread_only_violation.bml", "E404");
assert_pass!(test_missing_float_suffix, "missing_float_suffix.bml");
assert_pass!(test_unsuffixed_literal_init, "unsuffixed_literal_init.bml");
assert_error!(
    test_extern_fn_context_err,
    "extern_fn_context_error.bml",
    "E403"
);
assert_error!(
    test_import_alias_context_error,
    "import_alias_context_error.bml",
    "E403"
);
assert_error!(test_val_immutability, "val_immutability_error.bml", "E309");
// Move tracking: reading a Move-typed local (here a @dma value) consumes it,
// so a later read is a use-after-move.
assert_error!(test_move_after_move, "move_after_move_error.bml", "E304");
// Reassigning the whole local revives it; the later read is valid.
assert_pass!(test_move_revive, "move_revive_ok.bml");
// Flow-sensitive: a move inside a loop body is a use-after-move next iteration.
assert_error!(test_move_in_loop, "move_in_loop_error.bml", "E304");
// Flow-sensitive: maybe-moved after an `if` arm is treated as moved.
assert_error!(test_move_in_branch, "move_in_branch_error.bml", "E304");
// Reassigning before use each iteration revives the local; no false positive.
assert_pass!(test_move_in_loop_revive, "move_in_loop_revive_ok.bml");
// Move unioned across match arms (the `if` form is covered above).
assert_error!(test_move_in_match, "move_in_match_error.bml", "E304");
// Taking the address of a Move-typed local borrows it without consuming.
assert_pass!(test_addrof_move_no_consume, "addrof_move_no_consume.bml");
// A move inside a nested loop leaks through the outer loop's fixpoint.
assert_error!(
    test_move_in_nested_loop,
    "move_in_nested_loop_error.bml",
    "E304"
);
assert_error!(test_type_mismatch, "type_mismatch_error.bml", "E310");
assert_error!(
    test_return_type_mismatch,
    "return_type_mismatch_error.bml",
    "E300"
);
assert_error!(
    test_return_value_without_type,
    "return_value_without_type_error.bml",
    "E300"
);
assert_error!(test_return_missing, "return_missing_error.bml", "E329");
assert_error!(
    test_return_loop_break_before_return,
    "return_loop_break_before_return_error.bml",
    "E329"
);
assert_error!(test_call_args, "call_args_error.bml", "E307");
assert_error!(test_duplicate_name, "duplicate_name_error.bml", "E200");
assert_error!(test_ptr_write_const, "ptr_write_const_error.bml", "E314");
assert_error!(test_ptr_deref, "ptr_deref_error.bml", "E315");
assert_error!(test_bool_conditions, "bool_conditions_error.bml", "E302");
assert_error!(test_undefined_name, "undefined_name_error.bml", "E305");
assert_error!(
    test_var_type_mismatch,
    "var_type_mismatch_error.bml",
    "E300"
);
assert_error!(
    test_for_range_mismatch,
    "for_range_mismatch_error.bml",
    "E312"
);
assert_error!(
    test_for_bound_type_mismatch,
    "for_bound_type_mismatch_error.bml",
    "E312"
);
assert_error!(
    test_for_literal_out_of_range,
    "for_literal_out_of_range_error.bml",
    "E312"
);
assert_error!(test_for_var_non_int, "for_var_non_int_error.bml", "E312");
assert_error!(test_for_step_zero, "for_step_zero_error.bml", "E312");
assert_error!(test_array_mismatch, "array_mismatch_error.bml", "E313");
assert_error!(
    test_exclusive_unknown,
    "exclusive_unknown_error.bml",
    "E201"
);
assert_error!(test_ptr_mut_val, "ptr_mut_val_error.bml", "E309");
assert_error!(
    test_ptr_mut_val_index,
    "ptr_mut_val_index_error.bml",
    "E309"
);
assert_error!(
    test_ptr_mut_const_deref,
    "ptr_mut_const_deref_error.bml",
    "E314"
);
assert_error!(
    test_ptr_mut_const_index,
    "ptr_mut_const_index_error.bml",
    "E314"
);
assert_error!(
    test_ptr_mut_readonly_view_index,
    "ptr_mut_readonly_view_index_error.bml",
    "E334"
);
assert_error!(test_ptr_void_deref, "ptr_void_deref_error.bml", "E315");
assert_error!(
    test_struct_field_not_found,
    "struct_field_not_found.bml",
    "E318"
);
assert_error!(
    test_struct_duplicate_field,
    "struct_duplicate_field.bml",
    "E319"
);
assert_error!(
    test_struct_missing_field,
    "struct_missing_field.bml",
    "E320"
);
assert_error!(
    test_struct_duplicate_init_field,
    "struct_duplicate_init_field.bml",
    "E321"
);
assert_pass!(test_struct_layout_explicit, "struct_layout_explicit.bml");
assert_ir_contains!(
    test_struct_layout_explicit_ir,
    "struct_layout_explicit.bml",
    "{ i8, [1 x i8], i16 }"
);
assert_pass!(test_struct_repr_c, "struct_repr_c.bml");
assert_ir_contains!(test_struct_repr_c_ir, "struct_repr_c.bml", "{ i8, i32 }");
assert_pass!(test_struct_repr_packed, "struct_repr_packed.bml");
#[test]
fn test_struct_repr_packed_ir() {
    let ir = bml_ir("struct_repr_packed.bml");
    assert!(
        ir.contains("<{ i8, i32 }>"),
        "expected packed struct type\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("align 1"),
        "expected packed field access to use align 1\n--- IR ---\n{ir}\n-----------"
    );
}
assert_error!(
    test_struct_repr_packed_addr,
    "struct_repr_packed_addr_error.bml",
    "E357"
);
// Per-field endianness: `@be` fields round-trip through `llvm.bswap` on both the
// store (write) and load (read) paths; `@le`/native fields do not.
assert_pass!(test_struct_field_endian, "struct_field_endian.bml");
#[test]
fn test_struct_field_endian_ir() {
    let ir = bml_ir("struct_field_endian.bml");
    assert!(
        ir.contains("@llvm.bswap.i16("),
        "expected a u16 @be field swap\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("@llvm.bswap.i32("),
        "expected a u32 @be field swap\n--- IR ---\n{ir}\n-----------"
    );
    // The `@le` field (a plain i32 store with no swap) must still be written:
    // the store-through-a-pointer path was previously dropped entirely.
    let fill = extract_fn_body(&ir, "fill");
    let swaps = fill.matches("@llvm.bswap").count();
    assert_eq!(
        swaps, 2,
        "expected exactly two swaps in fill (u16 @be + u32 @be), not the @le \
         or u8 field\n--- fill ---\n{fill}\n-----------"
    );
}
assert_error!(
    test_struct_field_endian_non_int,
    "struct_field_endian_non_int_error.bml",
    "E359"
);
assert_error!(
    test_struct_field_endian_addr,
    "struct_field_endian_addr_error.bml",
    "E360"
);
// E360 is target-dependent: `@le` is native on the little-endian target, so its
// address is allowed (only the non-native `@be` order is rejected above).
assert_pass!(
    test_struct_field_endian_le_addr,
    "struct_field_endian_le_addr.bml"
);
assert_error!(
    test_sizeof_array_overflow,
    "sizeof_array_overflow_error.bml",
    "E358"
);
assert_error!(
    test_struct_padding_bad_type,
    "struct_padding_bad_type_error.bml",
    "E351"
);
assert_error!(
    test_struct_layout_misaligned,
    "struct_layout_misaligned_error.bml",
    "E352"
);
assert_error!(
    test_struct_layout_tail,
    "struct_layout_tail_error.bml",
    "E353"
);
assert_error!(
    test_struct_padding_init,
    "struct_padding_init_error.bml",
    "E354"
);
assert_error!(
    test_struct_padding_access,
    "struct_padding_access_error.bml",
    "E355"
);
assert_error!(
    test_sizeof_undefined_type,
    "sizeof_undefined_type_error.bml",
    "E305"
);
assert_error!(
    test_peripheral_field_undefined,
    "peripheral_field_error.bml",
    "E322"
);
assert_error!(
    test_peripheral_reg_undefined,
    "peripheral_reg_error.bml",
    "E322"
);
assert_error!(
    test_peripheral_field_read_undefined,
    "peripheral_field_read_error.bml",
    "E322"
);
assert_error!(
    test_enum_duplicate_variant,
    "enum_duplicate_variant.bml",
    "E319"
);
assert_error!(test_enum_dup_name, "enum_dup_name.bml", "E200");
assert_error!(
    test_enum_variant_not_found,
    "enum_variant_not_found.bml",
    "E322"
);
assert_error!(test_enum_out_of_range, "enum_out_of_range.bml", "E323");
assert_error!(test_enum_invalid_type, "enum_invalid_type.bml", "E323");
assert_error!(test_match_not_enum, "match_not_enum_error.bml", "E324");
assert_error!(
    test_match_non_exhaustive,
    "match_non_exhaustive_error.bml",
    "E325"
);
assert_error!(
    test_match_duplicate_arm,
    "match_duplicate_arm_error.bml",
    "E319"
);
assert_error!(
    test_match_duplicate_wildcard,
    "match_duplicate_wildcard_error.bml",
    "E319"
);
assert_error!(
    test_match_variant_not_found,
    "match_variant_not_found_error.bml",
    "E322"
);
assert_error!(
    test_match_wildcard_combined,
    "match_wildcard_combined_error.bml",
    "E326"
);
assert_error!(
    test_match_expr_type_mismatch,
    "match_expr_type_mismatch.bml",
    "E327"
);
assert_error!(test_if_expr_no_trailing, "if_expr_no_trailing.bml", "E328");
assert_error!(
    test_if_expr_type_mismatch,
    "if_expr_type_mismatch.bml",
    "E327"
);
assert_error!(test_block_expr_no_value, "block_expr_no_value.bml", "E328");
assert_error!(test_mod_not_found, "mod_not_found.bml", "E501");
assert_error!(
    test_selective_import_removed,
    "selective_import_removed.bml",
    "E109"
);
assert_error!(test_export_private, "export_private_error.bml", "E503");
assert_error!(test_circular_import, "circular_a.bml", "E500");

#[test]
fn test_circular_import_does_not_poison_later_imports() {
    let (ok, output) = bml_check("cycle_then_ok_root.bml");
    assert!(!ok, "expected circular import error, got success");
    let count = output.matches("error[E500]").count();
    assert_eq!(
        count, 1,
        "expected exactly one circular import error:\n{output}"
    );
}

assert_pass!(test_rename_collision, "rename_collision.bml");
assert_error!(
    test_import_alias_no_unqualified_access,
    "import_alias_no_unqualified_access.bml",
    "E305"
);
assert_error!(
    test_import_alias_no_unqualified_call,
    "import_alias_no_unqualified_call.bml",
    "E305"
);

assert_error!(
    test_unsuffixed_literal_out_of_range,
    "unsuffixed_literal_out_of_range.bml",
    "E300"
);
assert_error!(
    test_unsuffixed_float_out_of_range,
    "unsuffixed_float_out_of_range.bml",
    "E300"
);

// Lexer error tests
assert_error!(
    test_lexer_unterminated_comment,
    "lexer_unterminated_comment.bml",
    "E001"
);
assert_error!(
    test_lexer_invalid_literal,
    "lexer_invalid_literal.bml",
    "E002"
);
assert_error!(
    test_lexer_unknown_escape,
    "lexer_unknown_escape.bml",
    "E003"
);
assert_error!(
    test_lexer_unterminated_string,
    "lexer_unterminated_string.bml",
    "E004"
);
assert_error!(
    test_lexer_unexpected_char,
    "lexer_unexpected_char.bml",
    "E005"
);
assert_error!(
    test_lexer_unterminated_asm,
    "lexer_unterminated_asm.bml",
    "E006"
);

assert_warn!(test_int_narrowing, "int_narrowing_warn.bml", "W301");

// ─── parser diagnostics (added to close diagnostic-coverage gaps) ──────────
assert_error!(
    test_parser_expected_item,
    "parser_expected_item_error.bml",
    "E101"
);
assert_error!(
    test_parser_match_no_at,
    "parser_match_no_at_error.bml",
    "E100"
);
assert_error!(
    test_parser_bad_fn_annotation,
    "parser_bad_fn_annotation_error.bml",
    "E103"
);
assert_error!(
    test_parser_bad_storage_annotation,
    "parser_bad_storage_annotation_error.bml",
    "E104"
);
assert_error!(
    test_parser_field_no_bit,
    "parser_field_no_bit_error.bml",
    "E105"
);
assert_error!(
    test_parser_tailchain_not_bool,
    "parser_tailchain_not_bool_error.bml",
    "E106"
);
assert_error!(
    test_parser_expected_integer,
    "parser_expected_integer_error.bml",
    "E107"
);
assert_error!(
    test_parser_dup_context,
    "parser_dup_context_error.bml",
    "E108"
);
assert_error!(
    test_parser_priority_range,
    "parser_priority_range_error.bml",
    "E103"
);
assert_error!(
    test_parser_shared_ceiling_range,
    "parser_shared_ceiling_range_error.bml",
    "E104"
);
// Deeply nested input must be rejected with a diagnostic, never a stack
// overflow: the recursive-descent parser bounds its depth (MAX_PARSE_DEPTH).
assert_error!(
    test_parser_nesting_too_deep,
    "parser_nesting_too_deep_error.bml",
    "E113"
);

// comptime_assert: a const-true condition compiles; false is E342; a
// non-compile-time-constant condition is E343.
assert_pass!(test_comptime_assert_pass, "comptime_assert_pass.bml");
assert_error!(
    test_comptime_assert_false,
    "comptime_assert_false_error.bml",
    "E342"
);
assert_error!(
    test_comptime_assert_nonconst,
    "comptime_assert_nonconst_error.bml",
    "E343"
);

// Compound assignment reuses the assignment/operator checks: assigning to a
// `val` is E309, and a type mismatch in the implied `x = x OP y` is E310.
assert_error!(
    test_compound_assign_val,
    "compound_assign_val_error.bml",
    "E309"
);
assert_error!(
    test_compound_assign_type,
    "compound_assign_type_error.bml",
    "E310"
);
// `@align(N)` requires a power-of-two N.
assert_error!(test_align_bad_value, "align_bad_value_error.bml", "E104");
assert_error!(test_align_too_large, "align_too_large_error.bml", "E104");
// An asm output operand must be an assignable place.
assert_error!(
    test_asm_output_nonplace,
    "asm_output_nonplace_error.bml",
    "E314"
);
// Integer match: must have a `_` arm (E325); enum-variant pattern is rejected
// on an integer scrutinee (E324).
assert_error!(
    test_match_int_no_wildcard,
    "match_int_no_wildcard_error.bml",
    "E325"
);
assert_error!(test_match_int_kind, "match_int_kind_error.bml", "E324");
// A pattern value outside the scrutinee type's range is E344; the same value in
// two arms (an unreachable second arm) is E319.
assert_error!(
    test_match_pattern_range,
    "match_pattern_range_error.bml",
    "E344"
);
assert_error!(
    test_match_duplicate_value,
    "match_duplicate_value_error.bml",
    "E319"
);

// ─── checker diagnostics (added to close diagnostic-coverage gaps) ─────────
assert_error!(
    test_assign_type_mismatch,
    "assign_type_mismatch_error.bml",
    "E301"
);
assert_error!(
    test_while_cond_not_bool,
    "while_cond_not_bool_error.bml",
    "E303"
);
assert_error!(
    test_logical_not_not_bool,
    "logical_not_not_bool_error.bml",
    "E306"
);
assert_error!(
    test_call_arg_type_mismatch,
    "call_arg_type_mismatch_error.bml",
    "E308"
);
assert_error!(
    test_comparison_type_mismatch,
    "comparison_type_mismatch_error.bml",
    "E311"
);
assert_error!(test_bitwise_non_int, "bitwise_non_int_error.bml", "E317");

// Wrapping arithmetic (`+%`/`-%`/`*%`): integer-only (E336 on floats and
// pointers -- wrap on an address is never intent), and lowers to the same
// add/sub/mul opcodes as the plain operators. (Builds run in a unique
// `--out-dir`, so tests may share a fixture without racing on artifacts.)
assert_error!(test_wrap_float, "wrap_float_error.bml", "E336");
assert_error!(test_wrap_ptr, "wrap_ptr_error.bml", "E336");

#[test]
fn test_wrap_ops_lowering() {
    let ir = bml_ir("wrap_ops.bml");
    // `a +% 1` / `-% 1` / `*% 2` and the `+%=` compound all lower to plain
    // wrapping LLVM arithmetic (no nsw/nuw), exactly like `+`/`-`/`*`.
    assert!(ir.contains("add i32"), "expected add i32 in IR:\n{ir}");
    assert!(ir.contains("sub i32"), "expected sub i32 in IR:\n{ir}");
    assert!(ir.contains("mul i32"), "expected mul i32 in IR:\n{ir}");
    assert!(
        !ir.contains("nsw") && !ir.contains("nuw"),
        "wrap ops must not carry overflow flags:\n{ir}"
    );
}

// Critical section codegen tests. On v7-M (the default test target,
// priority_bits=4) a real ISR ceiling lowers to BASEPRI_MAX = ceiling << 4
// with save/restore -- ISRs above the ceiling keep running. cpsid is the
// fallback (v6-M, ceiling 0, thread-level sentinel 255).
// (Each build uses a unique `--out-dir`, so multiple tests may share a
// fixture without racing on artifacts.)
#[test]
fn test_shared_cs_thread_basepri() {
    let ir = bml_ir("shared_cs_thread.bml");
    assert!(
        ir.contains("asm sideeffect \"msr basepri_max, $0\", \"r,~{memory}\"(i32 32)"),
        "expected BASEPRI_MAX mask to ceiling 2 (hw 0x20):\n{ir}"
    );
    assert!(
        ir.contains("asm sideeffect \"msr basepri, $0\", \"r,~{memory}\"(i32 %"),
        "expected saved-BASEPRI restore:\n{ir}"
    );
    assert!(
        !ir.contains("asm sideeffect \"cpsid i\""),
        "global mask must not be used when BASEPRI applies:\n{ir}"
    );
}
assert_ir_contains!(
    test_shared_cs_isr_low,
    "shared_cs_isr_low.bml",
    "asm sideeffect \"msr basepri_max, $0\", \"r,~{memory}\"(i32 32)"
);
#[test]
fn test_shared_cs_isr_same_no_cs() {
    let ir = bml_ir("shared_cs_isr_same.bml");
    assert!(
        !ir.contains("asm sideeffect \"cpsid i\"") && !ir.contains("msr basepri_max"),
        "ceiling-priority accessor must take no critical section at all:\n{ir}"
    );
}
// Ceiling 0 encodes to BASEPRI=0 = masking disabled: global mask fallback.
#[test]
fn test_shared_cs_ceiling0_cpsid_fallback() {
    let ir = bml_ir("shared_cs_ceiling0.bml");
    assert!(
        ir.contains("asm sideeffect \"cpsid i\"") && !ir.contains("msr basepri_max"),
        "ceiling 0 must fall back to the global mask:\n{ir}"
    );
}
// ARMv6-M has no BASEPRI: a real ISR ceiling still lowers to cpsid there.
assert_ir_contains_target!(
    test_shared_cs_v6m_cpsid,
    "shared_cs_v6m.bml",
    "isr_v6m.target",
    "asm sideeffect \"cpsid i\""
);

// @isr priority grounding: reset_handler writes the NVIC IPR byte from the
// annotation (priority << (8 - priority_bits)); IRQ 0 -> 0xE000E400 = 3758154752.
assert_ir_contains!(
    test_isr_priority_programmed,
    "isr_priority_program.bml",
    "store volatile i8 48, ptr inttoptr (i32 3758154752 to ptr)"
);

assert_ir_contains_target!(
    test_isr_priority_v6m_word_composed,
    "isr_priority_v6m.bml",
    "isr_v6m.target",
    "store volatile i32 32832, ptr inttoptr (i32 3758154752 to ptr)"
);

// System-exception priority goes to the SCB SHPR, not the NVIC IPR.
// @isr("SysTick", priority=1) -> SHPR3 SysTick byte at 0xE000ED23 = 3758157091,
// value 1 << (8-4) = 16. (vector_system.target also lists SysTick in
// [interrupts]; that must NOT additionally program a peripheral IPR.)
assert_ir_contains_target!(
    test_shpr_systick_programmed,
    "vector_system.bml",
    "vector_system.target",
    "store volatile i8 16, ptr inttoptr (i32 3758157091 to ptr)"
);

// A system-exception label listed in [interrupts] is not an NVIC line: it is
// skipped in the IRQ loop, so no peripheral IPR is programmed for it. IRQ 15's
// IPR byte would be 0xE000E400 + 15 = 3758154767.
#[test]
fn test_shpr_systick_no_nvic_ipr() {
    let ir = bml_ir_with_target("vector_system.bml", Some("vector_system.target"));
    assert!(
        !ir.contains("i32 3758154767 to ptr"),
        "system exception SysTick must not program a peripheral IPR\n--- IR ---\n{ir}\n---"
    );
}

// ARMv6-M SHPR is word-access-only: the lanes are composed and the word stored.
// @isr("PendSV", priority=2) -> word at 0xE000ED20 = 3758157088, 128 << 16.
assert_ir_contains_target!(
    test_shpr_v6m_word_composed,
    "shpr_v6m.bml",
    "isr_v6m.target",
    "store volatile i32 8388608, ptr inttoptr (i32 3758157088 to ptr)"
);

// Fail-loudly hardening of @isr/vector-table misconfiguration (validate_interrupts).
// E406: a priority that doesn't fit priority_bits would be truncated.
#[test]
fn test_isr_priority_overflow_rejected() {
    let (ok, stderr) =
        bml_build_with_target("isr_priority_overflow.bml", Some("vector_labeled.target"));
    assert!(!ok, "priority exceeding priority_bits must be rejected\n{stderr}");
    assert!(stderr.contains("error[E406]"), "expected E406:\n{stderr}");
}
// E407: two @isr handlers on the same label -- one would be silently dropped.
#[test]
fn test_isr_duplicate_label_rejected() {
    let (ok, stderr) = bml_build_with_target("isr_dup_label.bml", Some("vector_labeled.target"));
    assert!(!ok, "duplicate @isr label must be rejected\n{stderr}");
    assert!(stderr.contains("error[E407]"), "expected E407:\n{stderr}");
}
// E409: a labeled @isr matching no system exception or [interrupts] entry --
// the handler would never be wired into the table.
#[test]
fn test_isr_unmatched_label_rejected() {
    let (ok, stderr) = bml_build_with_target("isr_unmatched_label.bml", Some("vector_labeled.target"));
    assert!(!ok, "unmatched @isr label must be rejected\n{stderr}");
    assert!(stderr.contains("error[E409]"), "expected E409:\n{stderr}");
}
// E409 gate: without a [interrupts] section the table mechanism is not in use,
// so a labeled @isr is not enforced (codegen-only fixtures keep building).
#[test]
fn test_isr_unmatched_label_no_interrupts_ok() {
    let (ok, stderr) = bml_build_with_target("isr_unmatched_label.bml", None);
    assert!(ok, "without [interrupts], an unmatched label must not error\n{stderr}");
}

// Derived ceilings (bare `@shared`, ceiling.rs): the number comes from the
// accessor contexts, and the lowering matches the hand-declared equivalent.
#[test]
fn test_shared_derived_isr_top() {
    let ir = bml_ir("shared_derived_isr_top.bml");
    assert!(
        !ir.contains("asm sideeffect \"cpsid i\"") && !ir.contains("msr basepri_max"),
        "derived-ceiling top accessor must take no critical section:\n{ir}"
    );
}
assert_ir_contains!(
    test_shared_derived_low_isr_cs,
    "shared_derived_low_isr_cs.bml",
    "asm sideeffect \"msr basepri_max, $0\", \"r,~{memory}\"(i32 16)"
);
// Thread-only accessor set: the derived ceiling is the 255 thread-level
// sentinel, not a real ISR priority -- BASEPRI=0xF0 would mask almost
// nothing, so the conservative global mask stays.
assert_ir_contains!(
    test_shared_derived_thread,
    "shared_derived_thread.bml",
    "asm sideeffect \"cpsid i\""
);

// Inline assembly tests
assert_ir_contains!(
    test_asm_nop,
    "asm_nop.bml",
    "call void asm sideeffect \"nop\""
);
assert_ir_contains!(test_asm_cpsid, "asm_cpsid.bml", "cpsid i\\0A");

// Function pointer tests
assert_pass!(test_fn_ptr_basic, "fn_ptr_basic.bml");
assert_pass!(test_fn_ptr_param, "fn_ptr_param.bml");
assert_pass!(test_fn_ptr_extern_c, "fn_ptr_extern_c.bml");
assert_pass!(test_fn_ptr_struct, "fn_ptr_struct.bml");
assert_error!(
    test_fn_ptr_error_context,
    "fn_ptr_error_context.bml",
    "E408"
);
assert_error!(
    test_fn_ptr_bare_error_context,
    "fn_ptr_bare_error_context.bml",
    "E408"
);
// `&context_fn` must report E408 exactly once: reading the `&` operand and the
// AddrOf type computation both used to emit it.
#[test]
fn test_fn_ptr_error_context_single_diagnostic() {
    let (ok, output) = bml_check("fn_ptr_error_context.bml");
    assert!(!ok, "expected error, got success");
    let count = output.matches("error[E408]").count();
    assert_eq!(count, 1, "expected exactly one E408:\n{output}");
}
// The bare form (`fp = context_fn`, no `&`) reaches the same function-symbol
// branch and must also report E408 exactly once.
#[test]
fn test_fn_ptr_bare_error_context_single_diagnostic() {
    let (ok, output) = bml_check("fn_ptr_bare_error_context.bml");
    assert!(!ok, "expected error, got success");
    let count = output.matches("error[E408]").count();
    assert_eq!(count, 1, "expected exactly one E408:\n{output}");
}
// Dedup is per-site, not per-function: two distinct `&context_fn` sites must
// report E408 exactly twice (the pre-fix bug emitted four).
#[test]
fn test_fn_ptr_error_context_multi_site() {
    let (ok, output) = bml_check("fn_ptr_error_context_multi.bml");
    assert!(!ok, "expected error, got success");
    let count = output.matches("error[E408]").count();
    assert_eq!(
        count, 2,
        "expected exactly two E408 (one per site):\n{output}"
    );
}
// `&context_fn` in call-argument position must also report E408 exactly once,
// independent of the surrounding syntax.
#[test]
fn test_fn_ptr_arg_error_context_single_diagnostic() {
    let (ok, output) = bml_check("fn_ptr_arg_error_context.bml");
    assert!(!ok, "expected error, got success");
    let count = output.matches("error[E408]").count();
    assert_eq!(count, 1, "expected exactly one E408:\n{output}");
}
// Address-of an `@isr` function is rejected the same as `@context(thread)`:
// only unrestricted (any-context) functions can become function pointers.
assert_error!(test_fn_ptr_error_isr, "fn_ptr_error_isr.bml", "E408");
assert_error!(test_fn_ptr_error_type, "fn_ptr_error_type.bml", "E300");

// Vector table tests
assert_ir_contains_target!(
    test_vector_system,
    "vector_system.bml",
    "vector_system.target",
    "ptr @SysTick_Handler,"
);
assert_ir_contains!(
    test_vector_reserved_null,
    "vector_reserved.bml",
    "ptr null,"
);
assert_ir_contains!(
    test_vector_unlabeled,
    "vector_unlabeled.bml",
    "ptr @first_isr,\n  ptr @second_isr"
);

// Single build for vector_default_handler to avoid race between two IR readers
#[test]
fn test_vector_default_handler() {
    let ir = bml_ir("vector_default_handler.bml");
    assert!(
        ir.contains("ptr @Default_Handler,"),
        "expected IR to contain Default_Handler\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        !ir.contains("@default_handler"),
        "expected IR to NOT contain @default_handler\n--- IR ---\n{ir}\n-----------"
    );
}

// Startup routine tests
#[test]
fn test_startup_basic() {
    // Auto-generated reset_handler: copies .data, zeroes .bss, calls main.
    let ir = bml_ir_with_target("startup_basic.bml", Some("stm32f401.target"));
    assert!(
        ir.contains("ptr @reset_handler,\n  ptr"),
        "expected vector table entry\n--- IR ---\n{ir}\n-----------"
    );
    check_snapshot(
        "startup_basic_reset",
        &extract_fn_body(&ir, "@reset_handler"),
    );
}

#[test]
fn test_startup_user_reset() {
    // A user-defined reset_handler is used directly: no auto-generated .data/.bss
    // startup code, which the snapshot of its body confirms.
    let ir = bml_ir_with_target("startup_user_reset.bml", Some("stm32f401.target"));
    assert!(
        ir.contains("ptr @reset_handler,\n  ptr"),
        "expected vector table entry\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        !ir.contains("@_sidata = external"),
        "expected NO auto-generated startup symbols\n--- IR ---\n{ir}\n-----------"
    );
    check_snapshot(
        "startup_user_reset",
        &extract_fn_body(&ir, "@reset_handler"),
    );
}

#[test]
fn test_startup_init_writes() {
    // A [startup] entry becomes a read-modify-write OR at the very top of
    // reset_handler, before the .data/.bss copy (CMSIS SystemInit ordering).
    // 0x5802453C = 1476543804, 0x60000000 = 1610612736.
    // Uses its own source fixture (not startup_basic.bml) so the .ll output path
    // does not race with test_startup_basic.
    let ir = bml_ir_with_target("startup_init.bml", Some("startup_init.target"));
    let body = extract_fn_body(&ir, "@reset_handler");
    assert!(
        body.contains("load volatile i32, ptr inttoptr (i32 1476543804 to ptr)")
            && body.contains("or i32")
            && body.contains("1610612736")
            && body.contains("store volatile i32"),
        "expected startup RMW-OR write\n--- reset_handler ---\n{body}"
    );
    let store = body.find("store volatile i32").expect("startup store");
    let data_copy = body.find("data_copy_test").expect("data copy block");
    assert!(
        store < data_copy,
        "startup write must precede the .data copy\n--- reset_handler ---\n{body}"
    );
}

// Array tests
#[test]
fn test_array_init() {
    let ir = bml_ir("array_init_basic.bml");
    assert!(
        ir.contains("alloca [4 x i32]"),
        "expected array alloca\n--- IR ---\n{ir}\n-----------"
    );
    // elements are initialized by indexed GEPs directly into the array alloca
    assert!(
        ir.contains("getelementptr [4 x i32], ptr"),
        "expected typed GEP into array\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("store i32"),
        "expected element stores\n--- IR ---\n{ir}\n-----------"
    );
}

#[test]
fn test_nested_expr_allocas_are_in_entry_block() {
    let body = extract_fn_body(&bml_ir("entry_allocas_nested_exprs.bml"), "@main");
    for var_name in [
        "view_len_tmp",
        "ring_cap_tmp",
        "ring_head_tmp",
        "ring_len_tmp",
        "bit_off_tmp",
        "bit_len_tmp",
        "if_cond_tmp",
        "while_cond_tmp",
        "for_start_tmp",
        "for_end_tmp",
        "for_step_tmp",
        "match_scrut_tmp",
        "asm_out_idx_tmp",
        "asm_in_tmp",
    ] {
        assert_alloca_before_first_label(&body, var_name);
    }
}

#[test]
fn test_nested_expr_phi_incoming_labels() {
    let body = extract_fn_body(&bml_ir("phi_incoming_nested_exprs.bml"), "@main");
    let phi_lines: Vec<_> = body
        .lines()
        .filter(|line| line.contains(" = phi i32 "))
        .collect();

    assert!(
        phi_lines
            .iter()
            .any(|line| line.contains("%view_idx_ok.") && line.contains("%if_else.")),
        "expected if-expression phi to use the view read's final label\n--- IR ---\n{body}\n-----------"
    );
    assert!(
        phi_lines
            .iter()
            .any(|line| line.contains("%view_idx_ok.") && line.contains("%match_arm.")),
        "expected match-expression phi to use the view read's final label\n--- IR ---\n{body}\n-----------"
    );
}

#[test]
fn test_array_read() {
    let ir = bml_ir("array_read_basic.bml");
    assert!(
        ir.contains("getelementptr i32, ptr"),
        "expected GEP for array read\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("load i32, ptr"),
        "expected element load\n--- IR ---\n{ir}\n-----------"
    );
}

#[test]
fn test_array_write() {
    let ir = bml_ir("array_write_basic.bml");
    assert!(
        ir.contains("getelementptr i32, ptr"),
        "expected GEP for array write\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("add i32 0, 42"),
        "expected the written value to be materialized\n--- IR ---\n{ir}\n-----------"
    );
}

// Bit-band field read: single-bit fields use alias load instead of RMW
// Single-bit bit-band fields read through their alias address (e.g. ODR3 ->
// 0x4200028C); multi-bit fields do not. The snapshot pins the alias addresses
// and the absence of masking on the read path.
#[test]
fn test_bitband_field_read() {
    snapshot_fn("bitband_field_read_main", "bitband_field_read.bml", "@main");
}

// Compound assignment on a peripheral field is a single-evaluation RMW: the
// register is read exactly once (volatile), so it is safe on read-sensitive
// registers. A naive `field = field OP x` desugar would read it twice.
#[test]
fn test_compound_field_rmw_single_read() {
    let body = extract_fn_body(&bml_ir("compound_field_rmw.bml"), "@bump");
    let vol_loads = body.matches("load volatile").count();
    let vol_stores = body.matches("store volatile").count();
    assert_eq!(
        vol_loads, 1,
        "expected exactly one volatile load (single-eval RMW), got {vol_loads}:\n{body}"
    );
    assert_eq!(
        vol_stores, 1,
        "expected one volatile store, got {vol_stores}"
    );
}

// Multi-bit field (MODER0, bit[0..1]) uses volatile RMW; single-bit fields
// (ODR0/ODR3) use bit-band alias stores.
#[test]
fn test_bitband_field_write() {
    snapshot_fn(
        "bitband_field_write_main",
        "bitband_field_write.bml",
        "@main",
    );
}

// Narrow fields do i32 RMW math with trunc on read and zext on write; the
// snapshot pins the exact width conversions.
#[test]
fn test_field_u8_rmw_widths() {
    snapshot_fn("field_u8_main", "peripheral_field_u8.bml", "@main");
}

#[test]
fn test_field_u16_rmw_widths() {
    snapshot_fn("field_u16_main", "peripheral_field_u16.bml", "@main");
}

#[test]
fn test_field_enum_u8_rmw_widths() {
    snapshot_fn("field_enum_u8_main", "peripheral_field_enum.bml", "@main");
}

// Multi-bit field range uses RMW (load/mask/or/store), not bit-band.
#[test]
fn test_bitband_multi_bit_rmw() {
    snapshot_fn("field_range_main", "peripheral_field_range.bml", "@main");
}

// @naked function: attribute group #0 (not "interrupt"), unreachable fallback.
#[test]
fn test_naked_fn() {
    snapshot_fn("naked_fn", "naked_fn.bml", "@naked_fn");
}

// @naked + @isr: still placed in the vector table, but no interrupt attribute
// on the function definition.
#[test]
fn test_naked_isr() {
    let ir = bml_ir("naked_isr.bml");
    assert!(
        ir.contains("@naked_isr"),
        "expected naked_isr in vector table\n--- IR ---\n{ir}\n-----------"
    );
    check_snapshot("naked_isr", &extract_fn_body(&ir, "@naked_isr"));
}

// @section() on function
assert_ir_contains!(test_fn_section, "fn_section.bml", "section \".ram_code\"");

// @section() on static (bug fix -- was parsed but not emitted)
#[test]
fn test_static_section() {
    let ir = bml_ir("static_section.bml");
    assert!(
        ir.contains("section \"my_data\""),
        "expected static @section in IR\n--- IR ---\n{ir}\n-----------"
    );
    // Confirm "@my_data = global" is NOT present (that's the fn name, not the static)
    assert!(
        ir.contains("@my_data = global"),
        "expected static symbol @my_data\n--- IR ---\n{ir}\n-----------"
    );
}

// tailchain=true leaf ISR: `bx lr`, no interrupt attribute, no push {lr}.
#[test]
fn test_tailchain_leaf() {
    let ir = bml_ir("tailchain_leaf.bml");
    assert!(
        ir.contains("@leaf_isr"),
        "expected leaf_isr in vector table\n--- IR ---\n{ir}\n-----------"
    );
    check_snapshot("tailchain_leaf", &extract_fn_body(&ir, "@leaf_isr"));
}

// tailchain=true ISR with calls: push {lr} / pop {pc}, no interrupt attribute.
#[test]
fn test_tailchain_calls() {
    let ir = bml_ir("tailchain_calls.bml");
    assert!(
        ir.contains("@call_isr"),
        "expected call_isr in vector table\n--- IR ---\n{ir}\n-----------"
    );
    check_snapshot("tailchain_calls", &extract_fn_body(&ir, "@call_isr"));
}

#[test]
fn test_tailchain_asm_input_calls() {
    let body = extract_fn_body(&bml_ir("tailchain_asm_input_calls.bml"), "@call_isr");
    assert!(
        body.contains("push {lr}"),
        "expected tailchain ISR with asm operand call to save lr:\n{body}"
    );
    assert!(
        body.contains("pop {pc}"),
        "expected tailchain ISR with asm operand call to restore via pc:\n{body}"
    );
}

// ─── Stack analysis tests ─────────────────────────────────────────────

fn bml_check_stack(fixture: &str) -> (bool, String) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture);

    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("check")
        .arg("--stack")
        .arg(&path)
        .output()
        .expect("failed to run bml");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

    (output.status.success(), combined)
}

#[test]
fn test_stack_empty() {
    let (ok, output) = bml_check_stack("stack_empty.bml");
    assert!(ok, "expected pass:\n{output}");
    assert!(output.contains("frame=   0"), "expected frame=0\n{output}");
    assert!(
        output.contains("total=     0"),
        "expected total=0\n{output}"
    );
}

#[test]
fn test_stack_basic() {
    let (ok, output) = bml_check_stack("stack_basic.bml");
    assert!(ok, "expected pass:\n{output}");
    // one u32 local = 4 bytes, leaf → no LR push
    assert!(
        output.contains("frame=   4"),
        "expected frame=4 for u32 local\n{output}"
    );
}

#[test]
fn test_stack_nested_calls() {
    let (ok, output) = bml_check_stack("stack_nested_calls.bml");
    assert!(ok, "expected pass:\n{output}");
    // level3: leaf, frame=0
    assert!(
        output.contains("level3"),
        "expected level3 in output\n{output}"
    );
    assert!(output.contains("leaf"), "expected leaf\n{output}");
    // main: local x (4) + LR (4) = 8, + deepest callee chain = 8
    assert!(output.contains("fn main"), "expected main\n{output}");
    assert!(
        output.contains("→ level1"),
        "expected direct callee level1\n{output}"
    );
}

#[test]
fn test_stack_isr() {
    let (ok, output) = bml_check_stack("stack_isr.bml");
    assert!(ok, "expected pass:\n{output}");
    // ISR: 2 u32 locals (8) + exception frame (32) = 40
    assert!(
        output.contains("frame=  40"),
        "expected frame=40 for ISR with 2 locals\n{output}"
    );
    assert!(
        output.contains("isr(prio=1)"),
        "expected ISR priority annotation\n{output}"
    );
}

#[test]
fn test_stack_fnptr() {
    let (ok, output) = bml_check_stack("stack_fnptr.bml");
    assert!(ok, "expected pass:\n{output}");
    // add_one: param x (4) = 4
    assert!(output.contains("fn add_one"), "expected add_one\n{output}");
    // main: fp (4) + result (4) + LR (4, has indirect) = 12, + add_one(4) = 16
    assert!(output.contains("fn main"), "expected main\n{output}");
}

#[test]
fn test_stack_recursive() {
    let (ok, output) = bml_check_stack("stack_recursive.bml");
    assert!(ok, "expected pass with warning:\n{output}");
    assert!(
        output.contains("recursive call chain"),
        "expected recursion warning\n{output}"
    );
    assert!(
        output.contains("W600"),
        "expected W600 recursion warning code\n{output}"
    );
}

#[test]
fn test_stack_struct() {
    let (ok, output) = bml_check_stack("stack_struct.bml");
    assert!(ok, "expected pass:\n{output}");
    // Point has 3 u32 fields = 12 bytes for var + 12 for struct init temp = 24
    assert!(
        output.contains("frame=  24"),
        "expected frame=24 for Point struct\n{output}"
    );
}

#[test]
fn test_stack_branch_locals() {
    let (ok, output) = bml_check_stack("stack_branch_locals.bml");
    assert!(ok, "expected pass:\n{output}");
    // All branch locals are pre-emitted in the entry block: five u32 locals.
    assert!(
        output.contains("frame=  20"),
        "expected frame=20 for all branch locals\n{output}"
    );
}

// ─── new tests for bugfixes ─────────────────────────────────────────

// 1. return should not produce double terminators
assert_ir_not_contains!(
    test_return_not_double_ret,
    "return_terminated.bml",
    "ret void\nret void"
);

// 2. block expression with local variable inside should work
assert_pass!(test_block_expr_with_local, "block_expr_with_local.bml");
assert_ir_contains!(
    test_block_expr_with_local_ir,
    "block_expr_with_local.bml",
    "store i32"
);

// 3. null assigned to non-pointer type should error
assert_error!(test_null_non_ptr, "null_non_ptr.bml", "E300");

// 4. logical and bitwise operators should validate operand types
assert_error!(test_operator_type_error, "operator_type_error.bml", "E316");

// 5. invalid peripheral bit specs
assert_error!(
    test_peripheral_bit_range_error,
    "peripheral_bit_range_error.bml",
    "E114"
);
assert_error!(
    test_peripheral_duplicate_top_level,
    "peripheral_duplicate_top_level_error.bml",
    "E200"
);
assert_error!(
    test_peripheral_duplicate_reg,
    "peripheral_duplicate_reg_error.bml",
    "E200"
);
assert_error!(
    test_peripheral_duplicate_field,
    "peripheral_duplicate_field_error.bml",
    "E319"
);
assert_error!(
    test_peripheral_field_view_stride,
    "peripheral_field_view_stride_error.bml",
    "E332"
);

// 6. large enum discriminant should be caught (not wrap negative)
assert_error!(test_enum_disc_wrap, "enum_disc_wrap.bml", "E323");

// 7. block and if expressions require a trailing value expression
assert_error!(
    test_block_expr_stmt_no_value,
    "block_expr_stmt_no_value.bml",
    "E328"
);
assert_error!(
    test_if_expr_stmt_no_value,
    "if_expr_stmt_no_value.bml",
    "E328"
);
assert_error!(
    test_block_expr_return_no_value,
    "block_expr_return_no_value.bml",
    "E328"
);

// ─── verify/IKOS: assume / assert ──────────────────────────────────────

assert_pass!(test_assume_assert_pass, "assume_assert_pass.bml");

assert_error!(
    test_assume_type_error,
    "assume_assert_type_error.bml",
    "E340"
);
assert_error!(test_assert_type_error, "assert_type_error.bml", "E341");

assert_ir_contains!(test_assume_ir_has_cmp, "assume_ir_cmp.bml", "br i1");
assert_ir_contains!(
    test_assume_ir_has_unreachable,
    "assume_ir_unreach.bml",
    "unreachable"
);
// assert IR emission in verify mode is covered by the integration.

// ─── verify integration (requires BML_IKOS_BIN) ───────────────────────

fn bml_verify(fixture: &str) -> (bool, String) {
    let (ok, stdout, stderr) = bml_verify_args(fixture, &[]);
    (ok, format!("{stdout}{stderr}"))
}

/// Run `bml verify` on a fixture with extra CLI flags, returning
/// (success, stdout, stderr) separately. JSON output goes to stdout and text
/// diagnostics to stderr, so the split lets contract tests check each stream.
fn bml_verify_args(fixture: &str, extra: &[&str]) -> (bool, String, String) {
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);

    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture);

    let ikos_bin = std::env::var("BML_IKOS_BIN").unwrap_or_else(|_| "ikos-analyzer".into());

    // Unique temp dir per call (the same fixture is verified with different
    // flags) to avoid IKOS DB lock contention when tests run in parallel. It is
    // both the IKOS scratch (TMPDIR) and the `.verify.*` artifact dir
    // (--out-dir), so nothing is written into the fixtures directory.
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmpdir = std::env::temp_dir().join(format!("bml_test_{}_{seq}", fixture.replace('.', "_")));
    let _ = std::fs::create_dir_all(&tmpdir);

    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("verify")
        .arg("--ikos-bin")
        .arg(&ikos_bin)
        .arg("--out-dir")
        .arg(&tmpdir)
        .args(extra)
        .arg(&path)
        .env("TMPDIR", &tmpdir)
        // The Homebrew LLVM 18 prefix is macOS-only; non-existent paths are
        // ignored on Linux so the prepended PATH is harmless there.
        .env(
            "PATH",
            format!(
                "/opt/homebrew/opt/llvm@18/bin:/opt/homebrew/opt/llvm/bin:/usr/bin:/bin:{}",
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .output()
        .expect("failed to run bml verify");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let _ = std::fs::remove_dir_all(&tmpdir);

    (output.status.success(), stdout, stderr)
}

// `verify --out-dir` keeps the `.verify.*` intermediates in the given directory
// and writes nothing next to the source. The redirect happens when bml emits the
// IR -- before IKOS runs -- so this checks artifact placement regardless of
// whether IKOS (or the fork's flags) is available. (Default verify already
// isolates via a unique auto-removed temp dir; this pins the explicit redirect.)
#[test]
fn test_verify_out_dir_redirects_artifacts() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let fixture = "verify_no_findings.bml";
    let path = dir.join(fixture);
    let out = unique_out_dir("verify_out_dir");
    let ikos_bin = std::env::var("BML_IKOS_BIN").unwrap_or_else(|_| "ikos-analyzer".into());

    let _ = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("verify")
        .arg("--ikos-bin")
        .arg(&ikos_bin)
        .arg("--out-dir")
        .arg(&out)
        .arg(&path)
        .env("TMPDIR", &out)
        .output()
        .expect("failed to run bml verify");

    let in_out_dir = out_artifact(&out, fixture, "verify.ll").exists();
    let beside_source = path.with_extension("verify.ll").exists();
    let _ = std::fs::remove_dir_all(&out);

    assert!(
        in_out_dir,
        "the verify .ll should land in --out-dir (written before IKOS runs)"
    );
    assert!(
        !beside_source,
        "no .verify.ll should be written next to the source"
    );
}

/// Run `bml verify <args>` directly with no IKOS setup, returning
/// (success, stdout, stderr). For CLI-contract tests whose argument errors are
/// reported before IKOS is ever invoked, so they need no toolchain.
fn run_verify_raw(args: &[&str]) -> (bool, String, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("verify")
        .args(args)
        .output()
        .expect("failed to run bml verify");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

macro_rules! assert_verify_fail {
    ($name:ident, $fixture:expr) => {
        #[test]
        fn $name() {
            if std::env::var("BML_IKOS_BIN").is_err() {
                eprintln!("skipping verify test (set BML_IKOS_BIN)");
                return;
            }
            let (ok, output) = bml_verify($fixture);
            assert!(!ok, "expected verify to fail, got success:\n{output}");
        }
    };
}

macro_rules! assert_verify_pass {
    ($name:ident, $fixture:expr) => {
        #[test]
        fn $name() {
            if std::env::var("BML_IKOS_BIN").is_err() {
                eprintln!("skipping verify test (set BML_IKOS_BIN)");
                return;
            }
            let (ok, output) = bml_verify($fixture);
            assert!(ok, "expected verify to pass, got failure:\n{output}");
        }
    };
}

assert_verify_fail!(test_verify_assert_fails, "verify_assert_fail.bml");
assert_verify_pass!(test_verify_assert_holds, "verify_assert_pass.bml");
// Unconditional out-of-bounds access -> V100 (buffer-overflow, error severity).
#[test]
fn test_verify_boa_oob() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, output) = bml_verify("verify_boa_oob.bml");
    assert!(!ok, "expected verify to fail, got success:\n{output}");
    assert!(
        output.contains("[V100]"),
        "expected V100 buffer-overflow finding, got:\n{output}"
    );
}

// Unconditional division by zero -> V120.
#[test]
fn test_verify_dbz() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, output) = bml_verify("verify_dbz.bml");
    assert!(!ok, "expected verify to fail, got success:\n{output}");
    assert!(
        output.contains("[V120]"),
        "expected V120 division-by-zero finding, got:\n{output}"
    );
}
assert_verify_fail!(test_verify_uio, "verify_uio.bml");

// `&&`/`||` lower short-circuit: branch around the RHS, i1 phi at the join,
// no eager `and i1`/`or i1` of both operands (the eager form read MMIO in
// the RHS even when the LHS decided -- a read-to-clear hazard).
#[test]
fn test_short_circuit_lowering() {
    let ir = bml_ir("short_circuit_ir.bml");
    assert!(ir.contains("and_rhs"), "expected and_rhs block:\n{ir}");
    assert!(ir.contains("or_rhs"), "expected or_rhs block:\n{ir}");
    assert!(ir.contains("phi i1"), "expected i1 phi join:\n{ir}");
    assert!(
        !ir.contains("= and i1") && !ir.contains("= or i1"),
        "logical ops must not lower to eager and/or:\n{ir}"
    );
}

// Spin loop with an MMIO conjunct in the guard: the branch-tree condition
// lowering makes the counter bound prove -- no suppression, and the
// post-loop assert holds.
assert_verify_pass!(test_verify_while_mmio_guard, "verify_while_mmio_guard.bml");

#[test]
fn test_while_guard_lowers_as_branch_tree() {
    let ir = bml_ir("verify_while_mmio_guard.bml");
    assert!(
        ir.contains("cond_and"),
        "expected a branch-tree conjunct block in the while guard:\n{ir}"
    );
    assert!(
        !ir.contains("= and i1"),
        "guard must not materialize an eager and i1:\n{ir}"
    );
}

// LANGUAGE CONTRACT: overflow on plain ops is excluded by verification --
// a POSSIBLE overflow (warning-level V130 on a havoc'd operand) fails the
// gate exactly like a definite one...
assert_verify_fail!(test_verify_uio_warning_is_red, "verify_uio_warning.bml");
// ...and the `+%` twin of the same may-overflow add passes: declared wrap.
assert_verify_pass!(
    test_verify_wrap_uio_warning_passes,
    "verify_wrap_uio_warning.bml"
);

// Signed-typed arithmetic is tagged `nsw` in the verify IR, flipping IKOS
// from the unsigned reading (which flagged `5 - 7` as i32 as a definite
// underflow) to the signed check with branch narrowing: bounded signed math
// proves with zero suppressions...
assert_verify_pass!(test_verify_signed_math, "verify_signed_math.bml");
// ...while a definite i32 overflow (INT_MAX + 1) goes red -- a true
// differential, because the unsigned reading would have passed it.
assert_verify_fail!(test_verify_sio_definite, "verify_sio_definite.bml");

// WRAP FAITHFULNESS (requires the --no-wrap-sign-only ikos fork patch):
// the nsw tag selects the sio check but must NOT double as an
// assume-no-overflow. A possible overflow (V130, escalated) followed by an
// assert that only holds if the add cannot wrap must report BOTH findings;
// stock IKOS proves the assert from the unproven overflow (no V200).
#[test]
fn test_verify_nsw_wrap_faithful() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, output) = bml_verify("verify_nsw_wrap_faithful.bml");
    assert!(!ok, "expected verify to fail, got success:\n{output}");
    assert!(
        output.contains("[V130]"),
        "expected the possible-overflow V130 finding:\n{output}"
    );
    assert!(
        output.contains("[V200]"),
        "the assert downstream of an unproven overflow must stay unproven \
         (wrapped value can be negative); a missing V200 means ikos still \
         applies assume-no-overflow semantics to nsw:\n{output}"
    );
}

// The nsw tag must NOT appear in runtime codegen (BML defines wrap; nsw
// would license UB-based optimization).
#[test]
fn test_signed_runtime_ir_has_no_nsw() {
    let ir = bml_ir("signed_no_nsw.bml");
    assert!(ir.contains("sub i32"), "expected sub i32 in IR:\n{ir}");
    assert!(
        !ir.contains("nsw") && !ir.contains("nuw"),
        "runtime IR must not carry overflow flags:\n{ir}"
    );
}

// Same overflow DECLARED with `+%`: the V130 finding is dropped (wrap is
// intent, carried via Program::wrap_spans, no ignore comment involved) and
// the wrapped value still proves (`assert(b == 0)` holds in machine
// arithmetic). Contrast with test_verify_uio above, which must stay red.
#[test]
fn test_verify_wrap_uio_passes() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, output) = bml_verify("verify_wrap_uio.bml");
    assert!(ok, "expected verify to pass with `+%`:\n{output}");
    assert!(
        !output.contains("[V130]"),
        "wrap-declared overflow must not report V130:\n{output}"
    );
}
assert_verify_pass!(test_verify_no_findings, "verify_no_findings.bml");
// assume_narrows: assume(b != 0) before a/b should prevent dbz
assert_verify_pass!(test_verify_assume_narrows, "verify_assume_narrows.bml");
assert_verify_fail!(test_verify_nullity, "verify_nullity.bml");
assert_verify_pass!(test_verify_global_ref, "verify_global_ref.bml");
assert_verify_fail!(test_verify_unlabeled_isr, "verify_unlabeled_isr.bml");
assert_verify_pass!(test_verify_ptr_u8, "verify_ptr_u8.bml");
assert_verify_pass!(test_verify_ptr_u16, "verify_ptr_u16.bml");
// Readonly linear view: bounds proven intra-procedurally and across a call
// (provenance), and an overstated len is still caught against the real buffer.
assert_verify_pass!(test_verify_view_read, "view_read.bml");
assert_verify_pass!(test_verify_view_helper, "view_helper.bml");
assert_verify_pass!(test_verify_view_from_array, "view_from_array.bml");
assert_verify_fail!(test_verify_view_len_overstates, "view_len_overstates.bml");
// A mutable-view index write: IKOS proves the store is in bounds (the
// descriptor stays SSA-transparent, so provenance to the backing array holds).
assert_verify_pass!(test_verify_view_mut_write, "view_mut_write.bml");
// Strided linear views: the backing index `i * K` stays a typed GEP because K
// is a compile-time constant, so IKOS proves the bound intra-procedurally and,
// crucially, across a call (the helper bakes the same constant `* K`, and the
// backing allocation propagates through the call). A mutable strided write is
// proven the same way.
assert_verify_pass!(test_verify_view_strided_read, "view_strided_read.bml");
assert_verify_pass!(test_verify_view_strided_helper, "view_strided_helper.bml");
assert_verify_pass!(
    test_verify_view_strided_mut_write,
    "view_strided_mut_write.bml"
);
// Ring views: the (head+i) % capacity physical index is bounded -- by the
// constant mask `& (cap-1)` when the capacity is a power of two (ring_read,
// ring_mut_write, len 8), or by `urem` otherwise (ring_npot_read, len 6). With
// the array-derived constant capacity IKOS proves read and write either way.
assert_verify_pass!(test_verify_ring_read, "ring_read.bml");
assert_verify_pass!(test_verify_ring_npot_read, "ring_npot_read.bml");
assert_verify_pass!(test_verify_ring_mut_write, "ring_mut_write.bml");
// Writing through a mutable view/ring passed to a helper: provenance flows
// through the call, so IKOS still proves the store in bounds.
assert_verify_pass!(test_verify_view_mut_param_write, "view_mut_param_write.bml");
assert_verify_pass!(test_verify_ring_mut_param_write, "ring_mut_param_write.bml");
// Bit views: the byte address (bit_offset + i) / 8 is bounded by assume(i <
// len_bits), so with the array-derived constant length IKOS proves both the
// read (bit extract) and the read-modify-write store.
// Views over a storage-wrapped (`@dma`/`@external`) array still prove in bounds:
// the backing is a known in-program allocation, storage class aside.
assert_verify_pass!(test_verify_view_over_dma, "view_over_dma.bml");
assert_verify_pass!(test_verify_bit_read, "bit_read.bml");
assert_verify_pass!(test_verify_bit_mut_write, "bit_mut_write.bml");
assert_verify_pass!(test_verify_bit_mut_param_write, "bit_mut_param_write.bml");
// Soundness: a len_bits that overstates the backing buffer does not let the
// assume mask a real out-of-bounds byte access; IKOS still catches it (V100).
assert_verify_fail!(test_verify_bit_len_overstates, "bit_len_overstates.bml");
// Characterize the runtime-capacity ring form: unlike the array-backed form, the
// backing pointer is an entry-point param and the capacity is runtime, so the
// verifier cannot prove the access. The `urem` by a runtime capacity admits a
// division-by-zero (V120). This documents the trust-boundary limitation.
#[test]
fn test_verify_ring_runtime_flags_div_by_zero() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (_ok, output) = bml_verify("ring_runtime.bml");
    assert!(
        output.contains("V120"),
        "expected a division-by-zero (V120) finding for the runtime ring form:\n{output}"
    );
}
// Preempt shim: no ISR writer → no forget_mem → prover can fold the value.
assert_verify_pass!(test_verify_shared_no_writer, "verify_shared_no_writer.bml");

// Preempt shim: ISR writer exists → forget_mem havocs the read → IKOS can no
// longer prove the assert and reports it (warning, not error, since the value
// is unknown rather than statically wrong).
#[test]
fn test_verify_shared_with_writer() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (_ok, output) = bml_verify("verify_shared_with_writer.bml");
    assert!(
        output.contains("[V200]"),
        "expected V200 assert finding from preempt shim, got:\n{output}"
    );
}

// Preempt shim soundness: a reader that ALSO writes the static is still
// havoc'd against other higher-priority writers (a fn only cannot preempt
// itself) -- the write-then-read-back is not provable outside a window.
#[test]
fn test_verify_shared_writeback_not_proven() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (_ok, output) = bml_verify("verify_shared_writeback.bml");
    assert!(
        output.contains("[V200]"),
        "the read-back must NOT be provable (ISR can write in between), got:\n{output}"
    );
}

// Claim-aware verify: the same read-back IS proven inside `claim` -- the
// window's mask makes the value stable, and the emitter suppresses the havoc
// for the claimed static in-window.
#[test]
fn test_verify_claim_window_proven() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, output) = bml_verify("verify_claim_window.bml");
    assert!(
        ok && !output.contains("[V200]"),
        "the in-window read-back must verify clean; got:\n{output}"
    );
}

// Bit-band targets: the single-bit field write goes through the 0x42 alias
// region, whose 32-word image per register is whitelisted in hwaddrs --
// without it IKOS reports the alias store as a definite V100.
#[test]
fn test_verify_bitband_alias_clean() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_bitband.target");
    let (ok, _stdout, stderr) = bml_verify_args("verify_bitband.bml", &["--target", &target]);
    assert!(
        ok && !stderr.contains("[V100]"),
        "the bit-band alias store must be whitelisted MMIO; stderr:\n{stderr}"
    );
}

// Pointer-related V11x mapping: an unknown pointer parameter that's
// dereferenced after a null-check produces a V114 finding from IKOS.
#[test]
fn test_verify_null_compare_v11x() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (_ok, output) = bml_verify("verify_null_compare.bml");
    assert!(
        output.contains("[V114]"),
        "expected V114 unknown-memory-access finding, got:\n{output}"
    );
}

// Self-writer exclusion: the writer is also the reader, so preempt analysis
// must skip it. No forget_mem emitted; assert is provable.
assert_verify_pass!(test_verify_self_writer, "verify_self_writer.bml");

// `// bml-verify: ignore V120` on the same line as a finding silences it.
assert_verify_pass!(test_verify_suppress_v120, "verify_suppress.bml");

// ISR-to-ISR preemption: high-priority ISR writes while a lower-priority
// ISR reads. Neither side is a thread, but the shim must still fire.
#[test]
fn test_verify_isr_to_isr() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (_ok, output) = bml_verify("verify_isr_to_isr.bml");
    assert!(
        output.contains("[V200]"),
        "expected V200 assert finding from cross-ISR preempt, got:\n{output}"
    );
}

// Bounded for loop: IKOS proves `i ∈ [0, 4)` so no V100/V101 fires.
assert_verify_pass!(test_verify_loop_safe, "verify_loop_safe.bml");
assert_verify_pass!(test_for_verify_continue, "for_verify_continue.bml");

// Out-of-bounds for loop: IKOS reports V101 buffer-overflow on the index
// (warning, not error, because the violation is conditional on iteration).
#[test]
fn test_verify_loop_oob() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (_ok, output) = bml_verify("verify_loop_oob.bml");
    assert!(
        output.contains("[V101]"),
        "expected V101 buffer-overflow warning, got:\n{output}"
    );
}

// Path-sensitive narrowing: `if i < 4` should let IKOS prove `buf[i]` safe.
assert_verify_pass!(test_verify_cond_narrow, "verify_cond_narrow.bml");

// Narrowing not tight enough: `if i < 10` leaves indices 4..=9, still OOB.
#[test]
fn test_verify_cond_loose() {
    if std::env::var("BML_IKOS_BIN").is_err() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (_ok, output) = bml_verify("verify_cond_loose.bml");
    assert!(
        output.contains("[V101]"),
        "expected V101 buffer-overflow warning, got:\n{output}"
    );
}

// ─── verify CLI contract: argument parsing (no toolchain needed) ───────────
//
// These errors are reported during argument parsing, before IKOS is invoked,
// so they run regardless of BML_IKOS_BIN.

#[test]
fn test_verify_format_unknown() {
    let (ok, _stdout, stderr) = run_verify_raw(&["--format", "xml"]);
    assert!(!ok, "unknown --format should exit non-zero");
    assert!(
        stderr.contains("unknown format `xml`"),
        "expected an unknown-format message, got:\n{stderr}"
    );
}

#[test]
fn test_verify_fail_on_unknown() {
    let (ok, _stdout, stderr) = run_verify_raw(&["--fail-on", "sometimes"]);
    assert!(!ok, "unknown --fail-on level should exit non-zero");
    assert!(
        stderr.contains("unknown level `sometimes`"),
        "expected an unknown-level message, got:\n{stderr}"
    );
}

#[test]
fn test_verify_missing_source() {
    let (ok, _stdout, stderr) = run_verify_raw(&[]);
    assert!(!ok, "missing source path should exit non-zero");
    assert!(
        stderr.contains("Usage: bml verify"),
        "expected usage text, got:\n{stderr}"
    );
}

// ─── verify CLI contract: --format json / --fail-on (requires BML_IKOS_BIN) ─

fn ikos_available() -> bool {
    std::env::var("BML_IKOS_BIN").is_ok()
}

// JSON output goes to stdout; a clean program is an empty findings array.
#[test]
fn test_verify_json_empty() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, stdout, _stderr) = bml_verify_args("verify_no_findings.bml", &["--format", "json"]);
    assert!(ok, "a clean program should exit 0");
    assert_eq!(
        stdout.trim(),
        "{\"findings\":[]}",
        "expected an empty findings array on stdout"
    );
}

// A real finding is rendered as a JSON object on stdout with the documented
// fields, and JSON mode keeps the text diagnostics off stderr.
#[test]
fn test_verify_json_error_finding() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, stdout, stderr) = bml_verify_args("verify_assert_fail.bml", &["--format", "json"]);
    assert!(!ok, "an error finding should exit 1");
    let s = stdout.trim();
    assert!(
        s.starts_with("{\"findings\":[") && s.ends_with("]}"),
        "expected a findings array on stdout, got:\n{stdout}"
    );
    for field in [
        "\"check\":\"assert\"",
        "\"severity\":\"error\"",
        "\"line\":",
        "\"column\":",
    ] {
        assert!(
            s.contains(field),
            "expected JSON to contain {field}, got:\n{stdout}"
        );
    }
    // text-format diagnostics must not leak to stderr in JSON mode
    assert!(
        !stderr.contains("error[assert]"),
        "JSON mode should not also emit text diagnostics on stderr, got:\n{stderr}"
    );
}

// --fail-on sets the severity threshold for a non-zero exit. A warning-level
// finding passes under the default `error` threshold but fails under `warning`.
#[test]
fn test_verify_fail_on_threshold() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok_default, _, _) = bml_verify_args("verify_loop_oob.bml", &[]);
    assert!(
        ok_default,
        "a warning finding should not fail under the default --fail-on error"
    );
    let (ok_warn, _, _) = bml_verify_args("verify_loop_oob.bml", &["--fail-on", "warning"]);
    assert!(
        !ok_warn,
        "--fail-on warning should exit 1 on a warning finding"
    );
}

// --fail-on never suppresses the non-zero exit even for an error finding.
#[test]
fn test_verify_fail_on_never() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, _, _) = bml_verify_args("verify_assert_fail.bml", &["--fail-on", "never"]);
    assert!(
        ok,
        "--fail-on never should exit 0 even with an error finding"
    );
}

// Regression: (1) struct debug metadata was emitted with doubled braces in the
// DICompositeType `elements:` list, so `opt` rejected the IR of any
// struct-containing program; (2) IKOS reports `"operands": null` on
// unreachable entries (the fixture's dead branch), which the JSON report
// parser rejected. Either bug makes this run fail outright.
#[test]
fn test_verify_struct_debug_info() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, _stdout, stderr) = bml_verify_args("verify_struct_debug_info.bml", &[]);
    assert!(
        !stderr.contains("ikos failed"),
        "verify pipeline error on a struct-containing program:\n{stderr}"
    );
    assert!(ok, "expected a finding-free exit, stderr:\n{stderr}");
}

// `in <region>` naming a region the target does not define is rejected (E600).
// Region placement is validated against the target, so this runs in `build`.
#[test]
fn test_region_unknown_placement() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--target")
        .arg(dir.join("region_unknown.target"))
        .arg(dir.join("region_unknown.bml"))
        .output()
        .expect("failed to run bml build");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "build should fail on placement into an unknown region; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E600"), "expected E600; stderr:\n{stderr}");
    assert!(
        stderr.contains("dma_shared"),
        "diagnostic should list the known regions; stderr:\n{stderr}"
    );
}

// A region-placed static with an initializer is rejected (E601): region memory
// is not initialized at startup, so the initializer would be silently dropped.
#[test]
fn test_region_init_rejected() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--target")
        .arg(dir.join("region_unknown.target"))
        .arg(dir.join("region_init.bml"))
        .output()
        .expect("failed to run bml build");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "build should fail on an initialized region static; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E601"), "expected E601; stderr:\n{stderr}");
}

// `in <region>` together with `@section(...)` is rejected (E602): both set the
// output section and would silently fight.
#[test]
fn test_region_section_conflict() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--target")
        .arg(dir.join("region_unknown.target"))
        .arg(dir.join("region_section_conflict.bml"))
        .output()
        .expect("failed to run bml build");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "build should fail on in+@section; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E602"), "expected E602; stderr:\n{stderr}");
}

// ─── ownership (`owns`, slice 2a) ─────────────────────────────────────────

/// Build a fixture with the default target; return (success, stderr). Used by
/// the ownership tests, which need no target (they check source-level claims).
fn bml_build_default(fixture: &str) -> (bool, String) {
    bml_build_with_target(fixture, None)
}

/// Build a fixture, optionally with a target file; return (success, stderr).
fn bml_build_with_target(fixture: &str, target: Option<&str>) -> (bool, String) {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let path = dir.join(fixture);
    let out = unique_out_dir(fixture);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bml"));
    cmd.arg("build").arg("--out-dir").arg(&out);
    if let Some(t) = target {
        cmd.arg("--target").arg(dir.join(t));
    }
    let output = cmd.arg(&path).output().expect("failed to run bml build");
    let _ = std::fs::remove_dir_all(&out);
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status.success(), stderr)
}

// `--out-dir` places every build artifact in the given directory and writes
// nothing next to the source -- the property the whole test harness relies on
// for race-free parallel builds.
#[test]
fn test_out_dir_redirects_artifacts() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let fixture = "owns_ok.bml";
    let path = dir.join(fixture);
    let out = unique_out_dir("out_dir_redirect");
    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--out-dir")
        .arg(&out)
        .arg(&path)
        .output()
        .expect("failed to run bml build");

    let in_out_dir: Vec<&str> = ["ll", "o", "ld"]
        .into_iter()
        .filter(|ext| out_artifact(&out, fixture, ext).exists())
        .collect();
    let beside_source: Vec<&str> = ["ll", "o", "ld"]
        .into_iter()
        .filter(|ext| path.with_extension(ext).exists())
        .collect();
    // Clean up before asserting so a failure doesn't strand the temp dir.
    let _ = std::fs::remove_dir_all(&out);

    assert!(
        output.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        in_out_dir,
        vec!["ll", "o", "ld"],
        "all artifacts should land in --out-dir"
    );
    assert!(
        beside_source.is_empty(),
        "no artifacts should be written next to the source, found: {beside_source:?}"
    );
}

// A module owning a peripheral it uses builds cleanly.
#[test]
fn test_owns_ok() {
    let (ok, stderr) = bml_build_default("owns_ok.bml");
    assert!(ok, "owns_ok should build; stderr:\n{stderr}");
}

// Two imported modules owning the same peripheral is a cross-module conflict
// (E604). Exercises whole-program flattening: both `owns` clauses reach one
// program through separate imports.
#[test]
fn test_owns_conflict_across_modules() {
    let (ok, stderr) = bml_build_default("owns_conflict.bml");
    assert!(!ok, "owns_conflict should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E604"), "expected E604; stderr:\n{stderr}");
    assert!(
        stderr.contains("GPIOZ"),
        "should name the register; stderr:\n{stderr}"
    );
}

// Owning a whole peripheral in one module and one of its registers in another
// also overlaps (E604) -- the conflict is not only same-path-vs-same-path.
#[test]
fn test_owns_conflict_peripheral_vs_register() {
    let (ok, stderr) = bml_build_default("owns_conflict_reg.bml");
    assert!(!ok, "owns_conflict_reg should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E604"), "expected E604; stderr:\n{stderr}");
    assert!(
        stderr.contains("GPIOZ.ODR"),
        "should name the overlapping register; stderr:\n{stderr}"
    );
}

// GPIO pin exclusivity (M4): `owns gpio[lo..hi]`. Two modules driving disjoint
// pin ranges build cleanly; overlapping ranges are the pin-level analogue of
// E604 (E650).
#[test]
fn test_pin_exclusive_disjoint_ok() {
    let (ok, stderr) = bml_build_default("pin_exclusive_ok.bml");
    assert!(ok, "disjoint pin ranges should build; stderr:\n{stderr}");
}
#[test]
fn test_pin_overlap_conflict() {
    let (ok, stderr) = bml_build_default("pin_overlap_error.bml");
    assert!(!ok, "overlapping pin ranges should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E650"), "expected E650; stderr:\n{stderr}");
    assert!(
        stderr.contains("gpio[16..18]"),
        "should name the overlapping range; stderr:\n{stderr}"
    );
}

// Owning a peripheral the program does not define is rejected (E603).
#[test]
fn test_owns_unknown_peripheral() {
    let (ok, stderr) = bml_build_default("owns_unknown.bml");
    assert!(!ok, "owns_unknown should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E603"), "expected E603; stderr:\n{stderr}");
    assert!(
        stderr.contains("Ghost"),
        "should name the bad path; stderr:\n{stderr}"
    );
}

// Field-level ownership is not yet supported and is rejected in the parser
// (E603) rather than silently narrowing to the register.
#[test]
fn test_owns_field_level_rejected() {
    let (ok, stderr) = bml_build_default("owns_field.bml");
    assert!(!ok, "owns_field should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E603"), "expected E603; stderr:\n{stderr}");
    assert!(
        stderr.contains("field-level"),
        "should explain field-level is unsupported; stderr:\n{stderr}"
    );
}

// ─── handoff-ownership rule (`owns` + handoff, slice 2b) ───────────────────

// Writing a handoff register without owning it is rejected (E605).
#[test]
fn test_handoff_write_requires_ownership() {
    let (ok, stderr) = bml_build_with_target("handoff_unowned.bml", Some("handoff.target"));
    assert!(!ok, "unowned handoff write should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E605"), "expected E605; stderr:\n{stderr}");
    assert!(
        stderr.contains("MyDma.CTRL") && stderr.contains("mydma"),
        "should name the handoff register and its agent; stderr:\n{stderr}"
    );
}

// Owning the handoff register (or its whole peripheral) licenses the write.
#[test]
fn test_handoff_write_with_ownership_ok() {
    let (ok, stderr) = bml_build_with_target("handoff_owned.bml", Some("handoff.target"));
    assert!(
        ok,
        "owning the register should allow the write; stderr:\n{stderr}"
    );
    let (ok2, stderr2) =
        bml_build_with_target("handoff_owned_peripheral.bml", Some("handoff.target"));
    assert!(
        ok2,
        "owning the peripheral should allow the write; stderr:\n{stderr2}"
    );
}

// DMA->FIFO bridge DREQ check (M5): a channel declares the transfer request that
// pairs it with its endpoint (`dreq = P.R.F = VARIANT`); selecting a different
// DREQ would over/underrun the FIFO (E651). Matching is clean.
#[test]
fn test_dreq_match_ok() {
    let (ok, stderr) = bml_build_with_target("dreq_ok.bml", Some("dreq.target"));
    assert!(ok, "matching DREQ should build; stderr:\n{stderr}");
}
#[test]
fn test_dreq_mismatch() {
    let (ok, stderr) = bml_build_with_target("dreq_mismatch_error.bml", Some("dreq.target"));
    assert!(!ok, "mismatched DREQ should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E651"), "expected E651; stderr:\n{stderr}");
}

// DMA->FIFO endpoint check (E652): a channel declares the peripheral register
// its write handoff must be pointed at (`endpoint = HANDOFF = P.R[i]`); pointing
// it at a different register is rejected. Matching builds clean.
#[test]
fn test_endpoint_match_ok() {
    let (ok, stderr) = bml_build_with_target("endpoint_ok.bml", Some("endpoint.target"));
    assert!(ok, "matching endpoint should build; stderr:\n{stderr}");
}
#[test]
fn test_endpoint_mismatch() {
    let (ok, stderr) =
        bml_build_with_target("endpoint_mismatch_error.bml", Some("endpoint.target"));
    assert!(!ok, "wrong endpoint should fail; stderr:\n{stderr}");
    assert!(stderr.contains("E652"), "expected E652; stderr:\n{stderr}");
}

// The rule applies only to handoff registers: writing an ordinary register of
// the same peripheral without owning anything is fine.
#[test]
fn test_non_handoff_write_needs_no_ownership() {
    let (ok, stderr) =
        bml_build_with_target("handoff_nonhandoff_write.bml", Some("handoff.target"));
    assert!(
        ok,
        "non-handoff write should not require ownership; stderr:\n{stderr}"
    );
}

// ─── handoff provenance obligations (verify, slice 4) ──────────────────────

/// Absolute path to a fixtures target file, for verify tests that need one.
fn fixture_target(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

// A descriptor placed in the region the agent reaches, whose address flows
// through a helper into the handoff register, discharges the reachability
// obligation: verify mode emits assume(addr in region) at `&DESC as u32` and
// assert(addr in reach) at the write, and IKOS proves it. Clean exit.
#[test]
fn test_verify_handoff_provenance_ok() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) = bml_verify_args("verify_handoff.bml", &["--target", &target]);
    assert!(
        !stderr.contains("ikos failed"),
        "verify pipeline error:\n{stderr}"
    );
    assert!(
        !stderr.contains("error[assert]"),
        "a properly-placed descriptor should discharge the obligation:\n{stderr}"
    );
    assert!(ok, "expected a clean verify exit; stderr:\n{stderr}");
}

// Extent unit cross-check (E618): `extent_by ... xN by P.R.F = V` makes the
// multiplier checked physics -- arming without establishing the unit field
// (or establishing a different value) is a compile error.
#[test]
fn test_extent_unit_ok() {
    let (ok, stderr) = bml_build_with_target("extent_unit_ok.bml", Some("extent_unit.target"));
    assert!(
        ok,
        "unit established before arming should build; stderr:\n{stderr}"
    );
}

// E618 with an ENUM-typed unit field: `MyDma.CTRL.SIZE = SizeSel@Word` (=2)
// must satisfy `when CTRL.SIZE = 2`. Regression for enum_discriminant resolving
// an enum-variant write to its discriminant.
#[test]
fn test_extent_unit_enum_ok() {
    let (ok, stderr) = bml_build_with_target("extent_unit_enum_ok.bml", Some("extent_unit.target"));
    assert!(
        ok,
        "enum-typed unit field written via @variant should satisfy E618; stderr:\n{stderr}"
    );
}

#[test]
fn test_extent_unit_missing_rejected() {
    let (ok, stderr) = bml_build_with_target("extent_unit_missing.bml", Some("extent_unit.target"));
    assert!(
        !ok,
        "arming without the unit write must fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E618"), "expected E618; stderr:\n{stderr}");
}

#[test]
fn test_extent_unit_wrong_rejected() {
    let (ok, stderr) = bml_build_with_target("extent_unit_wrong.bml", Some("extent_unit.target"));
    assert!(!ok, "a different unit value must fail; stderr:\n{stderr}");
    assert!(stderr.contains("E618"), "expected E618; stderr:\n{stderr}");
}

// Fixed-block extents (`extent = N`, EasyDMA-style engines with no count
// register): the obligation moves to the delivery -- the buffer must be at
// least N bytes (E619, compile time for direct `&X` deliveries).
#[test]
fn test_fixed_extent_ok() {
    let (ok, stderr) = bml_build_with_target("fixed_extent_ok.bml", Some("fixed_extent.target"));
    assert!(ok, "an exact-size block should build; stderr:\n{stderr}");
}

#[test]
fn test_fixed_extent_short_rejected() {
    let (ok, stderr) = bml_build_with_target("fixed_extent_short.bml", Some("fixed_extent.target"));
    assert!(!ok, "a short buffer must be rejected; stderr:\n{stderr}");
    assert!(stderr.contains("E619"), "expected E619; stderr:\n{stderr}");
}

// Descriptor-carried extents (`@extent(addr_field [, xN])` struct-field
// attribute): declaration sanity is E617 (compile time); the length-vs-
// delivered-buffer check itself lives entirely in verify (IKOS).
assert_error!(
    test_desc_extent_bad_sibling_rejected,
    "desc_extent_bad_sibling.bml",
    "E617"
);
assert_error!(
    test_desc_extent_not_addr_rejected,
    "desc_extent_not_addr.bml",
    "E617"
);

// Masked sub-field extent (`@extent(addr_field [, xN] [, mask N])`): a
// descriptor control word packs the length with control bits, so the obligation
// must read only the length sub-field. Declaration sanity is E617 (mask nonzero,
// fits the 32-bit field); a missing literal is E107. The mask is composable with
// the multiplier in the fixed order `xN` then `mask N`.
assert_error!(
    test_desc_extent_mask_zero_rejected,
    "desc_extent_mask_zero.bml",
    "E617"
);
assert_error!(
    test_desc_extent_mask_wide_rejected,
    "desc_extent_mask_wide.bml",
    "E617"
);
assert_error!(
    test_desc_extent_mask_noint_rejected,
    "desc_extent_mask_noint.bml",
    "E107"
);

#[test]
fn test_desc_extent_mask_builds() {
    let (ok, stderr) = bml_build_with_target("desc_extent_mask.bml", Some("verify_handoff.target"));
    assert!(ok, "a masked `@extent` should build; stderr:\n{stderr}");
}

#[test]
fn test_desc_extent_mask_with_multiplier_builds() {
    let (ok, stderr) =
        bml_build_with_target("desc_extent_mask_x.bml", Some("verify_handoff.target"));
    assert!(
        ok,
        "`@extent(buf1, x4, mask 0x3FFF)` should build; stderr:\n{stderr}"
    );
}

// The mask is ANDed into the count BEFORE the `count*N <= capacity` extent
// compare, so a set control bit cannot inflate the byte count. The `.verify.ll`
// is written before IKOS runs (see test_verify_out_dir_redirects_artifacts), so
// this needs no IKOS binary -- a bogus `--ikos-bin` is fine.
#[test]
fn test_desc_extent_mask_emits_and_before_compare() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let fixture = "desc_extent_mask.bml";
    let out = unique_out_dir("desc_extent_mask_ir");
    let _ = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("verify")
        .arg("--ikos-bin")
        .arg("/nonexistent-ikos")
        .arg("--target")
        .arg(dir.join("verify_handoff.target"))
        .arg("--out-dir")
        .arg(&out)
        .arg(dir.join(fixture))
        .env("TMPDIR", &out)
        .output()
        .expect("failed to run bml verify");
    let ir = std::fs::read_to_string(out_artifact(&out, fixture, "verify.ll")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&out);

    // 0x3FFF = 16383: the length bits, ANDed out before the extent multiply.
    let mask_at = ir
        .find("16383")
        .unwrap_or_else(|| panic!("expected the @extent mask 0x3FFF (=16383) in verify IR:\n{ir}"));
    let cmp_at = ir
        .find("icmp ule i32")
        .unwrap_or_else(|| panic!("expected the extent `icmp ule i32` compare:\n{ir}"));
    assert!(
        mask_at < cmp_at,
        "the mask must be applied before the extent compare:\n{ir}"
    );
}

#[test]
fn test_verify_desc_extent_ok() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) =
        bml_verify_args("verify_desc_extent_ok.bml", &["--target", &target]);
    assert!(
        ok && !stderr.contains("[V200]"),
        "an in-bounds descriptor length must verify clean; stderr:\n{stderr}"
    );
}

#[test]
fn test_verify_desc_extent_overrun_rejected() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) =
        bml_verify_args("verify_desc_extent_over.bml", &["--target", &target]);
    assert!(
        !ok,
        "a descriptor length past the buffer must fail:\n{stderr}"
    );
    assert!(
        stderr.contains("error[assert]"),
        "expected a definite descriptor-extent violation:\n{stderr}"
    );
}

// Masked descriptor extent, end-to-end through IKOS: with `mask 0x3FFF` a control
// word carrying a set bit above the length verifies clean (the mask isolates the
// low 14 bits); the unmasked twin reports the V200 the mask exists to express
// away. IKOS-gated (skips without BML_IKOS_BIN).
#[test]
fn test_verify_desc_extent_mask_clean() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) = bml_verify_args("desc_extent_mask.bml", &["--target", &target]);
    assert!(
        ok && !stderr.contains("[V200]"),
        "a masked extent (control bit set, length in range) must verify clean:\n{stderr}"
    );
}

#[test]
fn test_verify_desc_extent_nomask_v200() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) = bml_verify_args("desc_extent_nomask.bml", &["--target", &target]);
    assert!(
        !ok && stderr.contains("[V200]"),
        "without the mask the whole control word is the byte count, overrunning the buffer; \
         expected V200:\n{stderr}"
    );
}

// Witness operands: a failing assert carries the inferred ranges of the
// compared quantities (the fork records them in the check `info`; db.rs renders
// them). For the unmasked descriptor extent the whole control word becomes the
// byte count -- the report shows 1073742336 (= 512 | 0x40000000) against the
// 512-byte buffer, the value we previously had to hand-derive.
#[test]
fn test_verify_extent_witness_ranges() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) = bml_verify_args("desc_extent_nomask.bml", &["--target", &target]);
    assert!(
        !ok && stderr.contains("bytes in [1073742336") && stderr.contains("cap in [512"),
        "the V200 message should carry the witness ranges (bytes vs cap):\n{stderr}"
    );
}

// Native-check witnesses: IKOS already records operand intervals for
// buffer-overflow; db.rs surfaces them instead of the bare SSA operand name.
#[test]
fn test_verify_boa_witness_ranges() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let (ok, output) = bml_verify("verify_boa_oob.bml");
    assert!(
        !ok && output.contains("offset in [") && output.contains("access_size in ["),
        "the V100 message should carry the offset/access_size ranges:\n{output}"
    );
}

// Transfer-extent obligation (`extent_by`): arming the agent within the
// delivered buffer is proven; arming past it is a DEFINITE assert error
// (both sides constant: count*scale vs sizeof of the delivered static).
#[test]
fn test_verify_extent_ok() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_extent.target");
    let (ok, _stdout, stderr) = bml_verify_args("verify_extent_ok.bml", &["--target", &target]);
    assert!(
        ok && !stderr.contains("[V200]"),
        "an in-bounds extent must verify clean; stderr:\n{stderr}"
    );
}

#[test]
fn test_verify_extent_overrun_rejected() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_extent.target");
    let (ok, _stdout, stderr) = bml_verify_args("verify_extent_over.bml", &["--target", &target]);
    assert!(!ok, "an extent past the buffer must fail verify:\n{stderr}");
    assert!(
        stderr.contains("error[assert]"),
        "expected a definite extent violation:\n{stderr}"
    );
}

// Interior-pointer handoff (&DESC + 16): provable only because the provenance
// assume is tightened by the static's size (base <= block_end - sizeof).
#[test]
fn test_verify_handoff_offset_ok() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) =
        bml_verify_args("verify_handoff_offset.bml", &["--target", &target]);
    assert!(
        ok && !stderr.contains("[V200]"),
        "an interior offset within the static must be provable; stderr:\n{stderr}"
    );
}

// One past the static (&DESC + 32): the base may sit at block_end - 32, so the
// offset can land exactly on the block end -- unproven (V200 warning).
#[test]
fn test_verify_handoff_offset_oob_unproven() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (_ok, _stdout, stderr) =
        bml_verify_args("verify_handoff_offset_oob.bml", &["--target", &target]);
    assert!(
        stderr.contains("[V200]"),
        "an offset past the static must stay unproven; stderr:\n{stderr}"
    );
}

// Handing an address outside the agent's reach (a DTCM address, below the
// sram1-only reach) violates the reachability assert: IKOS reports a definite
// assert violation and verify fails. This is the DTCM footgun caught at the
// value level, complementing the placement-level checks (slices 0-1).
#[test]
fn test_verify_handoff_unreachable_addr() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _stdout, stderr) = bml_verify_args("verify_handoff_bad.bml", &["--target", &target]);
    assert!(
        !ok,
        "an out-of-reach handoff address should fail; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("error[assert]"),
        "expected a definite assert violation; stderr:\n{stderr}"
    );
}

// An address with no provable bound (a havoced volatile read) is reported as a
// warning, not an error or a silent pass -- the "unproven = warning" rung of
// the severity mapping. Default `--fail-on error` lets it pass; raising the bar
// to `warning` makes the same finding fail.
#[test]
fn test_verify_handoff_unproven_warns() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok_default, _o, stderr) =
        bml_verify_args("verify_handoff_unproven.bml", &["--target", &target]);
    assert!(
        stderr.contains("warning[assert]") && !stderr.contains("error[assert]"),
        "an unbounded handoff value should warn, not error:\n{stderr}"
    );
    assert!(
        ok_default,
        "a warning should not fail the default --fail-on error"
    );

    let (ok_strict, _o2, _e2) = bml_verify_args(
        "verify_handoff_unproven.bml",
        &["--target", &target, "--fail-on", "warning"],
    );
    assert!(
        !ok_strict,
        "--fail-on warning should fail on the unproven assert"
    );
}

// ─── in-memory handoffs (addr-typed fields, slice 6) ───────────────────────

// An `addr in R` struct field naming a region the target does not define is
// rejected (E607) -- otherwise the in-memory handoff obligation is silently
// skipped.
#[test]
fn test_addr_field_unknown_region() {
    let (ok, stderr) =
        bml_build_with_target("addr_unknown_region.bml", Some("verify_handoff.target"));
    assert!(
        !ok,
        "addr field in an unknown region should fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E607"), "expected E607; stderr:\n{stderr}");
}

// A buffer address (provably in the field's region) written into an `addr in R`
// descriptor field through a helper discharges the in-memory handoff
// obligation. Exercises the array-of-structs descriptor shape.
#[test]
fn test_addr_field_handoff_ok() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _o, stderr) = bml_verify_args("addr_handoff.bml", &["--target", &target]);
    assert!(
        !stderr.contains("ikos failed"),
        "verify pipeline error:\n{stderr}"
    );
    assert!(
        !stderr.contains("error[assert]"),
        "a buffer in the field's region should discharge:\n{stderr}"
    );
    assert!(ok, "expected a clean verify exit; stderr:\n{stderr}");
}

// Storing an out-of-region address (DTCM, outside dma_shared) into an `addr in
// dma_shared` field violates the in-memory handoff obligation: a definite
// assert error.
#[test]
fn test_addr_field_handoff_out_of_region() {
    if !ikos_available() {
        eprintln!("skipping verify test (set BML_IKOS_BIN)");
        return;
    }
    let target = fixture_target("verify_handoff.target");
    let (ok, _o, stderr) = bml_verify_args("addr_handoff_bad.bml", &["--target", &target]);
    assert!(
        !ok,
        "an out-of-region addr-field write should fail; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("error[assert]"),
        "expected a definite assert violation; stderr:\n{stderr}"
    );
}

// ─── transitive reach (E608) ───────────────────────────────────────────────

// Delivering a descriptor to an agent (`agent_handoff = &RX`) whose field is
// `addr in R` for a region the agent cannot reach is rejected (E608). This is
// the step past E607 (field names a real region) and validate_regions (the
// descriptor's own region is reachable): the field points into a *different*
// region outside the walking agent's reach.
#[test]
fn test_descriptor_field_unreachable_region() {
    let (ok, stderr) = bml_build_with_target("reach_handoff_bad.bml", Some("reach_handoff.target"));
    assert!(
        !ok,
        "a descriptor field in an unreachable region should fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E608"), "expected E608; stderr:\n{stderr}");
}

// The same delivery is sound when the descriptor's `addr in R` field names a
// region the agent reaches: no E608, clean build.
#[test]
fn test_descriptor_field_reachable_region() {
    let (ok, stderr) = bml_build_with_target("reach_handoff_ok.bml", Some("reach_handoff.target"));
    assert!(
        ok,
        "a reachable descriptor field should build; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("E608"),
        "no E608 expected for a reachable field; stderr:\n{stderr}"
    );
}

// ─── agent enable presence (E609) ──────────────────────────────────────────

// Clock-gate-before-touch: programming an agent (writing its handoff register)
// without ever setting the agent's `enabled_by` clock gate is rejected (E609) --
// the writes would hit a gated peripheral and vanish.
#[test]
fn test_agent_enable_missing() {
    let (ok, stderr) = bml_build_with_target("enable_missing.bml", Some("enable.target"));
    assert!(
        !ok,
        "programming an agent without enabling it should fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E609"), "expected E609; stderr:\n{stderr}");
}

// Setting the enable register before programming the agent satisfies the check:
// no E609, clean build.
#[test]
fn test_agent_enable_present() {
    let (ok, stderr) = bml_build_with_target("enable_ok.bml", Some("enable.target"));
    assert!(ok, "an enabled agent should build; stderr:\n{stderr}");
    assert!(
        !stderr.contains("E609"),
        "no E609 expected when the enable is set; stderr:\n{stderr}"
    );
}

// An `enabled_by` path that names no real register/field is itself an E609 --
// otherwise the presence check would be silently vacuous.
#[test]
fn test_agent_enable_unknown_register() {
    // Distinct source from enable_ok.bml: two tests building the same fixture
    // would race on its .o/.ll/.ld artifacts under parallel `cargo test`.
    let (ok, stderr) = bml_build_with_target("enable_badref.bml", Some("enable_badref.target"));
    assert!(
        !ok,
        "an unresolved enabled_by should fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E609"), "expected E609; stderr:\n{stderr}");
}

// ─── agent clock-stomp guard (E610) ────────────────────────────────────────

// A module that does not own the agent disables one of its `enabled_by` clock
// gates -> E610: a stranger gating an agent's clock off would silently stop it.
#[test]
fn test_agent_clock_stomp_rejected() {
    let (ok, stderr) = bml_build_with_target("clock_stomp.bml", Some("enable.target"));
    assert!(
        !ok,
        "disabling an agent's clock from a non-owner should fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E610"), "expected E610; stderr:\n{stderr}");
}

// The module that owns the agent may gate its own clock (deinit/reset) -> no
// E610. Falsification guard: the check must not fire on the legitimate owner.
#[test]
fn test_agent_clock_owner_may_disable() {
    let (ok, stderr) = bml_build_with_target("clock_stomp_owner_ok.bml", Some("enable.target"));
    assert!(ok, "the owner may gate its own clock; stderr:\n{stderr}");
    assert!(
        !stderr.contains("E610"),
        "no E610 expected for the owning module; stderr:\n{stderr}"
    );
}

// ─── alignment-as-derived-physics ──────────────────────────────────────────

// A static placed in a cacheable region shared with a non-coherent DMA agent
// (cortex-m7) is emitted with the 32-byte cache-line alignment, with no @align
// in source; a non-region static keeps the default. The number is physics
// derived from the target, not a per-static literal. See
// target.rs::region_alignments.
#[test]
fn test_region_alignment_derived_in_ir() {
    let ir = bml_ir_with_target("align_derive.bml", Some("verify_handoff.target"));
    assert!(
        ir.contains(
            "@DBUF = global [64 x i8] zeroinitializer, section \".region.dma_shared\", align 32"
        ),
        "DBUF should derive align 32 from the region; ir:\n{ir}"
    );
    assert!(
        ir.contains("@NORMAL = global [64 x i8] zeroinitializer, align 4"),
        "NORMAL (no region) should keep the default align 4; ir:\n{ir}"
    );
}

// ─── @dma read protection, derived from agent-shared placement ─────────────

// @dma's load-bearing property: a @dma array may be index-assigned but its
// rvalue index-read is rejected (E326), so software cannot alias memory it has
// handed to an agent. Derived-Move must reproduce this from placement alone.
#[test]
fn test_dma_array_rvalue_index_read_rejected() {
    let (ok, stderr) = bml_build_with_target("dma_index_read.bml", None);
    assert!(
        !ok,
        "a @dma rvalue index-read should be rejected; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E326"), "expected E326; stderr:\n{stderr}");
}

// The index-read protection extends to views: a plain `view(x)` over agent-shared
// memory is rejected (E335), closing the silent bypass of E326. `reclaim(x)` is
// the explicit, handshake-acknowledged escape.
assert_error!(
    test_view_over_agent_shared_rejected,
    "view_agent_shared.bml",
    "E335"
);

// `reclaim(x)` requires agent-shared memory; reclaiming a plain array is E335.
assert_error!(
    test_reclaim_requires_agent_shared,
    "reclaim_plain_array.bml",
    "E335"
);

// Sound-reclaim guard (B v0, E611): when the agent declares a `completes_by`
// flag, a `reclaim` gated on it (`if DMAC.SR.DONE { reclaim }`) is accepted, but
// an unguarded one is rejected -- the CPU could read mid-transfer.
#[test]
fn test_reclaim_guarded_ok() {
    let (ok, stderr) = bml_build_with_target("reclaim_guarded.bml", Some("reclaim_guard.target"));
    assert!(ok, "a guarded reclaim should build; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_unguarded_rejected() {
    let (ok, stderr) = bml_build_with_target("reclaim_unguarded.bml", Some("reclaim_guard.target"));
    assert!(
        !ok,
        "an unguarded reclaim should fail when completes_by is declared; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

// B broadening: a reclaim guarded by a completion-predicate helper
// (`if done() { reclaim }`, where `done` returns the flag) is accepted
// inter-procedurally.
#[test]
fn test_reclaim_guarded_helper_ok() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_guarded_helper.bml", Some("reclaim_guard.target"));
    assert!(
        ok,
        "a helper-guarded reclaim should build; stderr:\n{stderr}"
    );
}

// Soundness of the broadening: a guard calling a function that does NOT return
// the flag is not a completion predicate -> still E611.
#[test]
fn test_reclaim_nonpredicate_guard_rejected() {
    let (ok, stderr) = bml_build_with_target(
        "reclaim_guard_nonpredicate.bml",
        Some("reclaim_guard.target"),
    );
    assert!(
        !ok,
        "a non-predicate guard should not satisfy E611; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

// Cross-core sharing (E615): a second cpu agent with a declared entry runs
// our code on another core; a mutable static reachable from both cores is
// rejected (per-core masks provide no cross-core exclusion), partitioned
// data is fine, and @shared does not exempt.
#[test]
fn test_cross_core_static_rejected() {
    let (ok, stderr) = bml_build_with_target("cross_core_static.bml", Some("cross_core.target"));
    assert!(!ok, "a cross-core static must fail; stderr:\n{stderr}");
    assert!(stderr.contains("E615"), "expected E615; stderr:\n{stderr}");
}

#[test]
fn test_cross_core_partitioned_ok() {
    let (ok, stderr) =
        bml_build_with_target("cross_core_partitioned.bml", Some("cross_core.target"));
    assert!(
        ok,
        "partitioned per-core statics should build; stderr:\n{stderr}"
    );
}

#[test]
fn test_cross_core_shared_rejected() {
    let (ok, stderr) = bml_build_with_target("cross_core_shared.bml", Some("cross_core.target"));
    assert!(
        !ok,
        "@shared cannot exclude across cores and must be E615; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E615"), "expected E615; stderr:\n{stderr}");
}

// Cross-core claim (spinlock-backed): with spinlock physics declared, a
// cross-core @shared static is legal IFF every access sits inside a claim
// window; the lowering spin-acquires the assigned hardware lock.
#[test]
fn test_cross_core_locked_ok_and_lowering() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let path = dir.join("cross_core_locked.bml");
    let out_dir = unique_out_dir("cross_core_locked.bml");
    let out = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--target")
        .arg(dir.join("cross_core_locks.target"))
        .arg(&path)
        .output()
        .expect("failed to run bml build");
    let ll = std::fs::read_to_string(out_artifact(&out_dir, "cross_core_locked.bml", "ll"))
        .unwrap_or_default();
    let _ = std::fs::remove_dir_all(&out_dir);
    assert!(
        out.status.success(),
        "claimed cross-core @shared should build:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Spin-acquire load of SPINLOCK0 (0xD0000100 = 3489661184) + release.
    assert!(
        ll.contains("load volatile i32, ptr inttoptr (i32 3489661184 to ptr)"),
        "missing spinlock acquire:\n{ll}"
    );
    assert!(
        ll.contains("store volatile i32 1, ptr inttoptr (i32 3489661184 to ptr)"),
        "missing spinlock release:\n{ll}"
    );
}

#[test]
fn test_cross_core_unclaimed_rejected() {
    let (ok, stderr) =
        bml_build_with_target("cross_core_unclaimed.bml", Some("cross_core_locks.target"));
    assert!(!ok, "bare cross-core access must fail; stderr:\n{stderr}");
    assert!(stderr.contains("E615"), "expected E615; stderr:\n{stderr}");
    assert!(
        stderr.contains("claim"),
        "should point at claim; stderr:\n{stderr}"
    );
}

#[test]
fn test_cross_core_no_spinlocks_rejected() {
    // Same claimed program, but the target declares no spinlock physics.
    let (ok, stderr) =
        bml_build_with_target("cross_core_locked_nophys.bml", Some("cross_core.target"));
    assert!(
        !ok,
        "cross-core @shared without spinlock physics must fail; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("spinlock"),
        "should mention spinlocks; stderr:\n{stderr}"
    );
}

// PMSAv8 MPU emission (cortex-m33): MAIR0 = 0x44 at 0xE000EDC0, RBAR =
// base|SH=00|AP=01|XN=1, RLAR = limit|EN -- and a non-power-of-two region
// size is legal (32-byte granularity).
#[test]
fn test_pmsa8_mpu_emission() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures");
    let path = dir.join("pmsa8_mpu.bml");
    let out_dir = unique_out_dir("pmsa8_mpu.bml");
    let out = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--target")
        .arg(dir.join("pmsa8_mpu.target"))
        .arg(&path)
        .output()
        .expect("failed to run bml build");
    let ll =
        std::fs::read_to_string(out_artifact(&out_dir, "pmsa8_mpu.bml", "ll")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&out_dir);
    assert!(
        out.status.success(),
        "build failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // MAIR0 (0xE000EDC0 = 3758157248) = 68 (0x44, Normal non-cacheable).
    assert!(
        ll.contains("store volatile i32 68, ptr inttoptr (i32 3758157248 to ptr)"),
        "missing MAIR0 write:\n{ll}"
    );
    // RBAR = 0x20080000 | 0b011 = 537395203.
    assert!(
        ll.contains("store volatile i32 537395203"),
        "missing RBAR write:\n{ll}"
    );
    // RLAR = (0x20080000 + 12K - 32) | EN = 0x20082FE1 = 537407457.
    assert!(
        ll.contains("store volatile i32 537407457"),
        "missing RLAR write:\n{ll}"
    );
}

// Inverted-polarity gates and completion flags (`!` prefix): RP2350-class
// physics where the enable is CLEAR-to-enable (RESETS) and the completion
// signal is busy-HIGH (CTRL.BUSY).
#[test]
fn test_enable_inverted_ok() {
    let (ok, stderr) =
        bml_build_with_target("enable_inverted_ok.bml", Some("enable_inverted.target"));
    assert!(
        ok,
        "clearing the inverted gate should satisfy E609; stderr:\n{stderr}"
    );
}

#[test]
fn test_enable_inverted_missing_rejected() {
    let (ok, stderr) = bml_build_with_target(
        "enable_inverted_missing.bml",
        Some("enable_inverted.target"),
    );
    assert!(
        !ok,
        "never clearing the inverted gate is E609; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E609"), "expected E609; stderr:\n{stderr}");
}

#[test]
fn test_clock_stomp_inverted_rejected() {
    let (ok, stderr) =
        bml_build_with_target("clock_stomp_inverted.bml", Some("enable_inverted.target"));
    assert!(
        !ok,
        "a stranger re-asserting an inverted gate is E610; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E610"), "expected E610; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_waitset_ok() {
    let (ok, stderr) = bml_build_with_target("reclaim_waitset.bml", Some("reclaim_busy.target"));
    assert!(
        ok,
        "wait-while-set on a busy-high flag should build; stderr:\n{stderr}"
    );
}

#[test]
fn test_reclaim_busy_wrongform_rejected() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_busy_wrongform.bml", Some("reclaim_busy.target"));
    assert!(
        !ok,
        "reclaiming while the busy flag is SET is the unsafe direction; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

// `claim X { ... }` (unification: the masked window, CPU-side reclaim): one
// cpsid/cpsie pair for the whole block, views/index-reads of the claimed
// @shared static allowed inside, escapes and calls rejected (E614).
assert_pass!(test_claim_view_ok, "claim_view.bml");
assert_error!(
    test_claim_not_shared_rejected,
    "claim_not_shared.bml",
    "E614"
);
assert_error!(test_claim_call_rejected, "claim_call.bml", "E614");
assert_error!(test_claim_return_rejected, "claim_return.bml", "E614");
assert_error!(test_claim_break_rejected, "claim_break.bml", "E614");
assert_ir_contains!(
    test_claim_emits_window,
    "claim_view.bml",
    "asm sideeffect \"msr basepri_max, $0\", \"r,~{memory}\"(i32 32)"
);

// Pointer-call context closure: a stored function pointer travels
// invisibly, so address-taken Any fns inherit the contexts of every
// indirect-call site; declared core entries are exempt from E408.
assert_error!(test_ctx_ptr_launder_rejected, "ctx_ptr_launder.bml", "E404");
assert_pass!(test_ctx_ptr_thread_only_ok, "ctx_ptr_thread_ok.bml");

#[test]
fn test_entry_ctx_address_of_ok() {
    let (ok, stderr) = bml_build_with_target("entry_ctx_ok.bml", Some("entry_ctx.target"));
    assert!(
        ok,
        "taking a declared entry's address must be exempt from E408; stderr:\n{stderr}"
    );
}

// Call-graph context propagation (unification U3): an `Any` fn runs in its
// callers' contexts, so the Any hop no longer launders ISR access past
// E404/E402 or hides accessors from the derived ceiling.
assert_error!(test_ctx_launder_isr_rejected, "ctx_launder_isr.bml", "E404");
assert_pass!(test_ctx_launder_thread_only_ok, "ctx_launder_ok.bml");
assert_error!(
    test_ctx_launder_shared_pin_rejected,
    "ctx_launder_shared_pin.bml",
    "E402"
);
assert_ir_contains!(
    test_shared_derived_propagated_cs,
    "shared_derived_propagated.bml",
    "asm sideeffect \"msr basepri_max, $0\", \"r,~{memory}\"(i32 16)"
);

#[test]
fn test_region_isr_launder_rejected() {
    let (ok, stderr) =
        bml_build_with_target("region_isr_launder.bml", Some("reclaim_guard.target"));
    assert!(
        !ok,
        "ISR-vs-thread consumption of agent-shared memory through an Any helper \
         must be rejected; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E404"), "expected E404; stderr:\n{stderr}");
}

#[test]
fn test_shared_in_region_composed_ok() {
    // The fold: @shared + in <region> composes -- claim (CPU window) wrapping
    // a completion-guarded reclaim (agent window), from both contexts.
    let (ok, stderr) = bml_build_with_target("shared_in_region.bml", Some("reclaim_guard.target"));
    assert!(
        ok,
        "the composed claim+reclaim consumption should build; stderr:\n{stderr}"
    );
}

#[test]
fn test_shared_in_region_noclaim_rejected() {
    let (ok, stderr) =
        bml_build_with_target("shared_in_region_noclaim.bml", Some("reclaim_guard.target"));
    assert!(
        !ok,
        "reclaim of a @shared region static outside claim must fail; stderr:\n{stderr}"
    );
    assert!(
        stderr.contains("claim"),
        "the error should point at `claim`; stderr:\n{stderr}"
    );
}

// Per-core NVIC: a labeled ISR runs on the core(s) whose code writes its
// ISER bit (banked NVIC). The same program flips between E615 (enable in
// the other core's entry) and legal (enable on core0) on that fact alone.
#[test]
fn test_percore_isr_cross_rejected() {
    let (ok, stderr) = bml_build_with_target("percore_isr_cross.bml", Some("percore_isr.target"));
    assert!(
        !ok,
        "a core1-enabled ISR sharing a static with core0 must be cross-core; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E615"), "expected E615; stderr:\n{stderr}");
}

#[test]
fn test_percore_isr_core0_ok() {
    let (ok, stderr) =
        bml_build_with_target("percore_isr_core0_ok.bml", Some("percore_isr.target"));
    assert!(
        ok,
        "the same ISR enabled from core0 stays in the ceiling protocol; stderr:\n{stderr}"
    );
}

// The entry PROLOGUE grounds the banked NVIC IPRs (a secondary core never
// runs the reset handler): the same IPR store appears inside side_main.
#[test]
fn test_percore_entry_prologue_iprs() {
    let ir = bml_ir_with_target("percore_isr_core0_ok.bml", Some("percore_isr.target"));
    let entry_body = ir
        .split("define void @side_main")
        .nth(1)
        .expect("side_main in IR");
    let entry_body = &entry_body[..entry_body.find("\n}").unwrap_or(entry_body.len())];
    assert!(
        entry_body.contains("store volatile i8 32, ptr inttoptr (i32 3758154757 to ptr)"),
        "expected the IRQ5 IPR store in the entry prologue:\n{entry_body}"
    );
}

// S3 decoupling: a user reset_handler skips the generated startup, but a
// declared core entry must STILL ground the banked NVIC in its prologue --
// the priority sets are populated on the emitter unconditionally, not only
// when the reset handler is generated.
#[test]
fn test_percore_entry_prologue_iprs_user_reset() {
    let ir = bml_ir_with_target("percore_user_reset.bml", Some("percore_isr.target"));
    let entry_body = ir
        .split("define void @side_main")
        .nth(1)
        .expect("side_main in IR");
    let entry_body = &entry_body[..entry_body.find("\n}").unwrap_or(entry_body.len())];
    assert!(
        entry_body.contains("store volatile i8 32, ptr inttoptr (i32 3758154757 to ptr)"),
        "entry prologue must ground IRQ5 IPR even under a user reset:\n{entry_body}"
    );
    // The user reset handler is used directly: no auto-generated startup.
    assert!(
        !ir.contains("@_sidata = external"),
        "user reset handler must replace the generated startup\n--- IR ---\n{ir}\n---"
    );
}

// E611 precision pair: compared guard forms (`== true`, `== false`,
// `while F == false {}`) and the staleness rule (a second observation of a
// consumed flag after a re-arm needs a clearing write in between).
#[test]
fn test_reclaim_cmp_eq_ok() {
    let (ok, stderr) = bml_build_with_target("reclaim_cmp_eq.bml", Some("reclaim_release.target"));
    assert!(ok, "`== true` is a valid guard; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_cmp_blocking_ok() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_cmp_blocking.bml", Some("reclaim_release.target"));
    assert!(
        ok,
        "`while F == false {{}}` is a valid acquire; stderr:\n{stderr}"
    );
}

#[test]
fn test_reclaim_cmp_wrongpol_rejected() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_cmp_wrongpol.bml", Some("reclaim_release.target"));
    assert!(!ok, "`== false` guards the CLEAR state; stderr:\n{stderr}");
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_stale_rejected() {
    let (ok, stderr) = bml_build_with_target("reclaim_stale.bml", Some("reclaim_release.target"));
    assert!(
        !ok,
        "re-observing a consumed flag after re-arm without clearing must fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_stale_cleared_ok() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_stale_cleared.bml", Some("reclaim_release.target"));
    assert!(
        ok,
        "clearing the flag between observations is the sound idiom; stderr:\n{stderr}"
    );
}

// Per-buffer flag association: a direct delivery (`P.R = &BUF`) binds the
// buffer to that register's channel, so its reclaim is guarded by THAT
// channel's flags; indirect deliveries keep the conservative region union.
// A store to a DECLARED handoff register is followed by a completion
// barrier (`dsb`): arming an agent is a posted Device write, and one left
// in flight while the bus stays busy was an observed imprecise-BusFault
// source on real silicon (H723 ETH tail pointers). Ordering (`dmb`) is not
// enough. Non-handoff register writes get no barrier.
#[test]
fn test_handoff_store_emits_dsb() {
    let ir = bml_ir_with_target("chan_assoc_ok.bml", Some("chan_assoc.target"));
    let store_then_dsb = "to ptr)\n  call void asm sideeffect \"dsb\", \"~{memory}\"()";
    assert!(
        ir.contains(store_then_dsb),
        "expected dsb right after the handoff register store:\n{ir}"
    );
}

// Reset handler initializes .data/.bss WORD-wise (the .ld ALIGN(4)s the
// bounds): byte-wise init RMWs ECC-uninitialized words on ECC RAMs
// (STM32H7 RAMECC) and latches noise error flags. Also: with no agents
// declared there are no handoff completion barriers (no dsb at all).
#[test]
fn test_reset_word_init_and_no_dsb() {
    let ir = bml_ir("reset_word_init.bml");
    assert!(
        ir.contains("store volatile i32 0, ptr %") && !ir.contains("load volatile i8, ptr"),
        "expected word-wise .data/.bss init in reset_handler:\n{ir}"
    );
    assert!(
        !ir.contains("\"dsb\""),
        "no-agent build must not emit completion barriers:\n{ir}"
    );
}

// Agent-pointer volatile lowering + E620 (the H723 hoisted-spin finding):
// accesses through a raw pointer into agent-shared memory are volatile --
// the agent is a concurrent writer the optimizer cannot see -- and such a
// pointer must not escape the function that derived it (outside, the
// taint is invisible and the volatile lowering is silently lost).
#[test]
fn test_agent_ptr_volatile_lowering() {
    let ir = bml_ir_with_target("agent_ptr_volatile.bml", Some("reclaim_guard.target"));
    assert!(
        ir.contains("store volatile i32 %"),
        "store through agent pointer must be volatile:\n{ir}"
    );
    assert!(
        ir.contains("load volatile i32, ptr %"),
        "loads through agent pointer (the OWN spin) must be volatile:\n{ir}"
    );
}

#[test]
fn test_plain_ptr_stays_nonvolatile() {
    let ir = bml_ir("plain_ptr_no_volatile.bml");
    assert!(
        ir.contains("load i16, ptr %") && !ir.contains("load volatile i16"),
        "pointer into plain RAM must stay non-volatile:\n{ir}"
    );
    assert!(
        !ir.contains("store volatile i16"),
        "store through plain pointer must stay non-volatile:\n{ir}"
    );
}

// A write to a register holding a declared clock gate is followed by a
// derived volatile read-back (write-propagation: the first write to the
// newly-clocked peripheral is otherwise droppable). Non-gate registers
// get no read-back.
#[test]
fn test_gate_write_readback() {
    let ir = bml_ir_with_target("gate_readback.bml", Some("enable.target"));
    // Clk.CR = 0x40021000 = 1073876992. The field RMW already loads the
    // register once BEFORE the store; the derived read-back is a second
    // volatile load AFTER it.
    let gate_load = "load volatile i32, ptr inttoptr (i32 1073876992 to ptr)";
    // On this target the b1 gate write lowers through the bit-band alias
    // (0x42420000 = 1111621632); the read-back targets the CANONICAL
    // register address either way.
    let store = "ptr inttoptr (i32 1111621632 to ptr)";
    let store_at = ir.find(store).unwrap_or_else(|| {
        panic!("expected gate store in IR:\n{ir}");
    });
    assert!(
        ir[store_at..].contains(gate_load),
        "expected volatile read-back AFTER the gate store:\n{ir}"
    );
    // The handoff register write (MyDma.DESCADDR, 0x40030000 = 1073938432)
    // is not a gate: no read-back of it anywhere.
    assert!(
        !ir.contains("load volatile i32, ptr inttoptr (i32 1073938432 to ptr)"),
        "non-gate register must not get a read-back:\n{ir}"
    );
}

#[test]
fn test_agent_ptr_escape_arg_rejected() {
    let (ok, stderr) =
        bml_build_with_target("agent_ptr_escape_arg.bml", Some("reclaim_guard.target"));
    assert!(
        !ok && stderr.contains("E620"),
        "expected E620; stderr:\n{stderr}"
    );
}

#[test]
fn test_agent_ptr_escape_return_rejected() {
    let (ok, stderr) =
        bml_build_with_target("agent_ptr_escape_return.bml", Some("reclaim_guard.target"));
    assert!(
        !ok && stderr.contains("E620"),
        "expected E620; stderr:\n{stderr}"
    );
}

#[test]
fn test_agent_ptr_escape_asm_rejected() {
    let (ok, stderr) =
        bml_build_with_target("agent_ptr_escape_asm.bml", Some("reclaim_guard.target"));
    assert!(
        !ok && stderr.contains("E620"),
        "expected E620; stderr:\n{stderr}"
    );
}

#[test]
fn test_chan_assoc_cross_rejected() {
    let (ok, stderr) = bml_build_with_target("chan_assoc_cross.bml", Some("chan_assoc.target"));
    assert!(
        !ok,
        "another channel's flag must not justify the reclaim; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

#[test]
fn test_chan_assoc_own_flag_ok() {
    let (ok, stderr) = bml_build_with_target("chan_assoc_ok.bml", Some("chan_assoc.target"));
    assert!(
        ok,
        "the delivered channel's own flag should build; stderr:\n{stderr}"
    );
}

#[test]
fn test_chan_assoc_indirect_fallback_ok() {
    let (ok, stderr) = bml_build_with_target("chan_assoc_indirect.bml", Some("chan_assoc.target"));
    assert!(
        ok,
        "an indirect delivery keeps the union fallback; stderr:\n{stderr}"
    );
}

// B broadening: blocking-acquire guard forms. The busy-wait (`while !flag {}`,
// empty body) and the early exit (`if !flag { return; }`) establish the flag
// for the rest of the block; misordered or break-capable variants stay E611.
#[test]
fn test_reclaim_busywait_ok() {
    let (ok, stderr) = bml_build_with_target("reclaim_busywait.bml", Some("reclaim_guard.target"));
    assert!(ok, "a busy-wait acquire should build; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_busywait_helper_ok() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_busywait_helper.bml", Some("reclaim_guard.target"));
    assert!(
        ok,
        "a predicate busy-wait acquire should build; stderr:\n{stderr}"
    );
}

#[test]
fn test_reclaim_earlyexit_ok() {
    let (ok, stderr) = bml_build_with_target("reclaim_earlyexit.bml", Some("reclaim_guard.target"));
    assert!(ok, "an early-exit acquire should build; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_busywait_nonempty_body_rejected() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_busywait_body.bml", Some("reclaim_guard.target"));
    assert!(
        !ok,
        "a busy-wait with a body could hide a break; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

// Scoped view lifetimes (E616): the capability a window mints (a view over
// the claimed static, or a guarded reclaim's view) must not outlive its
// justification -- escaping the claim window, leaving the guard span, or
// surviving a release (handoff write) back to the agent.
assert_error!(
    test_claim_view_escape_rejected,
    "claim_view_escape.bml",
    "E616"
);
assert_error!(
    test_claim_view_escape_taint_rejected,
    "claim_view_escape_taint.bml",
    "E616"
);

#[test]
fn test_reclaim_view_escape_rejected() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_view_escape.bml", Some("reclaim_guard.target"));
    assert!(
        !ok,
        "a reclaimed view used past its try-acquire window must be rejected; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E616"), "expected E616; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_after_release_rejected() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_after_release.bml", Some("reclaim_release.target"));
    assert!(
        !ok,
        "a reclaim after the buffer was released back to the agent must be rejected; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_use_after_release_rejected() {
    let (ok, stderr) = bml_build_with_target(
        "reclaim_use_after_release.bml",
        Some("reclaim_release.target"),
    );
    assert!(
        !ok,
        "using the reclaimed view after re-arming the agent must be rejected; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E616"), "expected E616; stderr:\n{stderr}");
}

#[test]
fn test_reclaim_name_reuse_ok() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_name_reuse.bml", Some("reclaim_guard.target"));
    assert!(
        ok,
        "re-using a binding name across windows / rebinding it after the window \
         must not be flagged; stderr:\n{stderr}"
    );
}

#[test]
fn test_reclaim_release_before_guard_ok() {
    let (ok, stderr) = bml_build_with_target(
        "reclaim_release_before_guard.bml",
        Some("reclaim_release.target"),
    );
    assert!(
        ok,
        "arm -> wait -> reclaim -> use is the canonical order and should build; stderr:\n{stderr}"
    );
}

#[test]
fn test_reclaim_before_wait_rejected() {
    let (ok, stderr) =
        bml_build_with_target("reclaim_before_wait.bml", Some("reclaim_guard.target"));
    assert!(
        !ok,
        "a reclaim before the wait is unguarded; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E611"), "expected E611; stderr:\n{stderr}");
}

// Port-select check (E612): a handoff with `port_by F TAG` hands its address to
// an agent whose master port is software-selected (the H7 MDMA's TCM access via
// MDMA_CxTBR.DBUS). The address's mem block, against the agent's tagged bus
// windows, dictates the required state of F.
#[test]
fn test_handoff_port_missing_select_rejected() {
    let (ok, stderr) =
        bml_build_with_target("handoff_port_missing.bml", Some("handoff_port.target"));
    assert!(
        !ok,
        "a tcm-side handoff without the port select set should fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E612"), "expected E612; stderr:\n{stderr}");
}

#[test]
fn test_handoff_port_selected_ok() {
    let (ok, stderr) = bml_build_with_target("handoff_port_ok.bml", Some("handoff_port.target"));
    assert!(
        ok,
        "a tcm-side handoff with the port select set should build; stderr:\n{stderr}"
    );
}

#[test]
fn test_handoff_port_misroute_rejected() {
    let (ok, stderr) =
        bml_build_with_target("handoff_port_misroute.bml", Some("handoff_port.target"));
    assert!(
        !ok,
        "setting the port select for an axi-side address should fail; stderr:\n{stderr}"
    );
    assert!(stderr.contains("E612"), "expected E612; stderr:\n{stderr}");
}

// Derived-Move: the same array placed in an agent-shared region (no `@dma`
// adjective) is wrapped in `Type::AgentShared` at resolution because the region
// has a DMA agent, so the rvalue index-read is rejected with the same E326. The
// protection comes from placement -- usage dictates declaration. See
// region.rs::apply_derived_move.
#[test]
fn test_region_agent_shared_index_read_rejected() {
    let (ok, stderr) =
        bml_build_with_target("region_index_read.bml", Some("verify_handoff.target"));
    assert!(
        !ok,
        "an agent-shared rvalue index-read should be rejected (derived-Move); stderr:\n{stderr}"
    );
    assert!(stderr.contains("E326"), "expected E326; stderr:\n{stderr}");
}

// Control: derived-Move must NOT fire for a CPU-only region -- normal memory,
// freely indexable. The same array placed in `cpu_region` (no DMA/external
// agent) builds and reads fine.
#[test]
fn test_cpu_region_index_read_allowed() {
    let (ok, stderr) =
        bml_build_with_target("cpu_region_index_read.bml", Some("cpu_region.target"));
    assert!(
        ok,
        "a CPU-only region must stay freely indexable; stderr:\n{stderr}"
    );
    assert!(
        !stderr.contains("E326"),
        "no E326 expected for a CPU-only region; stderr:\n{stderr}"
    );
}
