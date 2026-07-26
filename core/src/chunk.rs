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
    // The chunk's own identifier (function/method/class/... name), when one
    // exists — `None` for preamble/whole-file chunks and any matched node
    // whose grammar doesn't expose a name in a shape `extract_name`
    // recognizes (e.g. an `impl` block, which names a type, not itself).
    // This is what powers exact-match symbol lookup (see `store.rs`'s
    // `name` column): fuzzy/semantic search alone can't guarantee "this is
    // literally the function called `validate_jwt`", only "this looks
    // related".
    pub name: Option<String>,
}

fn whole_file_chunk(source: &str) -> Chunk {
    Chunk {
        start_line: 1,
        end_line: source.lines().count().max(1) as i32,
        kind: "file".to_string(),
        content: source.to_string(),
        name: None,
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
        "go" => chunk_with(tree_sitter_go::LANGUAGE.into(), GO_QUERY_SRC, source),
        "java" => chunk_with(tree_sitter_java::LANGUAGE.into(), JAVA_QUERY_SRC, source),
        // ".h" is ambiguous between C and C++; default to C, the older and
        // more common convention for that bare extension. C++ headers
        // typically use one of the extensions below instead.
        "c" | "h" => chunk_with(tree_sitter_c::LANGUAGE.into(), C_QUERY_SRC, source),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => {
            chunk_with(tree_sitter_cpp::LANGUAGE.into(), CPP_QUERY_SRC, source)
        }
        _ => None,
    }
}

// (function_item) covers free functions *and* methods inside an impl block —
// `chunk_with` decides whether the impl or its individual methods end up as
// the actual chunks.
const RUST_QUERY_SRC: &str = "(function_item) @item (impl_item) @item";

// Same idea for Python: methods inside a class are `function_definition`
// nodes nested in the `class_definition`.
const PYTHON_QUERY_SRC: &str = "(function_definition) @item (class_definition) @item";

// Covers named function declarations/classes plus the common
// `const foo = () => {...}` / `const foo = function () {...}` pattern —
// idiomatic JS/TS rarely uses bare `function` declarations for top-level
// exports. `method_definition` covers methods inside a class body.
const JS_QUERY_SRC: &str = "
(function_declaration) @item
(generator_function_declaration) @item
(class_declaration) @item
(method_definition) @item
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
(method_definition) @item
(interface_declaration) @item
(lexical_declaration (variable_declarator value: (arrow_function))) @item
(lexical_declaration (variable_declarator value: (function_expression))) @item
";

// Go has no impl/class container: methods are top-level `method_declaration`
// nodes carrying their own receiver (`func (t *Thing) Method() {...}`), never
// nested inside the type they're defined on. `type_declaration` covers
// struct/interface type definitions, which stay meaningful chunks on their
// own (analogous to `interface_declaration` in TS) since methods never
// nest inside them.
const GO_QUERY_SRC: &str = "
(function_declaration) @item
(method_declaration) @item
(type_declaration) @item
";

// Java: `method_declaration`/`constructor_declaration` inside a class or
// interface body are matched the same way rust/python/js methods inside an
// impl/class are — the `body: (block)` constraint on `method_declaration`
// excludes abstract interface method *signatures* (no body), so an
// interface with only abstract methods still collapses to one meaningful
// "interface" chunk instead of a pile of one-line signature fragments.
const JAVA_QUERY_SRC: &str = "
(class_declaration) @item
(interface_declaration) @item
(enum_declaration) @item
(method_declaration body: (block)) @item
(constructor_declaration) @item
";

// C: `function_definition` is only the node for a function with a body —
// a bare prototype (`int foo(int);`) is a different node kind and never
// matched, same effect as Java's `body:` constraint above but the grammar
// already gives us that split for free.
const C_QUERY_SRC: &str = "
(function_definition) @item
(struct_specifier) @item
";

// C++: same `function_definition` shape as C, but member functions defined
// inline inside a class/struct body are also `function_definition` nodes,
// so `class_specifier`/`struct_specifier` need container treatment (see
// `is_container_kind`) the way rust `impl`/python `class` already get it.
const CPP_QUERY_SRC: &str = "
(function_definition) @item
(class_specifier) @item
(struct_specifier) @item
";

fn kind_of(node_kind: &str) -> &'static str {
    match node_kind {
        "impl_item" => "impl",
        "class_definition" | "class_declaration" | "class_specifier" => "class",
        "interface_declaration" => "interface",
        "type_declaration" => "type",
        "enum_declaration" => "enum",
        "struct_specifier" => "struct",
        _ => "function",
    }
}

// `impl`/`class`/`struct` blocks are containers: when they have methods
// inside, the methods should be the chunks, not one blob covering the whole
// container. `interface_declaration` is included too (see the comment on
// that arm below) even though for TS specifically it never actually
// triggers — a TS interface's members are type signatures, not matched
// nodes, so it always stays a single meaningful chunk on its own regardless.
fn is_container_kind(node_kind: &str) -> bool {
    matches!(
        node_kind,
        "impl_item"
            | "class_definition"
            | "class_declaration"
            | "class_specifier"
            | "struct_specifier"
            // Safe to include unconditionally: TS's `interface_declaration`
            // members are type/method *signatures*, never matched nodes, so
            // this never actually triggers for TS (see the TS query above).
            // Java's is different: default/static interface methods do have
            // a body and get matched, so this correctly swallows those into
            // individual chunks the same way a class's methods do.
            | "interface_declaration"
    )
}

// Pulls the identifier a matched node is named after, when the grammar
// exposes one. Three shapes, tried in order:
// 1. a direct `name` field — covers the common case across every grammar
//    here (rust `function_item`, python `function_definition`/
//    `class_definition`, java/js/ts declarations, go's `type_declaration`
//    when the field happens to sit directly on it, C/C++'s
//    `struct_specifier`/`class_specifier`).
// 2. one level down, on a named child — covers wrapper nodes where the
//    real name lives one level in: JS/TS `(lexical_declaration
//    (variable_declarator value: (arrow_function)))` (the query captures
//    the outer `lexical_declaration`, but the name is the inner
//    `variable_declarator`'s), and go's `type_declaration` (wraps a
//    `type_spec`, which is what actually carries the name).
// 3. following a `declarator` field chain down to a plain identifier —
//    C/C++ function names aren't exposed via a `name` field at all, they're
//    buried inside nested `declarator`s (`int *foo(...)` is
//    `function_declarator { declarator: pointer_declarator { declarator:
//    identifier } }`), so this walks that chain instead.
fn extract_name(node: Node, source: &str) -> Option<String> {
    let text_of = |n: Node| source[n.start_byte()..n.end_byte()].to_string();

    if let Some(n) = node.child_by_field_name("name") {
        return Some(text_of(n));
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if let Some(n) = child.child_by_field_name("name") {
            return Some(text_of(n));
        }
    }

    let mut declarator = node.child_by_field_name("declarator");
    while let Some(n) = declarator {
        if matches!(n.kind(), "identifier" | "field_identifier") {
            return Some(text_of(n));
        }
        declarator = n.child_by_field_name("declarator");
    }

    None
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

    // Drop a container (impl/class) when it has at least one other matched
    // node nested inside it — its methods become individually meaningful
    // chunks instead of one averaged-together blob. Measured on this
    // project's own `impl Store` (5 methods, ~180 lines): keeping it as one
    // chunk buried `hybrid_search`'s own scoring logic at rank 73 for a
    // query about... hybrid search scoring. that's the exact dilution
    // problem chunking exists to fix, just one level up from whole-file. A
    // container with no matched children (rare, but possible) still gets
    // kept whole rather than silently dropped.
    let candidates: Vec<Node> = nodes
        .iter()
        .copied()
        .filter(|&node| {
            if !is_container_kind(node.kind()) {
                return true;
            }
            !nodes.iter().any(|&other| {
                other.id() != node.id()
                    && other.start_byte() >= node.start_byte()
                    && other.end_byte() <= node.end_byte()
            })
        })
        .collect();

    let mut sorted = candidates;
    sorted.sort_by_key(|n| n.start_byte());

    // Keep only outermost matches among what's left: a closure or nested
    // function inside a function body is still swallowed into its parent
    // function's chunk (that nesting is a genuinely coherent single unit,
    // unlike a method sitting among several unrelated sibling methods).
    let mut kept: Vec<Node> = Vec::new();
    let mut last_end = 0usize;
    for node in sorted {
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
                name: None,
            });
        }
    }

    for node in kept {
        chunks.push(Chunk {
            start_line: node.start_position().row as i32 + 1,
            end_line: node.end_position().row as i32 + 1,
            kind: kind_of(node.kind()).to_string(),
            content: source[node.start_byte()..node.end_byte()].to_string(),
            // `impl_item` is special-cased out rather than left to the
            // generic fallback: its named children include the trait being
            // implemented (`impl Debug for Thing`), which itself often
            // exposes a `name` field ("Debug") — the fallback would
            // mistake that for the impl block's own name, which is
            // actively misleading for exact-match lookup, not just absent.
            name: if node.kind() == "impl_item" {
                None
            } else {
                extract_name(node, source)
            },
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
    fn methods_inside_an_impl_become_individual_chunks() {
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

        // preamble ("file"), free_function, method_one, method_two — no
        // "impl" chunk at all, since the impl block had matched children.
        // one blob covering both methods would dilute each method's own
        // signal exactly the way whole-file embedding used to, which is
        // the bug this test guards against (measured on this project's own
        // `impl Store`: a 5-method, ~180-line impl chunk buried its own
        // `hybrid_search` method at rank 73 for a query about hybrid search).
        assert_eq!(kinds, vec!["file", "function", "function", "function"]);

        let preamble = &chunks[0];
        assert!(preamble.content.contains("use std::fmt"));
        assert!(preamble.content.contains("const FOO"));

        let func = &chunks[1];
        assert!(func.content.contains("fn free_function"));
        assert!(!func.content.contains("impl Thing"));

        let method_one = &chunks[2];
        assert!(method_one.content.contains("method_one"));
        assert!(!method_one.content.contains("method_two"));

        let method_two = &chunks[3];
        assert!(method_two.content.contains("method_two"));
        assert!(!method_two.content.contains("method_one"));
    }

    #[test]
    fn impl_with_no_matched_children_is_kept_whole() {
        // an impl block that (for whatever reason) has no function_item
        // children matched shouldn't just vanish — fall back to keeping it
        // as one chunk rather than silently dropping real code.
        let source = "impl std::fmt::Debug for Thing {}\n";
        let chunks = chunk_source("rs", source).expect("should produce chunks");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "impl");
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
        assert!(chunk_source("rb", "def f; end").is_none());
    }

    #[test]
    fn file_with_no_functions_returns_none_so_caller_falls_back() {
        assert!(chunk_source("rs", "const X: i32 = 1;\nconst Y: i32 = 2;\n").is_none());
    }

    #[test]
    fn chunk_or_whole_file_falls_back_for_unsupported_language() {
        let chunks = chunk_or_whole_file("rb", "def f; end\n");
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
        // no "class" chunk: Thing had matched method children, so its
        // methods become individual chunks instead of one combined blob.
        assert_eq!(kinds, vec!["file", "function", "function", "function"]);

        assert!(chunks[1].content.contains("def free_function"));
        assert!(!chunks[1].content.contains("class Thing"));

        assert!(chunks[2].content.contains("method_one"));
        assert!(!chunks[2].content.contains("method_two"));

        assert!(chunks[3].content.contains("method_two"));
        assert!(!chunks[3].content.contains("method_one"));
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
        // no "class" chunk: Thing had a matched method_definition child, so
        // methodOne becomes its own chunk instead of being folded into Thing.
        assert_eq!(kinds, vec!["file", "function", "function", "function"]);

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

    #[test]
    fn go_functions_methods_and_types() {
        let source = r#"
package main

import "fmt"

type Thing struct {
    Name string
}

func (t *Thing) Greet() string {
    return "hi " + t.Name
}

func freeFunction(x int) int {
    return x + 1
}
"#;
        let chunks = chunk_source("go", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        // Go methods carry their own receiver and are never nested inside
        // the type they're defined on, so the struct's `type` chunk and its
        // method both survive as separate chunks (unlike impl/class, there's
        // no container to swallow the method into).
        assert_eq!(kinds, vec!["file", "type", "function", "function"]);

        assert!(chunks[0].content.contains(r#"import "fmt""#));

        let type_chunk = &chunks[1];
        assert!(type_chunk.content.contains("type Thing struct"));
        assert!(!type_chunk.content.contains("func"));

        let method = &chunks[2];
        assert!(method.content.contains("func (t *Thing) Greet"));

        let free_fn = &chunks[3];
        assert!(free_fn.content.contains("func freeFunction"));
    }

    #[test]
    fn go_interface_type_becomes_its_own_chunk() {
        let source = r#"
package main

type Greeter interface {
    Greet() string
}

func UseGreeter(g Greeter) string {
    return g.Greet()
}
"#;
        let chunks = chunk_source("go", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        // "package main" itself becomes the preamble "file" chunk, same as
        // imports/consts before the first matched node in the other tests.
        assert_eq!(kinds, vec!["file", "type", "function"]);
        assert!(chunks[0].content.contains("package main"));
        assert!(chunks[1].content.contains("type Greeter interface"));
        assert!(chunks[2].content.contains("func UseGreeter"));
    }

    #[test]
    fn java_methods_inside_a_class_become_individual_chunks() {
        let source = r#"
import java.util.List;

class Thing {
    Thing() {
        System.out.println("built");
    }

    int methodOne() {
        return 1;
    }

    int methodTwo() {
        return 2;
    }
}
"#;
        let chunks = chunk_source("java", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        // no "class" chunk: Thing had matched constructor/method children,
        // same swallowing behavior as rust impl / python class.
        assert_eq!(kinds, vec!["file", "function", "function", "function"]);

        assert!(chunks[0].content.contains("import java.util.List"));
        assert!(chunks[1].content.contains("Thing()"));
        assert!(chunks[2].content.contains("methodOne"));
        assert!(!chunks[2].content.contains("methodTwo"));
        assert!(chunks[3].content.contains("methodTwo"));
    }

    #[test]
    fn java_abstract_interface_stays_one_chunk_but_default_methods_split_out() {
        let source = r#"
interface Greeter {
    String greet();

    default String shout() {
        return greet().toUpperCase();
    }
}
"#;
        let chunks = chunk_source("java", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        // the abstract `greet()` signature has no body so it's never matched
        // at all; the interface has one matched child (`shout`, which does
        // have a body), so the interface container gets swallowed just like
        // a class with methods would. everything before the surviving
        // `shout` chunk — the interface opener plus the abstract `greet()`
        // signature — falls out as the leading "file" preamble chunk, same
        // mechanism as imports/consts before a first function elsewhere.
        assert_eq!(kinds, vec!["file", "function"]);
        assert!(chunks[0].content.contains("interface Greeter"));
        assert!(chunks[0].content.contains("String greet()"));
        assert!(chunks[1].content.contains("shout"));
        assert!(!chunks[1].content.contains("interface Greeter"));
    }

    #[test]
    fn java_interface_with_no_default_methods_is_kept_whole() {
        let source = r#"
interface Greeter {
    String greet();
}
"#;
        let chunks = chunk_source("java", source).expect("should produce chunks");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "interface");
        assert!(chunks[0].content.contains("String greet()"));
    }

    #[test]
    fn c_functions_and_structs() {
        let source = r#"
#include <stdio.h>

struct Point {
    int x;
    int y;
};

int add(int a, int b) {
    return a + b;
}

int square(int x);
"#;
        let chunks = chunk_source("c", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        // `square`'s prototype has no body, so it's a `declaration` node,
        // never matched — only the two real definitions show up.
        assert_eq!(kinds, vec!["file", "struct", "function"]);

        assert!(chunks[0].content.contains("#include <stdio.h>"));
        assert!(chunks[1].content.contains("struct Point"));
        assert!(chunks[2].content.contains("int add"));
        assert!(!chunks[2].content.contains("square"));
    }

    #[test]
    fn cpp_methods_inside_a_class_become_individual_chunks() {
        let source = r#"
#include <string>

class Thing {
public:
    int methodOne() {
        return 1;
    }

    int methodTwo() {
        return 2;
    }
};

int freeFunction(int x) {
    return x + 1;
}
"#;
        let chunks = chunk_source("cpp", source).expect("should produce chunks");
        let kinds: Vec<&str> = chunks.iter().map(|c| c.kind.as_str()).collect();
        // no "class" chunk: Thing had matched method children, methods
        // become individually meaningful chunks instead of one blob,
        // same swallowing behavior as rust impl / python class.
        assert_eq!(kinds, vec!["file", "function", "function", "function"]);

        assert!(chunks[0].content.contains("#include <string>"));
        assert!(chunks[1].content.contains("methodOne"));
        assert!(!chunks[1].content.contains("methodTwo"));
        assert!(chunks[2].content.contains("methodTwo"));
        assert!(chunks[3].content.contains("freeFunction"));
    }

    #[test]
    fn cpp_struct_with_no_matched_children_is_kept_whole() {
        let source = "struct Point {\n    int x;\n    int y;\n};\n";
        let chunks = chunk_source("cpp", source).expect("should produce chunks");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, "struct");
    }

    #[test]
    fn rust_function_and_impl_names() {
        let source = "fn free_function(x: i32) -> i32 {\n    x\n}\n\nimpl std::fmt::Debug for Thing {}\n";
        let chunks = chunk_source("rs", source).expect("should produce chunks");
        assert_eq!(chunks[0].name.as_deref(), Some("free_function"));
        // impl blocks name a type, not themselves — extract_name finds
        // nothing meaningful to call this chunk by, so it's None rather
        // than a misleading guess.
        assert_eq!(chunks[1].name, None);
    }

    #[test]
    fn js_arrow_const_name_comes_from_the_inner_declarator() {
        let source = "const arrowFn = () => {\n    return 2;\n};\n";
        let chunks = chunk_source("js", source).expect("should produce chunks");
        assert_eq!(chunks[0].name.as_deref(), Some("arrowFn"));
    }

    #[test]
    fn go_type_name_comes_from_the_inner_type_spec() {
        let source = "package main\n\ntype Greeter interface {\n    Greet() string\n}\n";
        let chunks = chunk_source("go", source).expect("should produce chunks");
        let type_chunk = chunks.iter().find(|c| c.kind == "type").unwrap();
        assert_eq!(type_chunk.name.as_deref(), Some("Greeter"));
    }

    #[test]
    fn c_function_name_follows_the_declarator_chain() {
        // a pointer return type means the identifier is nested inside a
        // pointer_declarator, not exposed as a direct `name` field —
        // exactly the case extract_name's declarator-chain fallback exists
        // for.
        let source = "int *make_thing(int x) {\n    return 0;\n}\n";
        let chunks = chunk_source("c", source).expect("should produce chunks");
        assert_eq!(chunks[0].name.as_deref(), Some("make_thing"));
    }
}
