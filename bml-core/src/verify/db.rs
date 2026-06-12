//! Read findings straight from the ikos-analyzer sqlite output database.
//!
//! This replaces the `ikos-report -f json` subprocess -- a Python script
//! that needs the IKOS virtualenv, the single worst install dependency of
//! `bml verify` -- with a direct read of the db ikos-analyzer already
//! writes. The aggregation rules are a faithful port of `generate_report()`
//! in ikos `analyzer/python/ikos/report.py` (no status/analysis filters):
//! checks are grouped per statement, then per call context; a statement
//! whose every context is unreachable yields nothing (bml drops kind-0
//! unreachable entries anyway -- they are branch-to-unreachable encoding
//! artifacts); otherwise each distinct (kind, operands, info) check emits
//! one finding at error or warning level, deduplicated across contexts.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::report::{Finding, Status};
use super::{VerifyError, check_name, check_to_bml_code};

struct StatementRow {
    function_id: i64,
    file_id: Option<i64>,
    line: Option<u32>,
    column: Option<u32>,
}

struct FunctionRow {
    file_id: Option<i64>,
    line: Option<u32>,
}

struct CheckRow {
    kind: i64,
    status: i64,
    statement_id: i64,
    operands: Option<String>,
    info: Option<String>,
    call_context_id: i64,
}

/// Read the `checks` table and aggregate it into findings.
///
/// # Errors
///
/// Returns `VerifyError::ParseError` if the database cannot be opened or
/// does not have the expected schema.
pub fn read_findings(db_path: &Path) -> Result<Vec<Finding>, VerifyError> {
    let conn = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| {
            VerifyError::ParseError(format!("failed to open {}: {e}", db_path.display()))
        })?;

    let err = |e: rusqlite::Error| VerifyError::ParseError(format!("verify db query: {e}"));

    // Lookup tables, read wholesale (verify dbs are small).
    let file_map: HashMap<i64, PathBuf> = {
        let mut stmt = conn.prepare("SELECT id, path FROM files").map_err(err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        rows.into_iter()
            .filter_map(|(id, path)| path.map(|p| (id, PathBuf::from(p))))
            .collect()
    };

    let stmt_map: HashMap<i64, StatementRow> = {
        let mut stmt = conn
            .prepare("SELECT id, function_id, file_id, line, column FROM statements")
            .map_err(err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    StatementRow {
                        function_id: r.get(1)?,
                        file_id: r.get(2)?,
                        line: r.get(3)?,
                        column: r.get(4)?,
                    },
                ))
            })
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        rows.into_iter().collect()
    };

    let fn_map: HashMap<i64, FunctionRow> = {
        let mut stmt = conn
            .prepare("SELECT id, file_id, line FROM functions")
            .map_err(err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    FunctionRow {
                        file_id: r.get(1)?,
                        line: r.get(2)?,
                    },
                ))
            })
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        rows.into_iter().collect()
    };

    let operand_map: HashMap<i64, String> = {
        let mut stmt = conn.prepare("SELECT id, repr FROM operands").map_err(err)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .map_err(err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(err)?;
        rows.into_iter()
            .filter_map(|(id, repr)| repr.map(|s| (id, s)))
            .collect()
    };

    let checks: Vec<CheckRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT kind, status, statement_id, operands, info, call_context_id \
                 FROM checks ORDER BY statement_id, call_context_id",
            )
            .map_err(err)?;
        stmt.query_map([], |r| {
            Ok(CheckRow {
                kind: r.get(0)?,
                status: r.get(1)?,
                statement_id: r.get(2)?,
                operands: r.get(3)?,
                info: r.get(4)?,
                call_context_id: r.get(5)?,
            })
        })
        .map_err(err)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)?
    };

    let mut findings = Vec::new();
    let mut i = 0;
    while i < checks.len() {
        // One statement's checks (the query is ordered by statement_id).
        let statement_id = checks[i].statement_id;
        let mut j = i;
        while j < checks.len() && checks[j].statement_id == statement_id {
            j += 1;
        }
        let stmt_checks = &checks[i..j];
        i = j;

        // Per call context: a context is unreachable if any of its checks
        // says so; errors/warnings are deduplicated by (kind, operands,
        // info) identity across contexts, insertion-ordered for stable
        // output. Mirrors generate_statement_result() in report.py.
        let mut all_contexts_unreachable = true;
        let mut seen: HashSet<(i64, Option<String>, Option<String>, i64)> = HashSet::new();
        let mut emit: Vec<(&CheckRow, Status)> = Vec::new();
        let mut k = 0;
        while k < stmt_checks.len() {
            let context_id = stmt_checks[k].call_context_id;
            let mut m = k;
            while m < stmt_checks.len() && stmt_checks[m].call_context_id == context_id {
                m += 1;
            }
            let ctx_checks = &stmt_checks[k..m];
            k = m;

            // report.py's per-context result is last-wins between
            // unreachable and error, which is order-dependent on an
            // unordered SQL result. In practice an unreachable statement
            // has ALL its checks at status 3; define the deterministic
            // version: a context is unreachable iff it has an unreachable
            // check and no error/warning check.
            let has_finding = ctx_checks.iter().any(|c| c.status == 1 || c.status == 2);
            let unreachable = !has_finding && ctx_checks.iter().any(|c| c.status == 3);
            if !unreachable {
                all_contexts_unreachable = false;
            }
            for c in ctx_checks {
                let status = match c.status {
                    1 => Status::Warning,
                    2 => Status::Error,
                    _ => continue, // ok / unreachable carry no finding
                };
                let key = (c.kind, c.operands.clone(), c.info.clone(), c.status);
                if seen.insert(key) {
                    emit.push((c, status));
                }
            }
        }
        // report.py emits a statement's errors before its warnings.
        emit.sort_by_key(|(_, status)| *status != Status::Error);

        // All contexts unreachable: report.py collapses the statement to a
        // single kind-0 "unreachable" entry, which bml drops as an encoding
        // artifact (every obligation is a branch-to-unreachable) -- so emit
        // nothing, including any error/warning checks recorded alongside.
        if all_contexts_unreachable {
            continue;
        }

        for (c, status) in emit {
            // Kind 0 ("unreachable") entries are encoding artifacts even on
            // reachable statements; parse_json_report dropped them too.
            if c.kind == 0 {
                continue;
            }

            let check = check_name(c.kind).to_string();
            let (code, severity) = check_to_bml_code(&check, status);

            // File/line from the statement, function-level fallback when the
            // statement has no location.
            let (file, line, column) = stmt_map
                .get(&c.statement_id)
                .map(|s| {
                    if s.file_id.is_none() && s.line.is_none() {
                        fn_map
                            .get(&s.function_id)
                            .map_or((PathBuf::new(), 0, 0), |f| {
                                let file = f
                                    .file_id
                                    .and_then(|fid| file_map.get(&fid).cloned())
                                    .unwrap_or_default();
                                (file, f.line.unwrap_or(0), 0u32)
                            })
                    } else {
                        let file = s
                            .file_id
                            .and_then(|fid| file_map.get(&fid).cloned())
                            .unwrap_or_default();
                        (file, s.line.unwrap_or(0), s.column.unwrap_or(0))
                    }
                })
                .unwrap_or_default();

            // The operands column holds JSON `[[no, operand_id], ...]`.
            let operand_names: Vec<String> = c
                .operands
                .as_deref()
                .and_then(|text| serde_json::from_str::<Vec<(i64, i64)>>(text).ok())
                .unwrap_or_default()
                .iter()
                .filter_map(|(_, id)| operand_map.get(id).cloned())
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
    }

    Ok(findings)
}
