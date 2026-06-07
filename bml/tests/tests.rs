use std::path::PathBuf;
use std::process::Command;

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
    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("build")
        .arg("--opt=0")
        .arg("-g")
        .arg("--save-temps")
        .arg(&path)
        .output()
        .expect("failed to run bml build -g");

    let ll_path = path.with_extension("ll");
    let ir = std::fs::read_to_string(&ll_path).unwrap_or_default();
    let _ = std::fs::remove_file(&ll_path);
    let _ = std::fs::remove_file(path.with_extension("o"));
    let _ = std::fs::remove_file(path.with_extension("ld"));

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

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_bml"));
    cmd.arg("build").arg("--opt=0").arg("--save-temps");
    if let Some(t) = target {
        let tpath = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(t);
        cmd.arg("--target").arg(&tpath);
    }
    cmd.arg(&path);
    let output = cmd.output().expect("failed to run bml build");

    let ll_path = path.with_extension("ll");
    let ir = std::fs::read_to_string(&ll_path).unwrap_or_default();

    let _ = std::fs::remove_file(&ll_path);
    let _ = std::fs::remove_file(path.with_extension("o"));
    let _ = std::fs::remove_file(path.with_extension("ld"));

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
assert_pass!(test_import_selective, "import_selective.bml");
assert_pass!(test_import_alias, "import_alias.bml");
assert_ir_contains!(
    test_import_alias_codegen,
    "import_alias.bml",
    "call i32 @__bml.alias.L.hello()"
);
assert_ir_contains!(
    test_import_alias_internal_codegen,
    "import_alias_internal_codegen.bml",
    "call i32 @__bml.alias.I.helper()"
);
assert_pass!(
    test_import_alias_struct_codegen_check,
    "import_alias_struct_codegen.bml"
);
assert_ir_contains!(
    test_import_alias_struct_codegen,
    "import_alias_struct_codegen.bml",
    "call { i32, i32 } @__bml.alias.S.make_point()"
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
    "call i32 @quux()"
);
assert_pass!(test_import_path, "import_path.bml");
assert_pass!(test_import_path_alias, "import_path_alias.bml");
assert_pass!(test_import_path_selective, "import_path_selective.bml");
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
assert_error!(test_private_access, "private_access.bml", "E503");
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

assert_error!(test_rename_collision, "rename_collision.bml", "E200");
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

// Critical section codegen tests
assert_ir_contains!(
    test_shared_cs_thread,
    "shared_cs_thread.bml",
    "asm sideeffect \"cpsid i\""
);
assert_ir_contains!(
    test_shared_cs_isr_low,
    "shared_cs_isr_low.bml",
    "asm sideeffect \"cpsid i\""
);
assert_ir_not_contains!(
    test_shared_cs_isr_same,
    "shared_cs_isr_same.bml",
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
    // flags) to avoid IKOS DB lock contention when tests run in parallel.
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmpdir = std::env::temp_dir().join(format!("bml_test_{}_{seq}", fixture.replace('.', "_")));
    let _ = std::fs::create_dir_all(&tmpdir);

    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("verify")
        .arg("--ikos-bin")
        .arg(&ikos_bin)
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

    // Clean up temp files from the fixture dir (created by --save-temps)
    let fixture_dir = path.parent().unwrap();
    if let Ok(entries) = fixture_dir.read_dir() {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".verify.ll")
                || name.ends_with(".verify.db")
                || name.ends_with(".verify.json")
                || name.ends_with(".verify.hwaddrs")
            {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    let _ = std::fs::remove_dir_all(&tmpdir);

    (output.status.success(), stdout, stderr)
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
