pub mod hwaddrs;
pub mod preempt;
pub mod report;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::context::Context;
use crate::ir::IrEmitter;
use crate::resolver::SymbolTable;
use crate::source::SourceMap;
use crate::target::Target;

use self::report::{Finding, Status, Suppressions, apply_suppressions, deduplicate};

pub struct VerifyConfig {
    pub opt_bin: PathBuf,
    pub ikos_bin: PathBuf,
    pub ikos_report_bin: PathBuf,
    pub domain: String,
    pub checks: Vec<String>,
    pub extra_hwaddrs: Vec<PathBuf>,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        VerifyConfig {
            opt_bin: PathBuf::from("opt"),
            ikos_bin: PathBuf::from("ikos-analyzer"),
            ikos_report_bin: PathBuf::from("ikos-report"),
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
/// Returns `VerifyError` if IKOS or ikos-report fail, or if the JSON report
/// cannot be parsed.
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

    // 2. Promote allocas to SSA registers so IKOS can refine values across
    // loads. Without mem2reg/sroa, BML's alloca-heavy lowering hides the
    // dataflow IKOS needs for assume narrowing and array-init bounds.
    let opt_ll_path = stem.with_extension("verify.opt.ll");
    let opt_status = Command::new(&config.opt_bin)
        .arg("-passes=mem2reg,sroa")
        .arg("-S")
        .arg(&ll_path)
        .arg("-o")
        .arg(&opt_ll_path)
        .output()
        .map_err(|e| {
            VerifyError::ToolInvocation(format!("failed to run {}: {e}", config.opt_bin.display()))
        })?;
    if !opt_status.status.success() {
        let stderr = String::from_utf8_lossy(&opt_status.stderr);
        return Err(VerifyError::ToolInvocation(format!(
            "{} mem2reg failed: {stderr}",
            config.opt_bin.display()
        )));
    }

    // LLVM 19+ defaults to "debug records" (`#dbg_value(...)` etc.) which
    // ikos-analyzer (built against LLVM 18) cannot parse. Strip them so the
    // pipeline still works when the only `opt` on PATH is newer. Instruction
    // !dbg locations survive — those are what IKOS uses for source mapping.
    if let Ok(opt_ir) = std::fs::read_to_string(&opt_ll_path)
        && opt_ir.contains("#dbg_")
    {
        let stripped: String = opt_ir
            .lines()
            .filter(|line| !line.trim_start().starts_with("#dbg_"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&opt_ll_path, stripped).map_err(|e| {
            VerifyError::ToolInvocation(format!("failed to rewrite {}: {e}", opt_ll_path.display()))
        })?;
    }

    // 3. Write hardware addresses file.
    let hwaddrs_path = stem.with_extension("verify.hwaddrs");
    hwaddrs::write_hwaddrs_file(symbols, target.has_bitband, &hwaddrs_path)
        .map_err(|e| VerifyError::ToolInvocation(format!("failed to write hwaddrs: {e}")))?;

    // 4. Collect entry points.
    let entry_points = collect_entry_points(symbols);
    let entry_points_str = entry_points.join(",");

    // 5. Build and run ikos-analyzer on the mem2reg'd IR. The LLVM 18 fork
    // accepts textual `.ll` through LLVM's parseIRFile(), so no llvm-as step
    // is needed here.
    let db_path = stem.with_extension("verify.db");
    let json_path = stem.with_extension("verify.json");

    let mut cmd = Command::new(&config.ikos_bin);
    cmd.arg(&opt_ll_path)
        .arg("--entry-points")
        .arg(&entry_points_str)
        .arg("-d")
        .arg(&config.domain)
        .arg("-a")
        .arg(config.checks.join(","))
        .arg("--hardware-addresses-file")
        .arg(&hwaddrs_path)
        .arg("--no-libc")
        .arg("--no-libcpp")
        // We emit nsw on signed ops purely so the sio check applies; BML
        // arithmetic always wraps. Without this flag ikos would treat each
        // unproven overflow as an assumption for everything downstream of it
        // (C semantics: nsw overflow is UB). Requires the feat/llvm18 fork;
        // a stock ikos-analyzer fails loudly on the unknown flag.
        .arg("--no-wrap-sign-only")
        .arg("-o")
        .arg(&db_path);

    for extra in &config.extra_hwaddrs {
        cmd.arg("--hardware-addresses-file");
        cmd.arg(extra);
    }

    let output = cmd.output().map_err(|e| {
        VerifyError::ToolInvocation(format!("failed to run {}: {e}", config.ikos_bin.display()))
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(VerifyError::IkosFailed(stderr.to_string()));
    }

    // 5. Convert DB to JSON via the matching ikos-report.
    let report_output = Command::new(&config.ikos_report_bin)
        .arg("-f")
        .arg("json")
        .arg("-o")
        .arg(&json_path)
        .arg(&db_path)
        .output()
        .map_err(|e| {
            let hint = if config.ikos_report_bin.exists() {
                "; the script exists, so its interpreter may be missing. Run the IKOS install step or pass --ikos-report-bin to a working ikos-report"
            } else {
                ""
            };
            VerifyError::ToolInvocation(format!(
                "failed to run {}: {e}{hint}",
                config.ikos_report_bin.display()
            ))
        })?;

    if !report_output.status.success() {
        let stderr = String::from_utf8_lossy(&report_output.stderr);
        return Err(VerifyError::IkosFailed(format!(
            "{} failed: {stderr}",
            config.ikos_report_bin.display()
        )));
    }

    // 6. Parse JSON report.
    let report_content = std::fs::read_to_string(&json_path).map_err(|e| {
        VerifyError::ParseError(format!(
            "failed to read report {}: {e}",
            json_path.display()
        ))
    })?;

    let findings = parse_json_report(&report_content)?;
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

/// Parse IKOS JSON report into findings.
/// Schema: integer kind/status codes, with statement/file lookups.
fn parse_json_report(content: &str) -> Result<Vec<Finding>, VerifyError> {
    #[derive(serde::Deserialize)]
    struct IkosRoot {
        files: Vec<IkosFile>,
        functions: Vec<IkosFunction>,
        statements: Vec<IkosStatement>,
        #[serde(default)]
        operands: Vec<IkosOperand>,
        reports: Vec<IkosReportEntry>,
    }

    #[derive(serde::Deserialize)]
    struct IkosFile {
        id: i64,
        path: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct IkosFunction {
        id: i64,
        name: String,
        file_id: Option<i64>,
        line: Option<u32>,
    }

    #[derive(serde::Deserialize)]
    struct IkosStatement {
        id: i64,
        kind: i64,
        function_id: i64,
        file_id: Option<i64>,
        line: Option<u32>,
        column: Option<u32>,
    }

    #[derive(serde::Deserialize)]
    struct IkosOperand {
        id: i64,
        repr: Option<String>,
    }

    #[derive(serde::Deserialize)]
    struct IkosReportEntry {
        kind: i64,
        status: i64,
        statement_id: i64,
        // `default` alone does not cover an explicit `null`: unreachable
        // entries carry `"operands": null`, so this must be Option.
        #[serde(default)]
        operands: Option<Vec<(i64, i64)>>,
        #[allow(dead_code)]
        info: Option<serde_json::Value>,
    }

    let root: IkosRoot = serde_json::from_str(content)
        .map_err(|e| VerifyError::ParseError(format!("JSON parse error: {e}")))?;

    // Build lookup maps
    let file_map: HashMap<i64, PathBuf> = root
        .files
        .iter()
        .filter_map(|f| f.path.as_ref().map(|p| (f.id, PathBuf::from(p))))
        .collect();
    let stmt_by_id: HashMap<i64, &IkosStatement> =
        root.statements.iter().map(|s| (s.id, s)).collect();
    let fn_by_id: HashMap<i64, &IkosFunction> = root.functions.iter().map(|f| (f.id, f)).collect();
    let operand_by_id: HashMap<i64, &str> = root
        .operands
        .iter()
        .filter_map(|o| o.repr.as_deref().map(|r| (o.id, r)))
        .collect();

    let mut findings = Vec::new();
    for entry in root.reports {
        let status = match entry.status {
            0 => Status::Safe,
            1 => Status::Warning,
            2 => Status::Error,
            3 => Status::Unreachable,
            _ => continue,
        };
        if status == Status::Safe {
            continue;
        }
        // Kind 0 ("unreachable") entries are dropped like Safe ones: bml
        // encodes every verify obligation (range assumes, view bounds) as a
        // branch-to-unreachable, so IKOS reports one such entry per
        // obligation BY CONSTRUCTION -- on bml-generated IR they are
        // encoding artifacts, not dead user code (~28 info findings burying
        // the real report on the H7 example).
        if entry.kind == 0 {
            continue;
        }

        let check = check_name(entry.kind).to_string();
        let (code, severity) = check_to_bml_code(&check, status);

        // Look up file/line from the associated statement,
        // falling back to function-level location when statement has none.
        let (file, line, column) = stmt_by_id
            .get(&entry.statement_id)
            .map(|s| {
                let (fid, s_line, s_col) = (s.file_id, s.line, s.column);
                if fid.is_none() && s_line.is_none() {
                    // Statement has no location; fall back to function location
                    fn_by_id
                        .get(&s.function_id)
                        .map_or((PathBuf::new(), 0, 0), |f| {
                            let file = f
                                .file_id
                                .and_then(|fid| file_map.get(&fid).cloned())
                                .unwrap_or_default();
                            (file, f.line.unwrap_or(0), 0u32)
                        })
                } else {
                    let file = fid
                        .and_then(|fid| file_map.get(&fid).cloned())
                        .unwrap_or_default();
                    (file, s_line.unwrap_or(0), s_col.unwrap_or(0))
                }
            })
            .unwrap_or_default();

        // Collect operand names IKOS thinks are involved in this finding.
        // The JSON encodes them as (kind, id) pairs; we look up the printable
        // repr from the operands table. Kind 17 in IKOS is a local variable;
        // others are constants/internals we skip.
        let operand_names: Vec<&str> = entry
            .operands
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter_map(|(_, id)| operand_by_id.get(id).copied())
            .filter(|name| !name.is_empty())
            .collect();
        let message = if operand_names.is_empty() {
            format!("[{severity}][{code}] {check} violation")
        } else {
            format!(
                "[{severity}][{code}] {check} violation (operand: {})",
                operand_names.join(", ")
            )
        };

        findings.push(Finding {
            check,
            code,
            status,
            message,
            file,
            line,
            column,
        });
    }

    Ok(findings)
}
