pub mod db;
pub mod hwaddrs;
pub mod preempt;
pub mod report;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
#[cfg(not(feature = "ikos-static"))]
use std::process::Command;

use crate::context::Context;
use crate::ir::IrEmitter;
use crate::resolver::SymbolTable;
use crate::source::SourceMap;
use crate::target::Target;

use self::report::{Finding, Status, Suppressions, apply_suppressions, deduplicate};

pub struct VerifyConfig {
    pub ikos_bin: PathBuf,
    pub domain: String,
    pub checks: Vec<String>,
    pub extra_hwaddrs: Vec<PathBuf>,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        VerifyConfig {
            ikos_bin: PathBuf::from("ikos-analyzer"),
            // `interval-congruence` (reduced product of interval + congruence)
            // is required for `upa` to prove modular alignment of array
            // indexing without flooding the report with V150 false positives.
            // Cost over plain `interval` is within measurement noise on the
            // verify fixture set.
            domain: "interval-congruence".to_string(),
            // `uva` is intentionally omitted: BML's frontend requires `var`
            // initialization, so the only V160 sources are IKOS modeling
            // artifacts (entry-point parameters, havoc'd shared reads). Opt
            // back in with `--checks ...,uva`.
            checks: vec![
                "boa".into(),
                "nullity".into(),
                "sio".into(),
                "uio".into(),
                "dbz".into(),
                "shc".into(),
                "poa".into(),
                "upa".into(),
                "dca".into(),
                "dfa".into(),
                "fca".into(),
                "prover".into(),
            ],
            extra_hwaddrs: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum VerifyError {
    ToolInvocation(String),
    ParseError(String),
    IkosFailed(String),
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::ToolInvocation(msg) => write!(f, "tool invocation error: {msg}"),
            VerifyError::ParseError(msg) => write!(f, "parse error: {msg}"),
            VerifyError::IkosFailed(msg) => write!(f, "ikos failed: {msg}"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Collect entry points from the symbol table.
#[must_use]
pub fn collect_entry_points(symbols: &SymbolTable) -> Vec<String> {
    let entries: Vec<String> = symbols
        .functions
        .iter()
        .filter(|(_, f)| matches!(f.context, Context::Thread | Context::Isr(_)))
        .map(|(name, _)| name.clone())
        .collect();

    if entries.is_empty() {
        let fallback: Vec<String> = symbols
            .functions
            .iter()
            .filter(|(_, f)| matches!(f.context, Context::Any))
            .map(|(name, _)| name.clone())
            .collect();
        return fallback;
    }

    entries
}

/// Main verification entry point.
///
/// # Errors
///
/// Returns `VerifyError` if `opt` or ikos-analyzer fail, or if the result
/// database cannot be read.
#[allow(clippy::too_many_arguments)]
pub fn verify(
    program: &crate::ast::Program,
    symbols: &SymbolTable,
    source_map: &SourceMap,
    target: &Target,
    config: &VerifyConfig,
    work_dir: &Path,
    source_path: &Path,
) -> Result<Vec<Finding>, VerifyError> {
    let file_stem = source_path.file_stem().unwrap_or_default();
    let stem = work_dir.join(file_stem);

    // 1. Emit LLVM 18 opaque-pointer IR with debug info forced on.
    let arch = target.to_arch();

    let mut emitter = IrEmitter::new_with_verify(
        arch,
        target.interrupts.clone(),
        target.has_bitband,
        true,
        Some(source_map.clone()),
    );
    emitter.set_preempt(preempt::analyze(program, symbols));
    // Region/agent obligations: emit the provenance assume + reachability
    // assert at handoff register writes.
    emitter.set_region_alignments(target.region_alignments());
    emitter.set_handoff_regs(
        target
            .agents
            .iter()
            .flat_map(crate::target::Agent::handoffs)
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
    emitter.set_handoff_obligations(
        region_addr_ranges(program, symbols, target),
        handoff_reach_bounds(target),
        region_ranges(target),
    );
    let (cap_shadows, extent_asserts) = extent_obligations(target);
    emitter.set_extent_obligations(cap_shadows, extent_asserts);
    let llvm_ir = emitter.emit(program, symbols);

    let ll_path = stem.with_extension("verify.ll");
    std::fs::write(&ll_path, &llvm_ir).map_err(|e| {
        VerifyError::ToolInvocation(format!("failed to write {}: {e}", ll_path.display()))
    })?;

    // 2. Alloca promotion (mem2reg,sroa) happens INSIDE ikos-analyzer via
    // its --mem2reg flag (fork feature): without it, BML's alloca-heavy
    // lowering hides the dataflow IKOS needs for assume narrowing and
    // array-init bounds. Running it in-process removed the external LLVM 18
    // `opt` dependency and the debug-record stripping workaround that came
    // with newer `opt` versions.

    // 3. Write hardware addresses file.
    let hwaddrs_path = stem.with_extension("verify.hwaddrs");
    hwaddrs::write_hwaddrs_file(symbols, target.has_bitband, &hwaddrs_path)
        .map_err(|e| VerifyError::ToolInvocation(format!("failed to write hwaddrs: {e}")))?;

    // 4. Collect entry points.
    let entry_points = collect_entry_points(symbols);
    let entry_points_str = entry_points.join(",");

    // 5. Run ikos-analyzer on the .ll (the LLVM 18 fork accepts textual IR
    // through parseIRFile(), so no llvm-as step is needed). The same argv
    // drives both invocation modes: the default spawns config.ikos_bin; the
    // `ikos-static` feature calls the analyzer linked into this binary.
    let db_path = stem.with_extension("verify.db");

    let mut ikos_args: Vec<std::ffi::OsString> = vec![
        ll_path.clone().into(),
        "--mem2reg".into(),
        "--entry-points".into(),
        entry_points_str.clone().into(),
        "-d".into(),
        config.domain.clone().into(),
        "-a".into(),
        config.checks.join(",").into(),
        "--hardware-addresses-file".into(),
        hwaddrs_path.clone().into(),
        "--no-libc".into(),
        "--no-libcpp".into(),
        // We emit nsw on signed ops purely so the sio check applies; BML
        // arithmetic always wraps. Without this flag ikos would treat each
        // unproven overflow as an assumption for everything downstream of it
        // (C semantics: nsw overflow is UB). Requires the feat/llvm18 fork;
        // a stock ikos-analyzer fails loudly on the unknown flag.
        "--no-wrap-sign-only".into(),
        "-o".into(),
        db_path.clone().into(),
    ];
    for extra in &config.extra_hwaddrs {
        ikos_args.push("--hardware-addresses-file".into());
        ikos_args.push(extra.clone().into());
    }

    run_ikos(config, &ikos_args)?;

    // 6. Read findings straight from the result database (see db.rs; this
    // replaced the Python ikos-report subprocess).
    let findings = db::read_findings(&db_path)?;
    let findings = deduplicate(findings);

    // 7. Drop V130 (unsigned-int-overflow) on every line covered by a
    // wrapping-arithmetic expression (`+%`/`-%`/`*%`): wrap there is declared
    // intent, not an accident to prove away. Same line granularity as the
    // `bml-verify: ignore` comments below, with the same tradeoff -- a line
    // mixing a wrapping and a plain op loses the plain op's check.
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let wrap_lines: std::collections::HashSet<(PathBuf, u32)> = program
        .wrap_spans
        .iter()
        .chain(emitter.generated_wrap_spans.iter())
        .flat_map(|span| {
            let path = canon(source_map.get_path(span.file));
            let loc = source_map.span_location(*span);
            (loc.start.line..=loc.end.line).map(move |l| (path.clone(), l as u32))
        })
        .collect();
    let findings: Vec<Finding> = findings
        .into_iter()
        .filter(|f| !(f.code == "V130" && wrap_lines.contains(&(canon(&f.file), f.line))))
        .collect();

    // 8. Apply per-line suppression directives parsed from the sources that
    // appeared in the findings.
    let mut per_file: std::collections::HashMap<PathBuf, Suppressions> =
        std::collections::HashMap::new();
    for f in &findings {
        if f.file.as_os_str().is_empty() || per_file.contains_key(&f.file) {
            continue;
        }
        if let Ok(src) = std::fs::read_to_string(&f.file) {
            per_file.insert(f.file.clone(), report::parse_suppressions(&src));
        }
    }
    let findings = apply_suppressions(findings, &per_file);

    // 9. LANGUAGE CONTRACT: overflow on plain arithmetic is a program error
    // that verification must exclude -- "may overflow" is as red as "does
    // overflow". Escalate surviving V130 warnings to errors so the default
    // gate (--fail-on error) rejects them. The sanctioned outcomes for a
    // V130 site are: prove it (bound the operands), declare the wrap (`+%`),
    // or carry a visible `bml-verify: ignore` with a justification (the
    // escape hatch, applied above, BEFORE this escalation). This is also
    // what keeps the nsw signed modeling sound for gated programs: the
    // verifier may reason as if plain ops never overflow precisely because
    // no program where they might overflow passes this gate.
    Ok(findings
        .into_iter()
        .map(|mut f| {
            if f.code == "V130" && f.status == Status::Warning {
                f.status = Status::Error;
                // The message carries the original "[warning]" tag from
                // parse time; rewrite it so the report is consistent.
                f.message = f.message.replacen("[warning]", "[error]", 1);
            }
            f
        })
        .collect())
}

/// Run the analyzer as a subprocess (default mode).
#[cfg(not(feature = "ikos-static"))]
fn run_ikos(config: &VerifyConfig, args: &[std::ffi::OsString]) -> Result<(), VerifyError> {
    let output = Command::new(&config.ikos_bin)
        .args(args)
        .output()
        .map_err(|e| {
            VerifyError::ToolInvocation(format!("failed to run {}: {e}", config.ikos_bin.display()))
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(VerifyError::IkosFailed(stderr.to_string()));
    }
    Ok(())
}

/// Run the analyzer linked into this binary (`ikos-static` feature): the
/// fork's `ikos_analyzer_run` C API (capi.h), fed the same argv as the
/// subprocess mode. config.ikos_bin is intentionally ignored.
#[cfg(feature = "ikos-static")]
fn run_ikos(_config: &VerifyConfig, args: &[std::ffi::OsString]) -> Result<(), VerifyError> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn ikos_analyzer_run(argc: c_int, argv: *const *const c_char) -> c_int;
    }

    // The analyzer parses its argv with LLVM's process-wide command-line
    // globals; concurrent calls would race on them.
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    let mut cargs: Vec<CString> = Vec::with_capacity(args.len() + 1);
    cargs.push(CString::new("ikos-analyzer").expect("static name"));
    for arg in args {
        cargs.push(
            CString::new(arg.as_bytes()).map_err(|_| {
                VerifyError::ToolInvocation(format!("argument contains NUL: {arg:?}"))
            })?,
        );
    }
    let argv: Vec<*const c_char> = cargs.iter().map(|c| c.as_ptr()).collect();

    let guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // SAFETY: argv outlives the call (cargs holds the CStrings) and the lock
    // serializes access to the analyzer's command-line globals.
    let rc = unsafe {
        ikos_analyzer_run(
            c_int::try_from(argv.len()).expect("argv length"),
            argv.as_ptr(),
        )
    };
    drop(guard);

    if rc != 0 {
        return Err(VerifyError::IkosFailed(format!(
            "in-process ikos-analyzer failed with exit code {rc} (diagnostics on stderr)"
        )));
    }
    Ok(())
}

/// Map IKOS `CheckKind` integer to a short string name.
const CHECK_KINDS: &[(i64, &str)] = &[
    (0, "unreachable"),
    (1, "unexpected-operand"),
    (2, "uninitialized-variable"),
    (3, "assert"),
    (4, "division-by-zero"),
    (5, "shift-count"),
    (7, "signed-int-underflow"),
    (8, "signed-int-overflow"),
    (9, "unsigned-int-underflow"),
    (10, "unsigned-int-overflow"),
    (12, "null-pointer-deref"),
    (13, "null-pointer-comparison"),
    (14, "invalid-pointer-comparison"),
    (15, "pointer-comparison"),
    (16, "pointer-overflow"),
    (17, "invalid-pointer-deref"),
    (18, "unknown-memory-access"),
    (19, "unaligned-pointer"),
    (21, "buffer-overflow-gets"),
    (22, "buffer-overflow"),
    (25, "ignored-store"),
    (32, "recursive-function-call"),
    (35, "function-call-inline-asm"),
    (36, "unknown-function-call-pointer"),
    (37, "function-call"),
    (39, "free"),
];

/// Region-placed static name -> `[lo, hi)` range the static's BASE address can
/// occupy. The provenance assume at `&X as u32` uses this.
///
/// The upper bound is tightened by the static's size: the linker places the
/// whole static inside the region's mem block, so its base is at most
/// `block_end - sizeof(X)`. Without the tightening, `&X + offset` handoffs
/// (descriptor entries past the first, second buffers, tail pointers) are
/// structurally unprovable -- the assume admits a base at the very end of the
/// block, where any positive offset exceeds it.
fn region_addr_ranges(
    program: &crate::ast::Program,
    symbols: &SymbolTable,
    target: &Target,
) -> HashMap<String, (u64, u64)> {
    let mut map = HashMap::new();
    for item in &program.items {
        if let crate::ast::Item::StaticDef(s) = item
            && let Some((rname, _)) = &s.region
            && let Some(region) = target.regions.iter().find(|r| &r.name == rname)
            && let Some(mem) = target.mem_blocks.iter().find(|m| m.name == region.mem)
        {
            let mut hi = mem.end();
            if let Some(sym) = symbols.statics.get(&s.name.0) {
                // Strip storage wrappers (Shared/AgentShared/...) down to the
                // value type before sizing.
                let mut ty = &sym.ty;
                loop {
                    let inner = ty.inner();
                    if std::ptr::eq(inner, ty) {
                        break;
                    }
                    ty = inner;
                }
                let size = u64::from(crate::types::element_size(ty));
                // A static larger than its block cannot link; keep the full
                // range rather than fabricating an empty (vacuously true) one.
                if size > 0 && size <= mem.end() - mem.base {
                    hi = mem.end() - size + 1;
                }
            }
            map.insert(s.name.0.clone(), (mem.base, hi));
        }
    }
    map
}

/// Build the transfer-extent obligation maps from `extent_by` declarations:
/// per agent, a capacity shadow global per handoff register (delivery side)
/// and the count field -> (scale, shadows) entry (arming side). See
/// `Agent::extent_by` and the emission sites in `ir.rs`.
#[allow(clippy::type_complexity)]
fn extent_obligations(
    target: &Target,
) -> (
    HashMap<(String, String), String>,
    HashMap<(String, String, String), (u32, Vec<String>)>,
) {
    let mut cap_shadows = HashMap::new();
    let mut asserts = HashMap::new();
    for agent in &target.agents {
        for ch in &agent.channels {
            let Some(crate::target::ExtentSpec::Counter(eb)) = &ch.extent else {
                continue;
            };
            let parts: Vec<&str> = eb.path.split('.').collect();
            let [ep, er, ef] = parts.as_slice() else {
                continue; // shape validated at target load
            };
            // Shadows of THIS channel's handoffs only: arming channel 0
            // is not checked against a buffer delivered to channel 1.
            let mut shadows = Vec::new();
            for h in &ch.handoffs {
                let mut reg_parts = h.register.split('.');
                let (Some(p), Some(r), None) =
                    (reg_parts.next(), reg_parts.next(), reg_parts.next())
                else {
                    continue;
                };
                let shadow = format!("__bml_cap_{}_{r}", agent.name);
                cap_shadows.insert((p.to_string(), r.to_string()), shadow.clone());
                shadows.push(shadow);
            }
            if shadows.is_empty() {
                continue;
            }
            shadows.sort();
            asserts.insert(
                ((*ep).to_string(), (*er).to_string(), (*ef).to_string()),
                (eb.scale, shadows),
            );
        }
    }
    (cap_shadows, asserts)
}

/// Region name -> `[lo, hi)` byte range of its mem block. A write to an
/// `addr in R` struct field (an in-memory handoff) asserts the stored address
/// is in this range.
fn region_ranges(target: &Target) -> HashMap<String, (u64, u64)> {
    let mut map = HashMap::new();
    for region in &target.regions {
        if let Some(mem) = target.mem_blocks.iter().find(|m| m.name == region.mem) {
            map.insert(region.name.clone(), (mem.base, mem.end()));
        }
    }
    map
}

/// Handoff register path (`P.R`) -> `[lo, hi)` bounding range of the owning
/// agent's reachable mem blocks. The reachability assert uses this. Agents that
/// reach everything (`reach = *`) or nothing impose no bound and are skipped --
/// the bound is the min base / max end across reachable blocks, a sound
/// over-approximation (it catches addresses below or above all reachable
/// memory; an address in a gap between disjoint blocks is not caught).
fn handoff_reach_bounds(target: &Target) -> HashMap<String, (u64, u64)> {
    let mut map = HashMap::new();
    for agent in &target.agents {
        if agent.reach_all || agent.reach.is_empty() {
            continue;
        }
        let blocks: Vec<_> = agent
            .reach
            .iter()
            .filter_map(|name| target.mem_blocks.iter().find(|m| &m.name == name))
            .collect();
        let (Some(lo), Some(hi)) = (
            blocks.iter().map(|m| m.base).min(),
            blocks.iter().map(|m| m.end()).max(),
        ) else {
            continue;
        };
        for h in agent.handoffs() {
            map.insert(h.register.clone(), (lo, hi));
        }
    }
    map
}

fn check_name(kind: i64) -> &'static str {
    for &(k, name) in CHECK_KINDS {
        if k == kind {
            return name;
        }
    }
    "unknown"
}

/// Map a check name to the BML V-series error code.
fn check_to_bml_code(check: &str, status: Status) -> (String, String) {
    use Status::{Error, Safe, Unreachable, Warning};
    let code = match (check, status) {
        ("buffer-overflow", Error) => "V100",
        ("buffer-overflow", Warning) => "V101",
        ("buffer-overflow-gets", Error) => "V100",
        ("buffer-overflow-gets", Warning) => "V101",
        ("null-pointer-deref", _) => "V110",
        ("division-by-zero", _) => "V120",
        (
            "signed-int-overflow"
            | "unsigned-int-overflow"
            | "signed-int-underflow"
            | "unsigned-int-underflow",
            _,
        ) => "V130",
        ("shift-count", _) => "V140",
        ("unaligned-pointer", _) => "V150",
        ("uninitialized-variable", _) => "V160",
        ("unreachable", _) => "V170",
        ("unknown-function-call-pointer", _) => "V180",
        ("function-call", _) => "V190",
        ("recursive-function-call", _) => "V191",
        ("function-call-inline-asm", _) => "V192",
        ("null-pointer-comparison", _) => "V111",
        ("invalid-pointer-deref", _) => "V112",
        ("pointer-overflow", _) => "V113",
        ("unknown-memory-access", _) => "V114",
        ("pointer-comparison" | "invalid-pointer-comparison", _) => "V115",
        ("ignored-store", _) => "V116",
        ("assert", _) => "V200",
        _ => "V999",
    };
    let severity = match status {
        Error => "error",
        Warning => "warning",
        Safe | Unreachable => "info",
    };
    (code.to_string(), severity.to_string())
}
