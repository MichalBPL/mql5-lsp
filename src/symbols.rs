use regex::Regex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tower_lsp::lsp_types::*;

/// A symbol found in the workspace via regex scanning.
#[derive(Clone, Debug)]
pub struct SymbolInfo {
    pub name: String,
    pub kind: SymbolType,
    pub detail: String,
    pub uri: Url,
    pub range: Range,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SymbolType {
    Function,
    Class,
    Struct,
    Enum,
    EnumValue,
    Define,
    InputVar,
    GlobalVar,
}

impl SymbolInfo {
    pub fn symbol_kind(&self) -> SymbolKind {
        match self.kind {
            SymbolType::Function => SymbolKind::FUNCTION,
            SymbolType::Class => SymbolKind::CLASS,
            SymbolType::Struct => SymbolKind::STRUCT,
            SymbolType::Enum => SymbolKind::ENUM,
            SymbolType::EnumValue => SymbolKind::ENUM_MEMBER,
            SymbolType::Define => SymbolKind::CONSTANT,
            SymbolType::InputVar => SymbolKind::PROPERTY,
            SymbolType::GlobalVar => SymbolKind::VARIABLE,
        }
    }

    pub fn completion_kind(&self) -> CompletionItemKind {
        match self.kind {
            SymbolType::Function => CompletionItemKind::FUNCTION,
            SymbolType::Class => CompletionItemKind::CLASS,
            SymbolType::Struct => CompletionItemKind::STRUCT,
            SymbolType::Enum => CompletionItemKind::ENUM,
            SymbolType::EnumValue => CompletionItemKind::ENUM_MEMBER,
            SymbolType::Define => CompletionItemKind::CONSTANT,
            SymbolType::InputVar => CompletionItemKind::PROPERTY,
            SymbolType::GlobalVar => CompletionItemKind::VARIABLE,
        }
    }
}

/// Holds all symbols found in the workspace.
pub struct WorkspaceSymbols {
    /// file path -> symbols in that file
    files: HashMap<PathBuf, Vec<SymbolInfo>>,
}

impl WorkspaceSymbols {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
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
            return; // Safety limit
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip hidden directories and common non-source dirs
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                self.scan_dir_recursive(&path, depth + 1);
            } else if is_mql5_file(&path) {
                let symbols = parse_file_symbols(&path);
                if !symbols.is_empty() {
                    self.files.insert(path, symbols);
                }
            }
        }
    }

    /// Re-scan a single file (on save or open).
    pub fn rescan_file(&mut self, path: &Path) {
        if is_mql5_file(path) {
            let symbols = parse_file_symbols(path);
            if symbols.is_empty() {
                self.files.remove(path);
            } else {
                self.files.insert(path.to_path_buf(), symbols);
            }
        }
    }

    /// Iterate over all symbols in the workspace.
    pub fn all_symbols(&self) -> impl Iterator<Item = &SymbolInfo> {
        self.files.values().flat_map(|v| v.iter())
    }

    /// Find the first symbol matching a name.
    pub fn find_symbol(&self, name: &str) -> Option<&SymbolInfo> {
        self.files
            .values()
            .flat_map(|v| v.iter())
            .find(|s| s.name == name)
    }
}

fn is_mql5_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("mq5" | "mqh")
    )
}

/// Parse a single file for symbols using regex patterns.
pub fn parse_file_symbols(path: &Path) -> Vec<SymbolInfo> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let uri = match Url::from_file_path(path) {
        Ok(u) => u,
        Err(_) => return vec![],
    };

    let mut symbols = Vec::new();

    // Regex patterns for MQL5 symbols
    let re_function = Regex::new(
        r"(?m)^\s*(?:virtual\s+)?(?:static\s+)?(\w[\w\s*&]*?)\s+(\w+)\s*\(([^)]*)\)\s*(?:const\s*)?[{;]"
    ).unwrap();

    let re_class = Regex::new(r"(?m)^\s*class\s+(\w+)").unwrap();
    let re_struct = Regex::new(r"(?m)^\s*struct\s+(\w+)").unwrap();
    let re_enum = Regex::new(r"(?m)^\s*enum\s+(\w+)").unwrap();
    let re_define = Regex::new(r"(?m)^#define\s+(\w+)").unwrap();
    let re_input = Regex::new(r"(?m)^\s*(?:input|sinput)\s+(\w+)\s+(\w+)").unwrap();

    for cap in re_function.captures_iter(&content) {
        let return_type = cap[1].trim();
        let name = &cap[2];
        let params = cap[3].trim();

        // Skip common false positives
        if matches!(name, "if" | "for" | "while" | "switch" | "return" | "else" | "do") {
            continue;
        }

        let line = line_of_offset(&content, cap.get(0).unwrap().start());
        symbols.push(SymbolInfo {
            name: name.to_string(),
            kind: SymbolType::Function,
            detail: format!("{} {}({})", return_type, name, truncate(params, 60)),
            uri: uri.clone(),
            range: line_range(line),
        });
    }

    for cap in re_class.captures_iter(&content) {
        let name = &cap[1];
        let line = line_of_offset(&content, cap.get(0).unwrap().start());
        symbols.push(SymbolInfo {
            name: name.to_string(),
            kind: SymbolType::Class,
            detail: format!("class {}", name),
            uri: uri.clone(),
            range: line_range(line),
        });
    }

    for cap in re_struct.captures_iter(&content) {
        let name = &cap[1];
        let line = line_of_offset(&content, cap.get(0).unwrap().start());
        symbols.push(SymbolInfo {
            name: name.to_string(),
            kind: SymbolType::Struct,
            detail: format!("struct {}", name),
            uri: uri.clone(),
            range: line_range(line),
        });
    }

    for cap in re_enum.captures_iter(&content) {
        let name = &cap[1];
        let line = line_of_offset(&content, cap.get(0).unwrap().start());
        symbols.push(SymbolInfo {
            name: name.to_string(),
            kind: SymbolType::Enum,
            detail: format!("enum {}", name),
            uri: uri.clone(),
            range: line_range(line),
        });
    }

    for cap in re_define.captures_iter(&content) {
        let name = &cap[1];
        let line = line_of_offset(&content, cap.get(0).unwrap().start());
        symbols.push(SymbolInfo {
            name: name.to_string(),
            kind: SymbolType::Define,
            detail: format!("#define {}", name),
            uri: uri.clone(),
            range: line_range(line),
        });
    }

    for cap in re_input.captures_iter(&content) {
        let type_name = &cap[1];
        let var_name = &cap[2];
        let line = line_of_offset(&content, cap.get(0).unwrap().start());
        symbols.push(SymbolInfo {
            name: var_name.to_string(),
            kind: SymbolType::InputVar,
            detail: format!("input {} {}", type_name, var_name),
            uri: uri.clone(),
            range: line_range(line),
        });
    }

    symbols
}

fn line_of_offset(content: &str, offset: usize) -> u32 {
    content[..offset].chars().filter(|c| *c == '\n').count() as u32
}

fn line_range(line: u32) -> Range {
    Range {
        start: Position {
            line,
            character: 0,
        },
        end: Position {
            line,
            character: 1000,
        },
    }
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        &s[..max]
    }
}
