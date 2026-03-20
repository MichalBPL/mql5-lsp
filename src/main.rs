mod builtins;
mod documents;
mod includes;
mod parser;
mod symbols;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use builtins::*;
use documents::DocumentStore;
use includes::IncludeResolver;
use parser::CompletionContext;
use symbols::SymbolIndex;

struct Mql5Lsp {
    client: Client,
    index: Arc<RwLock<SymbolIndex>>,
    documents: Arc<DocumentStore>,
    include_resolver: Arc<RwLock<IncludeResolver>>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Mql5Lsp {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Determine workspace root
        let workspace_root = params
            .workspace_folders
            .as_ref()
            .and_then(|f| f.first())
            .and_then(|f| f.uri.to_file_path().ok())
            .or_else(|| {
                params
                    .root_uri
                    .as_ref()
                    .and_then(|u| u.to_file_path().ok())
            });

        if let Some(ref root) = workspace_root {
            // Detect MQL5 include root
            let include_root;
            {
                let mut resolver = self.include_resolver.write().await;
                resolver.detect_include_root(root);
                include_root = resolver.include_root().cloned();
            }

            // Index workspace files
            {
                let mut index = self.index.write().await;
                index.scan_directory(root);
                log::info!(
                    "Workspace: {} files, {} symbols",
                    index.file_count(),
                    index.symbol_count()
                );

                // Also index MQL5 stdlib (Include/) for go-to-definition
                // and autocomplete of standard library classes
                if let Some(ref inc_root) = include_root {
                    index.scan_directory(inc_root);
                    log::info!(
                        "After stdlib: {} files, {} symbols",
                        index.file_count(),
                        index.symbol_count()
                    );
                }
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::INCREMENTAL,
                )),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_string(), ":".to_string()]),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                definition_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                })),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_string(), ",".to_string()]),
                    retrigger_characters: None,
                    work_done_progress_options: WorkDoneProgressOptions {
                        work_done_progress: None,
                    },
                }),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "mql5-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    // ── Document Sync ──

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let text = params.text_document.text.clone();
        let version = params.text_document.version;

        self.documents.open(uri.clone(), text.clone(), version);

        // Re-index this file
        if let Ok(path) = uri.to_file_path() {
            let mut index = self.index.write().await;
            index.rescan_file(&path, Some(&text));

            // Also index included files
            self.index_includes(&path, &text, &mut index).await;
        }

        // Publish diagnostics
        self.publish_diagnostics_for(&uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.clone();
        let version = params.text_document.version;

        self.documents
            .apply_changes(&uri, params.content_changes, version);

        // Re-index from updated content
        if let Ok(path) = uri.to_file_path() {
            if let Some(text) = self.documents.get_text(&uri) {
                let mut index = self.index.write().await;
                index.rescan_file(&path, Some(&text));
            }
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            let text = self.documents.get_text(&uri);
            let mut index = self.index.write().await;
            index.rescan_file(&path, text.as_deref());

            // Re-index includes on save
            let source = text
                .or_else(|| std::fs::read_to_string(&path).ok())
                .unwrap_or_default();
            self.index_includes(&path, &source, &mut index).await;
        }

        // Publish diagnostics on save
        self.publish_diagnostics_for(&uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        // Clear diagnostics when closing
        self.client
            .publish_diagnostics(params.text_document.uri.clone(), vec![], None)
            .await;
        self.documents.close(&params.text_document.uri);
    }

    // ── Completion ──

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let line = pos.line as usize;
        let col = pos.character as usize;

        // Get source content
        let source = self.get_source(uri);
        let source = match source {
            Some(s) => s,
            None => return Ok(None),
        };

        let context = parser::get_completion_context(&source, line, col);
        let mut items: Vec<CompletionItem> = Vec::new();

        match context {
            CompletionContext::DotAccess { object_text } => {
                // Find the type of the object and show its members
                self.complete_dot_access(&object_text, uri, &source, &mut items)
                    .await;
            }
            CompletionContext::ScopeResolution { scope_text } => {
                // Show enum values or class static members
                self.complete_scope_resolution(&scope_text, &mut items)
                    .await;
            }
            CompletionContext::General => {
                // Show everything: builtins + workspace symbols
                self.complete_general(uri, &mut items).await;
            }
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    // ── Hover ──

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let line = pos.line as usize;
        let col = pos.character as usize;

        let source = self.get_source(uri);
        let word = source.and_then(|s| parser::extract_word_at(&s, line, col));

        let word = match word {
            Some(w) => w,
            None => return Ok(None),
        };

        // Check builtins first
        if let Some(func) = find_function(&word) {
            let mut value = format!("```mql5\n{}\n```", func.signature);
            if let Some(doc) = func.doc {
                value.push_str(&format!("\n\n{}", doc));
            }
            return Ok(Some(make_hover(value)));
        }

        if let Some(e) = find_enum(&word) {
            let values_preview: Vec<&str> = e.values.iter().take(10).copied().collect();
            let mut value = format!("```mql5\nenum {}\n```", e.name);
            value.push_str(&format!("\n\nValues: `{}`", values_preview.join("`, `")));
            if e.values.len() > 10 {
                value.push_str(&format!(" ... ({} total)", e.values.len()));
            }
            return Ok(Some(make_hover(value)));
        }

        if let Some(s) = find_struct(&word) {
            let mut value = format!("```mql5\nstruct {}\n```\n\nFields:", s.name);
            for (fname, ftype) in s.fields {
                value.push_str(&format!("\n- `{} {}`", ftype, fname));
            }
            return Ok(Some(make_hover(value)));
        }

        if let Some(c) = find_constant(&word) {
            let mut value = format!("```mql5\n#define {} {}\n```", c.name, c.value);
            if let Some(doc) = c.doc {
                value.push_str(&format!("\n\n{}", doc));
            }
            return Ok(Some(make_hover(value)));
        }

        // Check if it's a builtin enum value
        for e in BUILTIN_ENUMS {
            if e.values.contains(&word.as_str()) {
                return Ok(Some(make_hover(format!(
                    "```mql5\n{}::{}\n```\n\nMember of `enum {}`",
                    e.name, word, e.name
                ))));
            }
        }

        // Check workspace symbols
        let index = self.index.read().await;
        if let Some(sym) = index.find_symbol(&word) {
            let file_info = sym
                .uri
                .to_file_path()
                .ok()
                .map(|p| {
                    // Show relative path if possible
                    p.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_default();

            let value = format!(
                "```mql5\n{}\n```\n\nDefined in `{}`",
                sym.detail, file_info
            );
            return Ok(Some(make_hover(value)));
        }

        Ok(None)
    }

    // ── Go to Definition ──

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let line = pos.line as usize;
        let col = pos.character as usize;

        let source = self.get_source(uri);
        let word = source.and_then(|s| parser::extract_word_at(&s, line, col));

        let word = match word {
            Some(w) => w,
            None => return Ok(None),
        };

        let index = self.index.read().await;

        // Find all definitions
        let matches = index.find_symbols(&word);
        if matches.is_empty() {
            return Ok(None);
        }

        if matches.len() == 1 {
            return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                uri: matches[0].uri.clone(),
                range: matches[0].range,
            })));
        }

        // Multiple definitions — return all
        let locations: Vec<Location> = matches
            .into_iter()
            .map(|s| Location {
                uri: s.uri.clone(),
                range: s.range,
            })
            .collect();

        Ok(Some(GotoDefinitionResponse::Array(locations)))
    }

    // ── Find All References ──

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let line = pos.line as usize;
        let col = pos.character as usize;

        let source = self.get_source(uri);
        let word = source.and_then(|s| parser::extract_word_at(&s, line, col));

        let word = match word {
            Some(w) => w,
            None => return Ok(None),
        };

        let index = self.index.read().await;
        let mut locations: Vec<Location> = Vec::new();

        // Include definitions if requested
        if params.context.include_declaration {
            for sym in index.find_symbols(&word) {
                locations.push(Location {
                    uri: sym.uri.clone(),
                    range: sym.range,
                });
            }
        }

        // Include all references (usages)
        for reference in index.find_references(&word) {
            let loc = Location {
                uri: reference.uri.clone(),
                range: reference.range,
            };
            // Avoid duplicates with definitions
            if !locations.iter().any(|l| l.uri == loc.uri && l.range == loc.range) {
                locations.push(loc);
            }
        }

        if locations.is_empty() {
            Ok(None)
        } else {
            Ok(Some(locations))
        }
    }

    // ── Rename ──

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = &params.text_document.uri;
        let pos = params.position;
        let line = pos.line as usize;
        let col = pos.character as usize;

        let source = self.get_source(uri);
        let source = match source {
            Some(s) => s,
            None => return Ok(None),
        };

        // Find the word at cursor and its range
        let target_line = match source.lines().nth(line) {
            Some(l) => l,
            None => return Ok(None),
        };
        let bytes = target_line.as_bytes();
        if col >= bytes.len() {
            return Ok(None);
        }

        let mut start = col;
        while start > 0 && is_ident_byte(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = col;
        while end < bytes.len() && is_ident_byte(bytes[end]) {
            end += 1;
        }

        if start == end {
            return Ok(None);
        }

        let word = &target_line[start..end];

        // Check that the symbol exists (either as a definition or a reference)
        let index = self.index.read().await;
        let has_definition = !index.find_symbols(word).is_empty();
        let has_references = !index.find_references(word).is_empty();

        if !has_definition && !has_references {
            // Also check builtins — we cannot rename builtins
            if find_function(word).is_some()
                || find_enum(word).is_some()
                || find_struct(word).is_some()
                || find_constant(word).is_some()
            {
                return Ok(None); // Cannot rename builtins
            }
            return Ok(None);
        }

        Ok(Some(PrepareRenameResponse::Range(Range {
            start: Position {
                line: pos.line,
                character: start as u32,
            },
            end: Position {
                line: pos.line,
                character: end as u32,
            },
        })))
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = &params.text_document_position.text_document.uri;
        let pos = params.text_document_position.position;
        let line = pos.line as usize;
        let col = pos.character as usize;
        let new_name = &params.new_name;

        let source = self.get_source(uri);
        let word = source.and_then(|s| parser::extract_word_at(&s, line, col));

        let word = match word {
            Some(w) => w,
            None => return Ok(None),
        };

        let index = self.index.read().await;

        // Collect all locations to rename: definitions + references
        let mut file_edits: HashMap<Url, Vec<TextEdit>> = HashMap::new();

        // Definitions
        for sym in index.find_symbols(&word) {
            file_edits
                .entry(sym.uri.clone())
                .or_default()
                .push(TextEdit {
                    range: sym.range,
                    new_text: new_name.clone(),
                });
        }

        // References
        for reference in index.find_references(&word) {
            file_edits
                .entry(reference.uri.clone())
                .or_default()
                .push(TextEdit {
                    range: reference.range,
                    new_text: new_name.clone(),
                });
        }

        if file_edits.is_empty() {
            return Ok(None);
        }

        let changes: HashMap<Url, Vec<TextEdit>> = file_edits;
        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        }))
    }

    // ── Signature Help ──

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let uri = &params.text_document_position_params.text_document.uri;
        let pos = params.text_document_position_params.position;
        let line = pos.line as usize;
        let col = pos.character as usize;

        let source = match self.get_source(uri) {
            Some(s) => s,
            None => return Ok(None),
        };

        // Find function name and active parameter by scanning backwards from cursor
        let (func_name, active_param) = match find_function_call_context(&source, line, col) {
            Some(ctx) => ctx,
            None => return Ok(None),
        };

        // Look up the function signature
        let signature_str;
        let doc_str;

        if let Some(func) = find_function(&func_name) {
            signature_str = func.signature.to_string();
            doc_str = func.doc.map(|d| d.to_string());
        } else {
            // Check workspace symbols
            let index = self.index.read().await;
            let sym = index.find_symbol(&func_name);
            match sym {
                Some(s)
                    if matches!(
                        s.kind,
                        parser::ParsedSymbolKind::Function | parser::ParsedSymbolKind::Method
                    ) =>
                {
                    signature_str = s.detail.clone();
                    doc_str = None;
                }
                _ => return Ok(None),
            }
        }

        // Parse parameters from signature
        let params_list = parse_signature_params(&signature_str);

        let param_infos: Vec<ParameterInformation> = params_list
            .iter()
            .map(|p| ParameterInformation {
                label: ParameterLabel::Simple(p.clone()),
                documentation: None,
            })
            .collect();

        let sig = SignatureInformation {
            label: signature_str,
            documentation: doc_str.map(|d| {
                Documentation::MarkupContent(MarkupContent {
                    kind: MarkupKind::Markdown,
                    value: d,
                })
            }),
            parameters: Some(param_infos),
            active_parameter: Some(active_param as u32),
        };

        Ok(Some(SignatureHelp {
            signatures: vec![sig],
            active_signature: Some(0),
            active_parameter: Some(active_param as u32),
        }))
    }

    // ── Document Symbols ──

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;

        let source = self.get_source(uri);
        let source = match source {
            Some(s) => s,
            None => return Ok(None),
        };

        let tree = match parser::parse(&source) {
            Some(t) => t,
            None => return Ok(None),
        };

        let parsed = parser::extract_symbols(&source, &tree);
        let items: Vec<SymbolInformation> = parsed
            .into_iter()
            .map(|s| {
                let kind = match s.kind {
                    parser::ParsedSymbolKind::Function => SymbolKind::FUNCTION,
                    parser::ParsedSymbolKind::Class => SymbolKind::CLASS,
                    parser::ParsedSymbolKind::Struct => SymbolKind::STRUCT,
                    parser::ParsedSymbolKind::Enum => SymbolKind::ENUM,
                    parser::ParsedSymbolKind::EnumValue => SymbolKind::ENUM_MEMBER,
                    parser::ParsedSymbolKind::Define => SymbolKind::CONSTANT,
                    parser::ParsedSymbolKind::InputVar => SymbolKind::PROPERTY,
                    parser::ParsedSymbolKind::GlobalVar => SymbolKind::VARIABLE,
                    parser::ParsedSymbolKind::Method => SymbolKind::METHOD,
                    parser::ParsedSymbolKind::Field => SymbolKind::FIELD,
                    parser::ParsedSymbolKind::TypeAlias => SymbolKind::TYPE_PARAMETER,
                };
                #[allow(deprecated)]
                SymbolInformation {
                    name: s.name,
                    kind,
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: uri.clone(),
                        range: Range {
                            start: Position {
                                line: s.start_line,
                                character: 0,
                            },
                            end: Position {
                                line: s.end_line,
                                character: 1000,
                            },
                        },
                    },
                    container_name: s.parent_name,
                }
            })
            .collect();

        Ok(Some(DocumentSymbolResponse::Flat(items)))
    }

    // ── Workspace Symbols ──

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.to_lowercase();
        let index = self.index.read().await;

        let results: Vec<SymbolInformation> = index
            .all_symbols()
            .filter(|s| {
                if query.is_empty() {
                    true
                } else {
                    s.name.to_lowercase().contains(&query)
                }
            })
            .take(100)
            .map(|s| {
                #[allow(deprecated)]
                SymbolInformation {
                    name: s.name.clone(),
                    kind: s.symbol_kind(),
                    tags: None,
                    deprecated: None,
                    location: Location {
                        uri: s.uri.clone(),
                        range: s.range,
                    },
                    container_name: s.parent_name.clone(),
                }
            })
            .collect();

        Ok(Some(results))
    }
}

// ── Helper implementations ──

impl Mql5Lsp {
    /// Get source content for a URI — prefers in-memory document, falls back to disk.
    fn get_source(&self, uri: &Url) -> Option<String> {
        self.documents
            .get_text(uri)
            .or_else(|| uri.to_file_path().ok().and_then(|p| std::fs::read_to_string(p).ok()))
    }

    /// Index files that a source file includes.
    async fn index_includes(
        &self,
        path: &PathBuf,
        source: &str,
        index: &mut SymbolIndex,
    ) {
        let mut resolver = self.include_resolver.write().await;
        let included = resolver.get_transitive_includes(
            path,
            source,
            &|p| std::fs::read_to_string(p).ok(),
        );

        for inc_path in included {
            index.index_file_from_disk(&inc_path);
        }
    }

    /// Publish diagnostics for a document.
    async fn publish_diagnostics_for(&self, uri: &Url) {
        let source = match self.get_source(uri) {
            Some(s) => s,
            None => return,
        };

        let tree = match parser::parse(&source) {
            Some(t) => t,
            None => return,
        };

        let mut diagnostics: Vec<Diagnostic> = Vec::new();

        // (a) Syntax errors from tree-sitter ERROR/MISSING nodes
        // DISABLED: tree-sitter-mql5 is based on C++ grammar, which marks
        // many valid MQL5 constructs as errors (color literals C'r,g,b',
        // dynamic arrays type arr[], reference arrays type& arr[], dot on
        // pointers, etc.). Re-enable when we have a proper MQL5 grammar.

        // (b) Unresolved includes
        {
            let includes = parser::extract_includes(&source, &tree);
            if let Ok(path) = uri.to_file_path() {
                let mut resolver = self.include_resolver.write().await;
                for inc in &includes {
                    if resolver.resolve(inc, &path).is_none() {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line: inc.line,
                                    character: 0,
                                },
                                end: Position {
                                    line: inc.line,
                                    character: 1000,
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            source: Some("mql5-lsp".to_string()),
                            message: format!(
                                "Unresolved include: {}",
                                inc.path
                            ),
                            ..Default::default()
                        });
                    }
                }
            }
        }

        // (c) Duplicate definitions within the same file
        {
            let parsed = parser::extract_symbols(&source, &tree);
            let mut seen: HashMap<String, u32> = HashMap::new();
            for sym in &parsed {
                // Only check top-level symbols (no parent) and non-methods
                if sym.parent_name.is_none()
                    && !matches!(sym.kind, parser::ParsedSymbolKind::EnumValue)
                {
                    if let Some(prev_line) = seen.get(&sym.name) {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position {
                                    line: sym.start_line,
                                    character: 0,
                                },
                                end: Position {
                                    line: sym.start_line,
                                    character: 1000,
                                },
                            },
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some("mql5-lsp".to_string()),
                            message: format!(
                                "Duplicate definition of `{}` (first defined on line {})",
                                sym.name,
                                prev_line + 1
                            ),
                            ..Default::default()
                        });
                    } else {
                        seen.insert(sym.name.clone(), sym.start_line);
                    }
                }
            }
        }

        // (d) Function call diagnostics: undeclared + wrong argument count
        {
            let calls = parser::extract_function_calls(&source, &tree);
            let index = self.index.read().await;
            for call in &calls {
                // Skip C/C++ keywords that look like calls
                if matches!(call.name.as_str(),
                    "if" | "for" | "while" | "switch" | "return" | "else" | "do"
                    | "sizeof" | "delete" | "new" | "typename" | "template"
                ) {
                    continue;
                }

                // Check if it's a builtin function — verify argument count
                // Skip method calls since the same name may have different signatures
                if let Some(func) = find_function(&call.name).filter(|_| !call.is_method) {
                    let expected = count_signature_params(func.signature);
                    // Only check if we have a definite expected count and call has args
                    if expected.min > 0 && call.arg_count < expected.min {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position { line: call.line, character: call.col },
                                end: Position { line: call.line, character: call.col + call.name.len() as u32 },
                            },
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some("mql5-lsp".to_string()),
                            message: format!(
                                "`{}` expects at least {} argument(s), got {}",
                                call.name, expected.min, call.arg_count
                            ),
                            ..Default::default()
                        });
                    } else if expected.max > 0 && call.arg_count > expected.max {
                        diagnostics.push(Diagnostic {
                            range: Range {
                                start: Position { line: call.line, character: call.col },
                                end: Position { line: call.line, character: call.col + call.name.len() as u32 },
                            },
                            severity: Some(DiagnosticSeverity::WARNING),
                            source: Some("mql5-lsp".to_string()),
                            message: format!(
                                "`{}` expects at most {} argument(s), got {}",
                                call.name, expected.max, call.arg_count
                            ),
                            ..Default::default()
                        });
                    }
                    continue;
                }

                // Skip method calls — we can't reliably resolve receiver types
                if call.is_method {
                    continue;
                }
                // Skip known types/constants/enums
                if find_enum(&call.name).is_some()
                    || find_struct(&call.name).is_some()
                    || find_constant(&call.name).is_some()
                    || is_builtin_type(&call.name)
                {
                    continue;
                }
                // Skip if found in workspace index (as definition or member)
                if !index.find_symbols(&call.name).is_empty() {
                    continue;
                }
                // Skip MQL5 event handlers
                if call.name.starts_with("On") && MQL5_KEYWORDS.contains(&call.name.as_str()) {
                    continue;
                }

                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position { line: call.line, character: call.col },
                        end: Position { line: call.line, character: call.col + call.name.len() as u32 },
                    },
                    severity: Some(DiagnosticSeverity::HINT),
                    source: Some("mql5-lsp".to_string()),
                    message: format!(
                        "Unknown function `{}` — not in builtins or workspace index",
                        call.name
                    ),
                    ..Default::default()
                });
            }
        }

        self.client
            .publish_diagnostics(uri.clone(), diagnostics, None)
            .await;
    }

    /// Completion after `.` — show members of the type.
    async fn complete_dot_access(
        &self,
        object_text: &str,
        uri: &Url,
        source: &str,
        items: &mut Vec<CompletionItem>,
    ) {
        // Try to find what type the object is
        let type_name = self.resolve_variable_type(object_text, uri, source).await;

        if let Some(ref type_name) = type_name {
            // Show members from workspace symbols
            let index = self.index.read().await;
            let members = index.find_members(type_name);
            for member in members {
                items.push(CompletionItem {
                    label: member.name.clone(),
                    kind: Some(member.completion_kind()),
                    detail: Some(member.detail.clone()),
                    ..Default::default()
                });
            }

            // Show members from builtin structs
            if let Some(s) = find_struct(type_name) {
                for (fname, ftype) in s.fields {
                    items.push(CompletionItem {
                        label: fname.to_string(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(format!("{} {}", ftype, fname)),
                        ..Default::default()
                    });
                }
            }
        }
    }

    /// Completion after `::` — show enum values or class statics.
    async fn complete_scope_resolution(
        &self,
        scope_text: &str,
        items: &mut Vec<CompletionItem>,
    ) {
        // Check builtin enums
        if let Some(e) = find_enum(scope_text) {
            for val in e.values {
                items.push(CompletionItem {
                    label: val.to_string(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(format!("{}::{}", e.name, val)),
                    ..Default::default()
                });
            }
        }

        // Check workspace symbols for class members
        let index = self.index.read().await;
        let members = index.find_members(scope_text);
        for member in members {
            items.push(CompletionItem {
                label: member.name.clone(),
                kind: Some(member.completion_kind()),
                detail: Some(member.detail.clone()),
                ..Default::default()
            });
        }
    }

    /// General completion — show all available symbols.
    async fn complete_general(&self, _uri: &Url, items: &mut Vec<CompletionItem>) {
        // Builtin functions
        for func in BUILTIN_FUNCTIONS {
            items.push(CompletionItem {
                label: func.name.to_string(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some(func.signature.to_string()),
                documentation: func.doc.map(|d| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d.to_string(),
                    })
                }),
                ..Default::default()
            });
        }

        // Builtin enums (as types)
        for e in BUILTIN_ENUMS {
            items.push(CompletionItem {
                label: e.name.to_string(),
                kind: Some(CompletionItemKind::ENUM),
                detail: Some(format!("enum {} ({} values)", e.name, e.values.len())),
                ..Default::default()
            });
            // Also add enum values directly
            for val in e.values {
                items.push(CompletionItem {
                    label: val.to_string(),
                    kind: Some(CompletionItemKind::ENUM_MEMBER),
                    detail: Some(format!("{}::{}", e.name, val)),
                    ..Default::default()
                });
            }
        }

        // Builtin structs
        for s in BUILTIN_STRUCTS {
            items.push(CompletionItem {
                label: s.name.to_string(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(format!("struct {}", s.name)),
                ..Default::default()
            });
        }

        // Builtin constants
        for c in BUILTIN_CONSTANTS {
            items.push(CompletionItem {
                label: c.name.to_string(),
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some(c.value.to_string()),
                ..Default::default()
            });
        }

        // Builtin globals
        for (name, type_name) in BUILTIN_GLOBALS {
            items.push(CompletionItem {
                label: name.to_string(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some(format!("{} {}", type_name, name)),
                ..Default::default()
            });
        }

        // MQL5 keywords
        for kw in MQL5_KEYWORDS {
            items.push(CompletionItem {
                label: kw.to_string(),
                kind: Some(CompletionItemKind::KEYWORD),
                ..Default::default()
            });
        }

        // Workspace symbols (skip current file's locals to avoid noise)
        let index = self.index.read().await;
        for sym in index.all_symbols() {
            // Skip members — they'll appear via dot completion
            if sym.parent_name.is_some() {
                continue;
            }
            items.push(CompletionItem {
                label: sym.name.clone(),
                kind: Some(sym.completion_kind()),
                detail: Some(sym.detail.clone()),
                ..Default::default()
            });
        }
    }

    /// Try to resolve the type of a variable by looking at declarations.
    /// Uses AST-based resolution first, then falls back to workspace index.
    async fn resolve_variable_type(
        &self,
        var_name: &str,
        _uri: &Url,
        source: &str,
    ) -> Option<String> {
        // AST-based: parse the source and walk declarations
        if let Some(tree) = parser::parse(source) {
            // Use line count as "use_line" to search the whole file
            let use_line = source.lines().count();
            if let Some(t) = parser::resolve_type_at(source, &tree, var_name, use_line) {
                return Some(t);
            }
        }

        // Fallback: check workspace symbols for the variable's type
        let index = self.index.read().await;
        if let Some(sym) = index.find_symbol(var_name) {
            // Parse type from detail string
            let detail = &sym.detail;
            let type_name = detail.split_whitespace().next();
            if let Some(t) = type_name {
                let t = t.trim_end_matches('*').trim_end_matches('&');
                if t != var_name {
                    return Some(t.to_string());
                }
            }
        }

        None
    }
}

// ── Free functions ──

fn make_hover(value: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Expected argument count range for a function.
struct ExpectedArgs {
    min: usize,
    max: usize, // 0 = unlimited (variadic)
}

/// Count expected parameters from a signature like "void Foo(int a, string b = \"x\")".
/// Parameters with `= default` are optional, reducing min count.
/// Variadic `...` makes max unlimited (0).
fn count_signature_params(signature: &str) -> ExpectedArgs {
    let start = match signature.find('(') {
        Some(p) => p + 1,
        None => return ExpectedArgs { min: 0, max: 0 },
    };
    let end = match signature.rfind(')') {
        Some(p) => p,
        None => return ExpectedArgs { min: 0, max: 0 },
    };
    if start >= end {
        return ExpectedArgs { min: 0, max: 0 };
    }

    let params_str = &signature[start..end].trim();
    if params_str.is_empty() {
        return ExpectedArgs { min: 0, max: 0 };
    }

    // Variadic functions — unlimited args
    if params_str.contains("...") {
        return ExpectedArgs { min: 0, max: 0 };
    }

    // Split by commas (respecting nested parens)
    let mut total = 0usize;
    let mut optional = 0usize;
    let mut depth = 0i32;
    let mut current = String::new();

    for ch in params_str.chars() {
        match ch {
            '(' | '<' | '[' => { depth += 1; current.push(ch); }
            ')' | '>' | ']' => { depth -= 1; current.push(ch); }
            ',' if depth == 0 => {
                if current.contains('=') { optional += 1; }
                total += 1;
                current.clear();
            }
            _ => { current.push(ch); }
        }
    }
    // Last parameter
    if !current.trim().is_empty() {
        if current.contains('=') { optional += 1; }
        total += 1;
    }

    ExpectedArgs {
        min: total.saturating_sub(optional),
        max: total,
    }
}

/// Scan backwards from (line, col) to find the enclosing function call name
/// and count commas to determine the active parameter index.
fn find_function_call_context(source: &str, line: usize, col: usize) -> Option<(String, usize)> {
    let lines: Vec<&str> = source.lines().collect();
    if line >= lines.len() {
        return None;
    }

    // Build the text from the start of the file to the cursor position
    let mut text = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i < line {
            text.push_str(l);
            text.push('\n');
        } else if i == line {
            let end = col.min(l.len());
            text.push_str(&l[..end]);
        }
    }

    // Scan backwards from the end of `text` to find the matching open paren
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut comma_count = 0usize;
    let mut paren_pos = None;

    let mut i = bytes.len();
    while i > 0 {
        i -= 1;
        match bytes[i] {
            b')' => depth += 1,
            b'(' => {
                if depth == 0 {
                    paren_pos = Some(i);
                    break;
                }
                depth -= 1;
            }
            b',' if depth == 0 => {
                comma_count += 1;
            }
            _ => {}
        }
    }

    let paren_pos = paren_pos?;

    // Extract the function name immediately before the open paren
    let before_paren = &text[..paren_pos];
    let trimmed = before_paren.trim_end();
    if trimmed.is_empty() {
        return None;
    }

    // Walk backwards from end of trimmed to find identifier
    let tb = trimmed.as_bytes();
    let end = tb.len();
    let mut start = end;
    while start > 0 && is_ident_byte(tb[start - 1]) {
        start -= 1;
    }

    if start == end {
        return None;
    }

    let func_name = &trimmed[start..end];
    Some((func_name.to_string(), comma_count))
}

/// Parse parameter strings from a function signature like "void Foo(int a, string b)".
fn parse_signature_params(signature: &str) -> Vec<String> {
    // Find the content between first ( and last )
    let start = match signature.find('(') {
        Some(p) => p + 1,
        None => return Vec::new(),
    };
    let end = match signature.rfind(')') {
        Some(p) => p,
        None => return Vec::new(),
    };

    if start >= end {
        return Vec::new();
    }

    let params_str = &signature[start..end];

    // Split by commas (respecting nested parens/brackets)
    let mut params = Vec::new();
    let mut depth = 0i32;
    let mut current = String::new();

    for ch in params_str.chars() {
        match ch {
            '(' | '<' | '[' => {
                depth += 1;
                current.push(ch);
            }
            ')' | '>' | ']' => {
                depth -= 1;
                current.push(ch);
            }
            ',' if depth == 0 => {
                let trimmed = current.trim().to_string();
                if !trimmed.is_empty() {
                    params.push(trimmed);
                }
                current.clear();
            }
            _ => {
                current.push(ch);
            }
        }
    }

    let trimmed = current.trim().to_string();
    if !trimmed.is_empty() {
        params.push(trimmed);
    }

    params
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Mql5Lsp {
        client,
        index: Arc::new(RwLock::new(SymbolIndex::new())),
        documents: Arc::new(DocumentStore::new()),
        include_resolver: Arc::new(RwLock::new(IncludeResolver::new())),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
