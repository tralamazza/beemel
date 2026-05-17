use crate::source::{SourceMap, Span};

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

    pub fn error(&mut self, message: impl Into<String>, code: impl Into<String>, span: Span) {
        self.diagnostics.push(Diagnostic {
            level: Level::Error,
            code: code.into(),
            message: message.into(),
            primary: span,
            labels: Vec::new(),
            notes: Vec::new(),
        });
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
    ) {
        self.diagnostics.push(Diagnostic {
            level: Level::Error,
            code: code.into(),
            message: message.into(),
            primary: span,
            labels: Vec::new(),
            notes,
        });
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.level == Level::Error)
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
