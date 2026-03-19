//! Tree-sitter based MQL5 parser for symbol extraction.

use tree_sitter::{Language, Node, Parser, Tree};

unsafe extern "C" {
    fn tree_sitter_mql5() -> *const std::ffi::c_void;
}

/// Get the tree-sitter MQL5 language.
pub fn mql5_language() -> Language {
    let ptr = unsafe { tree_sitter_mql5() };
    unsafe { Language::from_raw(ptr as *const _) }
}

/// Create a new parser configured for MQL5.
pub fn new_parser() -> Parser {
    let mut parser = Parser::new();
    parser
        .set_language(&mql5_language())
        .expect("failed to set MQL5 language");
    parser
}

/// Parse source code into a tree-sitter tree.
pub fn parse(source: &str) -> Option<Tree> {
    let mut parser = new_parser();
    parser.parse(source, None)
}

/// Incremental parse with an old tree.
pub fn parse_incremental(source: &str, old_tree: &Tree) -> Option<Tree> {
    let mut parser = new_parser();
    parser.parse(source, Some(old_tree))
}

// ── Symbol extraction from tree-sitter AST ──

#[derive(Clone, Debug)]
pub struct ParsedSymbol {
    pub name: String,
    pub kind: ParsedSymbolKind,
    pub detail: String,
    pub start_line: u32,
    pub end_line: u32,
    /// For class/struct members
    pub parent_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParsedSymbolKind {
    Function,
    Class,
    Struct,
    Enum,
    EnumValue,
    Define,
    InputVar,
    GlobalVar,
    Method,
    Field,
    TypeAlias,
}

/// Extract all symbols from parsed source code.
pub fn extract_symbols(source: &str, tree: &Tree) -> Vec<ParsedSymbol> {
    let mut symbols = Vec::new();
    let root = tree.root_node();
    extract_from_node(root, source, &mut symbols, None);
    symbols
}

fn extract_from_node(
    node: Node,
    source: &str,
    symbols: &mut Vec<ParsedSymbol>,
    parent_class: Option<&str>,
) {
    match node.kind() {
        "function_definition" | "function_declarator" => {
            if parent_class.is_none() {
                if let Some(sym) = extract_function(node, source, parent_class) {
                    symbols.push(sym);
                }
            }
        }
        "declaration" => {
            extract_declaration(node, source, symbols, parent_class);
        }
        "class_specifier" | "struct_specifier" => {
            extract_class_or_struct(node, source, symbols);
        }
        "enum_specifier" => {
            extract_enum(node, source, symbols);
        }
        "preproc_def" | "preproc_function_def" => {
            extract_define(node, source, symbols);
        }
        _ => {}
    }

    // Recurse into children (but not into class/struct bodies — handled separately)
    if !matches!(node.kind(), "class_specifier" | "struct_specifier") {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            extract_from_node(child, source, symbols, parent_class);
        }
    }
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn extract_function(
    node: Node,
    source: &str,
    parent_class: Option<&str>,
) -> Option<ParsedSymbol> {
    // Find the declarator node which contains the function name
    let declarator = find_child_by_kind(node, "function_declarator")
        .or_else(|| find_child_by_kind(node, "pointer_declarator"))?;

    let name_node = find_child_by_kind(declarator, "identifier")
        .or_else(|| find_child_by_kind(declarator, "field_identifier"))?;

    let name = node_text(name_node, source).to_string();

    // Skip common false positives
    if matches!(
        name.as_str(),
        "if" | "for" | "while" | "switch" | "return" | "else" | "do"
    ) {
        return None;
    }

    // Build signature from the full function node text (first line)
    let full_text = node_text(node, source);
    let first_line = full_text.lines().next().unwrap_or(&full_text);
    let detail = first_line
        .trim()
        .trim_end_matches('{')
        .trim()
        .to_string();

    let kind = if parent_class.is_some() {
        ParsedSymbolKind::Method
    } else {
        ParsedSymbolKind::Function
    };

    Some(ParsedSymbol {
        name,
        kind,
        detail,
        start_line: node.start_position().row as u32,
        end_line: node.end_position().row as u32,
        parent_name: parent_class.map(|s| s.to_string()),
    })
}

fn extract_declaration(
    node: Node,
    source: &str,
    symbols: &mut Vec<ParsedSymbol>,
    parent_class: Option<&str>,
) {
    // Check for input/sinput variables
    let text = node_text(node, source);
    if text.starts_with("input ") || text.starts_with("sinput ") {
        // Parse: input TYPE NAME = ...;
        let parts: Vec<&str> = text.split_whitespace().collect();
        if parts.len() >= 3 {
            let var_name = parts[2].trim_end_matches(';').trim_end_matches('=');
            if !var_name.is_empty() {
                symbols.push(ParsedSymbol {
                    name: var_name.to_string(),
                    kind: ParsedSymbolKind::InputVar,
                    detail: text.lines().next().unwrap_or(text).trim().to_string(),
                    start_line: node.start_position().row as u32,
                    end_line: node.end_position().row as u32,
                    parent_name: parent_class.map(|s| s.to_string()),
                });
            }
        }
        return;
    }

    // Check for typedef
    if text.starts_with("typedef ") {
        let parts: Vec<&str> = text.split_whitespace().collect();
        if let Some(name) = parts.last() {
            let name = name.trim_end_matches(';');
            if !name.is_empty() {
                symbols.push(ParsedSymbol {
                    name: name.to_string(),
                    kind: ParsedSymbolKind::TypeAlias,
                    detail: text.lines().next().unwrap_or(text).trim().to_string(),
                    start_line: node.start_position().row as u32,
                    end_line: node.end_position().row as u32,
                    parent_name: None,
                });
            }
        }
        return;
    }

    // Check for function declarations (not definitions — no body)
    if let Some(declarator) = find_child_by_kind(node, "function_declarator") {
        if let Some(sym) = extract_function(node, source, parent_class) {
            symbols.push(sym);
        }
        return;
    }

    // Regular variable declarations at file scope or as class members
    if parent_class.is_some() {
        // Class member variable
        if let Some(declarator) = find_child_by_kind(node, "identifier")
            .or_else(|| find_descendant_by_kind(node, "field_identifier"))
        {
            let name = node_text(declarator, source).to_string();
            let detail = text.lines().next().unwrap_or(text).trim().to_string();
            symbols.push(ParsedSymbol {
                name,
                kind: ParsedSymbolKind::Field,
                detail,
                start_line: node.start_position().row as u32,
                end_line: node.end_position().row as u32,
                parent_name: parent_class.map(|s| s.to_string()),
            });
        }
    }
}

fn extract_class_or_struct(node: Node, source: &str, symbols: &mut Vec<ParsedSymbol>) {
    let is_class = node.kind() == "class_specifier";
    let kind = if is_class {
        ParsedSymbolKind::Class
    } else {
        ParsedSymbolKind::Struct
    };

    // Find name
    let name_node = find_child_by_kind(node, "type_identifier");
    let name = match name_node {
        Some(n) => node_text(n, source).to_string(),
        None => return, // anonymous struct/class
    };

    // Find base class
    let base = find_child_by_kind(node, "base_class_clause").map(|bc| {
        let bc_text = node_text(bc, source);
        bc_text.trim_start_matches(':').trim().to_string()
    });

    let detail = if let Some(ref base) = base {
        format!(
            "{} {} : {}",
            if is_class { "class" } else { "struct" },
            name,
            base
        )
    } else {
        format!("{} {}", if is_class { "class" } else { "struct" }, name)
    };

    symbols.push(ParsedSymbol {
        name: name.clone(),
        kind,
        detail,
        start_line: node.start_position().row as u32,
        end_line: node.end_position().row as u32,
        parent_name: None,
    });

    // Extract members from the body
    if let Some(body) = find_child_by_kind(node, "field_declaration_list") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "field_declaration" => {
                    extract_class_member(child, source, symbols, &name);
                }
                "function_definition" => {
                    if let Some(sym) = extract_function(child, source, Some(&name)) {
                        symbols.push(sym);
                    }
                }
                "declaration" => {
                    extract_declaration(child, source, symbols, Some(&name));
                }
                "access_specifier" => {} // skip public:/private:/protected:
                _ => {}
            }
        }
    }
}

fn extract_class_member(
    node: Node,
    source: &str,
    symbols: &mut Vec<ParsedSymbol>,
    class_name: &str,
) {
    let text = node_text(node, source);

    // Check if this is a method declaration (has parameter_list)
    if find_descendant_by_kind(node, "function_declarator").is_some() {
        if let Some(sym) = extract_function(node, source, Some(class_name)) {
            symbols.push(sym);
        }
        return;
    }

    // It's a field
    let name_node = find_child_by_kind(node, "field_identifier")
        .or_else(|| find_descendant_by_kind(node, "field_identifier"));
    if let Some(n) = name_node {
        let name = node_text(n, source).to_string();
        symbols.push(ParsedSymbol {
            name,
            kind: ParsedSymbolKind::Field,
            detail: text.trim().trim_end_matches(';').trim().to_string(),
            start_line: node.start_position().row as u32,
            end_line: node.end_position().row as u32,
            parent_name: Some(class_name.to_string()),
        });
    }
}

fn extract_enum(node: Node, source: &str, symbols: &mut Vec<ParsedSymbol>) {
    let name_node = find_child_by_kind(node, "type_identifier");
    let name = match name_node {
        Some(n) => node_text(n, source).to_string(),
        None => return,
    };

    symbols.push(ParsedSymbol {
        name: name.clone(),
        kind: ParsedSymbolKind::Enum,
        detail: format!("enum {}", name),
        start_line: node.start_position().row as u32,
        end_line: node.end_position().row as u32,
        parent_name: None,
    });

    // Extract enum values
    if let Some(body) = find_child_by_kind(node, "enumerator_list") {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            if child.kind() == "enumerator" {
                if let Some(id) = find_child_by_kind(child, "identifier") {
                    let val_name = node_text(id, source).to_string();
                    symbols.push(ParsedSymbol {
                        name: val_name.clone(),
                        kind: ParsedSymbolKind::EnumValue,
                        detail: format!("{}::{}", name, val_name),
                        start_line: child.start_position().row as u32,
                        end_line: child.end_position().row as u32,
                        parent_name: Some(name.clone()),
                    });
                }
            }
        }
    }
}

fn extract_define(node: Node, source: &str, symbols: &mut Vec<ParsedSymbol>) {
    if let Some(name_node) = find_child_by_kind(node, "identifier") {
        let name = node_text(name_node, source).to_string();
        let text = node_text(node, source);
        symbols.push(ParsedSymbol {
            name,
            kind: ParsedSymbolKind::Define,
            detail: text.lines().next().unwrap_or(text).trim().to_string(),
            start_line: node.start_position().row as u32,
            end_line: node.end_position().row as u32,
            parent_name: None,
        });
    }
}

// ── Tree-sitter node helpers ──

fn find_child_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    let result = node.children(&mut cursor).find(|c| c.kind() == kind);
    result
}

fn find_descendant_by_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    let child_count = node.child_count();
    for i in 0..child_count {
        let child = node.child(i).unwrap();
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = find_descendant_by_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

// ── Include extraction ──

/// Extract #include directives from source code.
pub fn extract_includes(source: &str, tree: &Tree) -> Vec<IncludeDirective> {
    let mut includes = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();

    for child in root.children(&mut cursor) {
        if child.kind() == "preproc_include" {
            if let Some(inc) = parse_include_directive(child, source) {
                includes.push(inc);
            }
        }
    }

    // Also scan with regex as fallback for tree-sitter edge cases
    for (i, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("#include") {
            if let Some(inc) = parse_include_line(trimmed, i as u32) {
                // Avoid duplicates
                if !includes.iter().any(|existing: &IncludeDirective| existing.path == inc.path) {
                    includes.push(inc);
                }
            }
        }
    }

    includes
}

#[derive(Clone, Debug)]
pub struct IncludeDirective {
    pub path: String,
    pub is_system: bool, // true for <>, false for ""
    pub line: u32,
}

fn parse_include_directive(node: Node, source: &str) -> Option<IncludeDirective> {
    let text = node_text(node, source).trim();
    parse_include_line(text, node.start_position().row as u32)
}

fn parse_include_line(line: &str, line_num: u32) -> Option<IncludeDirective> {
    let after_include = line.strip_prefix("#include")?.trim();

    if after_include.starts_with('<') {
        let path = after_include.trim_start_matches('<').trim_end_matches('>');
        Some(IncludeDirective {
            path: path.replace('\\', "/"),
            is_system: true,
            line: line_num,
        })
    } else if after_include.starts_with('"') {
        let path = after_include.trim_matches('"');
        Some(IncludeDirective {
            path: path.replace('\\', "/"),
            is_system: false,
            line: line_num,
        })
    } else {
        None
    }
}

// ── Completion context detection ──

#[derive(Debug, PartialEq)]
pub enum CompletionContext {
    /// After a dot: `obj.` — show members of the type
    DotAccess { object_text: String },
    /// After `::` — show static members or scope resolution
    ScopeResolution { scope_text: String },
    /// Inside a function body — show locals + params + class members + globals
    General,
}

/// Determine what kind of completion to provide based on cursor position.
pub fn get_completion_context(source: &str, line: usize, col: usize) -> CompletionContext {
    let target_line = match source.lines().nth(line) {
        Some(l) => l,
        None => return CompletionContext::General,
    };

    // Get text before cursor on this line
    let before_cursor = if col <= target_line.len() {
        &target_line[..col]
    } else {
        target_line
    };
    let trimmed = before_cursor.trim_end();

    // Check for dot access: `expr.`
    if trimmed.ends_with('.') {
        let before_dot = trimmed[..trimmed.len() - 1].trim_end();
        // Extract the identifier before the dot
        let object = extract_last_identifier(before_dot);
        if !object.is_empty() {
            return CompletionContext::DotAccess {
                object_text: object.to_string(),
            };
        }
    }

    // Check for scope resolution: `Type::`
    if trimmed.ends_with("::") {
        let before_colons = trimmed[..trimmed.len() - 2].trim_end();
        let scope = extract_last_identifier(before_colons);
        if !scope.is_empty() {
            return CompletionContext::ScopeResolution {
                scope_text: scope.to_string(),
            };
        }
    }

    CompletionContext::General
}

fn extract_last_identifier(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    while end > 0 && !bytes[end - 1].is_ascii_alphanumeric() && bytes[end - 1] != b'_' {
        end -= 1;
    }
    let mut start = end;
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    &text[start..end]
}

/// Extract the word (identifier) at a given line/col position.
pub fn extract_word_at(source: &str, line: usize, col: usize) -> Option<String> {
    let target_line = source.lines().nth(line)?;
    let bytes = target_line.as_bytes();
    if col >= bytes.len() {
        return None;
    }

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

// ── Identifier extraction (for references) ──

/// Extract all identifier usages from a parsed tree.
/// Returns (name, line, start_col, end_col) tuples.
pub fn extract_identifiers(source: &str, tree: &Tree) -> Vec<(String, u32, u32, u32)> {
    let mut identifiers = Vec::new();
    collect_identifiers(tree.root_node(), source, &mut identifiers);
    identifiers
}

fn collect_identifiers(node: Node, source: &str, out: &mut Vec<(String, u32, u32, u32)>) {
    match node.kind() {
        "identifier" | "field_identifier" | "type_identifier" => {
            let text = &source[node.byte_range()];
            // Skip empty or whitespace-only nodes
            if !text.is_empty() && text.chars().next().is_some_and(|c| c.is_ascii_alphanumeric() || c == '_') {
                let start = node.start_position();
                let end = node.end_position();
                out.push((
                    text.to_string(),
                    start.row as u32,
                    start.column as u32,
                    end.column as u32,
                ));
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_identifiers(child, source, out);
    }
}

// ── Error node extraction (for diagnostics) ──

/// A syntax error found by tree-sitter.
#[derive(Clone, Debug)]
pub struct SyntaxError {
    pub message: String,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

/// Extract all ERROR and MISSING nodes from the parse tree.
pub fn extract_errors(source: &str, tree: &Tree) -> Vec<SyntaxError> {
    let mut errors = Vec::new();
    collect_errors(tree.root_node(), source, &mut errors);
    errors
}

fn collect_errors(node: Node, source: &str, out: &mut Vec<SyntaxError>) {
    if node.is_error() {
        let text = &source[node.byte_range()];
        let preview: String = text.chars().take(40).collect();
        let start = node.start_position();
        let end = node.end_position();
        out.push(SyntaxError {
            message: format!("Syntax error near `{}`", preview.trim()),
            start_line: start.row as u32,
            start_col: start.column as u32,
            end_line: end.row as u32,
            end_col: end.column as u32,
        });
    } else if node.is_missing() {
        let start = node.start_position();
        let end = node.end_position();
        out.push(SyntaxError {
            message: format!("Missing `{}`", node.kind()),
            start_line: start.row as u32,
            start_col: start.column as u32,
            end_line: end.row as u32,
            end_col: end.column as u32,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_errors(child, source, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple() {
        let source = r#"
void OnTick() {
    Print("Hello");
}
"#;
        let tree = parse(source).unwrap();
        let symbols = extract_symbols(source, &tree);
        assert!(symbols.iter().any(|s| s.name == "OnTick"));
    }

    #[test]
    fn test_include_extraction() {
        let source = r#"
#include <Trade\Trade.mqh>
#include "MyHelper.mqh"
"#;
        let tree = parse(source).unwrap();
        let includes = extract_includes(source, &tree);
        assert!(includes.iter().any(|i| i.path == "Trade/Trade.mqh" && i.is_system));
        assert!(includes.iter().any(|i| i.path == "MyHelper.mqh" && !i.is_system));
    }

    #[test]
    fn test_completion_context() {
        assert_eq!(
            get_completion_context("   trade.", 0, 9),
            CompletionContext::DotAccess {
                object_text: "trade".to_string()
            }
        );
        assert_eq!(
            get_completion_context("   CTrade::", 0, 11),
            CompletionContext::ScopeResolution {
                scope_text: "CTrade".to_string()
            }
        );
        assert_eq!(
            get_completion_context("   int x = ", 0, 11),
            CompletionContext::General
        );
    }
}
