//! Cross-file symbol index.
//!
//! Indexes all symbols from workspace files, included files, and builtins.
//! Also tracks identifier references (usages) for find-all-references and rename.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tower_lsp::lsp_types::*;

use crate::parser::{self, ParsedSymbol, ParsedSymbolKind};

/// A symbol in the index.
#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: ParsedSymbolKind,
    pub detail: String,
    pub uri: Url,
    pub range: Range,
    /// For class/struct members, the parent type name
    pub parent_name: Option<String>,
}

impl SymbolInfo {
    pub fn symbol_kind(&self) -> SymbolKind {
        match self.kind {
            ParsedSymbolKind::Function => SymbolKind::FUNCTION,
            ParsedSymbolKind::Class => SymbolKind::CLASS,
            ParsedSymbolKind::Struct => SymbolKind::STRUCT,
            ParsedSymbolKind::Enum => SymbolKind::ENUM,
            ParsedSymbolKind::EnumValue => SymbolKind::ENUM_MEMBER,
            ParsedSymbolKind::Define => SymbolKind::CONSTANT,
            ParsedSymbolKind::InputVar => SymbolKind::PROPERTY,
            ParsedSymbolKind::GlobalVar => SymbolKind::VARIABLE,
            ParsedSymbolKind::Method => SymbolKind::METHOD,
            ParsedSymbolKind::Field => SymbolKind::FIELD,
            ParsedSymbolKind::TypeAlias => SymbolKind::TYPE_PARAMETER,
        }
    }

    pub fn completion_kind(&self) -> CompletionItemKind {
        match self.kind {
            ParsedSymbolKind::Function => CompletionItemKind::FUNCTION,
            ParsedSymbolKind::Class => CompletionItemKind::CLASS,
            ParsedSymbolKind::Struct => CompletionItemKind::STRUCT,
            ParsedSymbolKind::Enum => CompletionItemKind::ENUM,
            ParsedSymbolKind::EnumValue => CompletionItemKind::ENUM_MEMBER,
            ParsedSymbolKind::Define => CompletionItemKind::CONSTANT,
            ParsedSymbolKind::InputVar => CompletionItemKind::PROPERTY,
            ParsedSymbolKind::GlobalVar => CompletionItemKind::VARIABLE,
            ParsedSymbolKind::Method => CompletionItemKind::METHOD,
            ParsedSymbolKind::Field => CompletionItemKind::FIELD,
            ParsedSymbolKind::TypeAlias => CompletionItemKind::TYPE_PARAMETER,
        }
    }
}

/// A reference (usage) of an identifier in source code.
#[derive(Clone, Debug)]
pub struct ReferenceInfo {
    pub name: String,
    pub uri: Url,
    pub range: Range,
}

/// Workspace-wide symbol index.
pub struct SymbolIndex {
    /// file path -> symbols in that file
    files: HashMap<PathBuf, Vec<SymbolInfo>>,
    /// file path -> references (identifier usages) in that file
    references: HashMap<PathBuf, Vec<ReferenceInfo>>,
}

impl SymbolIndex {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            references: HashMap::new(),
        }
    }

    /// Index a file from source content — extracts both definitions and references.
    pub fn index_file(&mut self, path: &Path, source: &str) {
        let uri = match Url::from_file_path(path) {
            Ok(u) => u,
            Err(_) => return,
        };

        let tree = match parser::parse(source) {
            Some(t) => t,
            None => return,
        };

        // Extract symbol definitions
        let parsed = parser::extract_symbols(source, &tree);
        let symbols: Vec<SymbolInfo> = parsed
            .into_iter()
            .map(|p| to_symbol_info(p, &uri))
            .collect();

        if symbols.is_empty() {
            self.files.remove(path);
        } else {
            self.files.insert(path.to_path_buf(), symbols);
        }

        // Extract identifier references (usages)
        let idents = parser::extract_identifiers(source, &tree);
        let refs: Vec<ReferenceInfo> = idents
            .into_iter()
            .map(|(name, row, col, end_col)| ReferenceInfo {
                name,
                uri: uri.clone(),
                range: Range {
                    start: Position {
                        line: row,
                        character: col,
                    },
                    end: Position {
                        line: row,
                        character: end_col,
                    },
                },
            })
            .collect();

        if refs.is_empty() {
            self.references.remove(path);
        } else {
            self.references.insert(path.to_path_buf(), refs);
        }
    }

    /// Index a file by reading it from disk.
    pub fn index_file_from_disk(&mut self, path: &Path) {
        if !is_mql5_file(path) {
            return;
        }
        if let Ok(source) = std::fs::read_to_string(path) {
            self.index_file(path, &source);
        }
    }

    /// Recursively scan a directory for .mq5 and .mqh files.
    pub fn scan_directory(&mut self, dir: &Path) {
        if !dir.is_dir() {
            return;
        }
        self.scan_dir_recursive(dir, 0);
    }

    fn scan_dir_recursive(&mut self, dir: &Path, depth: u32) {
        if depth > 10 {
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                self.scan_dir_recursive(&path, depth + 1);
            } else if is_mql5_file(&path) {
                self.index_file_from_disk(&path);
            }
        }
    }

    /// Re-index a single file.
    pub fn rescan_file(&mut self, path: &Path, source: Option<&str>) {
        if !is_mql5_file(path) {
            return;
        }
        match source {
            Some(s) => self.index_file(path, s),
            None => self.index_file_from_disk(path),
        }
    }

    /// Get all symbols across all files.
    pub fn all_symbols(&self) -> impl Iterator<Item = &SymbolInfo> {
        self.files.values().flat_map(|v| v.iter())
    }

    /// Get symbols for a specific file.
    #[allow(dead_code)]
    pub fn file_symbols(&self, path: &Path) -> Option<&[SymbolInfo]> {
        self.files.get(path).map(|v| v.as_slice())
    }

    /// Find symbol(s) by name across all files.
    pub fn find_symbol(&self, name: &str) -> Option<&SymbolInfo> {
        self.files
            .values()
            .flat_map(|v| v.iter())
            .find(|s| s.name == name)
    }

    /// Find all symbols matching a name.
    pub fn find_symbols(&self, name: &str) -> Vec<&SymbolInfo> {
        self.files
            .values()
            .flat_map(|v| v.iter())
            .filter(|s| s.name == name)
            .collect()
    }

    /// Find all references (usages) of a name across all indexed files.
    pub fn find_references(&self, name: &str) -> Vec<&ReferenceInfo> {
        self.references
            .values()
            .flat_map(|v| v.iter())
            .filter(|r| r.name == name)
            .collect()
    }

    /// Find all members of a class/struct.
    pub fn find_members(&self, class_name: &str) -> Vec<&SymbolInfo> {
        self.files
            .values()
            .flat_map(|v| v.iter())
            .filter(|s| s.parent_name.as_deref() == Some(class_name))
            .collect()
    }

    /// Find top-level symbols (no parent) matching a filter.
    #[allow(dead_code)]
    pub fn find_top_level<F>(&self, filter: F) -> Vec<&SymbolInfo>
    where
        F: Fn(&SymbolInfo) -> bool,
    {
        self.files
            .values()
            .flat_map(|v| v.iter())
            .filter(|s| s.parent_name.is_none() && filter(s))
            .collect()
    }

    /// Number of indexed files.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Total number of symbols.
    pub fn symbol_count(&self) -> usize {
        self.files.values().map(|v| v.len()).sum()
    }
}

fn to_symbol_info(parsed: ParsedSymbol, uri: &Url) -> SymbolInfo {
    SymbolInfo {
        name: parsed.name,
        kind: parsed.kind,
        detail: parsed.detail,
        uri: uri.clone(),
        range: Range {
            start: Position {
                line: parsed.start_line,
                character: 0,
            },
            end: Position {
                line: parsed.end_line,
                character: 1000,
            },
        },
        parent_name: parsed.parent_name,
    }
}

fn is_mql5_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("mq5" | "mqh")
    )
}
