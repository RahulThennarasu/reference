use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Node, Parser, Query, QueryCursor};

/// A slice of a source file that should get its own embedding — a whole
/// function, a class/impl block, or (for unsupported languages / parse
/// failures) the entire file. `start_line`/`end_line` are 1-indexed and
/// inclusive.
pub struct Chunk {
    pub start_line: i32,
    pub end_line: i32,
    pub kind: String,
    pub content: String,
}

fn whole_file_chunk(source: &str) -> Chunk {
    Chunk {
        start_line: 1,
        end_line: source.lines().count().max(1) as i32,
        kind: "file".to_string(),
        content: source.to_string(),
    }
}

/// Splits `source` into function/class-level chunks based on `extension`.
/// Returns `None` when there's no chunker for this extension, or parsing
/// fails — callers must fall back to whole-file indexing in that case, the
/// same way `read_text` skips gracefully rather than dropping a file.
pub fn chunk_source(extension: &str, source: &str) -> Option<Vec<Chunk>> {
    match extension {
        "rs" => chunk_with(tree_sitter_rust::LANGUAGE.into(), RUST_QUERY_SRC, source),
        "py" => chunk_with(tree_sitter_python::LANGUAGE.into(), PYTHON_QUERY_SRC, source),
        "js" | "jsx" | "mjs" | "cjs" => {
            chunk_with(tree_sitter_javascript::LANGUAGE.into(), JS_QUERY_SRC, source)
        }
        "ts" | "mts" | "cts" => chunk_with(
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            TS_QUERY_SRC,
            source,
        ),
        "tsx" => chunk_with(tree_sitter_typescript::LANGUAGE_TSX.into(), TS_QUERY_SRC, source),
        _ => None,
    }
}

// (function_item) covers free functions *and* methods inside an impl block —
// the containment filter in `chunk_with` is what keeps methods from becoming
// their own chunks separate from their enclosing impl.
const RUST_QUERY_SRC: &str = "(function_item) @item (impl_item) @item";

// Same idea for Python: methods inside a class are `function_definition`
// nodes nested in the `class_definition`, swallowed by the same filter.
const PYTHON_QUERY_SRC: &str = "(function_definition) @item (class_definition) @item";

// Covers named function declarations/classes plus the common
// `const foo = () => {...}` / `const foo = function () {...}` pattern —
// idiomatic JS/TS rarely uses bare `function` declarations for top-level
// exports. Methods inside a class body are nested `method_definition`
// nodes, swallowed into the class the same way Rust/Python methods are.
const JS_QUERY_SRC: &str = "
(function_declaration) @item
(generator_function_declaration) @item
(class_declaration) @item
(lexical_declaration (variable_declarator value: (arrow_function))) @item
(lexical_declaration (variable_declarator value: (function_expression))) @item
";

// TypeScript's grammar is a superset of JavaScript's for these node kinds,
// plus `interface_declaration` for type-only contracts, which are common
// and meaningful enough to cite on their own (e.g. "the User interface").
const TS_QUERY_SRC: &str = "
(function_declaration) @item
(generator_function_declaration) @item
(class_declaration) @item
(interface_declaration) @item
(lexical_declaration (variable_declarator value: (arrow_function))) @item
(lexical_declaration (variable_declarator value: (function_expression))) @item
";

fn kind_of(node_kind: &str) -> &'static str {
    match node_kind {
        "impl_item" => "impl",
        "class_definition" | "class_declaration" => "class",
        "interface_declaration" => "interface",
        _ => "function",
    }
}

fn chunk_with(language: Language, query_src: &str, source: &str) -> Option<Vec<Chunk>> {
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(source, None)?;

    let query = Query::new(&language, query_src).ok()?;
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query, tree.root_node(), source.as_bytes());

    let mut nodes: Vec<Node> = Vec::new();
    while let Some(m) = matches.next() {
        for c in m.captures {
            nodes.push(c.node);
        }
    }
    nodes.sort_by_key(|n| n.start_byte());

    // Keep only outermost matches: a closure or nested function inside a
    // function body, or a method inside a class/impl block, is swallowed
    // into its parent's chunk rather than becoming a separate row — a chunk
    // should be an independently meaningful unit, and a method floating
    // without its class/impl context isn't one.
    let mut kept: Vec<Node> = Vec::new();
    let mut last_end = 0usize;
    for node in nodes {
        if node.start_byte() >= last_end {
            last_end = node.end_byte();
            kept.push(node);
        }
    }

    if kept.is_empty() {
        return None;
    }

    let mut chunks = Vec::new();

    // Leftover top-of-file text before the first chunk (imports, top-level
    // consts, module doc comments) becomes its own small "file" chunk, so a
    // query like "what does this file import" still resolves to something.
    if let Some(first) = kept.first() {
        let preamble = &source[..first.start_byte()];
        if !preamble.trim().is_empty() {
            chunks.push(Chunk {
                start_line: 1,
                end_line: preamble.matches('\n').count() as i32 + 1,
                kind: "file".to_string(),
                content: preamble.trim().to_string(),
            });
        }
    }

    for node in kept {
        chunks.push(Chunk {
            start_line: node.start_position().row as i32 + 1,
            end_line: node.end_position().row as i32 + 1,
            kind: kind_of(node.kind()).to_string(),
            content: source[node.start_byte()..node.end_byte()].to_string(),
        });
    }

    Some(chunks)
}

/// Chunks `source`, falling back to a single whole-file chunk when there's
/// no chunker for `extension` or parsing produced nothing usable. Callers
/// should always use this instead of `chunk_source` directly, so a WIP file
/// with a syntax error still ends up indexed rather than silently dropped.
pub fn chunk_or_whole_file(extension: &str, source: &str) -> Vec<Chunk> {
    chunk_source(extension, source).unwrap_or_else(|| vec![whole_file_chunk(source)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_functions_and_impls_become_separate_chunks() {
        let source = r#"
use std::fmt;

const FOO: i32 = 1;

fn free_function(x: i32) -> i32 {
    x + 1
}

struct Thing;

impl Thing {
    fn method_one(&self) -> i32 {
        1
    }

    fn method_two(&self) -> i32 {
        2
    }
}
"#;
        let chunks = chunk_source("rs", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();

        // preamble ("file"), free_function ("function"), impl block ("impl") —
        // methods inside the impl must NOT show up as their own chunks.
        assert_eq!(kinds, vec!["file", "function", "impl"]);

        let preamble = &chunks[0];
        assert!(preamble.content.contains("use std::fmt"));
        assert!(preamble.content.contains("const FOO"));

        let func = &chunks[1];
        assert!(func.content.contains("fn free_function"));
        assert!(!func.content.contains("impl Thing"));

        let impl_chunk = &chunks[2];
        assert!(impl_chunk.content.contains("method_one"));
        assert!(impl_chunk.content.contains("method_two"));
    }

    #[test]
    fn nested_fn_is_swallowed_into_parent() {
        let source = r#"
fn outer() -> i32 {
    fn inner() -> i32 {
        2
    }
    inner()
}
"#;
        let chunks = chunk_source("rs", source).expect("should produce chunks");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "function");
        assert!(chunks[0].content.contains("fn inner"));
    }

    #[test]
    fn unsupported_extension_returns_none() {
        assert!(chunk_source("go", "func f() {}").is_none());
    }

    #[test]
    fn file_with_no_functions_returns_none_so_caller_falls_back() {
        assert!(chunk_source("rs", "const X: i32 = 1;\nconst Y: i32 = 2;\n").is_none());
    }

    #[test]
    fn chunk_or_whole_file_falls_back_for_unsupported_language() {
        let chunks = chunk_or_whole_file("go", "func f() {}\n");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "file");
    }

    #[test]
    fn python_functions_and_classes() {
        let source = r#"
import os

CONST = 1

def free_function(x):
    return x + 1

class Thing:
    def method_one(self):
        return 1

    def method_two(self):
        return 2
"#;
        let chunks = chunk_source("py", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        assert_eq!(kinds, vec!["file", "function", "class"]);

        assert!(chunks[1].content.contains("def free_function"));
        assert!(!chunks[1].content.contains("class Thing"));

        assert!(chunks[2].content.contains("method_one"));
        assert!(chunks[2].content.contains("method_two"));
    }

    #[test]
    fn javascript_functions_classes_and_arrow_consts() {
        let source = r#"
import { readFile } from "fs";

function namedFn() {
    return 1;
}

const arrowFn = () => {
    return 2;
};

class Thing {
    methodOne() {
        return 1;
    }
}
"#;
        let chunks = chunk_source("js", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        assert_eq!(kinds, vec!["file", "function", "function", "class"]);

        assert!(chunks[1].content.contains("namedFn"));
        assert!(chunks[2].content.contains("arrowFn"));
        assert!(chunks[3].content.contains("methodOne"));
    }

    #[test]
    fn typescript_interface_becomes_its_own_chunk() {
        let source = r#"
interface User {
    id: number;
    name: string;
}

function greet(user: User): string {
    return `hi ${user.name}`;
}
"#;
        let chunks = chunk_source("ts", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        assert_eq!(kinds, vec!["interface", "function"]);
        assert!(chunks[0].content.contains("interface User"));
        assert!(chunks[1].content.contains("function greet"));
    }
}
