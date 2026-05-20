use crate::source::{SourceMap, Span};

/// Witness that an error diagnostic was emitted. The only public way to
/// construct one is by calling `DiagnosticBag::error` (or a variant), which
/// guarantees a diagnostic landed in the bag before this value exists.
///
/// Used by `Type::Error(_)` so that constructing the error type requires
/// proof that the user will see *some* diagnostic explaining the failure —
/// preventing silent suppression of real type errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ErrorGuaranteed(());

impl ErrorGuaranteed {
    /// Escape hatch for sites that cannot themselves emit a diagnostic but
    /// know one was emitted upstream (e.g. IR emission running after the
    /// checker has already rejected malformed programs). Use sparingly; each
    /// call is a claim that needs to hold in every code path.
    #[must_use]
    pub fn unchecked_claim_error_was_emitted() -> Self {
        ErrorGuaranteed(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: Level,
    pub code: String,
    pub message: String,
    pub primary: Span,
    pub labels: Vec<LabeledSpan>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabeledSpan {
    pub span: Span,
    pub label: Option<String>,
}

#[derive(Debug, Default)]
pub struct DiagnosticBag {
    diagnostics: Vec<Diagnostic>,
}

impl DiagnosticBag {
    #[must_use]
    pub fn new() -> Self {
        DiagnosticBag {
            diagnostics: Vec::new(),
        }
    }

    pub fn error(
        &mut self,
        message: impl Into<String>,
        code: impl Into<String>,
        span: Span,
    ) -> ErrorGuaranteed {
        self.diagnostics.push(Diagnostic {
            level: Level::Error,
            code: code.into(),
            message: message.into(),
            primary: span,
            labels: Vec::new(),
            notes: Vec::new(),
        });
        ErrorGuaranteed(())
    }

    pub fn warn(&mut self, message: impl Into<String>, code: impl Into<String>, span: Span) {
        self.diagnostics.push(Diagnostic {
            level: Level::Warning,
            code: code.into(),
            message: message.into(),
            primary: span,
            labels: Vec::new(),
            notes: Vec::new(),
        });
    }

    pub fn error_with_notes(
        &mut self,
        message: impl Into<String>,
        code: impl Into<String>,
        span: Span,
        notes: Vec<String>,
    ) -> ErrorGuaranteed {
        self.diagnostics.push(Diagnostic {
            level: Level::Error,
            code: code.into(),
            message: message.into(),
            primary: span,
            labels: Vec::new(),
            notes,
        });
        ErrorGuaranteed(())
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.level == Level::Error)
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    pub fn merge(&mut self, other: DiagnosticBag) {
        self.diagnostics.extend(other.diagnostics);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    pub fn emit(&self, source_map: &SourceMap) {
        for diag in &self.diagnostics {
            let loc = source_map.span_location(diag.primary);
            let prefix = match diag.level {
                Level::Error => "error",
                Level::Warning => "warning",
            };
            eprintln!("{prefix}[{}]: {}", diag.code, diag.message);
            let path = source_map.get_path(diag.primary.file);
            eprintln!(
                "  → {}:{}:{}",
                path.display(),
                loc.start.line,
                loc.start.column
            );

            // Print source context
            let source = source_map.source(diag.primary.file);
            let lines: Vec<&str> = source.lines().collect();
            let line_idx = loc.start.line.saturating_sub(1);
            if line_idx < lines.len() {
                let line = lines[line_idx];
                eprintln!("{:>4} | {}", loc.start.line, line);
                eprintln!(
                    "     | {}{}",
                    " ".repeat(loc.start.column.saturating_sub(1)),
                    "^".repeat(
                        (loc.end.column.saturating_sub(loc.start.column))
                            .max(diag.primary.end.saturating_sub(diag.primary.start).max(1),)
                    )
                );
            }

            for note in &diag.notes {
                eprintln!("     = note: {note}");
            }
            eprintln!();
        }
    }
}
