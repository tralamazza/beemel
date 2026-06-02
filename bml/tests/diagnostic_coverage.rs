//! Diagnostic-coverage ratchet.
//!
//! Every diagnostic code the compiler can emit (`E###`/`W###`/`V###`) is part
//! of the language's observable contract. This test enumerates the codes that
//! appear in `bml-core/src` (inside string literals, so bare mentions in
//! comments don't count) and asserts each one is exercised by an assertion in
//! the black-box suite (`tests.rs` / `exec.rs`). A code that is emittable but
//! untested is a silent spec-coverage gap; adding a new code now forces either
//! a test or an explicit, documented entry in `ALLOWLIST`.
//!
//! This measures *which rules have a test*, complementing `cargo llvm-cov`
//! (which measures *which compiler lines run*). Neither proves the output is
//! correct, but together they make untested behavior visible.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Codes that are emittable but intentionally have no dedicated assertion yet.
/// Each entry must stay justified: the test also fails if an allowlisted code
/// is actually tested (remove it) or no longer emitted (remove it). Keep this
/// list shrinking.
const ALLOWLIST: &[(&str, &str)] = &[
    // V-series come from IKOS abstract-interpretation findings; each needs a
    // program that provokes that specific finding. Tracked separately; only a
    // handful are pinned so far.
    ("V110", "IKOS pointer finding: no fixture yet"),
    ("V111", "IKOS pointer finding: no fixture yet"),
    ("V112", "IKOS pointer finding: no fixture yet"),
    ("V113", "IKOS pointer finding: no fixture yet"),
    ("V115", "IKOS pointer finding: no fixture yet"),
    ("V116", "IKOS pointer finding: no fixture yet"),
    ("V130", "IKOS finding: no fixture yet"),
    ("V140", "IKOS finding: no fixture yet"),
    ("V150", "IKOS finding: no fixture yet"),
    ("V160", "IKOS finding: no fixture yet"),
    ("V170", "IKOS finding: no fixture yet"),
    ("V180", "IKOS finding: no fixture yet"),
    ("V190", "IKOS finding: no fixture yet"),
    ("V191", "IKOS finding: no fixture yet"),
    ("V192", "IKOS finding: no fixture yet"),
    (
        "V999",
        "IKOS catch-all/unmapped finding: not deterministically triggerable",
    ),
];

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Append every `E###`/`W###`/`V###` token found in `text` to `out`.
fn push_codes(text: &str, out: &mut BTreeSet<String>) {
    let b = text.as_bytes();
    let n = b.len();
    let mut i = 0;
    while i + 4 <= n {
        let lead = matches!(b[i], b'E' | b'W' | b'V');
        let digits =
            b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() && b[i + 3].is_ascii_digit();
        let left_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
        let right_ok = i + 4 == n || !b[i + 4].is_ascii_digit();
        if lead && digits && left_ok && right_ok {
            out.insert(String::from_utf8_lossy(&b[i..i + 4]).into_owned());
            i += 4;
        } else {
            i += 1;
        }
    }
}

/// Collect codes that appear *inside string literals* of a .rs file. Splitting
/// on `"` yields alternating outside/inside segments; the odd ones are quoted.
/// (Good enough: diagnostic/code strings here never contain escaped quotes.)
fn codes_in_strings(path: &Path, out: &mut BTreeSet<String>) {
    let src = std::fs::read_to_string(path).unwrap_or_default();
    for line in src.lines() {
        for (idx, seg) in line.split('"').enumerate() {
            if idx % 2 == 1 {
                push_codes(seg, out);
            }
        }
    }
}

fn collect_rs(dir: &Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            codes_in_strings(&path, out);
        }
    }
}

#[test]
fn every_emittable_diagnostic_has_a_test() {
    let core_src = manifest().join("..").join("bml-core").join("src");
    let mut defined = BTreeSet::new();
    collect_rs(&core_src, &mut defined);

    // Codes asserted by the black-box suites. The ratchet file is deliberately
    // excluded so the ALLOWLIST below doesn't count as "tested".
    let mut tested = BTreeSet::new();
    for f in ["tests.rs", "exec.rs"] {
        codes_in_strings(&manifest().join("tests").join(f), &mut tested);
    }

    let allow: BTreeSet<String> = ALLOWLIST.iter().map(|(c, _)| (*c).to_string()).collect();

    // The allowlist must stay honest.
    let stale_not_defined: Vec<_> = allow.difference(&defined).cloned().collect();
    assert!(
        stale_not_defined.is_empty(),
        "ALLOWLIST has codes the compiler no longer emits (remove them): {stale_not_defined:?}"
    );
    let stale_now_tested: Vec<_> = allow.intersection(&tested).cloned().collect();
    assert!(
        stale_now_tested.is_empty(),
        "ALLOWLIST codes that are now tested (remove them from ALLOWLIST): {stale_now_tested:?}"
    );

    let uncovered: Vec<_> = defined
        .difference(&tested)
        .filter(|c| !allow.contains(*c))
        .cloned()
        .collect();
    assert!(
        uncovered.is_empty(),
        "these emittable diagnostic codes have no test (add a fixture, or an \
         ALLOWLIST entry with a reason):\n  {}\n\
         defined={} tested={} allowlisted={}",
        uncovered.join(", "),
        defined.len(),
        tested.len(),
        allow.len(),
    );
}
