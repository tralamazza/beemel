use bml_core::ast;
use bml_core::borrow::BorrowChecker;
use bml_core::checker::Checker;
use bml_core::errors::DiagnosticBag;
use bml_core::imports::ImportResolver;
use bml_core::ir::IrEmitter;
use bml_core::parser::Parser;
use bml_core::resolver::Resolver;
use bml_core::source::SourceMap;
use bml_core::stack;
use bml_core::target::Target;
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
            let mut path: Option<PathBuf> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--stack" => stack_analysis = true,
                    other => path = Some(PathBuf::from(other)),
                }
                i += 1;
            }
            let path = path.unwrap_or_else(|| {
                eprintln!("Usage: bml check [--stack] <file.bml>");
                process::exit(1);
            });
            check_file(&path, stack_analysis);
        }
        "build" => {
            let mut target_path: Option<PathBuf> = None;
            let mut link_libs: Vec<PathBuf> = vec![];
            let mut source_path: Option<PathBuf> = None;
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
                    "--link" => {
                        i += 1;
                        if i < args.len() {
                            link_libs.push(PathBuf::from(&args[i]));
                        } else {
                            eprintln!("--link requires a path");
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
                    "Usage: bml build [--target <file.target>] [--opt=<level>] [--debug] [--save-temps] [--link <lib>]... [--stack] <file.bml>"
                );
                process::exit(1);
            });

            let target = match &target_path {
                Some(p) => Target::from_file(p).unwrap_or_else(|e| {
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
            );
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
    eprintln!("  check [--stack] <file.bml>                    Type-check a source file");
    eprintln!("  build [--target <file.target>] [--opt=<level>] [--debug] [--save-temps]");
    eprintln!("        [--link <lib>]... [--stack] <file.bml>  Compile and optionally link");
    eprintln!();
    eprintln!("Options:");
    eprintln!("  --opt=<level>   Optimization level: 0, 1, 2, 3, s, z (default: s)");
    eprintln!("  --debug, -g     Emit DWARF debug information");
    eprintln!("  --save-temps    Keep intermediate files (file.opt.ll)");
    eprintln!("  --stack         Perform compile-time stack usage analysis");
    eprintln!("  --target <path> Target specification file");
    eprintln!("  --link <lib>    Link with library (.a / .o), repeatable");
    eprintln!("  --help, -h      Show this help");
}

fn check_file(path: &Path, stack_analysis: bool) {
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
    let (program, aliases) = import_resolver.resolve(program, path);
    let source_map = import_resolver.source_map;
    diags.merge(import_resolver.diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    // Phase 2a -- Name resolution
    let resolver = Resolver::new();
    let symbols = resolver.resolve(&program, &mut diags, aliases);

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
            ast::Item::Import(i) => println!("  import {} (alias)", i.module.0),
            ast::Item::Export(e) => println!("  export ({} items)", e.names.len()),
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
        }
    }
}

fn build_file(
    path: &Path,
    target: &Target,
    link_libs: &[PathBuf],
    opt_level: &str,
    save_temps: bool,
    debug: bool,
    stack_analysis: bool,
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
    let (program, aliases) = import_resolver.resolve(program, path);
    let source_map = import_resolver.source_map;
    diags.merge(import_resolver.diags);

    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

    let resolver = Resolver::new();
    let symbols = resolver.resolve(&program, &mut diags, aliases);
    if diags.has_errors() {
        diags.emit(&source_map);
        process::exit(1);
    }

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

    if !diags.is_empty() {
        diags.emit(&source_map);
    }

    if stack_analysis {
        let report = stack::analyze(&program, &symbols);
        stack::print_report(&report, &symbols, 2048);
    }

    let triple = target.to_llvm_target_triple();
    let emitter = IrEmitter::new(
        triple,
        target.interrupts.clone(),
        target.has_bitband,
        debug,
        Some(source_map),
    );
    let llvm_ir = emitter.emit(&program, &symbols);

    let ll_path = path.with_extension("ll");
    std::fs::write(&ll_path, &llvm_ir).unwrap_or_else(|e| {
        eprintln!("Error writing {}: {e}", ll_path.display());
        process::exit(1);
    });

    let linker_script = target.generate_linker_script();
    let ld_path = path.with_extension("ld");
    std::fs::write(&ld_path, linker_script).unwrap_or_else(|e| {
        eprintln!("Error writing {}: {e}", ld_path.display());
        process::exit(1);
    });

    let obj_path = path.with_extension("o");

    if opt_level == "0" {
        // ── no optimization: llc only ──
        let llc_status = process::Command::new("llc")
            .args([
                "-O0",
                &format!("-mtriple={}", target.to_llvm_target_triple()),
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
        let opt_ll_path = path.with_extension("opt.ll");
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
                let elf_path = path.with_extension("elf");
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
