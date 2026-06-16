#[derive(Clone, Copy, PartialEq, Eq)]
enum FailOn {
    Error,
    Warning,
    Info,
    Never,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
}

use bml_core::ast;
use bml_core::borrow::BorrowChecker;
use bml_core::checker::Checker;
use bml_core::errors::DiagnosticBag;
use bml_core::imports::ImportResolver;
use bml_core::ir::IrEmitter;
use bml_core::libpath::assemble_lib_roots;
use bml_core::parser::Parser;
use bml_core::region;
use bml_core::resolver::Resolver;
use bml_core::source::SourceMap;
use bml_core::stack;
use bml_core::target::Target;
use bml_core::verify::{self, VerifyConfig};
use std::path::{Path, PathBuf};
use std::process;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let command = &args[1];
    match command.as_str() {
        "--help" | "-h" => {
            print_usage();
        }
        "check" => {
            let mut stack_analysis = false;
            let mut lib_dirs: Vec<PathBuf> = vec![];
            let mut path: Option<PathBuf> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--stack" => stack_analysis = true,
                    "--lib" => {
                        i += 1;
                        if i < args.len() {
                            lib_dirs.push(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--lib requires a path");
                            process::exit(1);
                        }
                    }
                    other => path = Some(PathBuf::from(other)),
                }
                i += 1;
            }
            let path = path.unwrap_or_else(|| {
                eprintln!("Usage: bml check [--stack] [--lib <dir>]... <file.bml>");
                process::exit(1);
            });
            let lib_roots = assemble_lib_roots(&lib_dirs);
            check_file(&path, stack_analysis, &lib_roots);
        }
        "build" => {
            let mut target_path: Option<PathBuf> = None;
            let mut link_libs: Vec<PathBuf> = vec![];
            let mut lib_dirs: Vec<PathBuf> = vec![];
            let mut source_path: Option<PathBuf> = None;
            let mut out_dir: Option<PathBuf> = None;
            let mut opt_level = "s".to_string();
            let mut save_temps = false;
            let mut debug = false;
            let mut stack_analysis = false;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--target" => {
                        i += 1;
                        if i < args.len() {
                            target_path = Some(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--target requires a path");
                            process::exit(1);
                        }
                    }
                    "--out-dir" => {
                        i += 1;
                        if i < args.len() {
                            out_dir = Some(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--out-dir requires a path");
                            process::exit(1);
                        }
                    }
                    "--link" => {
                        i += 1;
                        if i < args.len() {
                            link_libs.push(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--link requires a path");
                            process::exit(1);
                        }
                    }
                    "--lib" => {
                        i += 1;
                        if i < args.len() {
                            lib_dirs.push(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--lib requires a path");
                            process::exit(1);
                        }
                    }
                    "--save-temps" => {
                        save_temps = true;
                    }
                    "--debug" | "-g" => {
                        debug = true;
                    }
                    "--stack" => {
                        stack_analysis = true;
                    }
                    a if a.starts_with("--opt=") => {
                        let level = &a[6..];
                        if !matches!(level, "0" | "1" | "2" | "3" | "s" | "z") {
                            eprintln!(
                                "Invalid optimization level: `{level}`. Expected 0, 1, 2, 3, s, or z"
                            );
                            process::exit(1);
                        }
                        opt_level = level.to_string();
                    }
                    other => {
                        source_path = Some(PathBuf::from(other));
                    }
                }
                i += 1;
            }

            let source_path = source_path.unwrap_or_else(|| {
                eprintln!(
                    "Usage: bml build [--target <file.target>] [--opt=<level>] [--debug] [--save-temps] [--out-dir <dir>] [--link <lib>]... [--lib <dir>]... [--stack] <file.bml>"
                );
                process::exit(1);
            });

            let lib_roots = assemble_lib_roots(&lib_dirs);
            let target = match &target_path {
                Some(p) => Target::from_file_with_libs(p, &lib_roots).unwrap_or_else(|e| {
                    eprintln!("Error parsing target: {e}");
                    process::exit(1);
                }),
                None => Target::default(),
            };

            build_file(
                &source_path,
                &target,
                &link_libs,
                &opt_level,
                save_temps,
                debug,
                stack_analysis,
                out_dir.as_deref(),
                &lib_roots,
            );
        }
        "verify" => {
            let mut target_path: Option<PathBuf> = None;
            let mut lib_dirs: Vec<PathBuf> = vec![];
            let mut domain = "interval-congruence".to_string();
            let mut checks: Vec<String> = vec![];
            let mut ikos_bin: Option<PathBuf> = None;
            let mut save_temps = false;
            let mut out_dir: Option<PathBuf> = None;
            let mut source_path: Option<PathBuf> = None;
            let mut fail_on = FailOn::Error;
            let mut output_format = OutputFormat::Text;

            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--target" => {
                        i += 1;
                        if i < args.len() {
                            target_path = Some(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--target requires a path");
                            process::exit(1);
                        }
                    }
                    "--out-dir" => {
                        i += 1;
                        if i < args.len() {
                            out_dir = Some(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--out-dir requires a path");
                            process::exit(1);
                        }
                    }
                    "--domain" => {
                        i += 1;
                        if i < args.len() {
                            let raw = args[i].as_str();
                            domain = match raw {
                                "octagon" => "apron-octagon".to_string(),
                                "apron" => "apron-interval".to_string(),
                                other => other.to_string(),
                            };
                        } else {
                            eprintln!("--domain requires a name");
                            process::exit(1);
                        }
                    }
                    "--checks" => {
                        i += 1;
                        if i < args.len() {
                            checks = args[i].split(',').map(String::from).collect();
                        } else {
                            eprintln!("--checks requires a comma-separated list");
                            process::exit(1);
                        }
                    }
                    "--ikos-bin" => {
                        i += 1;
                        if i < args.len() {
                            ikos_bin = Some(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--ikos-bin requires a path");
                            process::exit(1);
                        }
                    }
                    "--lib" => {
                        i += 1;
                        if i < args.len() {
                            lib_dirs.push(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--lib requires a path");
                            process::exit(1);
                        }
                    }
                    "--save-temps" => {
                        save_temps = true;
                    }
                    "--fail-on" => {
                        i += 1;
                        if i < args.len() {
                            fail_on = match args[i].as_str() {
                                "error" => FailOn::Error,
                                "warning" => FailOn::Warning,
                                "info" => FailOn::Info,
                                "never" => FailOn::Never,
                                other => {
                                    eprintln!(
                                        "--fail-on: unknown level `{other}` (expected error, warning, info, never)"
                                    );
                                    process::exit(1);
                                }
                            };
                        } else {
                            eprintln!("--fail-on requires a level");
                            process::exit(1);
                        }
                    }
                    "--format" => {
                        i += 1;
                        if i < args.len() {
                            output_format = match args[i].as_str() {
                                "text" => OutputFormat::Text,
                                "json" => OutputFormat::Json,
                                other => {
                                    eprintln!(
                                        "--format: unknown format `{other}` (expected text, json)"
                                    );
                                    process::exit(1);
                                }
                            };
                        } else {
                            eprintln!("--format requires a name");
                            process::exit(1);
                        }
                    }
                    other => {
                        source_path = Some(PathBuf::from(other));
                    }
                }
                i += 1;
            }

            let source_path = source_path.unwrap_or_else(|| {
                eprintln!("Usage: bml verify [--target <file.target>] [--domain <name>] [--checks <list>] [--ikos-bin <path>] [--fail-on <level>] [--format <fmt>] [--save-temps] [--out-dir <dir>] [--lib <dir>]... <file.bml>");
                process::exit(1);
            });

            let lib_roots = assemble_lib_roots(&lib_dirs);
            let target = match &target_path {
                Some(p) => Target::from_file_with_libs(p, &lib_roots).unwrap_or_else(|e| {
                    eprintln!("Error parsing target: {e}");
                    process::exit(1);
                }),
                None => Target::default(),
            };

            verify_file(
                &source_path,
                &target,
                &domain,
                &checks,
                ikos_bin,
                save_temps,
                fail_on,
                output_format,
                out_dir.as_deref(),
                &lib_roots,
            );
        }
        "cflags" => {
            let mut target_path: Option<PathBuf> = None;
            let mut lib_dirs: Vec<PathBuf> = vec![];
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--target" => {
                        i += 1;
                        if i < args.len() {
                            target_path = Some(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--target requires a path");
                            process::exit(1);
                        }
                    }
                    "--lib" => {
                        i += 1;
                        if i < args.len() {
                            lib_dirs.push(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--lib requires a path");
                            process::exit(1);
                        }
                    }
                    other => {
                        eprintln!("Unknown option for cflags: {other}");
                        process::exit(1);
                    }
                }
                i += 1;
            }
            let target_path = target_path.unwrap_or_else(|| {
                eprintln!("Usage: bml cflags [--lib <dir>]... --target <file.target>");
                process::exit(1);
            });
            let lib_roots = assemble_lib_roots(&lib_dirs);
            let target =
                Target::from_file_with_libs(&target_path, &lib_roots).unwrap_or_else(|e| {
                    eprintln!("{e}");
                    process::exit(1);
                });
            let flags = target.to_gcc_flags().unwrap_or_else(|e| {
                eprintln!("{e}");
                process::exit(1);
            });
            println!("{}", flags.join(" "));
        }
        _ => {
            eprintln!("Unknown command: {command}");
            print_usage();
            process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!("Usage: bml <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  check [--stack] [--lib <dir>]... <file.bml>   Type-check a source file");
    eprintln!("  build [--target <file.target>] [--opt=<level>] [--debug] [--save-temps]");
    eprintln!("        [--out-dir <dir>] [--link <lib>]... [--lib <dir>]... [--stack] <file.bml>");
    eprintln!("                                                 Compile and optionally link");
    eprintln!("  verify [--target <file.target>] [--domain <name>] [--checks <list>]");
    eprintln!("         [--ikos-bin <path>] [--fail-on <level>] [--format <fmt>]");
    eprintln!("         [--save-temps] [--out-dir <dir>] [--lib <dir>]... <file.bml>");
    eprintln!("                                                 Run IKOS static analysis");
    eprintln!("  cflags [--lib <dir>]... --target <file.target> Print arm-none-eabi-gcc flags");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --opt=<level>   Optimization level: 0, 1, 2, 3, s, z (default: s)");
    eprintln!("  --debug, -g     Emit DWARF debug information");
    eprintln!("  --save-temps    Keep intermediate files (file.opt.ll)");
    eprintln!("  --out-dir <dir> Write artifacts to <dir> (created if needed) instead");
    eprintln!("                  of next to the source (build and verify)");
    eprintln!("  --stack         Perform compile-time stack usage analysis");
    eprintln!("  --target <path> Target specification file");
    eprintln!("  --link <lib>    Link with library (.a / .o), repeatable");
    eprintln!("  --lib <dir>     Library search root for target `include`s and source");
    eprintln!("                  `import`s, repeatable. Searched after the including/");
    eprintln!("                  importing file's own directory; also reads $BML_PATH and");
    eprintln!("                  the in-tree lib/ for dev builds");
    eprintln!("  --fail-on <l>   Exit non-zero when any finding meets level: error,");
    eprintln!("                  warning, info, or never (default: error)");
    eprintln!("  --format <fmt>  Output format: text or json (default: text)");
    eprintln!("  --help, -h      Show this help");
}

fn check_file(path: &Path, stack_analysis: bool, lib_roots: &[PathBuf]) {
    let mut source_map = SourceMap::new();
    let mut diags = DiagnosticBag::new();

    let file_id = match source_map.add_file(path.to_path_buf()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("Error reading {}: {e}", path.display());
            process::exit(1);
        }
    };

    let source = source_map.source(file_id);

    let mut parser = Parser::new(source, file_id, &mut diags);
    let program = parser.parse_program();

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Phase 1b -- Import resolution
    let mut import_resolver = ImportResolver::new();
    import_resolver.source_map = source_map;
    import_resolver.lib_roots = lib_roots.to_vec();
    let program = import_resolver.resolve(program, path);
    let source_map = import_resolver.source_map;
    diags.merge(import_resolver.diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Phase 2a -- Name resolution
    // `bml check` runs without a target, so byte-order diagnostics use the
    // default (little-endian) order; `build`/`verify` override it from the target.
    let resolver = Resolver::new();
    let symbols = resolver.resolve(&program, &mut diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Phase 2b -- Type checking
    Checker::check(&program, &symbols, &mut diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Phase 2c -- Borrow enforcement
    BorrowChecker::check(&program, &symbols, &mut diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    if !diags.is_empty() {
        diags.emit(&source_map);
    }

    if stack_analysis && !diags.has_errors() {
        let report = stack::analyze(&program, &symbols);
        stack::print_report(&report, &symbols, 2048);
    }

    println!("--- All checks passed ---");
    println!("Functions: {}", symbols.functions.len());
    println!("Statics:   {}", symbols.statics.len());
    println!("Consts:    {}", symbols.consts.len());
    println!("Peripherals: {}", symbols.peripherals.len());

    for item in &program.items {
        match item {
            ast::Item::FnDef(f) => {
                let ctx_str = if let Some(isr) = &f.isr {
                    if let Some(label) = &isr.label {
                        format!("isr(\"{label}\", prio={})", isr.priority)
                    } else {
                        format!("isr(prio={})", isr.priority)
                    }
                } else {
                    match &f.context {
                        ast::ContextExpr::Thread => "thread".into(),
                        ast::ContextExpr::Any => "any".into(),
                    }
                };
                println!("  fn {} @ {}", f.name.0, ctx_str);
            }
            ast::Item::StaticDef(s) => {
                let anns: Vec<String> = s.storage.iter().map(|a| format!("{a:?}")).collect();
                println!("  static {} : {:?}  [{}]", s.name.0, s.ty, anns.join(", "));
            }
            ast::Item::ConstDef(c) => println!("  const {} : {:?}", c.name.0, c.ty),
            ast::Item::PeripheralDef(p) => {
                println!("  peripheral {} at 0x{:08X}", p.name.0, p.base_addr);
            }
            ast::Item::Import(i) => println!(
                "  import {} (alias)",
                i.module
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(".")
            ),
            ast::Item::ExternFnDef(e) => {
                let ctx_str = if let Some(isr) = &e.isr {
                    if let Some(label) = &isr.label {
                        format!("isr(\"{label}\", prio={})", isr.priority)
                    } else {
                        format!("isr(prio={})", isr.priority)
                    }
                } else if let Some(ctx) = &e.context {
                    match ctx {
                        ast::ContextExpr::Thread => "thread".to_string(),
                        ast::ContextExpr::Any => "any".to_string(),
                    }
                } else {
                    "any".to_string()
                };
                println!("  extern fn {} @ {}", e.name.0, ctx_str);
            }
            ast::Item::StructDef(s) => {
                println!("  struct {} ({} fields)", s.name.0, s.fields.len());
            }
            ast::Item::EnumDef(e) => {
                println!(
                    "  enum {} : {:?} ({} variants)",
                    e.name.0,
                    e.ty,
                    e.variants.len()
                );
            }
            ast::Item::Owns(o) => println!("  owns ({} registers)", o.paths.len()),
            ast::Item::ComptimeAssert(_) => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_file(
    path: &Path,
    target: &Target,
    link_libs: &[PathBuf],
    opt_level: &str,
    save_temps: bool,
    debug: bool,
    stack_analysis: bool,
    out_dir: Option<&Path>,
    lib_roots: &[PathBuf],
) {
    let mut source_map = SourceMap::new();
    let mut diags = DiagnosticBag::new();

    let file_id = match source_map.add_file(path.to_path_buf()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("Error reading {}: {e}", path.display());
            process::exit(1);
        }
    };

    let source = source_map.source(file_id);

    let mut parser = Parser::new(source, file_id, &mut diags);
    let program = parser.parse_program();
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    let mut import_resolver = ImportResolver::new();
    import_resolver.source_map = source_map;
    import_resolver.lib_roots = lib_roots.to_vec();
    let program = import_resolver.resolve(program, path);
    let source_map = import_resolver.source_map;
    diags.merge(import_resolver.diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    let resolver = Resolver::new();
    let mut symbols = resolver.resolve(&program, &mut diags);
    // The target's native byte order drives byte-order field diagnostics (E360).
    symbols.target_endianness = target.to_arch().endianness();
    // Core entry points (E408 address-of exemption for the launch handshake).
    symbols.entry_fns = target
        .agents
        .iter()
        .filter_map(|a| a.entry.clone())
        .collect();
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Derive `@dma`-style index-read protection from agent-shared placement: an
    // array placed in a region a DMA/external agent mutates becomes
    // `Type::AgentShared`, so the E326 read restriction applies without a
    // hand-written `@dma`. Runs on clean resolution, right before the checker.
    region::apply_derived_move(&program, target, &mut symbols);
    Checker::check(&program, &symbols, &mut diags);
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    BorrowChecker::check(&program, &symbols, &mut diags);
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Region/agent placement and ownership checks. These need the target file
    // (regions and agents are declared there), so they run in build/verify, not
    // in the targetless `bml check`.
    region::check(&program, &symbols, target, &mut diags);
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    if !diags.is_empty() {
        diags.emit(&source_map);
    }

    if stack_analysis {
        let report = stack::analyze(&program, &symbols);
        stack::print_report(&report, &symbols, 2048);
    }

    let arch = target.to_arch();
    let triple = target.to_llvm_target_triple();
    let mut emitter = IrEmitter::new(
        arch,
        target.interrupts.clone(),
        target.has_bitband,
        debug,
        Some(source_map),
    );
    emitter.set_startup_init(target.startup_init.clone());
    emitter.set_ecc_scrub_blocks(target.ecc_scrub_blocks());
    emitter.set_handoff_regs(
        target
            .agents
            .iter()
            .flat_map(bml_core::target::Agent::handoffs)
            .map(|h| h.register.clone())
            .collect(),
    );
    emitter.set_enable_gates(
        &target
            .agents
            .iter()
            .flat_map(|a| a.enabled_by.clone())
            .collect::<Vec<_>>(),
    );
    emitter.set_mpu_regions(target.mpu_regions());
    emitter.set_mpu_flavor(target.mpu_flavor());
    emitter.set_cross_core_locks(
        region::cross_core_locks(&program, &symbols, target),
        target.spinlock_base.unwrap_or(0),
    );
    emitter.set_priority_bits(target.priority_bits);
    emitter.set_region_alignments(target.region_alignments());
    let llvm_ir = emitter.emit(&program, &symbols);

    // Artifact basename. With `--out-dir`, the source filename is relocated into
    // that directory (created if needed); the `.with_extension` calls below then
    // produce `<dir>/<stem>.{ll,opt.ll,o,ld,elf}` exactly as they would next to
    // the source. Without it, artifacts land beside the source as before.
    let out_base = match out_dir {
        Some(dir) => {
            std::fs::create_dir_all(dir).unwrap_or_else(|e| {
                eprintln!("Error creating out-dir {}: {e}", dir.display());
                process::exit(1);
            });
            dir.join(path.file_name().unwrap_or_else(|| {
                eprintln!("Error: source path {} has no file name", path.display());
                process::exit(1);
            }))
        }
        None => path.to_path_buf(),
    };

    let ll_path = out_base.with_extension("ll");
    std::fs::write(&ll_path, &llvm_ir).unwrap_or_else(|e| {
        eprintln!("Error writing {}: {e}", ll_path.display());
        process::exit(1);
    });

    let linker_script = target.generate_linker_script();
    let ld_path = out_base.with_extension("ld");
    std::fs::write(&ld_path, linker_script).unwrap_or_else(|e| {
        eprintln!("Error writing {}: {e}", ld_path.display());
        process::exit(1);
    });

    let obj_path = out_base.with_extension("o");

    if opt_level == "0" {
        // ── no optimization: llc only ──
        let llc_status = process::Command::new("llc")
            .args([
                "-O0",
                &format!("-mtriple={triple}"),
                "-filetype=obj",
                "-o",
                obj_path.to_str().unwrap(),
                ll_path.to_str().unwrap(),
            ])
            .status();
        codegen_result(
            llc_status, path, &obj_path, &ll_path, &ld_path, link_libs, triple,
        );
    } else if save_temps {
        // ── file mode: opt → file.opt.ll → llc ──
        let llc_opt = llc_opt_level(opt_level);
        let opt_ll_path = out_base.with_extension("opt.ll");
        let opt_status = process::Command::new("opt")
            .args([
                &format!("--O{opt_level}"),
                "-S",
                ll_path.to_str().unwrap(),
                "-o",
                opt_ll_path.to_str().unwrap(),
            ])
            .status();
        match opt_status {
            Ok(s) if s.success() => {
                println!("Wrote {}", opt_ll_path.display());
            }
            Ok(s) => {
                eprintln!("opt failed with exit code {}", s.code().unwrap_or(1));
                process::exit(1);
            }
            Err(e) => {
                eprintln!("Failed to run opt: {e}");
                eprintln!("  → install LLVM: apt install llvm");
                eprintln!("  → or use --opt=0 to skip optimization");
                process::exit(1);
            }
        }
        let llc_status = process::Command::new("llc")
            .args([
                &format!("-O{llc_opt}"),
                &format!("-mtriple={triple}"),
                "-filetype=obj",
                "-o",
                obj_path.to_str().unwrap(),
                opt_ll_path.to_str().unwrap(),
            ])
            .status();
        codegen_result(
            llc_status, path, &obj_path, &ll_path, &ld_path, link_libs, triple,
        );
    } else {
        // ── pipe mode (default): opt → stdout → llc stdin ──
        let llc_opt = llc_opt_level(opt_level);
        let mut opt_child = match process::Command::new("opt")
            .args([
                &format!("--O{opt_level}"),
                "-S",
                ll_path.to_str().unwrap(),
                "-o",
                "-",
            ])
            .stdout(process::Stdio::piped())
            .stderr(process::Stdio::inherit())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Failed to run opt: {e}");
                eprintln!("  → install LLVM: apt install llvm");
                eprintln!("  → or use --opt=0 to skip optimization");
                process::exit(1);
            }
        };

        let opt_stdout = opt_child.stdout.take().unwrap();
        let llc_status = process::Command::new("llc")
            .args([
                &format!("-O{llc_opt}"),
                &format!("-mtriple={triple}"),
                "-filetype=obj",
                "-o",
                obj_path.to_str().unwrap(),
                "-",
            ])
            .stdin(opt_stdout)
            .status();

        let opt_status = opt_child.wait();

        match opt_status {
            Ok(s) if !s.success() => {
                eprintln!("opt failed with exit code {}", s.code().unwrap_or(1));
                process::exit(1);
            }
            Err(e) => {
                eprintln!("opt failed: {e}");
                process::exit(1);
            }
            _ => {}
        }

        codegen_result(
            llc_status, path, &obj_path, &ll_path, &ld_path, link_libs, triple,
        );
    }
}

/// Map optimization level for llc (which only supports 0-3, not s/z).
#[allow(clippy::too_many_arguments)]
fn verify_file(
    path: &Path,
    target: &Target,
    domain: &str,
    checks: &[String],
    ikos_bin: Option<PathBuf>,
    save_temps: bool,
    fail_on: FailOn,
    output_format: OutputFormat,
    out_dir: Option<&Path>,
    lib_roots: &[PathBuf],
) {
    let mut source_map = SourceMap::new();
    let mut diags = DiagnosticBag::new();

    let file_id = match source_map.add_file(path.to_path_buf()) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("Error reading {}: {e}", path.display());
            process::exit(1);
        }
    };

    let source = source_map.source(file_id);

    let mut parser = Parser::new(source, file_id, &mut diags);
    let program = parser.parse_program();
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    let mut import_resolver = ImportResolver::new();
    import_resolver.source_map = source_map;
    import_resolver.lib_roots = lib_roots.to_vec();
    let program = import_resolver.resolve(program, path);
    let source_map = import_resolver.source_map;
    diags.merge(import_resolver.diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    let resolver = Resolver::new();
    let mut symbols = resolver.resolve(&program, &mut diags);
    // The target's native byte order drives byte-order field diagnostics (E360).
    symbols.target_endianness = target.to_arch().endianness();
    // Core entry points (E408 address-of exemption for the launch handshake).
    symbols.entry_fns = target
        .agents
        .iter()
        .filter_map(|a| a.entry.clone())
        .collect();
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Derive `@dma`-style index-read protection from agent-shared placement: an
    // array placed in a region a DMA/external agent mutates becomes
    // `Type::AgentShared`, so the E326 read restriction applies without a
    // hand-written `@dma`. Runs on clean resolution, right before the checker.
    region::apply_derived_move(&program, target, &mut symbols);
    Checker::check(&program, &symbols, &mut diags);
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    BorrowChecker::check(&program, &symbols, &mut diags);
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Region/agent placement and ownership checks. These need the target file
    // (regions and agents are declared there), so they run in build/verify, not
    // in the targetless `bml check`.
    region::check(&program, &symbols, target, &mut diags);
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    if !diags.is_empty() {
        diags.emit(&source_map);
    }

    let check_list = if checks.is_empty() {
        // `uva` omitted by default; see VerifyConfig::default.
        vec![
            "boa".to_string(),
            "nullity".to_string(),
            "sio".to_string(),
            "uio".to_string(),
            "dbz".to_string(),
            "shc".to_string(),
            "poa".to_string(),
            "upa".to_string(),
            "dca".to_string(),
            "dfa".to_string(),
            "fca".to_string(),
            "prover".to_string(),
        ]
    } else {
        checks.to_vec()
    };

    if cfg!(feature = "ikos-static") && ikos_bin.is_some() {
        eprintln!(
            "warning: this bml links IKOS statically; --ikos-bin is ignored and the analysis runs in-process"
        );
    }
    let ikos_bin = ikos_bin
        .or_else(|| std::env::var_os("BML_IKOS_BIN").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("ikos-analyzer"));

    let config = VerifyConfig {
        ikos_bin,
        domain: domain.to_string(),
        checks: check_list,
        extra_hwaddrs: Vec::new(),
    };

    // Where the `.verify.*` intermediates go. `--out-dir` wins (created if
    // needed, user-owned -- kept); else `--save-temps` keeps them beside the
    // source; else a unique temp dir that is removed afterward. The temp default
    // already isolates concurrent runs, so verify never had the build race.
    let (work_dir, ephemeral) = if let Some(dir) = out_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("failed to create out-dir {}: {e}", dir.display());
            process::exit(1);
        }
        (dir.to_path_buf(), false)
    } else if save_temps {
        (path.parent().unwrap_or(Path::new(".")).to_path_buf(), false)
    } else {
        let unique = format!(
            "bml-verify-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let dir = std::env::temp_dir().join(unique);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("failed to create temp dir {}: {e}", dir.display());
            process::exit(1);
        }
        (dir, true)
    };

    match verify::verify(
        &program,
        &symbols,
        &source_map,
        target,
        &config,
        &work_dir,
        path,
    ) {
        Ok(findings) => {
            emit_findings(&findings, output_format);
            if should_fail(&findings, fail_on) {
                process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("ikos failed: {e}");
            process::exit(1);
        }
    }

    if ephemeral {
        let _ = std::fs::remove_dir_all(&work_dir);
    }
}

fn severity_str(status: bml_core::verify::report::Status) -> &'static str {
    use bml_core::verify::report::Status;
    match status {
        Status::Error => "error",
        Status::Warning => "warning",
        Status::Safe | Status::Unreachable => "info",
    }
}

fn emit_findings(findings: &[bml_core::verify::report::Finding], format: OutputFormat) {
    match format {
        OutputFormat::Text => {
            for f in findings {
                let severity = severity_str(f.status);
                eprintln!("{severity}[{}]: {}", f.check, f.message);
                eprintln!("  \u{2192} {}:{}:{}", f.file.display(), f.line, f.column);
            }
        }
        OutputFormat::Json => {
            // Hand-roll JSON to avoid pulling serde_json into bml; the Finding
            // schema is small and fixed.
            print!("{{\"findings\":[");
            for (i, f) in findings.iter().enumerate() {
                if i > 0 {
                    print!(",");
                }
                let severity = severity_str(f.status);
                print!("{{");
                print!("\"check\":{},", json_string(&f.check));
                print!("\"severity\":{},", json_string(severity));
                print!("\"message\":{},", json_string(&f.message));
                print!("\"file\":{},", json_string(&f.file.display().to_string()));
                print!("\"line\":{},", f.line);
                print!("\"column\":{}", f.column);
                print!("}}");
            }
            println!("]}}");
        }
    }
}

fn should_fail(findings: &[bml_core::verify::report::Finding], fail_on: FailOn) -> bool {
    use bml_core::verify::report::Status;
    let threshold = match fail_on {
        FailOn::Error => 3,
        FailOn::Warning => 2,
        FailOn::Info => 1,
        FailOn::Never => return false,
    };
    findings.iter().any(|f| {
        let rank = match f.status {
            Status::Error => 3,
            Status::Warning => 2,
            Status::Safe | Status::Unreachable => 1,
        };
        rank >= threshold
    })
}

fn json_string(s: &str) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn llc_opt_level(level: &str) -> &str {
    match level {
        "0" => "0",
        "1" => "1",
        "s" | "z" | "2" => "2",
        "3" => "3",
        _ => "2",
    }
}

fn codegen_result(
    llc_status: std::io::Result<process::ExitStatus>,
    path: &Path,
    obj_path: &Path,
    ll_path: &Path,
    ld_path: &Path,
    link_libs: &[PathBuf],
    triple: &str,
) {
    match llc_status {
        Ok(s) if s.success() => {
            println!("Wrote {}", obj_path.display());
            println!("Wrote {}", ll_path.display());
            println!("Wrote {}", ld_path.display());

            if link_libs.is_empty() {
                println!(
                    "  → link with: ld.lld -T {} {} -o output.elf",
                    ld_path.display(),
                    obj_path.display()
                );
            } else {
                // Derive the ELF from the (already relocated) object path so it
                // honors `--out-dir` too.
                let elf_path = obj_path.with_extension("elf");
                let mut cmd = process::Command::new("ld.lld");
                cmd.arg("-T")
                    .arg(ld_path.to_str().unwrap())
                    .arg(obj_path.to_str().unwrap());
                for lib in link_libs {
                    cmd.arg(lib.to_str().unwrap());
                }
                cmd.arg("-o").arg(elf_path.to_str().unwrap());
                match cmd.status() {
                    Ok(s) if s.success() => {
                        println!("Wrote {}", elf_path.display());
                    }
                    Ok(s) => {
                        eprintln!("ld.lld failed with exit code {}", s.code().unwrap_or(1));
                        process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to run ld.lld: {e}");
                        eprintln!(
                            "  → link manually: ld.lld -T {} {} -o {}.elf",
                            ld_path.display(),
                            obj_path.display(),
                            path.file_stem()
                                .unwrap_or_default()
                                .to_str()
                                .unwrap_or("output")
                        );
                    }
                }
            }
        }
        Ok(s) => {
            eprintln!("llc failed with exit code {}", s.code().unwrap_or(1));
            process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to run llc: {e}");
            eprintln!("Wrote {}", ll_path.display());
            eprintln!(
                "  → compile manually: llc -mtriple={} -filetype=obj -o output.o {}",
                triple,
                ll_path.display()
            );
        }
    }
}
