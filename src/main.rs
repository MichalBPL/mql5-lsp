mod builtins;
mod symbols;

use std::sync::Arc;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use builtins::MQL5_BUILTINS;
use symbols::WorkspaceSymbols;

struct Mql5Lsp {
    client: Client,
    workspace_symbols: Arc<RwLock<WorkspaceSymbols>>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Mql5Lsp {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Index workspace files on init
        if let Some(folders) = &params.workspace_folders {
            let mut ws = self.workspace_symbols.write().await;
            for folder in folders {
                if let Ok(path) = folder.uri.to_file_path() {
                    ws.scan_directory(&path);
                }
            }
        } else if let Some(uri) = &params.root_uri {
            if let Ok(path) = uri.to_file_path() {
                let mut ws = self.workspace_symbols.write().await;
                ws.scan_directory(&path);
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

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let mut items: Vec<CompletionItem> = Vec::new();

        // Add MQL5 built-in functions
        for builtin in MQL5_BUILTINS.iter() {
            items.push(CompletionItem {
                label: builtin.name.to_string(),
                kind: Some(match builtin.kind {
                    builtins::BuiltinKind::Function => CompletionItemKind::FUNCTION,
                    builtins::BuiltinKind::Constant => CompletionItemKind::CONSTANT,
                    builtins::BuiltinKind::Type => CompletionItemKind::CLASS,
                    builtins::BuiltinKind::Keyword => CompletionItemKind::KEYWORD,
                    builtins::BuiltinKind::Variable => CompletionItemKind::VARIABLE,
                }),
                detail: Some(builtin.signature.to_string()),
                documentation: builtin.doc.map(|d| {
                    Documentation::MarkupContent(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: d.to_string(),
                    })
                }),
                insert_text: Some(builtin.insert_text()),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                ..Default::default()
            });
        }

        // Add workspace symbols
        let ws = self.workspace_symbols.read().await;
        let uri = &params.text_document_position.text_document.uri;
        for sym in ws.all_symbols() {
            // Don't show symbols from the current file as completions (they're already visible)
            if sym.uri.as_str() == uri.as_str() {
                continue;
            }
            items.push(CompletionItem {
                label: sym.name.clone(),
                kind: Some(sym.completion_kind()),
                detail: Some(sym.detail.clone()),
                ..Default::default()
            });
        }

        Ok(Some(CompletionResponse::Array(items)))
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params;
        let uri = &position.text_document.uri;
        let line = position.position.line as usize;
        let col = position.position.character as usize;

        // Try to extract the word at the cursor position
        let word = if let Ok(path) = uri.to_file_path() {
            extract_word_at(&path, line, col)
        } else {
            None
        };

        if let Some(word) = word {
            // Check built-ins
            if let Some(builtin) = MQL5_BUILTINS.iter().find(|b| b.name == word) {
                let mut value = format!("```mql5\n{}\n```", builtin.signature);
                if let Some(doc) = builtin.doc {
                    value.push_str(&format!("\n\n{}", doc));
                }
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value,
                    }),
                    range: None,
                }));
            }

            // Check workspace symbols
            let ws = self.workspace_symbols.read().await;
            if let Some(sym) = ws.find_symbol(&word) {
                return Ok(Some(Hover {
                    contents: HoverContents::Markup(MarkupContent {
                        kind: MarkupKind::Markdown,
                        value: format!("```mql5\n{}\n```\n\nDefined in `{}`",
                            sym.detail,
                            sym.uri.path()),
                    }),
                    range: None,
                }));
            }
        }

        Ok(None)
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params;
        let uri = &position.text_document.uri;
        let line = position.position.line as usize;
        let col = position.position.character as usize;

        let word = if let Ok(path) = uri.to_file_path() {
            extract_word_at(&path, line, col)
        } else {
            None
        };

        if let Some(word) = word {
            let ws = self.workspace_symbols.read().await;
            if let Some(sym) = ws.find_symbol(&word) {
                return Ok(Some(GotoDefinitionResponse::Scalar(Location {
                    uri: sym.uri.clone(),
                    range: sym.range,
                })));
            }
        }

        Ok(None)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = &params.text_document.uri;
        if let Ok(path) = uri.to_file_path() {
            let syms = symbols::parse_file_symbols(&path);
            let items: Vec<SymbolInformation> = syms
                .into_iter()
                .map(|s| {
                    let kind = s.symbol_kind();
                    #[allow(deprecated)]
                    SymbolInformation {
                        name: s.name,
                        kind,
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: uri.clone(),
                            range: s.range,
                        },
                        container_name: None,
                    }
                })
                .collect();
            return Ok(Some(DocumentSymbolResponse::Flat(items)));
        }
        Ok(None)
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.to_lowercase();
        let ws = self.workspace_symbols.read().await;
        let results: Vec<SymbolInformation> = ws
            .all_symbols()
            .filter(|s| s.name.to_lowercase().contains(&query))
            .take(50)
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
                    container_name: None,
                }
            })
            .collect();
        Ok(Some(results))
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        // Re-scan the saved file
        if let Ok(path) = params.text_document.uri.to_file_path() {
            let mut ws = self.workspace_symbols.write().await;
            ws.rescan_file(&path);
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        // Scan newly opened file if not already indexed
        if let Ok(path) = params.text_document.uri.to_file_path() {
            let mut ws = self.workspace_symbols.write().await;
            ws.rescan_file(&path);
        }
    }
}

/// Extract the word (identifier) at a given line/col position in a file.
fn extract_word_at(path: &std::path::Path, line: usize, col: usize) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let target_line = content.lines().nth(line)?;
    let bytes = target_line.as_bytes();
    if col >= bytes.len() {
        return None;
    }

    // Find word boundaries
    let mut start = col;
    while start > 0 && is_ident_char(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end < bytes.len() && is_ident_char(bytes[end]) {
        end += 1;
    }

    if start == end {
        return None;
    }
    Some(target_line[start..end].to_string())
}

fn is_ident_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Mql5Lsp {
        client,
        workspace_symbols: Arc::new(RwLock::new(WorkspaceSymbols::new())),
    });

    Server::new(stdin, stdout, socket).serve(service).await;
}
