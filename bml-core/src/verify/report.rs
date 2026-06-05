use std::collections::HashSet;
use std::path::PathBuf;

/// An IKOS verification finding, mapped from the JSON report.
#[derive(Debug)]
pub struct Finding {
    pub check: String,
    pub code: String,
    pub status: Status,
    pub message: String,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Status {
    Error,
    Warning,
    Safe,
    Unreachable,
}

/// Deduplicate findings that report the same check at the same location.
#[must_use]
pub fn deduplicate(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen: HashSet<(String, String, Status, String, PathBuf, u32, u32)> = HashSet::new();
    findings
        .into_iter()
        .filter(|f| {
            seen.insert((
                f.check.clone(),
                f.code.clone(),
                f.status,
                f.message.clone(),
                f.file.clone(),
                f.line,
                f.column,
            ))
        })
        .collect()
}

/// Per-line suppression directive parsed from a source file.
/// Maps a line number to the list of V-codes suppressed on that line
/// (or the literal `"all"` for a wildcard).
pub type Suppressions = std::collections::HashMap<u32, Vec<String>>;

/// Scan a source file for `// bml-verify: ignore <V-code>[, <V-code>...]`
/// directives. A directive on line N suppresses findings on line N or N+1.
#[must_use]
pub fn parse_suppressions(source: &str) -> Suppressions {
    const PREFIX: &str = "// bml-verify: ignore";
    let mut map: Suppressions = std::collections::HashMap::new();
    for (idx, line) in source.lines().enumerate() {
        let Some(pos) = line.find(PREFIX) else {
            continue;
        };
        let rest = line[pos + PREFIX.len()..].trim();
        let codes: Vec<String> = rest
            .split(',')
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        if codes.is_empty() {
            continue;
        }
        // Source lines are 1-indexed in diagnostics.
        let line_no = (idx as u32) + 1;
        map.insert(line_no, codes);
    }
    map
}

/// Filter findings using suppressions keyed by source file path.
#[must_use]
pub fn apply_suppressions<S: std::hash::BuildHasher>(
    findings: Vec<Finding>,
    suppressions: &std::collections::HashMap<PathBuf, Suppressions, S>,
) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|f| {
            let Some(file_supp) = suppressions.get(&f.file) else {
                return true;
            };
            let matches_line = |line: u32| -> bool {
                file_supp.get(&line).is_some_and(|codes| {
                    codes
                        .iter()
                        .any(|c| c.eq_ignore_ascii_case("all") || c == &f.code)
                })
            };
            // Suppressor on the finding's line or the line immediately above.
            !(matches_line(f.line) || (f.line > 1 && matches_line(f.line - 1)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(column: u32, message: &str) -> Finding {
        Finding {
            check: "buffer-overflow".to_string(),
            code: "V101".to_string(),
            status: Status::Warning,
            message: message.to_string(),
            file: PathBuf::from("main.bml"),
            line: 7,
            column,
        }
    }

    #[test]
    fn deduplicate_keeps_distinct_columns_on_same_line() {
        let findings = vec![finding(3, "first"), finding(20, "second")];

        assert_eq!(deduplicate(findings).len(), 2);
    }

    #[test]
    fn deduplicate_removes_exact_duplicate_findings() {
        let findings = vec![finding(3, "same"), finding(3, "same")];

        assert_eq!(deduplicate(findings).len(), 1);
    }
}
