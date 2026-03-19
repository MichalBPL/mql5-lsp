mod builtins;
mod documents;
mod includes;
mod parser;
mod symbols;

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
use symbols::{SymbolIndex, SymbolInfo};

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
            {
                let mut resolver = self.include_resolver.write().await;
                resolver.detect_include_root(root);
            }

            // Index workspace files
            {
                let mut index = self.index.write().await;
                index.scan_directory(root);
                log::info!(
                    "Indexed {} files, {} symbols",
                    index.file_count(),
                    index.symbol_count()
                );
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
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
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
    async fn complete_general(&self, uri: &Url, items: &mut Vec<CompletionItem>) {
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
    async fn resolve_variable_type(
        &self,
        var_name: &str,
        uri: &Url,
        source: &str,
    ) -> Option<String> {
        // Simple heuristic: search for "Type var_name" pattern in source
        for line in source.lines() {
            let trimmed = line.trim();
            // Match patterns like: ClassName var_name; or ClassName* var_name;
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 {
                let potential_var = parts[1]
                    .trim_start_matches('*')
                    .trim_start_matches('&')
                    .trim_end_matches(';')
                    .trim_end_matches(',')
                    .trim_end_matches('=');
                if potential_var == var_name {
                    let type_name = parts[0].trim_end_matches('*').trim_end_matches('&');
                    // Verify it looks like a type
                    if type_name.chars().next().is_some_and(|c| c.is_uppercase())
                        || is_builtin_type(type_name)
                    {
                        return Some(type_name.to_string());
                    }
                }
            }
        }

        // Also check workspace symbols for the variable's type
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

fn make_hover(value: String) -> Hover {
    Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value,
        }),
        range: None,
    }
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
