use std::collections::HashSet;
use std::path::PathBuf;

/// An IKOS verification finding, mapped from the JSON report.
#[derive(Debug)]
pub struct Finding {
    pub check: String,
    pub status: Status,
    pub message: String,
    pub file: PathBuf,
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Error,
    Warning,
    Safe,
    Unreachable,
}

/// Deduplicate findings that report the same check at the same location.
#[must_use]
pub fn deduplicate(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen: HashSet<(String, PathBuf, u32)> = HashSet::new();
    findings
        .into_iter()
        .filter(|f| seen.insert((f.check.clone(), f.file.clone(), f.line)))
        .collect()
}
