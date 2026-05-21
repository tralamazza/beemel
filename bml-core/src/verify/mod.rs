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

use self::report::{Finding, Status, deduplicate};

pub struct VerifyConfig {
    pub ikos_bin: PathBuf,
    pub ikos_report_bin: PathBuf,
    pub domain: String,
    pub checks: Vec<String>,
    pub extra_hwaddrs: Vec<PathBuf>,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        VerifyConfig {
            ikos_bin: PathBuf::from("ikos-analyzer"),
            ikos_report_bin: PathBuf::from("ikos-report"),
            domain: "interval".to_string(),
            checks: vec![
                "boa".into(),
                "nullity".into(),
                "sio".into(),
                "uio".into(),
                "dbz".into(),
                "shc".into(),
                "poa".into(),
                "upa".into(),
                "uva".into(),
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
    let llvm_ir = emitter.emit(program, symbols);

    let ll_path = stem.with_extension("verify.ll");
    std::fs::write(&ll_path, &llvm_ir).map_err(|e| {
        VerifyError::ToolInvocation(format!("failed to write {}: {e}", ll_path.display()))
    })?;

    // 2. Write hardware addresses file.
    let hwaddrs_path = stem.with_extension("verify.hwaddrs");
    hwaddrs::write_hwaddrs_file(symbols, &hwaddrs_path)
        .map_err(|e| VerifyError::ToolInvocation(format!("failed to write hwaddrs: {e}")))?;

    // 3. Collect entry points.
    let entry_points = collect_entry_points(symbols);
    let entry_points_str = entry_points.join(",");

    // 4. Build and run ikos-analyzer. The LLVM 18 fork accepts textual `.ll`
    // through LLVM's parseIRFile(), so no llvm-as step is needed here.
    let db_path = stem.with_extension("verify.db");
    let json_path = stem.with_extension("verify.json");

    let mut cmd = Command::new(&config.ikos_bin);
    cmd.arg(&ll_path)
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

    Ok(deduplicate(findings))
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
    struct IkosReportEntry {
        kind: i64,
        status: i64,
        statement_id: i64,
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

        let message = format!("[{severity}][{code}] {check} violation");

        findings.push(Finding {
            check,
            status,
            message,
            file,
            line,
            column,
        });
    }

    Ok(findings)
}
