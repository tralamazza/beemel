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

fn bml_ir(fixture: &str) -> String {
    bml_ir_with_target(fixture, None)
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
assert_error!(test_array_mismatch, "array_mismatch_error.bml", "E313");
assert_error!(
    test_exclusive_unknown,
    "exclusive_unknown_error.bml",
    "E201"
);
assert_error!(test_ptr_mut_val, "ptr_mut_val_error.bml", "E309");
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
    let ir = bml_ir_with_target("startup_basic.bml", Some("stm32f401.target"));
    assert!(
        ir.contains("define void @reset_handler()"),
        "expected auto-generated reset_handler\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("@_sidata = external global i8"),
        "expected .data load symbol\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("@_ebss = external global i8"),
        "expected .bss symbol\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("call void @main()"),
        "expected call to main\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("ptr @reset_handler,\n  ptr"),
        "expected vector table entry\n--- IR ---\n{ir}\n-----------"
    );
}

#[test]
fn test_startup_user_reset() {
    let ir = bml_ir_with_target("startup_user_reset.bml", Some("stm32f401.target"));
    // User-defined reset_handler is used directly, no auto-generated startup symbols
    assert!(
        ir.contains("define void @reset_handler()"),
        "expected user reset_handler\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        !ir.contains("@_sidata = external"),
        "expected NO auto-generated startup\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("ptr @reset_handler,\n  ptr"),
        "expected vector table entry\n--- IR ---\n{ir}\n-----------"
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
    assert!(
        ir.contains("getelementptr [4 x i32], ptr"),
        "expected typed GEP into array\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("load [4 x i32], ptr"),
        "expected array load\n--- IR ---\n{ir}\n-----------"
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
        ir.contains("store i32 %9, ptr %12"),
        "expected element store\n--- IR ---\n{ir}\n-----------"
    );
}

// Bit-band field read: single-bit fields use alias load instead of RMW
#[test]
fn test_bitband_field_read() {
    let ir = bml_ir("bitband_field_read.bml");
    // GPIOA.ODR at 0x40020014, ODR3 bit 3 → alias 0x4200028C = 1111491212
    assert!(
        ir.contains("i32 1111491212 to ptr)"),
        "expected bit-band alias for ODR3\n--- IR ---\n{ir}\n-----------"
    );
    // GPIOA.ODR at 0x40020014, ODR0 bit 0 → alias 0x42000280 = 1111491200
    assert!(
        ir.contains("i32 1111491200 to ptr)"),
        "expected bit-band alias for ODR0\n--- IR ---\n{ir}\n-----------"
    );
    // No masking/shifting in main() -- only the reset_handler uses `and` for .data copy
    let main_end = ir
        .find("define void @default_handler()")
        .unwrap_or(ir.len());
    let main_ir = &ir[..main_end];
    assert!(
        !main_ir.contains("and i32"),
        "expected no masking for bit-band read in main\n--- IR ---\n{ir}\n-----------"
    );
}

// Bit-band field write: single-bit fields use alias store instead of RMW
#[test]
fn test_bitband_field_write() {
    let ir = bml_ir("bitband_field_write.bml");
    // GPIOA.MODER.MODER0 is multi-bit (bit[0..1]), should use RMW (not bit-band)
    // MODER at offset 0x00 → addr 0x40020000 = 1073872896
    assert!(
        ir.contains("load volatile i32, ptr inttoptr (i32 1073872896 to ptr)"),
        "expected RMW load for MODER0\n--- IR ---\n{ir}\n-----------"
    );
    // GPIOA.ODR.ODR3 alias 0x4200028C = 1111491212
    assert!(
        ir.contains("1111491212 to ptr)"),
        "expected bit-band alias for ODR3 store\n--- IR ---\n{ir}\n-----------"
    );
    // GPIOA.ODR.ODR0 alias 0x42000280 = 1111491200
    assert!(
        ir.contains("1111491200 to ptr)"),
        "expected bit-band alias for ODR0 store\n--- IR ---\n{ir}\n-----------"
    );
}

// u8 field: i32 RMW math, trunc on read, zext on write
#[test]
fn test_field_u8_rmw_widths() {
    let ir = bml_ir("peripheral_field_u8.bml");
    assert!(
        ir.contains("trunc i32") && ir.contains(" to i8"),
        "expected i32→i8 trunc on read\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("zext i8") && ir.contains(" to i32"),
        "expected i8→i32 zext on write\n--- IR ---\n{ir}\n-----------"
    );
}

#[test]
fn test_field_u16_rmw_widths() {
    let ir = bml_ir("peripheral_field_u16.bml");
    assert!(
        ir.contains("trunc i32") && ir.contains(" to i16"),
        "expected i32→i16 trunc on read\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("zext i16") && ir.contains(" to i32"),
        "expected i16→i32 zext on write\n--- IR ---\n{ir}\n-----------"
    );
}

#[test]
fn test_field_enum_u8_rmw_widths() {
    let ir = bml_ir("peripheral_field_enum.bml");
    assert!(
        ir.contains("trunc i32") && ir.contains(" to i8"),
        "expected i32→i8 trunc for u8-backed enum read\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("zext i8") && ir.contains(" to i32"),
        "expected i8→i32 zext for u8-backed enum write\n--- IR ---\n{ir}\n-----------"
    );
}

// Multi-bit field range still uses RMW (not bit-band)
#[test]
fn test_bitband_multi_bit_rmw() {
    let ir = bml_ir("peripheral_field_range.bml");
    // MODER at offset 0x00 → addr 0x40020000 = 1073872896
    assert!(
        ir.contains("load volatile i32, ptr inttoptr (i32 1073872896 to ptr)"),
        "expected RMW load\n--- IR ---\n{ir}\n-----------"
    );
    // Range[0..1] mask = 0x3, Range[2..3] inv_mask = 0xFFFFFFF3 = 4294967283
    assert!(
        ir.contains("and i32"),
        "expected masking in RMW\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("or i32"),
        "expected combine in RMW\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        ir.contains("store volatile i32"),
        "expected RMW store\n--- IR ---\n{ir}\n-----------"
    );
}

// @naked function: attribute group #0 (not "interrupt"), no default ret
#[test]
fn test_naked_fn() {
    let ir = bml_ir("naked_fn.bml");
    // naked_fn uses #0 = nounwind, no "interrupt"
    let fn_body = extract_fn_body(&ir, "@naked_fn");
    assert!(
        fn_body.contains("#0 {\n  entry:"),
        "expected attr group #0\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        !fn_body.contains("\"interrupt\""),
        "expected no interrupt attr\n--- IR ---\n{ir}\n-----------"
    );
    // Should contain unreachable (fallback terminator for naked)
    assert!(
        fn_body.contains("unreachable"),
        "expected unreachable terminator\n--- IR ---\n{fn_body}\n-----------"
    );
}

// @naked + @isr: still in vector table, but no interrupt attribute on the fn def
#[test]
fn test_naked_isr() {
    let ir = bml_ir("naked_isr.bml");
    // Check vector table entry
    assert!(
        ir.contains("@naked_isr"),
        "expected naked_isr in vector table\n--- IR ---\n{ir}\n-----------"
    );
    // Check function definition has no interrupt attribute
    let fn_body = extract_fn_body(&ir, "@naked_isr");
    assert!(
        !fn_body.contains("\"interrupt\""),
        "expected no interrupt attr on naked ISR\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        fn_body.contains("#0 {"),
        "expected attr group #0\n--- IR ---\n{ir}\n-----------"
    );
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

// tailchain=true leaf ISR: bx lr, no interrupt attribute, in vector table
#[test]
fn test_tailchain_leaf() {
    let ir = bml_ir("tailchain_leaf.bml");
    // In vector table
    assert!(
        ir.contains("@leaf_isr"),
        "expected leaf_isr in vector table\n--- IR ---\n{ir}\n-----------"
    );
    // Function uses bx lr, not interrupt
    let fn_body = extract_fn_body(&ir, "@leaf_isr");
    assert!(
        fn_body.contains("asm sideeffect \"bx lr\""),
        "expected bx lr in leaf tailchain ISR\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        !fn_body.contains("\"interrupt\""),
        "expected no interrupt attr\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        fn_body.contains("#0 {"),
        "expected attr group #0\n--- IR ---\n{ir}\n-----------"
    );
    // Should NOT have push lr (leaf has no calls)
    assert!(
        !fn_body.contains("push {lr}"),
        "expected no push lr for leaf\n--- IR ---\n{ir}\n-----------"
    );
}

// tailchain=true ISR with calls: push/pop + no interrupt attribute
#[test]
fn test_tailchain_calls() {
    let ir = bml_ir("tailchain_calls.bml");
    // In vector table
    assert!(
        ir.contains("@call_isr"),
        "expected call_isr in vector table\n--- IR ---\n{ir}\n-----------"
    );
    let fn_body = extract_fn_body(&ir, "@call_isr");
    assert!(
        fn_body.contains("push {lr}"),
        "expected push lr in non-leaf tailchain ISR\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        fn_body.contains("pop {pc}"),
        "expected pop pc in non-leaf tailchain ISR\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        !fn_body.contains("\"interrupt\""),
        "expected no interrupt attr\n--- IR ---\n{ir}\n-----------"
    );
    assert!(
        fn_body.contains("#0 {"),
        "expected attr group #0\n--- IR ---\n{ir}\n-----------"
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
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(fixture);

    let ikos_bin = std::env::var("BML_IKOS_BIN").unwrap_or_else(|_| "ikos-analyzer".into());

    // Unique temp dir per fixture to avoid IKOS DB lock contention
    let tmpdir = std::env::temp_dir().join(format!("bml_test_{}", fixture.replace('.', "_")));
    let _ = std::fs::create_dir_all(&tmpdir);

    let output = Command::new(env!("CARGO_BIN_EXE_bml"))
        .arg("verify")
        .arg("--ikos-bin")
        .arg(&ikos_bin)
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!("{stdout}{stderr}");

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

    (output.status.success(), combined)
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
assert_verify_fail!(test_verify_boa_oob, "verify_boa_oob.bml");
assert_verify_fail!(test_verify_dbz, "verify_dbz.bml");
assert_verify_fail!(test_verify_uio, "verify_uio.bml");
assert_verify_pass!(test_verify_no_findings, "verify_no_findings.bml");
// assume_narrows: assume(b != 0) before a/b should prevent dbz
assert_verify_pass!(test_verify_assume_narrows, "verify_assume_narrows.bml");
assert_verify_fail!(test_verify_nullity, "verify_nullity.bml");
assert_verify_pass!(test_verify_global_ref, "verify_global_ref.bml");
assert_verify_fail!(test_verify_unlabeled_isr, "verify_unlabeled_isr.bml");
assert_verify_pass!(test_verify_ptr_u8, "verify_ptr_u8.bml");
assert_verify_pass!(test_verify_ptr_u16, "verify_ptr_u16.bml");
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
