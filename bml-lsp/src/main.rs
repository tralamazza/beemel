use bml_core::ast;
use bml_core::borrow::BorrowChecker;
use bml_core::checker::Checker;
use bml_core::errors::{self, DiagnosticBag, Level};
use bml_core::imports::{ImportResolver, ModuleCache};
use bml_core::parser::Parser;
use bml_core::resolver::{self, Resolver, SymbolTable};
use bml_core::source::{self, SourceMap};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response, ResponseError};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _,
};
use lsp_types::{
    CompletionItem, CompletionItemKind, CompletionList, CompletionResponse, Diagnostic,
    DiagnosticSeverity, GotoDefinitionResponse, Hover, HoverContents, InitializeParams, Location,
    MarkupContent, MarkupKind, Position, PositionEncodingKind, PublishDiagnosticsParams, Range,
    ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Uri,
};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct AnalysisResult {
    program: ast::Program,
    symbols: SymbolTable,
    source_map: SourceMap,
    root_file_id: bml_core::source::FileId,
}

struct Server {
    file_paths: HashMap<Uri, PathBuf>,
    file_sources: HashMap<Uri, String>,
    analysis_cache: HashMap<Uri, AnalysisResult>,
    position_encoding: PositionEncodingKind,
    /// Documents edited since their last analysis. Drained by `flush_dirty`
    /// after the debounce window, or before serving a feature request.
    dirty: HashSet<Uri>,
    /// Persistent parse cache for imported modules, reused across analyses so
    /// unchanged imports aren't re-read and re-parsed on every keystroke.
    module_cache: ModuleCache,
}

/// How long to wait for typing to settle before re-analyzing a changed
/// document and publishing diagnostics.
const DEBOUNCE: Duration = Duration::from_millis(200);

fn main() {
    let (conn, io_threads) = Connection::stdio();

    let (init_id, init_params) = conn.initialize_start().unwrap();
    let init_params: InitializeParams = serde_json::from_value(init_params).unwrap();
    let Some(position_encoding) = select_position_encoding(&init_params) else {
        conn.sender
            .send(Message::Response(Response::new_err(
                init_id,
                ErrorCode::RequestFailed as i32,
                "bml-lsp requires LSP UTF-8 or UTF-16 position encoding".to_string(),
            )))
            .ok();
        return;
    };

    let caps = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        hover_provider: Some(lsp_types::HoverProviderCapability::Simple(true)),
        definition_provider: Some(lsp_types::OneOf::Left(true)),
        completion_provider: Some(lsp_types::CompletionOptions::default()),
        position_encoding: Some(position_encoding.clone()),
        ..ServerCapabilities::default()
    };
    conn.initialize_finish(
        init_id,
        serde_json::json!({
            "capabilities": caps,
        }),
    )
    .unwrap();

    let mut server = Server {
        file_paths: HashMap::new(),
        file_sources: HashMap::new(),
        analysis_cache: HashMap::new(),
        position_encoding,
        dirty: HashSet::new(),
        module_cache: ModuleCache::default(),
    };

    server.run(&conn);
    io_threads.join().unwrap();
}

fn select_position_encoding(params: &InitializeParams) -> Option<PositionEncodingKind> {
    let encodings = params
        .capabilities
        .general
        .as_ref()
        .and_then(|general| general.position_encodings.as_ref());

    let Some(encodings) = encodings else {
        return Some(PositionEncodingKind::UTF16);
    };

    if encodings
        .iter()
        .any(|encoding| encoding == &PositionEncodingKind::UTF8)
    {
        Some(PositionEncodingKind::UTF8)
    } else if encodings
        .iter()
        .any(|encoding| encoding == &PositionEncodingKind::UTF16)
    {
        Some(PositionEncodingKind::UTF16)
    } else {
        None
    }
}

impl Server {
    fn run(&mut self, conn: &Connection) {
        loop {
            // While edits are pending, only wait out the debounce window before
            // flushing; otherwise block until the next message.
            let msg = if self.dirty.is_empty() {
                conn.receiver.recv().ok()
            } else {
                match conn.receiver.recv_timeout(DEBOUNCE) {
                    Ok(m) => Some(m),
                    Err(e) if e.is_timeout() => {
                        self.flush_dirty(conn);
                        continue;
                    }
                    Err(_) => None, // disconnected
                }
            };
            let Some(msg) = msg else {
                return;
            };

            match msg {
                Message::Request(req) => {
                    if conn.handle_shutdown(&req).unwrap() {
                        return;
                    }
                    // Feature requests read the analysis cache, so make sure any
                    // pending edits are analyzed before answering.
                    if !self.dirty.is_empty() {
                        self.flush_dirty(conn);
                    }
                    self.handle_request(conn, &req);
                }
                Message::Notification(not) => {
                    self.handle_notification(conn, not);
                }
                Message::Response(_) => {}
            }
        }
    }

    /// Re-analyze and publish diagnostics for every document edited since its
    /// last analysis.
    fn flush_dirty(&mut self, conn: &Connection) {
        let pending: Vec<Uri> = self.dirty.drain().collect();
        for uri in pending {
            if let (Some(path), Some(source)) = (
                self.file_paths.get(&uri).cloned(),
                self.file_sources.get(&uri).cloned(),
            ) {
                self.check_and_publish(conn, &uri, &path, &source);
            }
        }
    }

    fn handle_request(&mut self, conn: &Connection, req: &Request) {
        let id = req.id.clone();
        match req.method.as_str() {
            "textDocument/hover" => {
                let result = self.handle_hover(req);
                conn.sender
                    .send(Message::Response(Response {
                        id,
                        result: serde_json::to_value(result).ok(),
                        error: None,
                    }))
                    .ok();
            }
            "textDocument/definition" => {
                let result = self.handle_definition(req);
                conn.sender
                    .send(Message::Response(Response {
                        id,
                        result: serde_json::to_value(result).ok(),
                        error: None,
                    }))
                    .ok();
            }
            "textDocument/completion" => {
                let result = self.handle_completion(req);
                conn.sender
                    .send(Message::Response(Response {
                        id,
                        result: serde_json::to_value(result).ok(),
                        error: None,
                    }))
                    .ok();
            }
            _ => {
                conn.sender
                    .send(Message::Response(Response {
                        id,
                        result: None,
                        error: Some(ResponseError {
                            code: ErrorCode::MethodNotFound as i32,
                            message: format!("method not supported: {}", req.method),
                            data: None,
                        }),
                    }))
                    .ok();
            }
        }
    }

    fn handle_notification(&mut self, conn: &Connection, not: Notification) {
        match not.method.as_str() {
            DidOpenTextDocument::METHOD => {
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::DidOpenTextDocumentParams>(not.params)
                {
                    let doc = params.text_document;
                    let uri = doc.uri.clone();
                    let path = uri_to_pathbuf(&uri);
                    self.file_sources.insert(uri.clone(), doc.text.clone());
                    self.file_paths.insert(uri.clone(), path.clone());
                    self.check_and_publish(conn, &uri, &path, &doc.text);
                }
            }
            DidChangeTextDocument::METHOD => {
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::DidChangeTextDocumentParams>(not.params)
                {
                    let uri = params.text_document.uri;
                    // Store the new text and defer analysis; the debounce in
                    // `run` (or the next feature request) flushes it.
                    if self.file_paths.contains_key(&uri)
                        && let Some(last) = params.content_changes.last()
                    {
                        self.file_sources.insert(uri.clone(), last.text.clone());
                        self.dirty.insert(uri);
                    }
                }
            }
            DidSaveTextDocument::METHOD => {
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::DidSaveTextDocumentParams>(not.params)
                {
                    let uri = params.text_document.uri;
                    if let Some(path) = self.file_paths.get(&uri).cloned()
                        && let Some(source) = self.file_sources.get(&uri).cloned()
                    {
                        self.check_and_publish(conn, &uri, &path, &source);
                    }
                }
            }
            DidCloseTextDocument::METHOD => {
                if let Ok(params) =
                    serde_json::from_value::<lsp_types::DidCloseTextDocumentParams>(not.params)
                {
                    let uri = params.text_document.uri;
                    self.file_paths.remove(&uri);
                    self.file_sources.remove(&uri);
                    self.analysis_cache.remove(&uri);
                    self.dirty.remove(&uri);
                    // Clear diagnostics for the closed file
                    let params = PublishDiagnosticsParams {
                        uri,
                        diagnostics: vec![],
                        version: None,
                    };
                    let not = Notification::new(
                        lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
                        params,
                    );
                    conn.sender.send(Message::Notification(not)).ok();
                }
            }
            _ => {}
        }
    }

    fn check_and_publish(&mut self, conn: &Connection, uri: &Uri, path: &Path, source: &str) {
        // This document is now being analyzed, so it's no longer pending.
        self.dirty.remove(uri);
        let (analysis, diags) = analyze_file(path, source, &mut self.module_cache);

        let lsp_diags: Vec<Diagnostic> = diags
            .diagnostics()
            .iter()
            .map(|d| diagnostic_to_lsp(d, &analysis.source_map, &self.position_encoding))
            .collect();

        self.analysis_cache.insert(uri.clone(), analysis);

        let params = PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics: lsp_diags,
            version: None,
        };

        let not = Notification::new(
            lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
            params,
        );
        conn.sender.send(Message::Notification(not)).ok();
    }

    fn handle_hover(&self, req: &Request) -> Option<Hover> {
        let uri_str = req.params.get("textDocument")?.get("uri")?.as_str()?;
        let pos = req.params.get("position")?;
        let line = pos.get("line")?.as_u64()? as u32;
        let character = pos.get("character")?.as_u64()? as u32;
        let uri: Uri = uri_str.parse().ok()?;
        let lsp_pos = Position { line, character };

        let source = self.file_sources.get(&uri)?;
        let analysis = self.analysis_cache.get(&uri)?;

        let offset = pos_to_offset(source, lsp_pos, &self.position_encoding);
        let ident = find_ident_at(&analysis.program, analysis.root_file_id, offset)?;
        let name = &ident.0;

        // A local binding in the enclosing function shadows any global of the
        // same name, so resolve it first.
        let local_ty = enclosing_fn(&analysis.program, analysis.root_file_id, offset)
            .and_then(|f| local_type_in_fn(f, name));

        let (bml_decl, extra) = if let Some(ty) = local_ty {
            (ty, None)
        } else if let Some(f) = analysis.symbols.functions.get(name) {
            let params: Vec<String> = f.params.iter().map(|(n, t)| format!("{n}: {t}")).collect();
            let ret = f
                .ret
                .as_ref()
                .map(|r| format!(" -> {r}"))
                .unwrap_or_default();
            let sig = format!("fn {name}({}){ret}", params.join(", "));

            let call_info = find_call_at(&analysis.program, analysis.root_file_id, offset)
                .and_then(|call| format_call_args(&call, f, name));
            (sig, call_info)
        } else if let Some(s) = analysis.symbols.statics.get(name) {
            (format!("static {name}: {}", s.ty), None)
        } else if let Some(c) = analysis.symbols.consts.get(name) {
            let val = find_const_value(name, &analysis.program)
                .map(|v| format!(" = {v}"))
                .unwrap_or_default();
            (format!("const {name}: {}{val}", c.ty), None)
        } else if let Some(p) = analysis.symbols.peripherals.get(name) {
            let bml = format!("peripheral {name} at 0x{:08X}", p.base_addr);
            let extra = format!("{} registers", p.regs.len());
            (bml, Some(extra))
        } else if let Some((periph_name, reg)) =
            find_periph_reg(name, &analysis.symbols.peripherals)
        {
            let bml = format!("reg {} offset 0x{:X} {{ }}", name, reg.offset);
            let extra = format!("{} fields (in {})", reg.fields.len(), periph_name);
            (bml, Some(extra))
        } else if let Some((periph_name, reg_name, field)) =
            find_periph_field(name, &analysis.symbols.peripherals)
        {
            let bml = format!("field {}: {:?}", name, field.bit_spec);
            let extra = format!("in {periph_name}.{reg_name}");
            (bml, Some(extra))
        } else if let Some(s) = analysis.symbols.structs.get(name) {
            let fields: Vec<String> = s
                .fields
                .iter()
                .map(|(n, t)| format!("  {n}: {t}"))
                .collect();
            (
                format!("struct {name} {{\n{}\n}}", fields.join(",\n")),
                None,
            )
        } else if let Some(alias_info) = analysis.symbols.import_aliases.get(name) {
            let mut counts: Vec<String> = Vec::new();
            let mut funcs = 0;
            let mut statics = 0;
            let mut consts = 0;
            let mut peripherals = 0;
            let mut structs = 0;
            let mut enums = 0;
            for item in alias_info.exports.values() {
                match item {
                    ast::Item::FnDef(_) | ast::Item::ExternFnDef(_) => funcs += 1,
                    ast::Item::StaticDef(_) => statics += 1,
                    ast::Item::ConstDef(_) => consts += 1,
                    ast::Item::PeripheralDef(_) => peripherals += 1,
                    ast::Item::StructDef(_) => structs += 1,
                    ast::Item::EnumDef(_) => enums += 1,
                    _ => {}
                }
            }
            if funcs > 0 {
                counts.push(format!("{funcs} functions"));
            }
            if statics > 0 {
                counts.push(format!("{statics} statics"));
            }
            if consts > 0 {
                counts.push(format!("{consts} consts"));
            }
            if peripherals > 0 {
                counts.push(format!("{peripherals} peripherals"));
            }
            if structs > 0 {
                counts.push(format!("{structs} structs"));
            }
            if enums > 0 {
                counts.push(format!("{enums} enums"));
            }
            (format!("import alias `{name}`"), Some(counts.join(", ")))
        } else {
            (format!("ident {name}"), None)
        };

        let value = if let Some(extra) = extra {
            format!("```bml\n{bml_decl}\n```\n\n{extra}")
        } else {
            format!("```bml\n{bml_decl}\n```")
        };

        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value,
            }),
            range: None,
        })
    }

    fn handle_definition(&self, req: &Request) -> Option<GotoDefinitionResponse> {
        let uri_str = req.params.get("textDocument")?.get("uri")?.as_str()?;
        let pos = req.params.get("position")?;
        let line = pos.get("line")?.as_u64()? as u32;
        let character = pos.get("character")?.as_u64()? as u32;
        let uri: Uri = uri_str.parse().ok()?;
        let lsp_pos = Position { line, character };

        let source = self.file_sources.get(&uri)?;
        let analysis = self.analysis_cache.get(&uri)?;

        let offset = pos_to_offset(source, lsp_pos, &self.position_encoding);
        let ident = find_ident_at(&analysis.program, analysis.root_file_id, offset)?;
        let name = &ident.0;

        let target_range =
            find_definition_span(name, &analysis.program, analysis.root_file_id, offset)
                .or_else(|| find_def_in_aliases(name, &analysis.symbols))?;

        let target_uri = if target_range.file == analysis.root_file_id {
            uri.clone()
        } else {
            path_to_uri(analysis.source_map.get_path(target_range.file))
        };

        Some(GotoDefinitionResponse::Scalar(Location {
            uri: target_uri,
            range: span_to_range(&analysis.source_map, target_range, &self.position_encoding),
        }))
    }

    fn handle_completion(&self, req: &Request) -> Option<CompletionResponse> {
        let uri_str = req.params.get("textDocument")?.get("uri")?.as_str()?;
        let pos = req.params.get("position")?;
        let line = pos.get("line")?.as_u64()? as u32;
        let character = pos.get("character")?.as_u64()? as u32;
        let uri: Uri = uri_str.parse().ok()?;
        let lsp_pos = Position { line, character };

        let source = self.file_sources.get(&uri)?;
        let analysis = self.analysis_cache.get(&uri)?;

        let offset = pos_to_offset(source, lsp_pos, &self.position_encoding);

        let items = collect_completions(
            &analysis.program,
            &analysis.symbols,
            analysis.root_file_id,
            offset,
        );
        Some(CompletionResponse::List(CompletionList {
            is_incomplete: false,
            items,
        }))
    }
}

fn analyze_file(
    path: &Path,
    source: &str,
    module_cache: &mut ModuleCache,
) -> (AnalysisResult, DiagnosticBag) {
    let mut source_map = SourceMap::new();
    let file_id = source_map.add_file_with_source(path.to_path_buf(), source.to_string());
    let source_text = source_map.source(file_id);
    let mut diags = DiagnosticBag::new();

    let mut parser = Parser::new(source_text, file_id, &mut diags);
    let mut program = parser.parse_program();
    let mut symbols = SymbolTable::default();

    if !diags.has_errors() {
        let mut import_resolver = ImportResolver::new();
        import_resolver.source_map = source_map;
        // Lend the persistent parse cache to the resolver and take it back
        // (now updated) afterwards.
        std::mem::swap(&mut import_resolver.cache, module_cache);
        let (resolved_program, aliases) = import_resolver.resolve(program, path);
        std::mem::swap(&mut import_resolver.cache, module_cache);
        program = resolved_program;
        source_map = import_resolver.source_map;
        diags.merge(import_resolver.diags);

        let resolver = Resolver::new();
        symbols = resolver.resolve(&program, &mut diags, aliases);

        if !diags.has_errors() {
            Checker::check(&program, &symbols, &mut diags);
            if !diags.has_errors() {
                BorrowChecker::check(&program, &symbols, &mut diags);
            }
        }
    }

    let analysis = AnalysisResult {
        program,
        symbols,
        source_map,
        root_file_id: file_id,
    };

    (analysis, diags)
}

fn diagnostic_to_lsp(
    d: &errors::Diagnostic,
    source_map: &SourceMap,
    encoding: &PositionEncodingKind,
) -> Diagnostic {
    let severity = match d.level {
        Level::Error => DiagnosticSeverity::ERROR,
        Level::Warning => DiagnosticSeverity::WARNING,
    };
    Diagnostic {
        range: span_to_range(source_map, d.primary, encoding),
        severity: Some(severity),
        code: Some(lsp_types::NumberOrString::String(d.code.clone())),
        source: Some("bml".to_string()),
        message: d.message.clone(),
        ..Default::default()
    }
}

fn uri_to_pathbuf(uri: &Uri) -> PathBuf {
    let raw = uri.path().as_str();
    let decoded = percent_decode(raw);
    PathBuf::from(decoded)
}

fn path_to_uri(path: &Path) -> Uri {
    let encoded = percent_encode_path(path);
    format!("file://{encoded}").parse().expect("valid file URI")
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex) = u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                16,
            )
        {
            result.push(hex);
            i += 3;
            continue;
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(result).unwrap_or_else(|_| s.to_string())
}

fn percent_encode_path(path: &Path) -> String {
    use std::fmt::Write;
    let s = path.to_string_lossy();
    let mut result = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/' | b':') {
            result.push(b as char);
        } else {
            let _ = write!(result, "%{b:02X}");
        }
    }
    result
}

fn span_to_range(
    source_map: &SourceMap,
    span: source::Span,
    encoding: &PositionEncodingKind,
) -> Range {
    let source = source_map.source(span.file);
    Range {
        start: offset_to_pos(source, span.start, encoding),
        end: offset_to_pos(source, span.end, encoding),
    }
}

fn offset_to_pos(source: &str, offset: usize, encoding: &PositionEncodingKind) -> Position {
    let offset = offset.min(source.len());
    let line_start = source[..offset].rfind('\n').map_or(0, |idx| idx + 1);
    let line = source[..line_start]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count() as u32;
    let line_prefix = &source[line_start..offset];
    let character = if encoding == &PositionEncodingKind::UTF16 {
        line_prefix.encode_utf16().count()
    } else {
        line_prefix.len()
    };

    Position {
        line,
        character: character as u32,
    }
}

fn pos_to_offset(source: &str, pos: Position, encoding: &PositionEncodingKind) -> usize {
    let (line_start, line_end) = line_range(source, pos.line);
    if encoding == &PositionEncodingKind::UTF16 {
        utf16_pos_to_offset(
            &source[line_start..line_end],
            line_start,
            pos.character as usize,
        )
    } else {
        utf8_pos_to_offset(source, line_start, line_end, pos.character as usize)
    }
}

fn line_range(source: &str, line: u32) -> (usize, usize) {
    let mut start = 0;
    for _ in 0..line {
        let Some(idx) = source[start..].find('\n') else {
            return (source.len(), source.len());
        };
        start += idx + 1;
    }

    let end = source[start..]
        .find('\n')
        .map_or(source.len(), |idx| start + idx);
    (start, end)
}

fn utf8_pos_to_offset(source: &str, line_start: usize, line_end: usize, character: usize) -> usize {
    let mut offset = line_start + character.min(line_end - line_start);
    while offset > line_start && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn utf16_pos_to_offset(line: &str, line_start: usize, character: usize) -> usize {
    let mut units = 0;
    for (idx, ch) in line.char_indices() {
        let width = ch.len_utf16();
        if units + width > character {
            return line_start + idx;
        }
        units += width;
        if units == character {
            return line_start + idx + ch.len_utf8();
        }
    }
    line_start + line.len()
}

/// The `FileId` that an item's spans belong to, i.e. the file it was parsed
/// from. After import resolution `program.items` holds items inlined from many
/// files, each carrying byte offsets local to its own file; offset-based
/// position lookups must restrict to the requested file or a main.bml offset
/// can collide with a span from an imported module.
fn item_file(item: &ast::Item) -> Option<source::FileId> {
    match item {
        ast::Item::FnDef(f) => Some(f.name.1.file),
        ast::Item::ExternFnDef(e) => Some(e.name.1.file),
        ast::Item::StaticDef(s) => Some(s.name.1.file),
        ast::Item::ConstDef(c) => Some(c.name.1.file),
        ast::Item::PeripheralDef(p) => Some(p.name.1.file),
        ast::Item::StructDef(s) => Some(s.name.1.file),
        ast::Item::EnumDef(e) => Some(e.name.1.file),
        _ => None,
    }
}

fn find_ident_at(
    program: &ast::Program,
    file: source::FileId,
    offset: usize,
) -> Option<ast::Ident> {
    for item in &program.items {
        if item_file(item) != Some(file) {
            continue;
        }
        if let Some(ident) = find_ident_in_item(item, offset) {
            return Some(ident);
        }
    }
    None
}

fn find_ident_in_item(item: &ast::Item, offset: usize) -> Option<ast::Ident> {
    match item {
        ast::Item::FnDef(f) => {
            if span_contains(&f.name.1, offset) {
                Some(f.name.clone())
            } else {
                for p in &f.params {
                    if span_contains(&p.name.1, offset) {
                        return Some(p.name.clone());
                    }
                }
                find_ident_in_block(&f.body, offset)
            }
        }
        ast::Item::ExternFnDef(e) => {
            if span_contains(&e.name.1, offset) {
                Some(e.name.clone())
            } else {
                for p in &e.params {
                    if span_contains(&p.name.1, offset) {
                        return Some(p.name.clone());
                    }
                }
                None
            }
        }
        ast::Item::StaticDef(s) => span_contains(&s.name.1, offset).then_some(s.name.clone()),
        ast::Item::ConstDef(c) => span_contains(&c.name.1, offset).then_some(c.name.clone()),
        ast::Item::PeripheralDef(p) => {
            if span_contains(&p.name.1, offset) {
                return Some(p.name.clone());
            }
            for reg in &p.regs {
                if span_contains(&reg.name.1, offset) {
                    return Some(reg.name.clone());
                }
                for field in &reg.fields {
                    if span_contains(&field.name.1, offset) {
                        return Some(field.name.clone());
                    }
                }
            }
            None
        }
        ast::Item::StructDef(s) => span_contains(&s.name.1, offset).then_some(s.name.clone()),
        ast::Item::EnumDef(e) => span_contains(&e.name.1, offset).then_some(e.name.clone()),
        _ => None,
    }
}

fn find_ident_in_block(block: &ast::Block, offset: usize) -> Option<ast::Ident> {
    for stmt in &block.stmts {
        if let Some(ident) = find_ident_in_stmt(stmt, offset) {
            return Some(ident);
        }
    }
    block
        .trailing
        .as_ref()
        .and_then(|e| find_ident_in_expr(e, offset))
}

fn find_ident_in_stmt(stmt: &ast::Stmt, offset: usize) -> Option<ast::Ident> {
    match stmt {
        ast::Stmt::VarDecl(v) => span_contains(&v.name.1, offset)
            .then_some(v.name.clone())
            .or_else(|| find_ident_in_expr(&v.init, offset)),
        ast::Stmt::Assign(a) => {
            find_ident_in_lvalue(&a.target, offset).or_else(|| find_ident_in_expr(&a.value, offset))
        }
        ast::Stmt::Expr(e) => find_ident_in_expr(e, offset),
        ast::Stmt::If(i) => find_ident_in_expr(&i.cond, offset)
            .or_else(|| find_ident_in_block(&i.then_block, offset))
            .or_else(|| {
                i.else_branch
                    .as_ref()
                    .and_then(|s| find_ident_in_stmt(s, offset))
            }),
        ast::Stmt::Loop(l) => find_ident_in_block(&l.body, offset),
        ast::Stmt::While(w) => {
            find_ident_in_expr(&w.cond, offset).or_else(|| find_ident_in_block(&w.body, offset))
        }
        ast::Stmt::For(f) => span_contains(&f.var.1, offset)
            .then_some(f.var.clone())
            .or_else(|| find_ident_in_expr(&f.start, offset))
            .or_else(|| find_ident_in_expr(&f.end, offset))
            .or_else(|| f.step.as_ref().and_then(|s| find_ident_in_expr(s, offset)))
            .or_else(|| find_ident_in_block(&f.body, offset)),
        ast::Stmt::Return(r) => r.value.as_ref().and_then(|e| find_ident_in_expr(e, offset)),
        ast::Stmt::Match(m) => find_ident_in_expr(&m.scrutinee, offset).or_else(|| {
            m.arms
                .iter()
                .find_map(|arm| find_ident_in_block(&arm.body, offset))
        }),
        ast::Stmt::Block(b) => find_ident_in_block(b, offset),
        ast::Stmt::CompoundAssign(a) => {
            find_ident_in_lvalue(&a.target, offset).or_else(|| find_ident_in_expr(&a.value, offset))
        }
        ast::Stmt::Asm(a) => a
            .outputs
            .iter()
            .chain(a.inputs.iter())
            .find_map(|(_, e)| find_ident_in_expr(e, offset)),
        ast::Stmt::Assume(a) => find_ident_in_expr(&a.cond, offset),
        ast::Stmt::Assert(a) => find_ident_in_expr(&a.cond, offset),
        ast::Stmt::Break(_) | ast::Stmt::Continue(_) => None,
    }
}

fn find_ident_in_expr(expr: &ast::Expr, offset: usize) -> Option<ast::Ident> {
    match expr {
        ast::Expr::Ident(i) => span_contains(&i.1, offset).then_some(i.clone()),
        ast::Expr::Call(f, args) => find_ident_in_expr(f, offset)
            .or_else(|| args.iter().find_map(|a| find_ident_in_expr(a, offset))),
        ast::Expr::Binary(l, _, r) => {
            find_ident_in_expr(l, offset).or_else(|| find_ident_in_expr(r, offset))
        }
        ast::Expr::Unary(_, e) => find_ident_in_expr(e, offset),
        ast::Expr::Cast(e, _) => find_ident_in_expr(e, offset),
        ast::Expr::Group(e) => find_ident_in_expr(e, offset),
        ast::Expr::FieldAccess(e, (name, s)) => {
            if span_contains(s, offset) {
                find_ident_in_expr(e, offset).or(Some((name.clone(), *s)))
            } else {
                find_ident_in_expr(e, offset)
            }
        }
        ast::Expr::Index(base, index) => {
            find_ident_in_expr(base, offset).or_else(|| find_ident_in_expr(index, offset))
        }
        ast::Expr::ArrayInit(elems, _) => elems.iter().find_map(|e| find_ident_in_expr(e, offset)),
        ast::Expr::StructInit { name, fields, .. } => span_contains(&name.1, offset)
            .then_some(name.clone())
            .or_else(|| {
                fields
                    .iter()
                    .find_map(|(_, e)| find_ident_in_expr(e, offset))
            }),
        ast::Expr::EnumVariant {
            enum_name: en,
            variant: vn,
            ..
        } => span_contains(&en.1, offset)
            .then_some(en.clone())
            .or_else(|| span_contains(&vn.1, offset).then_some(vn.clone())),
        ast::Expr::Match(m) => find_ident_in_expr(&m.scrutinee, offset).or_else(|| {
            m.arms
                .iter()
                .find_map(|arm| find_ident_in_block(&arm.body, offset))
        }),
        ast::Expr::Block(b) => find_ident_in_block(&b.block, offset),
        ast::Expr::If(i) => find_ident_in_expr(&i.cond, offset)
            .or_else(|| find_ident_in_block(&i.then_block, offset))
            .or_else(|| find_ident_in_expr(&i.else_branch, offset)),
        ast::Expr::ViewNew {
            base, len, stride, ..
        } => find_ident_in_expr(base, offset)
            .or_else(|| len.as_ref().and_then(|e| find_ident_in_expr(e, offset)))
            .or_else(|| stride.as_ref().and_then(|e| find_ident_in_expr(e, offset))),
        ast::Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => find_ident_in_expr(base, offset)
            .or_else(|| {
                capacity
                    .as_ref()
                    .and_then(|e| find_ident_in_expr(e, offset))
            })
            .or_else(|| find_ident_in_expr(head, offset))
            .or_else(|| find_ident_in_expr(len, offset)),
        ast::Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => find_ident_in_expr(base, offset)
            .or_else(|| {
                bit_offset
                    .as_ref()
                    .and_then(|e| find_ident_in_expr(e, offset))
            })
            .or_else(|| {
                len_bits
                    .as_ref()
                    .and_then(|e| find_ident_in_expr(e, offset))
            }),
        ast::Expr::IntLiteral(..)
        | ast::Expr::FloatLiteral(..)
        | ast::Expr::BoolLiteral(..)
        | ast::Expr::StringLiteral(..)
        | ast::Expr::NullLiteral(_)
        | ast::Expr::SizeOf(..) => None,
    }
}

fn find_ident_in_lvalue(lv: &ast::LValue, offset: usize) -> Option<ast::Ident> {
    match lv {
        ast::LValue::Name(i) => span_contains(&i.1, offset).then_some(i.clone()),
        ast::LValue::Field(l, ident) => {
            if span_contains(&ident.1, offset) {
                return Some(ident.clone());
            }
            find_ident_in_lvalue(l, offset)
        }
        ast::LValue::Index(l, index) => {
            find_ident_in_lvalue(l, offset).or_else(|| find_ident_in_expr(index, offset))
        }
        ast::LValue::Deref(e) => find_ident_in_expr(e, offset),
    }
}

fn span_contains(span: &source::Span, offset: usize) -> bool {
    offset >= span.start && offset < span.end
}

/// Find the function whose signature or body contains `offset`. Functions do
/// not overlap, so the first match is unique. The range spans from the name
/// (`f.name.1.start`) through the body end so goto/hover on a parameter
/// declaration still resolves to its own function. Restricted to `file` so an
/// offset never matches a same-offset function inlined from an imported module.
fn enclosing_fn(
    program: &ast::Program,
    file: source::FileId,
    offset: usize,
) -> Option<&ast::FnDef> {
    program.items.iter().find_map(|item| match item {
        ast::Item::FnDef(f)
            if f.name.1.file == file && offset >= f.name.1.start && offset < f.body.span.end =>
        {
            Some(f)
        }
        _ => None,
    })
}

/// Resolve `name` to a parameter or local declaration *within* a single
/// function. Used so a local correctly shadows a same-named global.
fn local_type_in_fn(f: &ast::FnDef, name: &str) -> Option<String> {
    for p in &f.params {
        if p.name.0 == name {
            return Some(format!("{name}: {}", p.ty));
        }
    }
    find_local_in_block(name, &f.body)
}

fn find_local_in_block(name: &str, block: &ast::Block) -> Option<String> {
    for stmt in &block.stmts {
        match stmt {
            ast::Stmt::VarDecl(v) if v.name.0 == name => {
                let ty = v
                    .ty_ann
                    .as_ref()
                    .map_or_else(|| format!("{name} (inferred)"), |t| format!("{name}: {t}"));
                return Some(ty);
            }
            ast::Stmt::Block(b) => {
                let r = find_local_in_block(name, b);
                if r.is_some() {
                    return r;
                }
            }
            ast::Stmt::If(i) => {
                let r = find_local_in_block(name, &i.then_block);
                if r.is_some() {
                    return r;
                }
                if let Some(ref else_s) = i.else_branch {
                    let r = find_local_in_stmt(name, else_s);
                    if r.is_some() {
                        return r;
                    }
                }
            }
            ast::Stmt::Loop(l) => {
                let r = find_local_in_block(name, &l.body);
                if r.is_some() {
                    return r;
                }
            }
            ast::Stmt::While(w) => {
                let r = find_local_in_block(name, &w.body);
                if r.is_some() {
                    return r;
                }
            }
            ast::Stmt::For(f) => {
                let r = find_local_in_block(name, &f.body);
                if r.is_some() {
                    return r;
                }
            }
            ast::Stmt::Match(m) => {
                for arm in &m.arms {
                    let r = find_local_in_block(name, &arm.body);
                    if r.is_some() {
                        return r;
                    }
                }
            }
            _ => {}
        }
    }
    block
        .trailing
        .as_ref()
        .and_then(|e| find_local_in_expr(name, e))
}

fn find_local_in_stmt(name: &str, stmt: &ast::Stmt) -> Option<String> {
    match stmt {
        ast::Stmt::VarDecl(v) if v.name.0 == name => {
            let ty = v
                .ty_ann
                .as_ref()
                .map_or_else(|| format!("{name} (inferred)"), |t| format!("{name}: {t}"));
            Some(ty)
        }
        ast::Stmt::Block(b) => find_local_in_block(name, b),
        ast::Stmt::If(i) => {
            let r = find_local_in_block(name, &i.then_block);
            if r.is_some() {
                return r;
            }
            i.else_branch
                .as_ref()
                .and_then(|s| find_local_in_stmt(name, s))
        }
        _ => None,
    }
}

fn find_local_in_expr(name: &str, expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Block(b) => find_local_in_block(name, &b.block),
        ast::Expr::If(i) => find_local_in_block(name, &i.then_block)
            .or_else(|| find_local_in_expr(name, &i.else_branch)),
        _ => None,
    }
}

fn find_periph_reg<'a>(
    name: &str,
    peripherals: &'a HashMap<String, resolver::PeripheralSymbol>,
) -> Option<(&'a str, &'a resolver::RegSymbol)> {
    for (pname, periph) in peripherals {
        if let Some(reg) = periph.regs.get(name) {
            return Some((pname.as_str(), reg));
        }
    }
    None
}

fn find_periph_field<'a>(
    name: &str,
    peripherals: &'a HashMap<String, resolver::PeripheralSymbol>,
) -> Option<(&'a str, &'a str, &'a resolver::FieldSymbol)> {
    for (pname, periph) in peripherals {
        for (rname, reg) in &periph.regs {
            if let Some(field) = reg.fields.get(name) {
                return Some((pname.as_str(), rname.as_str(), field));
            }
        }
    }
    None
}

fn find_def_in_aliases(name: &str, symbols: &SymbolTable) -> Option<bml_core::source::Span> {
    use bml_core::ast::Item;
    for alias_info in symbols.import_aliases.values() {
        for item in alias_info.exports.values() {
            match item {
                Item::FnDef(f) if f.name.0 == name => return Some(f.name.1),
                Item::ExternFnDef(e) if e.name.0 == name => return Some(e.name.1),
                Item::StaticDef(s) if s.name.0 == name => return Some(s.name.1),
                Item::ConstDef(c) if c.name.0 == name => return Some(c.name.1),
                Item::PeripheralDef(p) => {
                    if p.name.0 == name {
                        return Some(p.name.1);
                    }
                    for reg in &p.regs {
                        if reg.name.0 == name {
                            return Some(reg.name.1);
                        }
                        for field in &reg.fields {
                            if field.name.0 == name {
                                return Some(field.name.1);
                            }
                        }
                    }
                }
                Item::StructDef(s) if s.name.0 == name => return Some(s.name.1),
                Item::EnumDef(e) if e.name.0 == name => return Some(e.name.1),
                _ => {}
            }
        }
    }
    None
}

fn find_local_def_in_block(name: &str, block: &ast::Block) -> Option<source::Span> {
    for stmt in &block.stmts {
        if let Some(span) = find_local_def_in_stmt(name, stmt) {
            return Some(span);
        }
    }
    block
        .trailing
        .as_ref()
        .and_then(|e| find_local_def_in_expr(name, e))
}

fn find_local_def_in_stmt(name: &str, stmt: &ast::Stmt) -> Option<source::Span> {
    match stmt {
        ast::Stmt::VarDecl(v) if v.name.0 == name => Some(v.name.1),
        ast::Stmt::Block(b) => find_local_def_in_block(name, b),
        ast::Stmt::If(i) => find_local_def_in_block(name, &i.then_block).or_else(|| {
            i.else_branch
                .as_ref()
                .and_then(|s| find_local_def_in_stmt(name, s))
        }),
        ast::Stmt::Loop(l) => find_local_def_in_block(name, &l.body),
        ast::Stmt::While(w) => find_local_def_in_block(name, &w.body),
        ast::Stmt::For(f) => {
            if f.var.0 == name {
                return Some(f.var.1);
            }
            find_local_def_in_block(name, &f.body)
        }
        ast::Stmt::Match(m) => m
            .arms
            .iter()
            .find_map(|arm| find_local_def_in_block(name, &arm.body)),
        _ => None,
    }
}

fn find_local_def_in_expr(name: &str, expr: &ast::Expr) -> Option<source::Span> {
    match expr {
        ast::Expr::Block(b) => find_local_def_in_block(name, &b.block),
        ast::Expr::If(i) => find_local_def_in_block(name, &i.then_block)
            .or_else(|| find_local_def_in_expr(name, &i.else_branch)),
        _ => None,
    }
}

fn find_definition_span(
    name: &str,
    program: &ast::Program,
    file: source::FileId,
    offset: usize,
) -> Option<source::Span> {
    // Prefer a binding in the enclosing function: a parameter or a local
    // declaration. This keeps resolution scoped, so a local never jumps to a
    // same-named local/param in another function and correctly shadows globals.
    if let Some(f) = enclosing_fn(program, file, offset) {
        for p in &f.params {
            if p.name.0 == name {
                return Some(p.name.1);
            }
        }
        if let Some(span) = find_local_def_in_block(name, &f.body) {
            return Some(span);
        }
    }

    // Fall back to top-level item definitions, matching names only. Never
    // descend into other functions' params/locals.
    for item in &program.items {
        match item {
            ast::Item::FnDef(f) if f.name.0 == name => return Some(f.name.1),
            ast::Item::ExternFnDef(e) if e.name.0 == name => return Some(e.name.1),
            ast::Item::StaticDef(s) if s.name.0 == name => return Some(s.name.1),
            ast::Item::ConstDef(c) if c.name.0 == name => return Some(c.name.1),
            ast::Item::PeripheralDef(p) => {
                if p.name.0 == name {
                    return Some(p.name.1);
                }
                for reg in &p.regs {
                    if reg.name.0 == name {
                        return Some(reg.name.1);
                    }
                    for field in &reg.fields {
                        if field.name.0 == name {
                            return Some(field.name.1);
                        }
                    }
                }
            }
            ast::Item::StructDef(s) if s.name.0 == name => return Some(s.name.1),
            ast::Item::EnumDef(e) if e.name.0 == name => return Some(e.name.1),
            _ => {}
        }
    }
    None
}

// ─── Completion helpers ────────────────────────────────────────────────

struct CompletionScope {
    scopes: Vec<HashMap<String, String>>, // name → type string
}

impl CompletionScope {
    fn new() -> Self {
        CompletionScope {
            scopes: vec![HashMap::new()],
        }
    }

    fn push(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn insert(&mut self, name: String, ty: String) {
        self.scopes.last_mut().unwrap().insert(name, ty);
    }

    fn all_visible(&self) -> Vec<(String, &String)> {
        let mut seen = HashMap::new();
        for scope in self.scopes.iter().rev() {
            for (name, ty) in scope {
                seen.entry(name.clone()).or_insert(ty);
            }
        }
        seen.into_iter().collect()
    }
}

fn collect_completions(
    program: &ast::Program,
    symbols: &SymbolTable,
    file: source::FileId,
    offset: usize,
) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    // Keywords
    for kw in BML_KEYWORDS {
        items.push(CompletionItem {
            label: kw.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            sort_text: Some(format!("2_{kw}")),
            ..Default::default()
        });
    }

    // Globals from symbol table
    for name in symbols.functions.keys() {
        if let Some(f) = symbols.functions.get(name) {
            let params: Vec<String> = f.params.iter().map(|(n, t)| format!("{n}: {t}")).collect();
            let detail = format!("fn {name}({})", params.join(", "));
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(detail),
                sort_text: Some(format!("1_{name}")),
                ..Default::default()
            });
        }
    }
    for name in symbols.statics.keys() {
        if let Some(s) = symbols.statics.get(name) {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(format!("static {name}: {}", s.ty)),
                sort_text: Some(format!("1_{name}")),
                ..Default::default()
            });
        }
    }
    for name in symbols.consts.keys() {
        if let Some(c) = symbols.consts.get(name) {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some(format!("const {name}: {}", c.ty)),
                sort_text: Some(format!("1_{name}")),
                ..Default::default()
            });
        }
    }
    for name in symbols.peripherals.keys() {
        if let Some(p) = symbols.peripherals.get(name) {
            items.push(CompletionItem {
                label: name.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(format!("peripheral {name} ({} registers)", p.regs.len())),
                sort_text: Some(format!("1_{name}")),
                ..Default::default()
            });
        }
    }
    for name in symbols.structs.keys() {
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::STRUCT),
            detail: Some(format!("struct {name}")),
            sort_text: Some(format!("1_{name}")),
            ..Default::default()
        });
    }
    for name in symbols.enums.keys() {
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::ENUM),
            detail: Some(format!("enum {name}")),
            sort_text: Some(format!("1_{name}")),
            ..Default::default()
        });
    }

    for name in symbols.import_aliases.keys() {
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::MODULE),
            detail: Some("import alias".to_string()),
            sort_text: Some(format!("1_{name}")),
            ..Default::default()
        });
    }

    // Peripheral registers and fields
    for (periph_name, p) in &symbols.peripherals {
        for reg_name in p.regs.keys() {
            if let Some(reg) = p.regs.get(reg_name) {
                items.push(CompletionItem {
                    label: reg_name.clone(),
                    kind: Some(CompletionItemKind::PROPERTY),
                    detail: Some(format!(
                        "reg {reg_name} (in {periph_name}, offset 0x{:02X})",
                        reg.offset
                    )),
                    sort_text: Some(format!("1_{reg_name}")),
                    ..Default::default()
                });
                for field_name in reg.fields.keys() {
                    if let Some(field) = reg.fields.get(field_name) {
                        items.push(CompletionItem {
                            label: field_name.clone(),
                            kind: Some(CompletionItemKind::FIELD),
                            detail: Some(format!(
                                "field {field_name} ({periph_name}.{reg_name}, {:?})",
                                field.bit_spec
                            )),
                            sort_text: Some(format!("1_{field_name}")),
                            ..Default::default()
                        });
                    }
                }
            }
        }
    }

    // Locals via scope walk
    let mut scope = CompletionScope::new();
    for item in &program.items {
        if let ast::Item::FnDef(f) = item
            && f.name.1.file == file
            && offset >= f.body.span.start
            && offset < f.body.span.end
        {
            for p in &f.params {
                scope.insert(p.name.0.clone(), format!("{}: {}", p.name.0, p.ty));
            }
            walk_block_for_scope(&f.body, offset, &mut scope);
            break;
        }
    }

    for (name, ty) in scope.all_visible() {
        // Don't duplicate globals
        if symbols.functions.contains_key(&name)
            || symbols.statics.contains_key(&name)
            || symbols.consts.contains_key(&name)
        {
            continue;
        }
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VARIABLE),
            detail: Some(ty.clone()),
            sort_text: Some(format!("0_{name}")),
            ..Default::default()
        });
    }

    items
}

fn walk_block_for_scope(block: &ast::Block, offset: usize, scope: &mut CompletionScope) -> bool {
    scope.push();
    for stmt in &block.stmts {
        if offset < stmt_start(stmt) {
            return true;
        }
        if walk_stmt_for_scope(stmt, offset, scope) {
            return true;
        }
    }
    if let Some(trailing) = &block.trailing
        && span_contains(&trailing.span(), offset)
    {
        return walk_expr_for_scope(trailing, offset, scope);
    }
    true
}

fn walk_stmt_for_scope(stmt: &ast::Stmt, offset: usize, scope: &mut CompletionScope) -> bool {
    match stmt {
        ast::Stmt::VarDecl(v) => {
            if offset < expr_end(&v.init) {
                return true;
            }
            let ty = v.ty_ann.as_ref().map_or_else(
                || format!("{}: ?", v.name.0),
                |t| format!("{}: {}", v.name.0, t),
            );
            scope.insert(v.name.0.clone(), ty);
            false
        }
        ast::Stmt::Block(b) if offset >= b.span.start && offset < b.span.end => {
            walk_block_for_scope(b, offset, scope)
        }
        ast::Stmt::If(i) => {
            if span_contains(&i.cond.span(), offset) {
                true
            } else if offset >= i.then_block.span.start && offset < i.then_block.span.end {
                walk_block_for_scope(&i.then_block, offset, scope)
            } else if let Some(ref else_s) = i.else_branch {
                walk_stmt_for_scope(else_s, offset, scope)
            } else {
                false
            }
        }
        ast::Stmt::Loop(l) if offset >= l.body.span.start && offset < l.body.span.end => {
            walk_block_for_scope(&l.body, offset, scope)
        }
        ast::Stmt::While(w) if offset >= w.body.span.start && offset < w.body.span.end => {
            walk_block_for_scope(&w.body, offset, scope)
        }
        ast::Stmt::While(w) if span_contains(&w.cond.span(), offset) => true,
        ast::Stmt::For(f) if offset >= f.body.span.start && offset < f.body.span.end => {
            scope.insert(f.var.0.clone(), format!("{} (for loop)", f.var.0));
            walk_block_for_scope(&f.body, offset, scope)
        }
        ast::Stmt::For(f)
            if span_contains(&f.var.1, offset)
                || span_contains(&f.start.span(), offset)
                || span_contains(&f.end.span(), offset)
                || f.step
                    .as_ref()
                    .is_some_and(|s| span_contains(&s.span(), offset)) =>
        {
            true
        }
        ast::Stmt::Match(m) => {
            if span_contains(&m.scrutinee.span(), offset) {
                return true;
            }
            for arm in &m.arms {
                if offset >= arm.body.span.start && offset < arm.body.span.end {
                    return walk_block_for_scope(&arm.body, offset, scope);
                }
            }
            false
        }
        ast::Stmt::Expr(e) | ast::Stmt::Return(ast::ReturnStmt { value: Some(e) })
            if span_contains(&e.span(), offset) =>
        {
            walk_expr_for_scope(e, offset, scope)
        }
        ast::Stmt::Assign(a) if span_contains(&a.value.span(), offset) => {
            walk_expr_for_scope(&a.value, offset, scope)
        }
        _ => false,
    }
}

fn stmt_start(stmt: &ast::Stmt) -> usize {
    match stmt {
        ast::Stmt::VarDecl(v) => v.name.1.start,
        ast::Stmt::Assign(a) => lvalue_start(&a.target),
        ast::Stmt::CompoundAssign(a) => lvalue_start(&a.target),
        ast::Stmt::Expr(e) => e.span().start,
        ast::Stmt::If(i) => i.cond.span().start,
        ast::Stmt::Loop(l) => l.body.span.start,
        ast::Stmt::While(w) => w.cond.span().start,
        ast::Stmt::For(f) => f.var.1.start,
        ast::Stmt::Return(r) => r.value.as_ref().map_or(0, |e| e.span().start),
        ast::Stmt::Break(span)
        | ast::Stmt::Continue(span)
        | ast::Stmt::Block(ast::Block { span, .. })
        | ast::Stmt::Asm(ast::AsmStmt { span, .. }) => span.start,
        ast::Stmt::Match(m) => m.scrutinee.span().start,
        ast::Stmt::Assume(a) => a.span.start,
        ast::Stmt::Assert(a) => a.span.start,
    }
}

fn lvalue_start(lvalue: &ast::LValue) -> usize {
    match lvalue {
        ast::LValue::Name((_, span)) => span.start,
        ast::LValue::Field(base, _) | ast::LValue::Index(base, _) => lvalue_start(base),
        ast::LValue::Deref(expr) => expr.span().start,
    }
}

fn expr_end(expr: &ast::Expr) -> usize {
    match expr {
        ast::Expr::Unary(_, inner) | ast::Expr::Cast(inner, _) | ast::Expr::Group(inner) => {
            expr_end(inner)
        }
        ast::Expr::Binary(left, _, right) => expr_end(left).max(expr_end(right)),
        ast::Expr::Call(func, args) => args.iter().map(expr_end).fold(expr_end(func), usize::max),
        ast::Expr::FieldAccess(base, (_, span)) => expr_end(base).max(span.end),
        ast::Expr::Index(base, index) => expr_end(base).max(expr_end(index)),
        ast::Expr::ArrayInit(_, span)
        | ast::Expr::StructInit { span, .. }
        | ast::Expr::EnumVariant { span, .. } => span.end,
        ast::Expr::Block(block) => block.span.end,
        ast::Expr::Match(m) => m.span.end,
        ast::Expr::If(i) => i.span.end,
        _ => expr.span().end,
    }
}

fn walk_expr_for_scope(expr: &ast::Expr, offset: usize, scope: &mut CompletionScope) -> bool {
    match expr {
        ast::Expr::Block(b) => walk_block_for_scope(&b.block, offset, scope),
        ast::Expr::If(i) => {
            if span_contains(&i.cond.span(), offset) {
                true
            } else if offset >= i.then_block.span.start && offset < i.then_block.span.end {
                walk_block_for_scope(&i.then_block, offset, scope)
            } else {
                walk_expr_for_scope(&i.else_branch, offset, scope)
            }
        }
        ast::Expr::Match(m) => {
            if span_contains(&m.scrutinee.span(), offset) {
                return true;
            }
            for arm in &m.arms {
                if offset >= arm.body.span.start && offset < arm.body.span.end {
                    return walk_block_for_scope(&arm.body, offset, scope);
                }
            }
            true
        }
        _ => true,
    }
}

const BML_KEYWORDS: &[&str] = &[
    "fn",
    "var",
    "val",
    "static",
    "const",
    "peripheral",
    "reg",
    "field",
    "import",
    "export",
    "as",
    "asm",
    "if",
    "else",
    "loop",
    "while",
    "for",
    "return",
    "break",
    "continue",
    "match",
    "enum",
    "struct",
    "extern",
    "i8",
    "i16",
    "i32",
    "i64",
    "u8",
    "u16",
    "u32",
    "u64",
    "f16",
    "f32",
    "f64",
    "b1",
    "b8",
    "void",
];

fn find_const_value(name: &str, program: &ast::Program) -> Option<String> {
    for item in &program.items {
        if let ast::Item::ConstDef(c) = item
            && c.name.0 == name
        {
            return Some(expr_to_string(&c.value));
        }
    }
    None
}

fn expr_to_string(expr: &ast::Expr) -> String {
    match expr {
        ast::Expr::IntLiteral(v, _, _) => v.to_string(),
        ast::Expr::FloatLiteral(v, _, _) => v.to_string(),
        ast::Expr::BoolLiteral(v, _) => v.to_string(),
        ast::Expr::StringLiteral(v, _) => format!("\"{v}\""),
        ast::Expr::NullLiteral(_) => "null".to_string(),
        ast::Expr::Ident((name, _)) => name.clone(),
        ast::Expr::Unary(op, inner) => {
            let s = expr_to_string(inner);
            match op {
                ast::UnaryOp::Neg => format!("-({s})"),
                ast::UnaryOp::Not => format!("!({s})"),
                ast::UnaryOp::BitNot => format!("~({s})"),
                ast::UnaryOp::Deref => format!("*({s})"),
                ast::UnaryOp::AddrOf => format!("&({s})"),
                ast::UnaryOp::AddrOfMut => format!("&mut ({s})"),
            }
        }
        ast::Expr::Binary(lhs, op, rhs) => {
            let op_str = match op {
                ast::BinaryOp::Add => "+",
                ast::BinaryOp::Sub => "-",
                ast::BinaryOp::Mul => "*",
                ast::BinaryOp::Div => "/",
                ast::BinaryOp::Mod => "%",
                ast::BinaryOp::And => "&&",
                ast::BinaryOp::Or => "||",
                ast::BinaryOp::BitAnd => "&",
                ast::BinaryOp::BitOr => "|",
                ast::BinaryOp::BitXor => "^",
                ast::BinaryOp::Shl => "<<",
                ast::BinaryOp::Shr => ">>",
                ast::BinaryOp::Eq => "==",
                ast::BinaryOp::NotEq => "!=",
                ast::BinaryOp::Lt => "<",
                ast::BinaryOp::Gt => ">",
                ast::BinaryOp::LtEq => "<=",
                ast::BinaryOp::GtEq => ">=",
            };
            format!("{} {} {}", expr_to_string(lhs), op_str, expr_to_string(rhs))
        }
        ast::Expr::Cast(inner, ty) => format!("{} as {}", expr_to_string(inner), ty),
        ast::Expr::FieldAccess(base, (name, _)) => format!("{}.{}", expr_to_string(base), name),
        ast::Expr::Group(inner) => format!("({})", expr_to_string(inner)),
        _ => "...".to_string(),
    }
}

fn find_call_at(program: &ast::Program, file: source::FileId, offset: usize) -> Option<ast::Expr> {
    for item in &program.items {
        if let ast::Item::FnDef(f) = item
            && f.name.1.file == file
            && let Some(call) = find_call_in_block(&f.body, offset)
        {
            return Some(call);
        }
    }
    None
}

fn find_call_in_block(block: &ast::Block, offset: usize) -> Option<ast::Expr> {
    for stmt in &block.stmts {
        if let Some(call) = find_call_in_stmt(stmt, offset) {
            return Some(call);
        }
    }
    block
        .trailing
        .as_ref()
        .and_then(|e| find_call_in_expr(e, offset))
}

fn find_call_in_stmt(stmt: &ast::Stmt, offset: usize) -> Option<ast::Expr> {
    match stmt {
        ast::Stmt::Expr(e) => find_call_in_expr(e, offset),
        ast::Stmt::VarDecl(v) => find_call_in_expr(&v.init, offset),
        ast::Stmt::Assign(a) => find_call_in_expr(&a.value, offset),
        ast::Stmt::Return(r) => r.value.as_ref().and_then(|e| find_call_in_expr(e, offset)),
        ast::Stmt::If(i) => find_call_in_expr(&i.cond, offset)
            .or_else(|| find_call_in_block(&i.then_block, offset))
            .or_else(|| {
                i.else_branch
                    .as_ref()
                    .and_then(|s| find_call_in_stmt(s, offset))
            }),
        ast::Stmt::Loop(l) => find_call_in_block(&l.body, offset),
        ast::Stmt::While(w) => {
            find_call_in_expr(&w.cond, offset).or_else(|| find_call_in_block(&w.body, offset))
        }
        ast::Stmt::For(f) => find_call_in_expr(&f.start, offset)
            .or_else(|| find_call_in_expr(&f.end, offset))
            .or_else(|| f.step.as_ref().and_then(|s| find_call_in_expr(s, offset)))
            .or_else(|| find_call_in_block(&f.body, offset)),
        ast::Stmt::Match(m) => find_call_in_expr(&m.scrutinee, offset).or_else(|| {
            m.arms
                .iter()
                .find_map(|arm| find_call_in_block(&arm.body, offset))
        }),
        ast::Stmt::Block(b) => find_call_in_block(b, offset),
        ast::Stmt::CompoundAssign(a) => find_call_in_expr(&a.value, offset),
        ast::Stmt::Asm(a) => a
            .outputs
            .iter()
            .chain(a.inputs.iter())
            .find_map(|(_, e)| find_call_in_expr(e, offset)),
        ast::Stmt::Assume(a) => find_call_in_expr(&a.cond, offset),
        ast::Stmt::Assert(a) => find_call_in_expr(&a.cond, offset),
        ast::Stmt::Break(_) | ast::Stmt::Continue(_) => None,
    }
}

fn find_call_in_expr(expr: &ast::Expr, offset: usize) -> Option<ast::Expr> {
    if !span_contains(&expr.span(), offset) {
        return None;
    }
    match expr {
        ast::Expr::Call(f, _) => {
            if span_contains(&f.span(), offset) {
                // Cursor is on the function name -- return the whole call
                Some(expr.clone())
            } else if span_contains(&expr.span(), offset) {
                // Cursor somewhere in the call args
                Some(expr.clone())
            } else {
                None
            }
        }
        ast::Expr::Binary(l, _, r) => {
            find_call_in_expr(l, offset).or_else(|| find_call_in_expr(r, offset))
        }
        ast::Expr::Unary(_, e) => find_call_in_expr(e, offset),
        ast::Expr::Cast(e, _) => find_call_in_expr(e, offset),
        ast::Expr::Group(e) => find_call_in_expr(e, offset),
        ast::Expr::FieldAccess(e, _) => find_call_in_expr(e, offset),
        ast::Expr::Index(base, index) => {
            find_call_in_expr(base, offset).or_else(|| find_call_in_expr(index, offset))
        }
        ast::Expr::ArrayInit(elems, _) => elems.iter().find_map(|e| find_call_in_expr(e, offset)),
        ast::Expr::StructInit { fields, .. } => fields
            .iter()
            .find_map(|(_, e)| find_call_in_expr(e, offset)),
        ast::Expr::Match(m) => find_call_in_expr(&m.scrutinee, offset).or_else(|| {
            m.arms
                .iter()
                .find_map(|arm| find_call_in_block(&arm.body, offset))
        }),
        ast::Expr::Block(b) => find_call_in_block(&b.block, offset),
        ast::Expr::If(i) => find_call_in_expr(&i.cond, offset)
            .or_else(|| find_call_in_block(&i.then_block, offset))
            .or_else(|| find_call_in_expr(&i.else_branch, offset)),
        ast::Expr::ViewNew {
            base, len, stride, ..
        } => find_call_in_expr(base, offset)
            .or_else(|| len.as_ref().and_then(|e| find_call_in_expr(e, offset)))
            .or_else(|| stride.as_ref().and_then(|e| find_call_in_expr(e, offset))),
        ast::Expr::RingNew {
            base,
            capacity,
            head,
            len,
            ..
        } => find_call_in_expr(base, offset)
            .or_else(|| capacity.as_ref().and_then(|e| find_call_in_expr(e, offset)))
            .or_else(|| find_call_in_expr(head, offset))
            .or_else(|| find_call_in_expr(len, offset)),
        ast::Expr::BitNew {
            base,
            bit_offset,
            len_bits,
            ..
        } => find_call_in_expr(base, offset)
            .or_else(|| {
                bit_offset
                    .as_ref()
                    .and_then(|e| find_call_in_expr(e, offset))
            })
            .or_else(|| len_bits.as_ref().and_then(|e| find_call_in_expr(e, offset))),
        _ => None,
    }
}

fn format_call_args(
    call: &ast::Expr,
    fn_sym: &resolver::FnSymbol,
    _fn_name: &str,
) -> Option<String> {
    let ast::Expr::Call(_, args) = call else {
        return None;
    };

    let mut lines = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let arg_str = expr_to_string(arg);
        if let Some((param_name, _param_ty)) = fn_sym.params.get(i) {
            lines.push(format!("  {param_name} = {arg_str}"));
        } else if i >= fn_sym.params.len() {
            lines.push(format!("  [extra] = {arg_str}"));
        }
    }
    if args.len() < fn_sym.params.len() {
        for i in args.len()..fn_sym.params.len() {
            let (param_name, _param_ty) = &fn_sym.params[i];
            lines.push(format!("  {param_name} = _ (missing)"));
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn completion_labels(source: &str) -> Vec<String> {
        let marker = "$0";
        let offset = source
            .find(marker)
            .expect("source must contain cursor marker");
        let source = source.replace(marker, "");
        let (analysis, diags) = analyze_file(
            Path::new("/tmp/completion_test.bml"),
            &source,
            &mut ModuleCache::default(),
        );
        assert!(!diags.has_errors());
        collect_completions(
            &analysis.program,
            &analysis.symbols,
            analysis.root_file_id,
            offset,
        )
        .into_iter()
        .map(|item| item.label)
        .collect()
    }

    fn contains_label(labels: &[String], label: &str) -> bool {
        labels.iter().any(|item| item == label)
    }

    #[test]
    fn position_encoding_defaults_to_utf16() {
        let params = InitializeParams::default();
        assert_eq!(
            select_position_encoding(&params),
            Some(PositionEncodingKind::UTF16)
        );
    }

    #[test]
    fn position_encoding_prefers_utf8_when_advertised() {
        let mut params = InitializeParams::default();
        params.capabilities.general = Some(lsp_types::GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF16,
                PositionEncodingKind::UTF8,
            ]),
            ..Default::default()
        });

        assert_eq!(
            select_position_encoding(&params),
            Some(PositionEncodingKind::UTF8)
        );
    }

    #[test]
    fn position_conversions_handle_utf8_and_utf16() {
        let source = "fn café() {\n    café\n}\n";
        let byte_offset = source.find("() {").expect("function name suffix exists");

        assert_eq!(
            offset_to_pos(source, byte_offset, &PositionEncodingKind::UTF8),
            Position {
                line: 0,
                character: 8,
            }
        );
        assert_eq!(
            offset_to_pos(source, byte_offset, &PositionEncodingKind::UTF16),
            Position {
                line: 0,
                character: 7,
            }
        );

        assert_eq!(
            pos_to_offset(
                source,
                Position {
                    line: 0,
                    character: 8,
                },
                &PositionEncodingKind::UTF8,
            ),
            byte_offset
        );
        assert_eq!(
            pos_to_offset(
                source,
                Position {
                    line: 0,
                    character: 7,
                },
                &PositionEncodingKind::UTF16,
            ),
            byte_offset
        );
    }

    #[test]
    fn completion_includes_function_local() {
        let labels = completion_labels(
            r"
fn main() {
    val local: u32 = 1u32;
    $0
}
",
        );

        assert!(contains_label(&labels, "local"));
    }

    #[test]
    fn completion_excludes_local_declared_after_cursor() {
        let labels = completion_labels(
            r"
fn main() {
    $0
    val later: u32 = 1u32;
}
",
        );

        assert!(!contains_label(&labels, "later"));
    }

    #[test]
    fn completion_excludes_declared_local_inside_own_initializer() {
        let labels = completion_labels(
            r"
fn main() {
    val local: u32 = $01u32;
}
",
        );

        assert!(!contains_label(&labels, "local"));
    }

    #[test]
    fn completion_excludes_local_from_sibling_block() {
        let labels = completion_labels(
            r"
fn main() {
    {
        val hidden: u32 = 1u32;
    }
    $0
}
",
        );

        assert!(!contains_label(&labels, "hidden"));
    }

    #[test]
    fn completion_includes_nested_block_locals() {
        let labels = completion_labels(
            r"
fn main() {
    val outer: u32 = 1u32;
    {
        val inner: u32 = 2u32;
        $0
    }
}
",
        );

        assert!(contains_label(&labels, "outer"));
        assert!(contains_label(&labels, "inner"));
    }

    #[test]
    fn completion_includes_for_loop_variable() {
        let labels = completion_labels(
            r"
fn main() {
    for i: u32 in 0 upto 2 {
        $0
    }
}
",
        );

        assert!(contains_label(&labels, "i"));
    }

    // ─── hover / goto-definition resolution ────────────────────────────

    /// Parse + analyze a snippet containing a single `$0` cursor marker and
    /// return the analysis plus the byte offset where the marker stood.
    /// Asserts the snippet is free of diagnostics.
    fn analyze_at(source: &str) -> (AnalysisResult, usize) {
        let (analysis, offset, diags) = parse_at(source);
        assert!(!diags.has_errors(), "unexpected diagnostics");
        (analysis, offset)
    }

    /// Like `analyze_at` but does not assert on diagnostics, for snippets whose
    /// AST we want even if later checker stages reject them.
    fn parse_at(source: &str) -> (AnalysisResult, usize, DiagnosticBag) {
        let marker = "$0";
        let offset = source
            .find(marker)
            .expect("source must contain cursor marker");
        let source = source.replace(marker, "");
        let (analysis, diags) = analyze_file(
            Path::new("/tmp/lsp_resolve_test.bml"),
            &source,
            &mut ModuleCache::default(),
        );
        (analysis, offset, diags)
    }

    #[test]
    fn definition_resolves_local_in_enclosing_fn() {
        // Both functions declare `x`; the cursor is on the second's use of it.
        let src = "\
fn first() {
    val x: u32 = 1u32;
    val a: u32 = x;
}
fn second() {
    val x: u32 = 2u32;
    val b: u32 = $0x;
}
";
        let (analysis, offset) = analyze_at(src);
        let ident = find_ident_at(&analysis.program, analysis.root_file_id, offset)
            .expect("ident at cursor");
        assert_eq!(ident.0, "x");

        let span = find_definition_span("x", &analysis.program, analysis.root_file_id, offset)
            .expect("definition resolved");
        let clean = src.replace("$0", "");
        // The `x` of the *second* `val x` declaration, not the first.
        let second_decl = clean.match_indices("val x").nth(1).expect("two decls").0 + "val ".len();
        assert_eq!(span.start, second_decl);
    }

    #[test]
    fn hover_resolves_enclosing_fn_param_and_local() {
        let src = "\
fn main(p: u32) {
    val local_v: u32 = 1u32;
    val b: u32 = $0local_v;
}
";
        let (analysis, offset) = analyze_at(src);
        let f =
            enclosing_fn(&analysis.program, analysis.root_file_id, offset).expect("enclosing fn");
        assert_eq!(
            local_type_in_fn(f, "local_v").as_deref(),
            Some("local_v: u32")
        );
        assert_eq!(local_type_in_fn(f, "p").as_deref(), Some("p: u32"));
    }

    #[test]
    fn definition_found_in_compound_assign_rhs() {
        let src = "\
fn helper() -> u32 {
    return 1u32;
}
fn main() {
    var x: u32 = 0u32;
    x += $0helper();
}
";
        let (analysis, offset) = analyze_at(src);
        let ident = find_ident_at(&analysis.program, analysis.root_file_id, offset)
            .expect("ident in compound-assign");
        assert_eq!(ident.0, "helper");

        let span = find_definition_span("helper", &analysis.program, analysis.root_file_id, offset)
            .expect("definition resolved");
        let clean = src.replace("$0", "");
        // First "helper" is the function definition's name.
        assert_eq!(span.start, clean.find("helper").expect("fn name"));
    }

    #[test]
    fn ident_found_in_view_constructor() {
        let src = "\
fn main() @context(thread) {
    var buf: [u32; 4] = [0u32, 0u32, 0u32, 0u32];
    val v: view u32 = view($0buf);
}
";
        let (analysis, offset) = analyze_at(src);
        let ident = find_ident_at(&analysis.program, analysis.root_file_id, offset)
            .expect("ident in view ctor");
        assert_eq!(ident.0, "buf");

        let span = find_definition_span("buf", &analysis.program, analysis.root_file_id, offset)
            .expect("definition resolved");
        let clean = src.replace("$0", "");
        // First "buf" is the `var buf` declaration.
        assert_eq!(span.start, clean.find("buf").expect("buf decl"));
    }

    #[test]
    fn ident_found_in_index_lvalue() {
        let src = "\
fn main() @context(thread) {
    var arr: [u32; 4] = [0u32, 0u32, 0u32, 0u32];
    val idx: u32 = 1u32;
    arr[$0idx] = 5u32;
}
";
        let (analysis, offset) = analyze_at(src);
        let ident = find_ident_at(&analysis.program, analysis.root_file_id, offset)
            .expect("ident in index lvalue");
        assert_eq!(ident.0, "idx");
    }

    /// Regression: after import resolution `program.items` inlines items from
    /// every imported module, each with byte offsets local to its own file.
    /// Position lookups must restrict to the requested file, otherwise an offset
    /// in main.bml collides with a same-offset span in an imported module and
    /// goto-definition jumps into the wrong file. See the NUCLEO example where
    /// goto on a local `board_init()` call landed in an imported SVD module.
    #[test]
    fn definition_does_not_cross_into_imported_file() {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("bml_lsp_scope_{}_{nonce}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let main_src = "\
import biglib;

fn board_init() {
}

fn main() @context(thread) {
    board_init();
}
";
        // Byte offset of the *call* to `board_init`. The definition is followed
        // by ` {`, so the `();` form matches only the call site.
        let call_off = main_src.find("board_init();").expect("call site");

        // Craft an imported module whose `imported_pad` definition-name starts
        // at exactly `call_off` in its own file: a leading comment pads the
        // prefix so `// + filler + \n + "fn "` is `call_off` bytes long. With
        // offset-only matching, find_ident_at would return this imported name
        // when the cursor is on the call in main.bml.
        let mut biglib = String::from("//");
        biglib.push_str(&"x".repeat(call_off - 6));
        biglib.push('\n');
        biglib.push_str("fn imported_pad() {\n}\n");
        assert_eq!(
            biglib.find("imported_pad"),
            Some(call_off),
            "scaffolding: imported name must collide with the call offset"
        );

        let main_path = dir.join("main.bml");
        fs::write(&main_path, main_src).unwrap();
        fs::write(dir.join("biglib.bml"), &biglib).unwrap();

        let (analysis, diags) = analyze_file(&main_path, main_src, &mut ModuleCache::default());
        assert!(!diags.has_errors(), "snippet should analyze cleanly");

        let ident = find_ident_at(&analysis.program, analysis.root_file_id, call_off)
            .expect("ident at call site");
        assert_eq!(
            ident.0, "board_init",
            "must resolve the call in main.bml, not the colliding imported name"
        );

        let span = find_definition_span(
            "board_init",
            &analysis.program,
            analysis.root_file_id,
            call_off,
        )
        .expect("definition resolved");
        assert_eq!(
            span.file, analysis.root_file_id,
            "definition must stay in main.bml"
        );
        assert_eq!(
            span.start,
            main_src.find("fn board_init").unwrap() + "fn ".len()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    // ─── persistent import parse cache ─────────────────────────────────

    /// The `FileId` of the inlined `helper` definition in a resolved program.
    fn helper_file(a: &AnalysisResult) -> source::FileId {
        for item in &a.program.items {
            if let ast::Item::FnDef(f) = item
                && f.name.0 == "helper"
            {
                return f.name.1.file;
            }
        }
        panic!("`helper` not inlined into resolved program");
    }

    #[test]
    fn module_cache_reuses_import_parse_across_analyses() {
        use std::fs;
        use std::time::{SystemTime, UNIX_EPOCH};

        // Unique temp dir so parallel/repeat runs don't collide.
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("bml_lsp_cache_{}_{nonce}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let lib_path = dir.join("helper_lib.bml");
        fs::write(
            &lib_path,
            "export fn helper;\n\nfn helper() -> u32 @context(thread) {\n    return 1;\n}\n",
        )
        .unwrap();

        let main_path = dir.join("main.bml");
        let main_src = "\
import helper_lib { helper };

fn main() @context(thread) {
    var x = helper();
}
";
        fs::write(&main_path, main_src).unwrap();

        let mut cache = ModuleCache::default();

        let (a1, d1) = analyze_file(&main_path, main_src, &mut cache);
        assert!(!d1.has_errors(), "first analysis should be clean");
        let f1 = helper_file(&a1);
        assert_eq!(a1.source_map.get_path(f1), lib_path.as_path());

        let (a2, d2) = analyze_file(&main_path, main_src, &mut cache);
        assert!(!d2.has_errors(), "second analysis should be clean");
        let f2 = helper_file(&a2);

        // A cache miss would re-`add_file` and mint a fresh FileId; equality
        // proves the cached parse (and its SourceFile) was reused and that its
        // spans stay valid in the second analysis's fresh SourceMap.
        assert_eq!(f1, f2, "imported parse should be reused from the cache");
        assert_eq!(a2.source_map.get_path(f2), lib_path.as_path());

        let _ = fs::remove_dir_all(&dir);
    }
}
