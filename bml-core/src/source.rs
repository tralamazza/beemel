use std::collections::HashMap;
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileId(u32);

impl Default for FileId {
    fn default() -> Self {
        Self::new()
    }
}

impl FileId {
    pub fn new() -> Self {
        FileId(NEXT_ID.fetch_add(1, Ordering::SeqCst))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[must_use]
    pub fn new(file: FileId, start: usize, end: usize) -> Self {
        Span { file, start, end }
    }

    #[must_use]
    pub fn merge(self, other: Span) -> Span {
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    #[must_use]
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    #[must_use]
    pub fn empty(file: FileId, pos: usize) -> Self {
        Span {
            file,
            start: pos,
            end: pos,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: FileId,
    pub path: PathBuf,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct SourceMap {
    files: Vec<SourceFile>,
    line_starts: HashMap<FileId, Vec<usize>>,
}

impl Default for SourceMap {
    fn default() -> Self {
        Self::new()
    }
}

impl SourceMap {
    #[must_use]
    pub fn new() -> Self {
        SourceMap {
            files: Vec::new(),
            line_starts: HashMap::new(),
        }
    }

    /// Add a source file to the map.
    ///
    /// # Errors
    ///
    /// Returns an I/O error if the file cannot be read.
    pub fn add_file(&mut self, path: PathBuf) -> std::io::Result<FileId> {
        let source = fs::read_to_string(&path)?;
        let id = FileId::new();
        let line_starts = compute_line_starts(&source);
        self.line_starts.insert(id, line_starts);
        self.files.push(SourceFile { id, path, source });
        Ok(id)
    }

    pub fn add_file_with_source(&mut self, path: PathBuf, source: String) -> FileId {
        let id = FileId::new();
        let line_starts = compute_line_starts(&source);
        self.line_starts.insert(id, line_starts);
        self.files.push(SourceFile { id, path, source });
        id
    }

    /// Re-insert a previously parsed file, preserving its existing `FileId`.
    /// The LSP module cache uses this so a cached AST's spans (which reference
    /// the original `FileId`) stay valid even though each analysis builds a
    /// fresh `SourceMap`.
    pub fn insert_file(&mut self, file: SourceFile) {
        let line_starts = compute_line_starts(&file.source);
        self.line_starts.insert(file.id, line_starts);
        self.files.push(file);
    }

    /// Look up a file by its ID.
    ///
    /// # Panics
    ///
    /// Panics if the `FileId` is not found in the source map.
    #[must_use]
    pub fn get_file(&self, id: FileId) -> &SourceFile {
        self.files
            .iter()
            .find(|f| f.id == id)
            .expect("FileId not found in SourceMap")
    }

    #[must_use]
    pub fn get_path(&self, id: FileId) -> &Path {
        &self.get_file(id).path
    }

    #[must_use]
    pub fn source(&self, id: FileId) -> &str {
        &self.get_file(id).source
    }

    #[must_use]
    pub fn line_col(&self, file: FileId, offset: usize) -> Location {
        let starts = &self.line_starts[&file];
        let line = starts
            .binary_search(&offset)
            .unwrap_or_else(|i| i.saturating_sub(1));
        let line_start = starts[line];
        let col = offset - line_start;
        Location {
            line: line + 1,  // 1-indexed
            column: col + 1, // 1-indexed
        }
    }

    #[must_use]
    pub fn span_location(&self, span: Span) -> SpanLocation {
        let start = self.line_col(span.file, span.start);
        let end = self.line_col(span.file, span.end);
        SpanLocation { start, end }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Location {
    pub line: usize,
    pub column: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct SpanLocation {
    pub start: Location,
    pub end: Location,
}

fn compute_line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, c) in source.char_indices() {
        if c == '\n' {
            starts.push(i + 1);
        }
    }
    starts
}
