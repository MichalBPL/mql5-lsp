fn main() {
    let mut c_config = cc::Build::new();
    c_config.include("tree-sitter-mql5/src");
    c_config.file("tree-sitter-mql5/src/parser.c");
    c_config.flag_if_supported("-w"); // suppress warnings from generated code
    c_config.compile("tree_sitter_mql5");
}
