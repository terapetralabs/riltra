// SPDX-License-Identifier: Apache-2.0
//! M2 syntax baseline: tree-sitter definitions, imports, and local call
//! candidates with source spans and byte offsets.
//!
//! No semantic resolution anywhere here: a "call" records the callee text as
//! written (e.g. `w.process`), never a resolved target. Anything the grammar
//! cannot represent is reported via `errors` or left out, never guessed.

use std::collections::HashSet;
use std::path::Path;

use crate::json::escape;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Language {
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Rust,
    C,
    Cpp,
    Go,
    Java,
}

pub fn language_name(lang: Language) -> &'static str {
    match lang {
        Language::Python => "python",
        Language::JavaScript => "javascript",
        Language::TypeScript => "typescript",
        Language::Tsx => "tsx",
        Language::Rust => "rust",
        Language::C => "c",
        Language::Cpp => "cpp",
        Language::Go => "go",
        Language::Java => "java",
    }
}

/// Grammar crate version backing a language, matching `store::grammar_versions`
/// and `Cargo.lock` (pinned by test on both sides).
pub fn grammar_version(lang: Language) -> &'static str {
    match lang {
        Language::Python => "python:0.25.0",
        Language::JavaScript => "javascript:0.25.0",
        Language::TypeScript => "typescript:0.23.2",
        Language::Tsx => "tsx:0.23.2",
        Language::Rust => "rust:0.24.2",
        Language::C => "c:0.24.2",
        Language::Cpp => "cpp:0.23.4",
        Language::Go => "go:0.25.0",
        Language::Java => "java:0.23.5",
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FileKind {
    Supported(Language),
    /// CUDA is explicitly experimental/unsupported; never force-parsed.
    CudaExperimental,
    UnsupportedExtension,
}

/// Extension map. Headers (`.h`) and `.inl` template bodies parse as C++:
/// real-world headers mix both languages, and the C++ grammar covers C
/// declarations, while the C grammar misreads C++ constructs (namespaces and
/// classes) as functions with errors. A `.inl` fragment that needs its header
/// context keeps its errors in `errors`. Use `.c` for strict C parsing.
pub fn classify(path: &Path) -> FileKind {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("py") => FileKind::Supported(Language::Python),
        Some("js" | "mjs" | "cjs") => FileKind::Supported(Language::JavaScript),
        Some("ts" | "mts" | "cts") => FileKind::Supported(Language::TypeScript),
        Some("tsx") => FileKind::Supported(Language::Tsx),
        Some("rs") => FileKind::Supported(Language::Rust),
        Some("go") => FileKind::Supported(Language::Go),
        Some("java") => FileKind::Supported(Language::Java),
        Some("c") => FileKind::Supported(Language::C),
        Some("h" | "hh" | "hpp" | "hxx" | "inl") => FileKind::Supported(Language::Cpp),
        Some("cc" | "cpp" | "cxx") => FileKind::Supported(Language::Cpp),
        Some("cu" | "cuh") => FileKind::CudaExperimental,
        _ => FileKind::UnsupportedExtension,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CalleeSpan {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    /// "definition" | "import" | "call" (call = unresolved candidate).
    pub kind: &'static str,
    /// Definition/call name; `None` only when the grammar yields none.
    pub name: Option<String>,
    /// Raw source text for imports; `None` otherwise.
    pub text: Option<String>,
    pub span: Span,
    pub byte_start: usize,
    pub byte_end: usize,
    /// Call-callee expression range (`function` field); `None` for defs,
    /// imports, and calls whose grammar yields no function field.
    pub callee: Option<CalleeSpan>,
    /// Terminal identifier/property within `callee` (receiver split off);
    /// `None` when inseparable or absent.
    pub tcallee: Option<CalleeSpan>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileFacts {
    /// For each definition, `(start, end, name_start, name_end)`: where its own
    /// identifier stands. Never stored; an anchor proof reads it.
    pub identifiers: Vec<(usize, usize, usize, usize)>,
    pub language: Language,
    pub bytes: usize,
    pub definitions: Vec<Fact>,
    pub imports: Vec<Fact>,
    pub calls: Vec<Fact>,
    /// Name occurrences of a C++ source (E7 Slice 2): `kind` is the role, the
    /// name is the identifier text, the span and bytes address the identifier.
    /// Never part of the `scan` document; stored and queried by the index only.
    pub reference_occurrences: Vec<Fact>,
    /// Heritage facts (PR 6): supertypes named by a type declaration. Never
    /// part of the `scan` document; stored by the index only.
    pub heritage: Vec<Heritage>,
    /// Containment facts (PR 6): each definition, and each symbol-less container,
    /// that has an enclosing container, paired with its nearest one, in visit
    /// order. Never part of the `scan` document; stored by the index only.
    pub containment: Vec<Containment>,
    /// Doc comments (PR 6): the comment block above a definition, and a Python
    /// docstring. Never part of the `scan` document; stored by the index only.
    pub doc_comments: Vec<DocComment>,
    pub errors: u32,
}

/// A definition, or a container without a symbol, and its nearest enclosing
/// container (PR 6). Both sides are identified by the byte range a stored
/// definition has (or, for a symbol-less container, the node's range), never
/// by a symbol rowid. Never part of the `scan` document.
#[derive(Debug, Clone, PartialEq)]
pub struct Containment {
    pub child_kind: &'static str,
    pub child_name: Option<String>,
    pub child_byte_start: usize,
    pub child_byte_end: usize,
    pub parent_kind: &'static str,
    pub parent_name: Option<String>,
    pub parent_byte_start: usize,
    pub parent_byte_end: usize,
}

/// A type declaration naming a supertype (PR 6). `supertype_text` is the exact
/// source text (the written evidence); `supertype_name` is a normalised lookup
/// key, never a resolution. Never part of the `scan` document.
#[derive(Debug, Clone, PartialEq)]
pub struct Heritage {
    pub subtype: String,
    pub relation: &'static str,
    pub supertype_text: String,
    pub supertype_name: Option<String>,
    /// The supertype expression.
    pub span: Span,
    pub byte_start: usize,
    pub byte_end: usize,
    /// The stored definition's byte range (Rust: the `impl` item).
    pub decl_byte_start: usize,
    pub decl_byte_end: usize,
}

/// Longest doc text stored per row, in bytes (cut on a UTF-8 character boundary).
pub const DOC_TEXT_CAP: usize = 4096;

/// A doc comment of a stored definition (PR 6). The definition is identified by
/// its byte range, as `symbols` stores it. Never part of the `scan` document.
#[derive(Debug, Clone, PartialEq)]
pub struct DocComment {
    pub def_byte_start: usize,
    pub def_byte_end: usize,
    /// "above" | "docstring".
    pub form: &'static str,
    /// Raw source text, comment markers kept, at most `DOC_TEXT_CAP` bytes.
    pub text: String,
    /// True when the block was longer than `DOC_TEXT_CAP`.
    pub truncated: bool,
    /// The whole block, not only the kept text.
    pub span: Span,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Roles of a reference occurrence. These strings are stored in
/// `reference_occurrences.role` and reported by `search --occurrences`.
pub const ROLE_MEMBER_ACCESS: &str = "member-access";
pub const ROLE_ADDRESS_OF_BINDING: &str = "address-of-binding";
pub const ROLE_MACRO_ARGUMENT: &str = "macro-argument";

/// Confidence of an occurrence, derived from its role and never stored: the
/// syntax proves the role and the location (`observed`), or the role needs an
/// assumption outside the syntax (`candidate`).
pub fn occurrence_confidence(role: &str) -> &'static str {
    match role {
        ROLE_MEMBER_ACCESS | ROLE_ADDRESS_OF_BINDING => "observed",
        _ => "candidate",
    }
}

fn ts_language(lang: Language) -> tree_sitter::Language {
    match lang {
        Language::Python => tree_sitter_python::LANGUAGE.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::C => tree_sitter_c::LANGUAGE.into(),
        Language::Cpp => tree_sitter_cpp::LANGUAGE.into(),
        Language::Go => tree_sitter_go::LANGUAGE.into(),
        Language::Java => tree_sitter_java::LANGUAGE.into(),
    }
}

fn node_text<'a>(node: tree_sitter::Node, src: &'a [u8]) -> Option<&'a str> {
    node.utf8_text(src).ok()
}

fn span_of(node: tree_sitter::Node) -> (Span, usize, usize) {
    let s = node.start_position();
    let e = node.end_position();
    (
        Span {
            start_line: (s.row + 1) as u32,
            start_col: (s.column + 1) as u32,
            end_line: (e.row + 1) as u32,
            end_col: (e.column + 1) as u32,
        },
        node.start_byte(),
        node.end_byte(),
    )
}

/// Name child via the `name` field, else the first identifier-like child.
fn def_name_node<'t>(node: tree_sitter::Node<'t>, src: &[u8]) -> Option<tree_sitter::Node<'t>> {
    // `property` names a JS class field (`m = () => {}`).
    node.child_by_field_name("name")
        .or_else(|| node.child_by_field_name("property"))
        .filter(|n| node_text(*n, src).is_some())
        .or_else(|| {
            (0..node.named_child_count())
                .filter_map(|i| node.named_child(i))
                .find(|child| {
                    matches!(
                        child.kind(),
                        "identifier" | "type_identifier" | "property_identifier"
                    ) && node_text(*child, src).is_some()
                })
        })
}

fn def_name(node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    def_name_node(node, src).and_then(|n| node_text(n, src).map(str::to_string))
}

/// Record that the definition spanning `node` declares the identifier `name`.
fn declares(facts: &mut FileFacts, node: tree_sitter::Node, name: Option<tree_sitter::Node>) {
    if let Some(name) = name {
        facts.identifiers.push((
            node.start_byte(),
            node.end_byte(),
            name.start_byte(),
            name.end_byte(),
        ));
    }
}

/// C/C++ declarator chain: `function_declarator`/`pointer`/`parenthesized`
/// wrappers around the real identifier (`field_identifier` for methods,
/// `qualified_identifier` for `Scope::name` definitions, kept verbatim as an
/// unresolved candidate). Bounded walk, `None` when anonymous.
fn c_function_name(node: tree_sitter::Node, src: &[u8]) -> Option<String> {
    c_name_node(node).and_then(|n| node_text(n, src).map(str::to_string))
}

fn c_name_node(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
    let mut current = node.child_by_field_name("declarator")?;
    for _ in 0..6 {
        match current.kind() {
            "identifier" | "field_identifier" | "qualified_identifier" => return Some(current),
            "function_declarator"
            | "parenthesized_declarator"
            | "pointer_declarator"
            | "array_declarator" => {
                current = current.child_by_field_name("declarator").or_else(|| {
                    for i in 0..current.named_child_count() {
                        if let Some(c) = current.named_child(i) {
                            if c.is_named()
                                && matches!(
                                    c.kind(),
                                    "identifier"
                                        | "field_identifier"
                                        | "qualified_identifier"
                                        | "function_declarator"
                                        | "parenthesized_declarator"
                                        | "pointer_declarator"
                                        | "array_declarator"
                                )
                            {
                                return Some(c);
                            }
                        }
                    }
                    None
                })?;
            }
            _ => return None,
        }
    }
    None
}

/// Extra validation for relationship derivation, never new syntax facts.
/// Whole-function ranges cannot identify a declaration token or distinguish
/// a nested anonymous scope. All ranges index the digest-checked source.
#[derive(Default)]
pub struct RelationshipProof {
    pub declarations: Vec<(usize, usize)>,
    pub owners: std::collections::BTreeMap<(usize, usize), Option<(usize, usize)>>,
    pub call_blocks: std::collections::BTreeMap<(usize, usize), Option<(usize, usize)>>,
    pub local_function_aliases: Vec<LocalFunctionAlias>,
}

/// Syntax candidate only; graph derivation still has to match both SCIP
/// identities and a source-backed callable definition.
pub struct LocalFunctionAlias {
    pub binding: (usize, usize),
    pub target: (usize, usize),
    pub block: (usize, usize),
}

/// JS/TS `const f = () => {}`, `const f = function () {}` and class fields
/// `m = () => {}`: a plain name bound to a function expression. The binding is
/// the definition; destructuring patterns and other values are not. Calls
/// inside stay unattributed to any owner: the M6 corpus fixes that an anonymous
/// callable has no named owner and no closure inference is made.
/// shortcut: a function value reassigned later (`f = () => {}` without a
/// declaration) is not a binding here.
fn is_named_function_binding(node: tree_sitter::Node<'_>) -> bool {
    if !matches!(
        node.kind(),
        "variable_declarator" | "field_definition" | "public_field_definition"
    ) {
        return false;
    }
    let named = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("property"))
        .is_some_and(|n| {
            matches!(
                n.kind(),
                "identifier" | "property_identifier" | "private_property_identifier"
            )
        });
    named
        && node.child_by_field_name("value").is_some_and(|v| {
            matches!(
                v.kind(),
                "arrow_function" | "function_expression" | "function"
            )
        })
}

/// A class field bound to a function is a method; a variable is a function.
fn binding_kind(node: tree_sitter::Node<'_>) -> &'static str {
    if node.kind() == "variable_declarator" {
        "function"
    } else {
        "method"
    }
}

pub fn relationship_proof(lang: Language, src: &[u8]) -> RelationshipProof {
    fn callable(node: tree_sitter::Node<'_>) -> bool {
        matches!(
            node.kind(),
            "function_definition"
                | "function_declaration"
                | "generator_function_declaration"
                | "function_item"
                | "method_definition"
                | "method_declaration"
                | "constructor_declaration"
                | "compact_constructor_declaration"
        )
    }
    fn name(node: tree_sitter::Node<'_>) -> Option<tree_sitter::Node<'_>> {
        node.child_by_field_name("name")
            .or_else(|| c_name_node(node))
    }
    /// What a node's ancestors say about it: the nearest enclosing named
    /// callable definition (none when a lambda, closure or class is closer)
    /// and the nearest enclosing `block`. Each node passes its own
    /// contribution to its children, so no node climbs its `parent()` chain.
    #[derive(Clone, Copy)]
    struct Scope {
        owner: Option<(usize, usize)>,
        block: Option<(usize, usize)>,
    }
    /// `parent` is the node the walk reached `node` from.
    fn rust_alias(
        node: tree_sitter::Node<'_>,
        parent: Option<tree_sitter::Node<'_>>,
    ) -> Option<LocalFunctionAlias> {
        let block = parent.filter(|p| p.kind() == "block")?;
        let pattern = node.child_by_field_name("pattern")?;
        if pattern.kind() != "identifier"
            || node.child_by_field_name("type").is_some()
            || (0..node.named_child_count()).any(|i| {
                node.named_child(i)
                    .is_some_and(|child| child.kind() == "mutable_specifier")
            })
        {
            return None;
        }
        let value = node.child_by_field_name("value")?;
        let target = match value.kind() {
            "identifier" => value,
            "scoped_identifier"
                if value
                    .child_by_field_name("path")
                    .is_some_and(|path| path.kind() == "identifier") =>
            {
                value.child_by_field_name("name")?
            }
            _ => return None,
        };
        Some(LocalFunctionAlias {
            binding: (pattern.start_byte(), pattern.end_byte()),
            target: (target.start_byte(), target.end_byte()),
            block: (block.start_byte(), block.end_byte()),
        })
    }
    let mut proof = RelationshipProof::default();
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&ts_language(lang)).is_err() {
        return proof;
    }
    // shortcut: no parse time limit either; see `parse_source`.
    let Some(tree) = parser.parse(src, None) else {
        return proof;
    };
    if tree.root_node().has_error() {
        return proof;
    }
    let mut pending = vec![(
        tree.root_node(),
        None,
        Scope {
            owner: None,
            block: None,
        },
    )];
    while let Some((node, parent, scope)) = pending.pop() {
        let declared = if callable(node) { name(node) } else { None };
        if let Some(n) = declared {
            proof.declarations.push((n.start_byte(), n.end_byte()));
        }
        let span = (node.start_byte(), node.end_byte());
        if matches!(
            node.kind(),
            "call" | "call_expression" | "method_invocation"
        ) {
            proof.owners.insert(span, scope.owner);
            proof.call_blocks.insert(span, scope.block);
        }
        if lang == Language::Rust && node.kind() == "let_declaration" {
            if let Some(alias) = rust_alias(node, parent) {
                proof.local_function_aliases.push(alias);
            }
        }
        // A C++ direct-initialization call has no `call_expression` node, so
        // its owner is recorded against the same range the call fact uses.
        if node.kind() == "declaration" {
            if let Some((ty, args)) = direct_init_parts(node) {
                proof
                    .owners
                    .insert((ty.start_byte(), args.end_byte()), scope.owner);
            }
        }
        let inner = Scope {
            owner: if declared.is_some() {
                Some(span)
            } else if matches!(
                node.kind(),
                "lambda"
                    | "lambda_expression"
                    | "arrow_function"
                    | "closure_expression"
                    | "function_expression"
                    | "func_literal"
                    | "generator_function"
                    | "class_definition"
                    | "class_declaration"
                    | "class_specifier"
                    | "struct_specifier"
            ) {
                None
            } else {
                scope.owner
            },
            block: if node.kind() == "block" {
                Some(span)
            } else {
                scope.block
            },
        };
        for i in (0..node.named_child_count()).rev() {
            if let Some(child) = node.named_child(i) {
                pending.push((child, Some(node), inner));
            }
        }
    }
    proof
}

fn push_def(facts: &mut FileFacts, kind: &'static str, node: tree_sitter::Node, src: &[u8]) {
    let (span, byte_start, byte_end) = span_of(node);
    declares(facts, node, def_name_node(node, src));
    facts.definitions.push(Fact {
        kind,
        name: def_name(node, src),
        text: None,
        span,
        byte_start,
        byte_end,
        callee: None,
        tcallee: None,
    });
}

/// Go `type X struct{...}`, `type X interface{...}`, or any other named type
/// (alias, defined type); one fact per `type_spec`, kind from its type node.
fn push_go_type(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let kind = match node.child_by_field_name("type").map(|t| t.kind()) {
        Some("struct_type") => "struct",
        Some("interface_type") => "interface",
        _ => "type",
    };
    push_def(facts, kind, node, src);
}

/// `class EXPORT Name { ... };` with an unknown macro between the keyword and
/// the name parses as a function definition: a return type `class EXPORT`, a bare
/// identifier declarator `Name`, and the class body as the function body. A
/// function definition as written has a parameter list, so a bare identifier declarator
/// under a tag-specifier type without a body is a macro-headed class or struct.
/// shortcut: members are recorded as plain functions (no class context), and a
/// macro with arguments (`class API(1) Name`) still fails to parse.
fn macro_headed_tag(node: tree_sitter::Node<'_>) -> Option<&'static str> {
    let declarator = node.child_by_field_name("declarator")?;
    if declarator.kind() != "identifier" {
        return None;
    }
    let ty = node.child_by_field_name("type")?;
    if ty.child_by_field_name("body").is_some() {
        return None;
    }
    match ty.kind() {
        "class_specifier" => Some("class"),
        "struct_specifier" => Some("struct"),
        _ => None,
    }
}

fn push_c_def(facts: &mut FileFacts, kind: &'static str, node: tree_sitter::Node, src: &[u8]) {
    let (span, byte_start, byte_end) = span_of(node);
    let mut identifier = c_name_node(node);
    while let Some(qualified) = identifier.filter(|n| n.kind() == "qualified_identifier") {
        identifier = qualified.child_by_field_name("name");
    }
    declares(facts, node, identifier);
    facts.definitions.push(Fact {
        kind,
        name: c_function_name(node, src),
        text: None,
        span,
        byte_start,
        byte_end,
        callee: None,
        tcallee: None,
    });
}

fn push_import(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    // Preprocessor nodes span the trailing newline; trim one so the fact
    // covers the directive itself.
    let mut end = node.end_byte();
    if end > node.start_byte() && src.get(end - 1) == Some(&b'\n') {
        end -= 1;
    }
    let pos = node.start_position();
    // Both end coordinates come from the same trimmed offset.
    let (end_line, end_col) = position_from(
        src,
        node.start_byte(),
        ((pos.row + 1) as u32, (pos.column + 1) as u32),
        end,
    );
    let span = Span {
        start_line: (pos.row + 1) as u32,
        start_col: (pos.column + 1) as u32,
        end_line,
        end_col,
    };
    facts.imports.push(Fact {
        kind: "import",
        name: None,
        text: std::str::from_utf8(&src[node.start_byte()..end])
            .ok()
            .map(|s| s.trim().to_string()),
        span,
        byte_start: node.start_byte(),
        byte_end: end,
        callee: None,
        tcallee: None,
    });
}

/// Named `struct X {...}` or anonymous `typedef struct {...} X` (name from the
/// enclosing `type_definition` declarator, span of the whole declaration).
fn push_struct(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    if node.child_by_field_name("name").is_some() {
        // A bare `struct X *p;` or forward declaration is a use, not a definition.
        if node.child_by_field_name("body").is_some() {
            push_tagged(facts, "struct", node, src);
        }
        return;
    }
    let Some(parent) = node.parent() else {
        return;
    };
    if parent.kind() != "type_definition" {
        return;
    }
    let identifier = parent
        .child_by_field_name("declarator")
        .filter(|d| matches!(d.kind(), "identifier" | "type_identifier"));
    let name = identifier.and_then(|d| node_text(d, src).map(|s| s.to_string()));
    let (span, byte_start, byte_end) = span_of(parent);
    declares(facts, parent, identifier);
    facts.definitions.push(Fact {
        kind: "struct",
        name,
        text: None,
        span,
        byte_start,
        byte_end,
        callee: None,
        tcallee: None,
    });
}

/// Whether a Rust item sits directly in an `impl` block.
fn in_impl(node: tree_sitter::Node) -> bool {
    node.parent()
        .and_then(|p| p.parent())
        .is_some_and(|g| g.kind() == "impl_item")
}

/// `enum X {...}` and `enum class X {...}` (a named enum with a body; a bare
/// `enum X v;` is a use, not a definition).
fn push_enum(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    if node.child_by_field_name("name").is_some() && node.child_by_field_name("body").is_some() {
        push_tagged(facts, "type", node, src);
    }
}

/// A tagged definition with a body (`struct X {...}`, `class X {...}`,
/// `enum X {...}`). In `typedef struct X {...} X;` the alias repeats the tag:
/// one fact covers the whole declaration, so the trailing alias name is also
/// an anchor. A differing alias is its own fact (`push_typedef`).
fn push_tagged(facts: &mut FileFacts, kind: &'static str, node: tree_sitter::Node, src: &[u8]) {
    let tag = node
        .child_by_field_name("name")
        .and_then(|n| node_text(n, src));
    let alias_node = node
        .parent()
        .filter(|p| p.kind() == "type_definition")
        .and_then(|p| p.child_by_field_name("declarator").map(|d| (p, d)))
        .filter(|(_, d)| node_text(*d, src) == tag);
    let Some((typedef, alias)) = alias_node else {
        push_def(facts, kind, node, src);
        return;
    };
    let (span, byte_start, byte_end) = span_of(typedef);
    declares(facts, typedef, node.child_by_field_name("name"));
    declares(facts, typedef, Some(alias));
    facts.definitions.push(Fact {
        kind,
        name: tag.map(|s| s.to_string()),
        text: None,
        span,
        byte_start,
        byte_end,
        callee: None,
        tcallee: None,
    });
}

/// `typedef <type> Name;` with a plain name. A struct or enum defined in the
/// same declaration is already recorded by `push_struct` / `push_enum` (an
/// anonymous struct under the typedef name), so it is not recorded twice.
/// shortcut: function-pointer and array typedefs are skipped rather than
/// guessed, and `typedef int A, B;` records only `A` (the first declarator);
/// upgrade at the first real project that needs them.
fn push_typedef(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let alias = node
        .child_by_field_name("declarator")
        .and_then(|d| node_text(d, src));
    // The tag's own fact already covers this declaration when the type has a
    // body and either is anonymous (a struct, named by the alias) or repeats
    // the alias as its tag.
    let defines_itself = node.child_by_field_name("type").is_some_and(|t| {
        let tag = t
            .child_by_field_name("name")
            .and_then(|n| node_text(n, src));
        t.child_by_field_name("body").is_some()
            && match t.kind() {
                "struct_specifier" => tag.is_none() || tag == alias,
                "class_specifier" | "enum_specifier" => tag.is_some() && tag == alias,
                _ => false,
            }
    });
    let Some(declarator) = node
        .child_by_field_name("declarator")
        .filter(|d| matches!(d.kind(), "identifier" | "type_identifier"))
    else {
        return;
    };
    if defines_itself {
        return;
    }
    let (span, byte_start, byte_end) = span_of(node);
    declares(facts, node, Some(declarator));
    facts.definitions.push(Fact {
        kind: "type",
        name: node_text(declarator, src).map(|s| s.to_string()),
        text: None,
        span,
        byte_start,
        byte_end,
        callee: None,
        tcallee: None,
    });
}

/// C++ data member: a `field_declaration` whose declarator ends in a
/// `field_identifier`. The fact points at the NAME (the identifier), so the
/// anchor of a member is its identifier, not its type.
///
/// A `function_declarator` is never a data member: a method declaration, a
/// constructor and the ambiguous function-pointer form
/// (`int (*target)();`) are all skipped rather than guessed.
fn push_field(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let mut cursor = node.walk();
    for declarator in node.children_by_field_name("declarator", &mut cursor) {
        let Some(name_node) = field_identifier_of(declarator) else {
            continue;
        };
        let (span, byte_start, byte_end) = span_of(name_node);
        declares(facts, name_node, Some(name_node));
        facts.definitions.push(Fact {
            kind: "field",
            name: node_text(name_node, src).map(str::to_string),
            text: None,
            span,
            byte_start,
            byte_end,
            callee: None,
            tcallee: None,
        });
    }
}

/// The `field_identifier` at the end of a data-member declarator chain.
/// Pointer, reference, array, parenthesized and attributed declarators wrap
/// it; a `function_declarator` never does, so it yields nothing.
fn field_identifier_of(declarator: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut current = declarator;
    for _ in 0..6 {
        match current.kind() {
            "field_identifier" => return Some(current),
            "function_declarator" => return None,
            "pointer_declarator"
            | "reference_declarator"
            | "array_declarator"
            | "parenthesized_declarator"
            | "attributed_declarator" => {
                current = inner_declarator(current)?;
            }
            _ => return None,
        }
    }
    None
}

/// The declarator inside a wrapper node: its `declarator` field when it has
/// one, else the first named child that can carry it (the reference and
/// parenthesized declarators have no field of their own).
fn inner_declarator(node: tree_sitter::Node) -> Option<tree_sitter::Node> {
    if let Some(inner) = node.child_by_field_name("declarator") {
        return Some(inner);
    }
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(i) else {
            continue;
        };
        if matches!(
            child.kind(),
            "field_identifier"
                | "function_declarator"
                | "pointer_declarator"
                | "reference_declarator"
                | "array_declarator"
                | "parenthesized_declarator"
                | "attributed_declarator"
        ) {
            return Some(child);
        }
    }
    None
}

fn push_occurrence(facts: &mut FileFacts, role: &'static str, name: tree_sitter::Node, src: &[u8]) {
    let (span, byte_start, byte_end) = span_of(name);
    facts.reference_occurrences.push(Fact {
        kind: role,
        name: node_text(name, src).map(str::to_string),
        text: None,
        span,
        byte_start,
        byte_end,
        callee: None,
        tcallee: None,
    });
}

/// C++ member access: a `field_expression` whose field is a plain
/// `field_identifier` (`a.b`, `p->b`, `p->*b`; `a.T::b` has a qualified field
/// and is not recorded). The row is the field identifier. Under `&` the role
/// is the address-of binding, which replaces the member access for the same
/// identifier, so one identifier is one row.
fn push_member_access(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(field) = node
        .child_by_field_name("field")
        .filter(|f| f.kind() == "field_identifier")
    else {
        return;
    };
    let role = if is_address_of_operand(node) {
        ROLE_ADDRESS_OF_BINDING
    } else {
        ROLE_MEMBER_ACCESS
    };
    push_occurrence(facts, role, field, src);
}

/// True when `node` is the direct operand of a unary `&`.
fn is_address_of_operand(node: tree_sitter::Node) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == "pointer_expression"
            && parent
                .child_by_field_name("operator")
                .is_some_and(|op| op.kind() == "&")
            && parent
                .child_by_field_name("argument")
                .is_some_and(|arg| arg.id() == node.id())
    })
}

/// A bare identifier argument of a call whose callee is a bare identifier
/// naming a function-like macro defined in this file. The macro's expansion
/// is unknown, so the row is a candidate; the callee itself is never a row.
fn push_macro_argument(
    facts: &mut FileFacts,
    node: tree_sitter::Node,
    src: &[u8],
    macros: &HashSet<&str>,
) {
    let Some(args) = node.parent().filter(|p| p.kind() == "argument_list") else {
        return;
    };
    let Some(call) = args.parent().filter(|p| p.kind() == "call_expression") else {
        return;
    };
    let is_call_arguments = call
        .child_by_field_name("arguments")
        .is_some_and(|a| a.id() == args.id());
    let calls_a_local_macro = call
        .child_by_field_name("function")
        .filter(|f| f.kind() == "identifier")
        .and_then(|f| node_text(f, src))
        .is_some_and(|name| macros.contains(name));
    if is_call_arguments && calls_a_local_macro {
        push_occurrence(facts, ROLE_MACRO_ARGUMENT, node, src);
    }
}

/// Names of the function-like macros (`#define NAME(args)`) defined in the
/// file, wherever the definition sits (any preprocessor branch, before or
/// after the use). Object-like macros are not callable and are not included.
fn function_macro_names<'a>(root: tree_sitter::Node<'a>, src: &'a [u8]) -> HashSet<&'a str> {
    let mut names = HashSet::new();
    let mut pending = vec![root];
    while let Some(node) = pending.pop() {
        if node.kind() == "preproc_function_def" {
            if let Some(name) = node
                .child_by_field_name("name")
                .and_then(|n| node_text(n, src))
            {
                names.insert(name);
            }
        }
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i) {
                pending.push(child);
            }
        }
    }
    names
}

fn push_call(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    push_call_to(facts, node, node.child_by_field_name("function"), src);
}

/// Java `recv.name(args)` / `name(args)`. The call name is the source text from
/// the receiver (if any) through the method name, as written; the callee span
/// is the method name. `new T(...)` is not a call candidate here.
fn push_java_call(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(name) = node.child_by_field_name("name") else {
        return;
    };
    let start = node
        .child_by_field_name("object")
        .map_or(name.start_byte(), |o| o.start_byte());
    let callee = src
        .get(start..name.end_byte())
        .and_then(|b| std::str::from_utf8(b).ok())
        .map(str::to_string);
    let (span, byte_start, byte_end) = span_of(node);
    facts.calls.push(Fact {
        kind: "call",
        name: callee,
        text: None,
        span,
        byte_start,
        byte_end,
        callee: Some(callee_span_of(name)),
        tcallee: Some(callee_span_of(name)),
    });
}

/// Go call candidates. An explicit type argument hides the callee two ways:
/// `F[int](1, 2)` is a call whose callee is an index expression (name
/// `F[int]`), and `F[int](1)` parses as a `type_conversion_expression` with no
/// call node at all. Record both under the function name, as for C++ template
/// arguments. shortcut: `fns[0](x)` has the same shape as `F[int](x)` and is
/// recorded as a call to `fns`; telling them apart needs type information.
/// A candidate only; a real conversion `Celsius[float64](x)` is
/// indistinguishable from a call without types too.
fn push_go_call(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    fn base(f: tree_sitter::Node) -> tree_sitter::Node {
        match f.kind() {
            "index_expression" => f
                .child_by_field_name("operand")
                .filter(|o| matches!(o.kind(), "identifier" | "selector_expression"))
                .unwrap_or(f),
            _ => f,
        }
    }
    let func = match node.kind() {
        "type_conversion_expression" => node
            .child_by_field_name("type")
            .filter(|t| t.kind() == "generic_type")
            .and_then(|t| t.child_by_field_name("type")),
        _ => node.child_by_field_name("function").map(base),
    };
    if func.is_some() {
        push_call_to(facts, node, func, src);
    }
}

fn push_call_to(
    facts: &mut FileFacts,
    node: tree_sitter::Node,
    func: Option<tree_sitter::Node>,
    src: &[u8],
) {
    let callee = func.and_then(|f| node_text(f, src).map(|s| s.to_string()));
    let (span, byte_start, byte_end) = span_of(node);
    facts.calls.push(Fact {
        kind: "call",
        name: callee,
        text: None,
        span,
        byte_start,
        byte_end,
        callee: func.map(callee_span_of),
        tcallee: func.and_then(|f| terminal_span(src, f)),
    });
}

/// Span and byte range of a node, as a call fact records its callee.
fn callee_span_of(node: tree_sitter::Node) -> CalleeSpan {
    let (span, byte_start, byte_end) = span_of(node);
    CalleeSpan {
        start_line: span.start_line,
        start_col: span.start_col,
        end_line: span.end_line,
        end_col: span.end_col,
        byte_start,
        byte_end,
    }
}

/// C++ direct initialization (`T var(args);`) has no `call_expression` node:
/// the grammar models it as `declaration` -> `init_declarator` ->
/// `argument_list`, with the called type in the declaration's type field.
/// Only that form is a constructor call: the argument list holds
/// expressions, so C++ reads the declaration as a variable definition.
///
/// The grammar also models `T var(a, b);` as a `function_declarator`
/// (C++'s most-vexing-parse shape). There the arguments are read as a
/// parameter-type-list, and C++ reads the declaration as a function
/// declaration when the argument names denote types and as a variable
/// definition when they denote variables. Only name lookup can decide, so
/// the syntax layer must not assert a constructor call for that form: a
/// block-scope function declaration is legal C++, so the scope does not
/// settle it either. Returns the called type node and the argument list.
fn direct_init_parts(node: tree_sitter::Node) -> Option<(tree_sitter::Node, tree_sitter::Node)> {
    let decl = node.child_by_field_name("declarator")?;
    // Direct initialization puts the argument list in the declarator's
    // `value` field; copy initialization (`T v = T(args);`) puts a
    // `call_expression` there, which is captured on its own.
    if decl.kind() != "init_declarator" {
        return None;
    }
    let value = decl.child_by_field_name("value")?;
    if value.kind() != "argument_list" {
        return None;
    }
    let ty = node.child_by_field_name("type")?;
    // Only a named type can be a constructor: `int x(5);` is not a call.
    // shortcut: a typedef is indistinguishable from a class here, so
    // `size_t n(0);` is recorded as a call to `size_t` although it may be
    // a functional cast; a name lookup would settle it (gap E12).
    if !matches!(ty.kind(), "type_identifier" | "qualified_identifier") {
        return None;
    }
    Some((ty, value))
}

/// Record a C++ direct-initialization constructor call as a call fact. The
/// call range runs from the called type to the end of the argument list
/// (`Widget w(1)`), the callee is the type (`Widget`), and the terminal is
/// the type's last name part.
fn push_direct_init_call(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some((ty, args)) = direct_init_parts(node) else {
        return;
    };
    let (ty_span, ty_start, _) = span_of(ty);
    let (args_span, _, args_end) = span_of(args);
    let span = Span {
        start_line: ty_span.start_line,
        start_col: ty_span.start_col,
        end_line: args_span.end_line,
        end_col: args_span.end_col,
    };
    facts.calls.push(Fact {
        kind: "call",
        name: node_text(ty, src).map(|s| s.to_string()),
        text: None,
        span,
        byte_start: ty_start,
        byte_end: args_end,
        callee: Some(callee_span_of(ty)),
        tcallee: terminal_span(src, ty),
    });
}

/// Name node of a callee whose last name carries a template argument list:
/// `make_shared` in `std::make_shared<T>`, `get` in `obj.get<T>()` and
/// `ptr->template get<T>()`, `vector` in `std::vector<T> v(n)`. `None` for
/// every other callee shape, including template arguments that only appear in
/// a scope (`Foo<T>::create`).
fn template_name(callee: tree_sitter::Node) -> Option<tree_sitter::Node> {
    let mut node = callee;
    loop {
        match node.kind() {
            "template_function" | "template_method" | "template_type" => {
                return node.child_by_field_name("name");
            }
            "qualified_identifier" => node = node.child_by_field_name("name")?,
            "field_expression" => node = node.child_by_field_name("field")?,
            "dependent_name" => node = node.named_child(0)?,
            _ => return None,
        }
    }
}

/// Terminal identifier/property of a callee expression. A callee whose last
/// name has template arguments takes its terminal from the grammar: the text
/// after a separator would start inside the arguments (`PatchMatchController>`
/// for `std::make_shared<mvs::PatchMatchController>`) and the arguments are
/// not part of the called name. Every other callee takes the text after the
/// last `.`, `::`, or `->` separator. Separators are ASCII, so byte
/// arithmetic on the UTF-8 source is exact. Returns `None` when the callee
/// ends in a separator or is not valid UTF-8; a plain `foo` is its own
/// terminal.
fn terminal_span(src: &[u8], callee: tree_sitter::Node) -> Option<CalleeSpan> {
    if let Some(name) = template_name(callee) {
        return Some(callee_span_of(name));
    }
    let (callee_bs, callee_be) = (callee.start_byte(), callee.end_byte());
    let text = std::str::from_utf8(src.get(callee_bs..callee_be)?).ok()?;
    let mut cut: Option<usize> = None;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.' {
            cut = Some(i + 1);
            i += 1;
        } else if bytes.get(i..i + 2) == Some(b"::".as_slice())
            || bytes.get(i..i + 2) == Some(b"->".as_slice())
        {
            cut = Some(i + 2);
            i += 2;
        } else {
            i += 1;
        }
    }
    let start = match cut {
        // Plain identifier call: the whole callee IS the terminal.
        None => callee_bs,
        Some(off) => callee_bs + off,
    };
    if start >= callee_be {
        return None;
    }
    let start_point = callee.start_position();
    let start_pos = (
        (start_point.row + 1) as u32,
        (start_point.column + 1) as u32,
    );
    let (start_line, start_col) = position_from(src, callee_bs, start_pos, start);
    let (end_line, end_col) = position_from(src, callee_bs, start_pos, callee_be);
    Some(CalleeSpan {
        start_line,
        start_col,
        end_line,
        end_col,
        byte_start: start,
        byte_end: callee_be,
    })
}

/// 1-based (line, byte-column) of byte offset `off`, computed from `base`, the
/// start of a node, and `base_pos`, that node's 1-based position as tree-sitter
/// reports it. Only the bytes from `base` to `off` are read. The result equals
/// counting the newlines before `off` in the whole file: the line is one plus
/// that count, and the column is one plus the bytes after the last newline.
fn position_from(src: &[u8], base: usize, base_pos: (u32, u32), off: usize) -> (u32, u32) {
    let off = off.min(src.len());
    debug_assert!(base <= off, "a position is taken from its node's own start");
    let rel = src.get(base..off).unwrap_or_default();
    match rel.iter().rposition(|b| *b == b'\n') {
        Some(last) => {
            let newlines = rel.iter().filter(|b| **b == b'\n').count();
            (base_pos.0 + newlines as u32, (rel.len() - last) as u32)
        }
        None => (base_pos.0, base_pos.1 + rel.len() as u32),
    }
}

/// Normalised lookup key of a supertype's text: one leading `*` and one
/// trailing type-argument list are stripped, and what remains must be
/// identifiers joined by `.` or `::`, none starting with a digit. A call or an
/// expression has no key. `<...>` is a type-argument list in every language;
/// `[...]` is one only when `brackets` says so (Python and Go). A `[...]` whose
/// content starts with a digit or a quote is an index and is never stripped.
/// The key is a lookup hint, never a resolution.
fn heritage_name(text: &str, brackets: bool) -> Option<String> {
    let text = text.strip_prefix('*').unwrap_or(text);
    let base = match text.chars().last() {
        Some(close @ ('>' | ']')) if close == '>' || brackets => {
            let open = if close == '>' { '<' } else { '[' };
            let mut depth = 0usize;
            let mut cut = None;
            // The scan starts on the closing bracket, so depth stays positive
            // until the matching opener brings it back to zero.
            for (i, c) in text.char_indices().rev() {
                if c == close {
                    depth += 1;
                } else if c == open {
                    depth -= 1;
                    if depth == 0 {
                        cut = Some(i);
                        break;
                    }
                }
            }
            let cut = cut?;
            // Both brackets are one byte, so the content lies between them.
            let content = &text[cut + 1..text.len() - 1];
            if close == ']'
                && content.trim_start().starts_with(|c: char| {
                    c.is_ascii_digit() || matches!(c, '"' | '\'' | '-' | '+' | ':')
                })
            {
                return None;
            }
            &text[..cut]
        }
        _ => text,
    };
    let identifiers = base.replace("::", ".").split('.').all(|seg| {
        !seg.is_empty()
            && !seg.starts_with(|c: char| c.is_ascii_digit())
            && seg
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
    });
    identifiers.then(|| base.to_string())
}

/// The first named child of `node` with the given kind.
fn named_child_of_kind<'a>(
    node: tree_sitter::Node<'a>,
    kind: &str,
) -> Option<tree_sitter::Node<'a>> {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .find(|c| c.kind() == kind)
}

/// The named children of a list node, comments left out. Rust and Java name
/// their comments `line_comment` and `block_comment`, the others `comment`.
fn list_items(node: tree_sitter::Node<'_>) -> Vec<tree_sitter::Node<'_>> {
    (0..node.named_child_count())
        .filter_map(|i| node.named_child(i))
        .filter(|c| !matches!(c.kind(), "comment" | "line_comment" | "block_comment"))
        .collect()
}

/// The subtype name and declaring byte range of the definition the current
/// arm just pushed. `None` when the declaration has no name: no heritage row.
fn last_definition(facts: &FileFacts) -> Option<(String, (usize, usize))> {
    let def = facts.definitions.last()?;
    Some((def.name.clone()?, (def.byte_start, def.byte_end)))
}

/// Records one heritage row for `owner` (subtype name and declaring range).
/// The supertype's bytes run from `first` to `last`: one node, except
/// TypeScript `Base<number>`, which spans the value and its type arguments.
fn push_heritage(
    facts: &mut FileFacts,
    owner: &(String, (usize, usize)),
    relation: &'static str,
    first: tree_sitter::Node,
    last: tree_sitter::Node,
    src: &[u8],
) {
    let (subtype, (decl_byte_start, decl_byte_end)) = owner;
    let Some(text) = src
        .get(first.start_byte()..last.end_byte())
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
    else {
        return;
    };
    let (start, end) = (first.start_position(), last.end_position());
    facts.heritage.push(Heritage {
        subtype: subtype.clone(),
        relation,
        supertype_text: text.to_string(),
        // A literal or keyword node (`null`, `None`, `this`) reads like a name
        // but is not one: the text is kept, the lookup key is NULL.
        supertype_name: if matches!(
            first.kind(),
            "null"
                | "undefined"
                | "true"
                | "false"
                | "none"
                | "this"
                | "super"
                | "number"
                | "integer"
                | "float"
                | "string"
        ) {
            None
        } else {
            heritage_name(
                text,
                matches!(facts.language, Language::Python | Language::Go),
            )
        },
        span: Span {
            start_line: (start.row + 1) as u32,
            start_col: (start.column + 1) as u32,
            end_line: (end.row + 1) as u32,
            end_col: (end.column + 1) as u32,
        },
        byte_start: first.start_byte(),
        byte_end: last.end_byte(),
        decl_byte_start: *decl_byte_start,
        decl_byte_end: *decl_byte_end,
    });
}

/// Python `class C(B, mod.D, metaclass=M)`: each positional base is `extends`.
fn push_python_heritage(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    let Some(bases) = node.child_by_field_name("superclasses") else {
        return;
    };
    // shortcut: splat bases (`*bases`, `**kw`) are skipped, not expanded; upgrade when a query needs them.
    for base in list_items(bases) {
        if matches!(
            base.kind(),
            "keyword_argument" | "list_splat" | "dictionary_splat"
        ) {
            continue;
        }
        push_heritage(facts, &owner, "extends", base, base, src);
    }
}

/// JavaScript `class C extends E {}`: the expression after `extends`.
fn push_js_heritage(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    // shortcut: class expressions (`const C = class extends B {}`) are not recorded; upgrade when a query needs them.
    let Some(base) =
        named_child_of_kind(node, "class_heritage").and_then(|h| list_items(h).into_iter().next())
    else {
        return;
    };
    push_heritage(facts, &owner, "extends", base, base, src);
}

/// TypeScript `class C<T> extends B<T> implements I, J {}` (and abstract).
fn push_ts_class_heritage(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    let Some(heritage) = named_child_of_kind(node, "class_heritage") else {
        return;
    };
    if let Some(clause) = named_child_of_kind(heritage, "extends_clause") {
        if let Some(value) = clause.child_by_field_name("value") {
            let last = clause
                .child_by_field_name("type_arguments")
                .unwrap_or(value);
            push_heritage(facts, &owner, "extends", value, last, src);
        }
    }
    if let Some(clause) = named_child_of_kind(heritage, "implements_clause") {
        for ty in list_items(clause) {
            push_heritage(facts, &owner, "implements", ty, ty, src);
        }
    }
}

/// TypeScript `interface I extends J, K {}`.
fn push_ts_interface_heritage(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    let Some(clause) = named_child_of_kind(node, "extends_type_clause") else {
        return;
    };
    let mut cursor = clause.walk();
    for ty in clause.children_by_field_name("type", &mut cursor) {
        push_heritage(facts, &owner, "extends", ty, ty, src);
    }
}

/// Java `superclass` (extends) and `interfaces` (implements) of a class,
/// record or enum. Each is absent when the declaration has none.
fn push_java_supertypes(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    // The `superclass` node wraps the type written after `extends`; a comment
    // before it is no type.
    if let Some(ty) = node
        .child_by_field_name("superclass")
        .and_then(|s| list_items(s).into_iter().next())
    {
        push_heritage(facts, &owner, "extends", ty, ty, src);
    }
    if let Some(list) = node
        .child_by_field_name("interfaces")
        .and_then(|s| named_child_of_kind(s, "type_list"))
    {
        for ty in list_items(list) {
            push_heritage(facts, &owner, "implements", ty, ty, src);
        }
    }
}

/// Java `interface I extends J, K {}`: the `type_list` of `extends_interfaces`.
fn push_java_interface_heritage(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    let Some(list) = named_child_of_kind(node, "extends_interfaces")
        .and_then(|e| named_child_of_kind(e, "type_list"))
    else {
        return;
    };
    for ty in list_items(list) {
        push_heritage(facts, &owner, "extends", ty, ty, src);
    }
}

/// Rust `impl Trait for Type`: the trait is `implements`, the subtype is the
/// impl's type as written, and the declaring range is the impl item. An
/// inherent impl has no `trait` field and gives no row, and neither does a
/// negative impl (`impl !Send for T {}`), whose `!` precedes the trait.
/// shortcut: Rust supertraits (`trait A: B`) and generic bounds are not recorded; upgrade when a query needs them.
fn push_rust_impl_heritage(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let (Some(trait_node), Some(ty)) = (
        node.child_by_field_name("trait"),
        node.child_by_field_name("type"),
    ) else {
        return;
    };
    let mut cursor = node.walk();
    if node
        .children(&mut cursor)
        .take_while(|c| c.id() != trait_node.id())
        .any(|c| c.kind() == "!")
    {
        return;
    }
    let Some(subtype) = node_text(ty, src) else {
        return;
    };
    let owner = (subtype.to_string(), (node.start_byte(), node.end_byte()));
    push_heritage(facts, &owner, "implements", trait_node, trait_node, src);
}

/// C++ `class C : public B, private ns::D<int> {}`: every named child of the
/// base clause except access specifiers and attributes (`[[deprecated]]`). The
/// caller has checked that the specifier is a named definition with a body.
/// A pack expansion `Bs...` keeps its `...` in the text, span and bytes: a
/// pack stands for any number of bases, so it has no single lookup name.
fn push_cpp_bases(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    let Some(clause) = named_child_of_kind(node, "base_class_clause") else {
        return;
    };
    for base in list_items(clause) {
        if matches!(base.kind(), "access_specifier" | "attribute_declaration") {
            continue;
        }
        // A comment between the base and its `...` is an extra: skip it.
        let mut next = base.next_sibling();
        while next.is_some_and(|n| n.is_extra()) {
            next = next.and_then(|n| n.next_sibling());
        }
        let dots = next.filter(|t| !t.is_named() && t.kind() == "...");
        push_heritage(facts, &owner, "extends", base, dots.unwrap_or(base), src);
    }
}

/// Go `type S struct { B; *P; pkg.Q }`: each field without a name is an
/// embedded struct field (`embeds`). The caller has checked the spec's kind.
/// shortcut: Go interface embedding (`interface { B }`) is not recorded; upgrade when a query needs interface supertypes.
fn push_go_embeds(facts: &mut FileFacts, node: tree_sitter::Node, src: &[u8]) {
    let Some(owner) = last_definition(facts) else {
        return;
    };
    let Some(fields) = node
        .child_by_field_name("type")
        .filter(|t| t.kind() == "struct_type")
        .and_then(|s| named_child_of_kind(s, "field_declaration_list"))
    else {
        return;
    };
    for field in list_items(fields) {
        if field.kind() != "field_declaration" || field.child_by_field_name("name").is_some() {
            continue;
        }
        // The `*` of an embedded pointer is a token of the field, not of its
        // type, so the text starts at the field and ends at the type (a tag
        // comes after the type and is left out).
        if let Some(ty) = field.child_by_field_name("type") {
            push_heritage(facts, &owner, "embeds", field, ty, src);
        }
    }
}

/// A definition or container as containment rows identify it: its kind, its
/// name as written, and its byte range (PR 6).
#[derive(Clone)]
struct Identity {
    kind: &'static str,
    name: Option<String>,
    byte_start: usize,
    byte_end: usize,
}

/// A container open on the walk (PR 6). It is the nearest container of every
/// node that starts before `node_end`, the end byte of its own node.
struct OpenContainer {
    node_end: usize,
    identity: Identity,
}

/// A container with no stored definition (PR 6): C++ namespace, TypeScript
/// `namespace` and `module`, Rust `impl` and `mod`. The kinds are `namespace`,
/// `module` and `impl`; the name is the text of the node's name (or, for an
/// impl, type) field, `None` when absent.
fn symbol_less_container(lang: Language, node: tree_sitter::Node, src: &[u8]) -> Option<Identity> {
    let (kind, field) = match (lang, node.kind()) {
        (Language::Cpp, "namespace_definition") => ("namespace", "name"),
        (Language::TypeScript | Language::Tsx, "internal_module") => ("namespace", "name"),
        (Language::TypeScript | Language::Tsx, "module") => ("module", "name"),
        (Language::Rust, "impl_item") => ("impl", "type"),
        (Language::Rust, "mod_item") => ("module", "name"),
        _ => return None,
    };
    let name = node
        .child_by_field_name(field)
        .and_then(|n| node_text(n, src))
        .map(str::to_string);
    Some(Identity {
        kind,
        name,
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
    })
}

/// Node kinds that are containers when they push a definition in the same
/// visit (PR 6). Go has none, and C only through a macro-headed struct.
fn is_symbol_container(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Python => kind == "class_definition",
        Language::JavaScript => kind == "class_declaration",
        Language::TypeScript | Language::Tsx => matches!(
            kind,
            "class_declaration"
                | "abstract_class_declaration"
                | "interface_declaration"
                | "enum_declaration"
        ),
        Language::Rust => kind == "trait_item",
        Language::Cpp => matches!(
            kind,
            "class_specifier" | "struct_specifier" | "enum_specifier"
        ),
        Language::Java => matches!(
            kind,
            "class_declaration"
                | "record_declaration"
                | "interface_declaration"
                | "annotation_type_declaration"
                | "enum_declaration"
        ),
        Language::C | Language::Go => false,
    }
}

/// A `function_definition` that `macro_headed_tag` stored as a class or struct
/// is a container of its member functions (PR 6).
fn is_macro_headed_tag(lang: Language, node: tree_sitter::Node, children: &[Identity]) -> bool {
    matches!(lang, Language::C | Language::Cpp)
        && node.kind() == "function_definition"
        && children
            .first()
            .is_some_and(|c| matches!(c.kind, "class" | "struct"))
}

/// Records the containment rows of `node` after `visit_node` ran on it. Its
/// children are the definitions it pushed (`defs_before` marks where they
/// start) and, for a symbol-less container, the node itself. Each child is
/// recorded against the innermost open container before `node` opens its own
/// one, so a container is no child of itself.
/// shortcut: a definition nested in a function takes the nearest container, not the function (upgrade when a reader needs function-level nesting).
fn record_containment(
    lang: Language,
    node: tree_sitter::Node,
    src: &[u8],
    facts: &mut FileFacts,
    defs_before: usize,
    containers: &mut Vec<OpenContainer>,
) {
    let mut children: Vec<Identity> = facts.definitions[defs_before..]
        .iter()
        .map(|d| Identity {
            kind: d.kind,
            name: d.name.clone(),
            byte_start: d.byte_start,
            byte_end: d.byte_end,
        })
        .collect();
    // A container node pushes exactly one definition, its own identity.
    let own = match symbol_less_container(lang, node, src) {
        Some(container) => {
            children.push(container.clone());
            Some(container)
        }
        None if is_symbol_container(lang, node.kind())
            || is_macro_headed_tag(lang, node, &children) =>
        {
            children.first().cloned()
        }
        None => None,
    };
    if let Some(parent) = containers.last() {
        for child in children {
            facts.containment.push(Containment {
                child_kind: child.kind,
                child_name: child.name,
                child_byte_start: child.byte_start,
                child_byte_end: child.byte_end,
                parent_kind: parent.identity.kind,
                parent_name: parent.identity.name.clone(),
                parent_byte_start: parent.identity.byte_start,
                parent_byte_end: parent.identity.byte_end,
            });
        }
    }
    if let Some(identity) = own {
        containers.push(OpenContainer {
            node_end: node.end_byte(),
            identity,
        });
    }
}

/// Whether `kind` is a comment node of the language (PR 6).
fn is_comment(lang: Language, kind: &str) -> bool {
    match lang {
        Language::Rust | Language::Java => matches!(kind, "line_comment" | "block_comment"),
        _ => kind == "comment",
    }
}

/// Whether `parent` wraps the definition node `node` for doc comments (PR 6).
/// Go wraps a spec only when the `type` keyword is on the spec's own line.
fn is_wrapper(lang: Language, node: tree_sitter::Node, parent: tree_sitter::Node) -> bool {
    match lang {
        Language::Python => parent.kind() == "decorated_definition",
        Language::JavaScript | Language::TypeScript | Language::Tsx => matches!(
            parent.kind(),
            "export_statement" | "lexical_declaration" | "variable_declaration"
        ),
        Language::C | Language::Cpp => matches!(
            parent.kind(),
            "template_declaration" | "type_definition" | "declaration" | "field_declaration"
        ),
        Language::Go => {
            parent.kind() == "type_declaration"
                && parent.start_position().row == node.start_position().row
        }
        Language::Rust | Language::Java => false,
    }
}

/// The node a doc comment sits above (PR 6): the outermost wrapper of `node`,
/// or for Rust the first of the attributes directly before the item.
/// shortcut: TypeScript `declare` (ambient) and C++ `extern "C"` wrappers are not passed through, so their definitions get no doc comment (upgrade when a reader needs them).
fn doc_anchor(lang: Language, node: tree_sitter::Node) -> tree_sitter::Node {
    if lang == Language::Rust {
        let mut anchor = node;
        while let Some(prev) = anchor
            .prev_sibling()
            .filter(|p| p.kind() == "attribute_item")
        {
            anchor = prev;
        }
        return anchor;
    }
    let mut current = node;
    while let Some(parent) = current.parent() {
        if !is_wrapper(lang, node, parent) {
            break;
        }
        current = parent;
    }
    current
}

/// The end byte of a comment without its trailing `\r` and `\n` bytes: a
/// comment node may include its line terminator (PR 6).
fn comment_end(src: &[u8], node: tree_sitter::Node) -> usize {
    let mut end = node.end_byte();
    while end > node.start_byte() && matches!(src.get(end - 1), Some(b'\r' | b'\n')) {
        end -= 1;
    }
    end
}

/// The 1-based line of a comment's last character (PR 6).
fn comment_end_line(src: &[u8], node: tree_sitter::Node) -> u32 {
    let start = node.start_position();
    let base = ((start.row + 1) as u32, (start.column + 1) as u32);
    position_from(src, node.start_byte(), base, comment_end(src, node)).0
}

/// Whether only spaces and tabs precede byte `start` on its line (PR 6).
fn starts_its_line(src: &[u8], start: usize) -> bool {
    let mut i = start;
    while i > 0 && matches!(src[i - 1], b' ' | b'\t') {
        i -= 1;
    }
    i == 0 || src[i - 1] == b'\n'
}

/// The comment block above `node`'s anchor (PR 6), as its first and last comment
/// node. The block runs upward through comments that each start their line and
/// are not separated by a blank line; it attaches only when its last comment
/// ends on the line directly above the anchor.
fn comment_block<'a>(
    lang: Language,
    node: tree_sitter::Node<'a>,
    src: &[u8],
) -> Option<(tree_sitter::Node<'a>, tree_sitter::Node<'a>)> {
    let anchor = doc_anchor(lang, node);
    let anchor_line = anchor.start_position().row as u32 + 1;
    let mut next_line = anchor_line;
    let mut nearest = None;
    let mut farthest = None;
    let mut cursor = anchor.prev_sibling();
    while let Some(comment) = cursor {
        if !is_comment(lang, comment.kind()) || !starts_its_line(src, comment.start_byte()) {
            break;
        }
        if comment_end_line(src, comment) + 1 < next_line {
            break;
        }
        if nearest.is_none() {
            nearest = Some(comment);
        }
        farthest = Some(comment);
        next_line = comment.start_position().row as u32 + 1;
        cursor = comment.prev_sibling();
    }
    let (nearest, farthest) = (nearest?, farthest?);
    (comment_end_line(src, nearest) + 1 == anchor_line).then_some((farthest, nearest))
}

/// Cuts doc text to `DOC_TEXT_CAP` bytes on a character boundary (PR 6).
fn cut_doc(text: &str) -> (&str, bool) {
    if text.len() <= DOC_TEXT_CAP {
        return (text, false);
    }
    let mut end = DOC_TEXT_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// A doc text as it sits in the source (PR 6), before a definition is attached.
struct DocText {
    text: String,
    truncated: bool,
    span: Span,
    byte_start: usize,
    byte_end: usize,
}

/// The doc text under `byte_start..byte_end`. `None` when those bytes are not
/// UTF-8, the same rule `node_text` applies.
fn doc_text(src: &[u8], span: Span, byte_start: usize, byte_end: usize) -> Option<DocText> {
    let full = std::str::from_utf8(&src[byte_start..byte_end]).ok()?;
    let (kept, truncated) = cut_doc(full);
    Some(DocText {
        text: kept.to_string(),
        truncated,
        span,
        byte_start,
        byte_end,
    })
}

/// The doc text of a comment block: from its first comment's start to its last
/// comment's end, the comments between them included verbatim (PR 6).
fn comment_block_text(
    src: &[u8],
    first: tree_sitter::Node,
    last: tree_sitter::Node,
) -> Option<DocText> {
    let (mut span, byte_start, _) = span_of(first);
    let byte_end = comment_end(src, last);
    let (end_line, end_col) =
        position_from(src, byte_start, (span.start_line, span.start_col), byte_end);
    span.end_line = end_line;
    span.end_col = end_col;
    doc_text(src, span, byte_start, byte_end)
}

/// The docstring of a Python function or class: the string that is the first
/// non-comment named child of the body, when that child is an expression
/// statement holding a plain string (PR 6). An f-string or bytes literal is
/// not a docstring.
/// shortcut: a docstring built from concatenated strings is not recorded (upgrade when a reader needs multi-part docstrings).
fn python_docstring<'t>(
    lang: Language,
    node: tree_sitter::Node<'t>,
    src: &[u8],
) -> Option<tree_sitter::Node<'t>> {
    if lang != Language::Python
        || !matches!(node.kind(), "function_definition" | "class_definition")
    {
        return None;
    }
    let body = node.child_by_field_name("body")?;
    let statement = (0..body.named_child_count())
        .filter_map(|i| body.named_child(i))
        .find(|child| child.kind() != "comment")?;
    if statement.kind() != "expression_statement" {
        return None;
    }
    let string = statement.named_child(0)?;
    if string.kind() != "string" {
        return None;
    }
    let prefix_has_f_or_b = src[string.start_byte()..]
        .iter()
        .take_while(|b| b.is_ascii_alphabetic())
        .any(|&b| matches!(b, b'f' | b'F' | b'b' | b'B'));
    (!prefix_has_f_or_b).then_some(string)
}

/// Records the doc comment rows of the definitions `node` pushed in this visit
/// (PR 6): the block above its anchor (form `above`) and a Python docstring
/// (form `docstring`). Each definition gets at most one row of each form.
fn record_doc_comments(
    lang: Language,
    node: tree_sitter::Node,
    src: &[u8],
    facts: &mut FileFacts,
    defs_before: usize,
) {
    if facts.definitions.len() == defs_before {
        return;
    }
    let above = comment_block(lang, node, src)
        .and_then(|(first, last)| comment_block_text(src, first, last));
    let docstring = python_docstring(lang, node, src).and_then(|string| {
        let (span, byte_start, byte_end) = span_of(string);
        doc_text(src, span, byte_start, byte_end)
    });
    let mut rows = Vec::new();
    for definition in &facts.definitions[defs_before..] {
        for (form, found) in [("above", &above), ("docstring", &docstring)] {
            let Some(found) = found else {
                continue;
            };
            rows.push(DocComment {
                def_byte_start: definition.byte_start,
                def_byte_end: definition.byte_end,
                form,
                text: found.text.clone(),
                truncated: found.truncated,
                span: found.span.clone(),
                byte_start: found.byte_start,
                byte_end: found.byte_end,
            });
        }
    }
    facts.doc_comments.extend(rows);
}

/// Visit every node under `root` in pre-order. Iterative on purpose: nesting
/// depth is bounded only by the file size cap, and a recursive walk overflows
/// the stack on deeply nested source.
fn visit(
    lang: Language,
    root: tree_sitter::Node,
    src: &[u8],
    facts: &mut FileFacts,
    macros: &HashSet<&str>,
) {
    let mut cursor = root.walk();
    // The containers open at the current node, innermost last (PR 6).
    let mut containers: Vec<OpenContainer> = Vec::new();
    loop {
        let node = cursor.node();
        while containers
            .last()
            .is_some_and(|c| c.node_end <= node.start_byte())
        {
            containers.pop();
        }
        let defs_before = facts.definitions.len();
        visit_node(lang, node, src, facts, macros);
        record_doc_comments(lang, node, src, facts, defs_before);
        record_containment(lang, node, src, facts, defs_before, &mut containers);
        if cursor.goto_first_child() {
            continue;
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

fn visit_node(
    lang: Language,
    node: tree_sitter::Node,
    src: &[u8],
    facts: &mut FileFacts,
    macros: &HashSet<&str>,
) {
    // Missing tokens (e.g. a `)` the parser inserts) are anonymous: count
    // every child, named or not, or recovery evidence goes missing.
    if node.is_missing() || node.kind() == "ERROR" {
        facts.errors += 1;
    }
    match lang {
        Language::Python => match node.kind() {
            "function_definition" => push_def(facts, "function", node, src),
            "class_definition" => {
                push_def(facts, "class", node, src);
                push_python_heritage(facts, node, src);
            }
            "import_statement" | "import_from_statement" => push_import(facts, node, src),
            "call" => push_call(facts, node, src),
            _ => {}
        },
        Language::JavaScript => match node.kind() {
            "function_declaration" | "generator_function_declaration" => {
                push_def(facts, "function", node, src)
            }
            "method_definition" => push_def(facts, "method", node, src),
            "variable_declarator" | "field_definition" if is_named_function_binding(node) => {
                push_def(facts, binding_kind(node), node, src)
            }
            "class_declaration" => {
                push_def(facts, "class", node, src);
                push_js_heritage(facts, node, src);
            }
            "import_statement" => push_import(facts, node, src),
            "call_expression" => push_call(facts, node, src),
            _ => {}
        },
        Language::TypeScript | Language::Tsx => match node.kind() {
            "function_declaration" | "generator_function_declaration" => {
                push_def(facts, "function", node, src)
            }
            "method_definition" => push_def(facts, "method", node, src),
            "variable_declarator" | "public_field_definition"
                if is_named_function_binding(node) =>
            {
                push_def(facts, binding_kind(node), node, src)
            }
            "class_declaration" | "abstract_class_declaration" => {
                push_def(facts, "class", node, src);
                push_ts_class_heritage(facts, node, src);
            }
            "interface_declaration" => {
                push_def(facts, "interface", node, src);
                push_ts_interface_heritage(facts, node, src);
            }
            "type_alias_declaration" | "enum_declaration" => push_def(facts, "type", node, src),
            "import_statement" => push_import(facts, node, src),
            "call_expression" => push_call(facts, node, src),
            _ => {}
        },
        Language::Rust => match node.kind() {
            "function_item" | "function_signature_item" => push_def(facts, "function", node, src),
            "struct_item" => push_def(facts, "struct", node, src),
            "enum_item" => push_def(facts, "type", node, src),
            // An associated type in an impl block is not a type alias.
            "type_item" if !in_impl(node) => push_def(facts, "type", node, src),
            "trait_item" => push_def(facts, "interface", node, src),
            "impl_item" => push_rust_impl_heritage(facts, node, src),
            "use_declaration" => push_import(facts, node, src),
            "call_expression" => push_call(facts, node, src),
            _ => {}
        },
        Language::Go => match node.kind() {
            "function_declaration" => push_def(facts, "function", node, src),
            "method_declaration" => push_def(facts, "method", node, src),
            "type_spec" => {
                push_go_type(facts, node, src);
                push_go_embeds(facts, node, src);
            }
            "type_alias" => push_go_type(facts, node, src),
            "import_declaration" => push_import(facts, node, src),
            "call_expression" | "type_conversion_expression" => push_go_call(facts, node, src),
            _ => {}
        },
        Language::Java => match node.kind() {
            "class_declaration" | "record_declaration" => {
                push_def(facts, "class", node, src);
                push_java_supertypes(facts, node, src);
            }
            "interface_declaration" => {
                push_def(facts, "interface", node, src);
                push_java_interface_heritage(facts, node, src);
            }
            "annotation_type_declaration" => push_def(facts, "interface", node, src),
            "enum_declaration" => {
                push_def(facts, "type", node, src);
                push_java_supertypes(facts, node, src);
            }
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration" => push_def(facts, "method", node, src),
            "import_declaration" => push_import(facts, node, src),
            "method_invocation" => push_java_call(facts, node, src),
            _ => {}
        },
        Language::C => match node.kind() {
            "function_definition" => push_c_def(
                facts,
                macro_headed_tag(node).unwrap_or("function"),
                node,
                src,
            ),
            "struct_specifier" => push_struct(facts, node, src),
            "enum_specifier" => push_enum(facts, node, src),
            "type_definition" => push_typedef(facts, node, src),
            "preproc_include" => push_import(facts, node, src),
            "call_expression" => push_call(facts, node, src),
            _ => {}
        },
        Language::Cpp => match node.kind() {
            "function_definition" => push_c_def(
                facts,
                macro_headed_tag(node).unwrap_or("function"),
                node,
                src,
            ),
            "struct_specifier" => {
                push_struct(facts, node, src);
                if node.child_by_field_name("name").is_some()
                    && node.child_by_field_name("body").is_some()
                {
                    push_cpp_bases(facts, node, src);
                }
            }
            "class_specifier"
                if node.child_by_field_name("body").is_some()
                    && node.child_by_field_name("name").is_some() =>
            {
                push_tagged(facts, "class", node, src);
                push_cpp_bases(facts, node, src);
            }
            "enum_specifier" => push_enum(facts, node, src),
            "type_definition" => push_typedef(facts, node, src),
            "alias_declaration" => push_def(facts, "type", node, src),
            "field_declaration" => push_field(facts, node, src),
            "preproc_include" => push_import(facts, node, src),
            "call_expression" => push_call(facts, node, src),
            "declaration" => push_direct_init_call(facts, node, src),
            "field_expression" => push_member_access(facts, node, src),
            "identifier" => push_macro_argument(facts, node, src, macros),
            _ => {}
        },
    }
}

/// Parse UTF-8 source. Tree-sitter always produces a tree (error recovery);
/// syntax problems surface as `errors`, never as a failure.
pub fn parse_source(lang: Language, src: &[u8]) -> FileFacts {
    let mut facts = FileFacts {
        language: lang,
        bytes: src.len(),
        definitions: Vec::new(),
        identifiers: Vec::new(),
        imports: Vec::new(),
        calls: Vec::new(),
        reference_occurrences: Vec::new(),
        heritage: Vec::new(),
        containment: Vec::new(),
        doc_comments: Vec::new(),
        errors: 0,
    };
    let mut parser = tree_sitter::Parser::new();
    let language = ts_language(lang);
    if parser.set_language(&language).is_err() {
        facts.errors += 1;
        return facts;
    }
    // shortcut: no parse time limit or budget; a time limit would make the
    // index depend on the machine. Add one at the first parse that hangs
    // (`parse_with_options` with a progress callback).
    if let Some(tree) = parser.parse(src, None) {
        let macros = if lang == Language::Cpp {
            function_macro_names(tree.root_node(), src)
        } else {
            HashSet::new()
        };
        visit(lang, tree.root_node(), src, &mut facts, &macros);
    } else {
        facts.errors += 1;
    }
    facts
}

fn span_json(span: &Span) -> String {
    format!(
        "{{\"start_line\":{},\"start_col\":{},\"end_line\":{},\"end_col\":{}}}",
        span.start_line, span.start_col, span.end_line, span.end_col
    )
}

fn opt_json(value: &Option<String>) -> String {
    match value {
        Some(s) => format!("\"{}\"", escape(s)),
        None => "null".to_string(),
    }
}

fn fact_json(f: &Fact) -> String {
    format!(
        "{{\"kind\":\"{}\",\"name\":{},\"text\":{},\"span\":{},\"bytes\":[{},{}]}}",
        f.kind,
        opt_json(&f.name),
        opt_json(&f.text),
        span_json(&f.span),
        f.byte_start,
        f.byte_end
    )
}

fn facts_json(facts: &[Fact]) -> String {
    facts.iter().map(fact_json).collect::<Vec<_>>().join(",")
}

/// File entry with relative slash-separated path. Fixed key order.
pub fn file_facts_json(rel: &str, facts: &FileFacts) -> String {
    format!(
        "{{\"path\":\"{}\",\"status\":\"ok\",\"language\":\"{}\",\"bytes\":{},\"definitions\":[{}],\"imports\":[{}],\"calls\":[{}],\"errors\":{}}}",
        escape(rel),
        language_name(facts.language),
        facts.bytes,
        facts_json(&facts.definitions),
        facts_json(&facts.imports),
        facts_json(&facts.calls),
        facts.errors
    )
}

#[cfg(test)]
mod tests {
    use super::{
        classify, grammar_version, language_name, parse_source, position_from, FileKind, Language,
    };
    use std::path::Path;

    #[test]
    fn maps_extensions_to_languages() {
        assert_eq!(
            classify(Path::new("a.py")),
            FileKind::Supported(Language::Python)
        );
        assert_eq!(
            classify(Path::new("a.js")),
            FileKind::Supported(Language::JavaScript)
        );
        assert_eq!(
            classify(Path::new("a.ts")),
            FileKind::Supported(Language::TypeScript)
        );
        assert_eq!(
            classify(Path::new("a.tsx")),
            FileKind::Supported(Language::Tsx)
        );
        assert_eq!(
            classify(Path::new("a.rs")),
            FileKind::Supported(Language::Rust)
        );
        assert_eq!(classify(Path::new("a.c")), FileKind::Supported(Language::C));
        assert_eq!(
            classify(Path::new("a.h")),
            FileKind::Supported(Language::Cpp)
        );
        assert_eq!(
            classify(Path::new("a.cpp")),
            FileKind::Supported(Language::Cpp)
        );
        assert_eq!(
            classify(Path::new("widget.inl")),
            FileKind::Supported(Language::Cpp)
        );
        assert_eq!(
            classify(Path::new("A.PY")),
            FileKind::Supported(Language::Python)
        );
    }

    #[test]
    fn refuses_cuda_and_unknown_extensions() {
        assert_eq!(classify(Path::new("k.cu")), FileKind::CudaExperimental);
        assert_eq!(classify(Path::new("k.cuh")), FileKind::CudaExperimental);
        assert_eq!(
            classify(Path::new("note.xyz")),
            FileKind::UnsupportedExtension
        );
        // Other C++ template-body extensions stay out of scope (see README).
        assert_eq!(
            classify(Path::new("widget.ipp")),
            FileKind::UnsupportedExtension
        );
        assert_eq!(
            classify(Path::new("widget.tpp")),
            FileKind::UnsupportedExtension
        );
        assert_eq!(
            classify(Path::new("Makefile")),
            FileKind::UnsupportedExtension
        );
    }

    #[test]
    fn callee_and_terminal_spans_are_precise() {
        // Whole-call parity: the call span still covers the full expression.
        let src = b"def f():\n    return foo(a, b)\n";
        let facts = parse_source(Language::Python, src);
        assert_eq!(facts.calls.len(), 1);
        let call = &facts.calls[0];
        assert_eq!(&src[call.byte_start..call.byte_end], b"foo(a, b)");
        let callee = call.callee.as_ref().expect("callee span recorded");
        assert_eq!(&src[callee.byte_start..callee.byte_end], b"foo");
        let terminal = call.tcallee.as_ref().expect("terminal span recorded");
        assert_eq!(&src[terminal.byte_start..terminal.byte_end], b"foo");
        // Member call: callee covers the dotted path, terminal only the
        // property — the object-base reference must never prove a call.
        let src = b"def f():\n    return w.process(x)\n";
        let facts = parse_source(Language::Python, src);
        let call = &facts.calls[0];
        let callee = call.callee.as_ref().expect("callee span recorded");
        assert_eq!(&src[callee.byte_start..callee.byte_end], b"w.process");
        let terminal = call.tcallee.as_ref().expect("terminal span recorded");
        assert_eq!(&src[terminal.byte_start..terminal.byte_end], b"process");
        assert!(terminal.byte_start > callee.byte_start);
    }

    #[test]
    fn js_and_ts_generator_functions_are_function_definitions() {
        let src = b"function* gen() { yield 1; }\nexport function* exported() {}\n";
        for lang in [Language::JavaScript, Language::TypeScript, Language::Tsx] {
            let facts = parse_source(lang, src);
            let names: Vec<_> = facts
                .definitions
                .iter()
                .map(|d| (d.kind, d.name.as_deref()))
                .collect();
            assert_eq!(
                names,
                vec![("function", Some("gen")), ("function", Some("exported"))],
                "{lang:?}"
            );
        }
    }

    /// Every definition a language records names the identifier it declares:
    /// an occurrence proves the definition only there.
    #[test]
    fn every_definition_reports_its_declaration_identifier() {
        let sources: &[(Language, &str)] = &[
            (Language::Python, "class A(object):\n    def m(self):\n        pass\n\n@dec\ndef f():\n    pass\n"),
            (Language::JavaScript, "function f() {}\nfunction* g() {}\nclass A { m() {} x = () => {} }\nconst h = () => {};\n"),
            (Language::TypeScript, "interface I {}\ntype T = number;\nenum E { A }\nabstract class A { private m(): void {} y = function () {} }\nexport const h = () => {};\n"),
            (Language::Tsx, "function View() { return <div/>; }\n"),
            (Language::Rust, "pub struct S;\nenum E { A }\ntrait T { fn s(&self); }\ntype U = u32;\nimpl S { fn r#match(&self) {} }\n"),
            (Language::Go, "package p\ntype S struct{}\ntype I interface{}\ntype A = int\nfunc F() {}\nfunc (s S) M() {}\n"),
            (Language::Java, "class A { A() {} void m() {} }\ninterface I {}\nenum E { X }\nrecord R(int a) { R {} }\n@interface N {}\n"),
            (Language::C, "struct S { int a; };\ntypedef struct { int b; } T;\ntypedef struct U { int c; } U;\nenum E { X };\ntypedef int N;\nint *f(void) { return 0; }\n"),
            (Language::Cpp, "class C { int member; void m() {} };\nvoid C::n() {}\nenum class E { X };\nusing A = int;\nnamespace ns { struct S {}; }\nint *ptr_fn(int *x) { return x; }\n"),
        ];
        let mut sources: Vec<(Language, String)> = sources
            .iter()
            .map(|(language, source)| (*language, source.to_string()))
            .collect();
        // The multilingual corpus as well, whatever it holds.
        let inline = sources.len();
        let mut pending =
            vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/m6")];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            } else if let FileKind::Supported(language) = classify(&path) {
                sources.push((language, std::fs::read_to_string(&path).unwrap()));
            }
        }
        assert!(
            sources.len() - inline > 20,
            "corpus files: {}",
            sources.len() - inline
        );
        for (index, (language, source)) in sources.iter().enumerate() {
            let facts = parse_source(*language, source.as_bytes());
            assert!(
                index >= inline || !facts.definitions.is_empty(),
                "{language:?}"
            );
            for def in facts
                .definitions
                .iter()
                .filter(|d| index < inline || d.name.is_some())
            {
                let name = def.name.as_deref().unwrap_or_default();
                let short = name.rsplit("::").next().unwrap_or(name);
                let found: Vec<&str> = facts
                    .identifiers
                    .iter()
                    .filter(|i| (i.0, i.1) == (def.byte_start, def.byte_end))
                    .map(|i| &source[i.2..i.3])
                    .collect();
                assert!(
                    !found.is_empty() && found.iter().all(|text| *text == short),
                    "{language:?} {} {name:?}: {found:?}",
                    def.kind
                );
            }
        }
    }

    #[test]
    fn extracts_python_facts_with_spans() {
        let src = b"import os\n\n\ndef add(a, b):\n    return calc(a)\n";
        let facts = parse_source(Language::Python, src);
        assert_eq!(facts.errors, 0);
        assert_eq!(facts.definitions.len(), 1);
        assert_eq!(facts.definitions[0].name.as_deref(), Some("add"));
        assert_eq!(facts.definitions[0].span.start_line, 4);
        assert_eq!(facts.imports.len(), 1);
        assert_eq!(facts.calls.len(), 1);
        assert_eq!(facts.calls[0].name.as_deref(), Some("calc"));
    }

    #[test]
    fn python_multiline_import_end_is_on_its_last_line() {
        let src = b"from pkg import (\n    alpha,\n    beta,\n)\n";
        let facts = parse_source(Language::Python, src);
        assert_eq!(facts.errors, 0);
        assert_eq!(facts.imports.len(), 1);
        let import = &facts.imports[0];
        assert_eq!(
            &src[import.byte_start..import.byte_end],
            b"from pkg import (\n    alpha,\n    beta,\n)"
        );
        assert_eq!((import.span.start_line, import.span.start_col), (1, 1));
        assert_eq!((import.span.end_line, import.span.end_col), (4, 2));
    }

    #[test]
    fn typescript_multiline_import_end_is_on_its_last_line() {
        let src = b"import {\n  alpha,\n  beta,\n} from \"./pkg\";\n";
        let facts = parse_source(Language::TypeScript, src);
        assert_eq!(facts.errors, 0);
        assert_eq!(facts.imports.len(), 1);
        let import = &facts.imports[0];
        assert_eq!(
            &src[import.byte_start..import.byte_end],
            b"import {\n  alpha,\n  beta,\n} from \"./pkg\";"
        );
        assert_eq!((import.span.start_line, import.span.start_col), (1, 1));
        assert_eq!((import.span.end_line, import.span.end_col), (4, 16));
    }

    #[test]
    fn single_line_python_import_span_is_unchanged() {
        let src = b"import os\n";
        let facts = parse_source(Language::Python, src);
        assert_eq!(facts.imports.len(), 1);
        let import = &facts.imports[0];
        assert_eq!(&src[import.byte_start..import.byte_end], b"import os");
        assert_eq!((import.span.start_line, import.span.start_col), (1, 1));
        assert_eq!((import.span.end_line, import.span.end_col), (1, 10));
    }

    #[test]
    fn include_trailing_newline_is_trimmed_before_end_coordinates() {
        let src = b"#include <stdio.h>\nint main(void) {\n  return 0;\n}\n";
        let facts = parse_source(Language::C, src);
        assert_eq!(facts.imports.len(), 1);
        let import = &facts.imports[0];
        assert_eq!(import.byte_end, 18);
        assert_eq!(
            &src[import.byte_start..import.byte_end],
            b"#include <stdio.h>"
        );
        assert_eq!((import.span.start_line, import.span.start_col), (1, 1));
        assert_eq!((import.span.end_line, import.span.end_col), (1, 19));
    }

    #[test]
    fn counts_syntax_errors_without_failing() {
        let src = b"def broken(:\n    pass\n";
        let facts = parse_source(Language::Python, src);
        assert!(facts.errors > 0, "malformed input must surface errors");
    }

    #[test]
    fn language_names_are_stable() {
        assert_eq!(language_name(Language::Cpp), "cpp");
        assert_eq!(language_name(Language::Tsx), "tsx");
    }

    /// The C++ data members of a source: (name, start_line, start_col, text).
    fn cpp_fields(src: &[u8]) -> Vec<(String, u32, u32, String)> {
        parse_source(Language::Cpp, src)
            .definitions
            .iter()
            .filter(|d| d.kind == "field")
            .map(|d| {
                (
                    d.name.clone().unwrap_or_default(),
                    d.span.start_line,
                    d.span.start_col,
                    String::from_utf8_lossy(&src[d.byte_start..d.byte_end]).into_owned(),
                )
            })
            .collect()
    }

    #[test]
    fn extracts_a_cpp_data_member_as_a_field() {
        // A data member points at its NAME, so the anchor is the identifier
        // (the frozen CP1 anchor `patch_match_options.h:56:10`).
        let src = b"class Options {\n  double max_triangulation_angle = -1.0f;\n};\n";
        let fields = cpp_fields(src);
        assert_eq!(fields.len(), 1, "{fields:?}");
        assert_eq!(fields[0].0, "max_triangulation_angle");
        assert_eq!((fields[0].1, fields[0].2), (2, 10));
        assert_eq!(fields[0].3, "max_triangulation_angle");
    }

    #[test]
    fn a_cpp_field_with_an_initializer_is_a_field() {
        let src = b"struct Holder { int value = 1; };\n";
        let fields = cpp_fields(src);
        assert_eq!(fields.len(), 1, "{fields:?}");
        assert_eq!(fields[0].0, "value");
        assert_eq!(fields[0].3, "value");
    }

    #[test]
    fn multiple_cpp_declarators_are_separate_fields() {
        let src = b"struct S { int a, b; };\n";
        let fields = cpp_fields(src);
        let names: Vec<&str> = fields.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, vec!["a", "b"], "{fields:?}");
    }

    #[test]
    fn pointer_reference_and_array_members_are_fields() {
        let src = b"struct S {\n  double* p;\n  double& r;\n  double arr[4];\n};\n";
        let fields = cpp_fields(src);
        let names: Vec<&str> = fields.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, vec!["p", "r", "arr"], "{fields:?}");
    }

    #[test]
    fn a_cpp_method_declaration_is_not_a_field() {
        // A method declaration is a field_declaration node whose declarator
        // is a function declarator: it is never a data member.
        let src = b"class W {\n public:\n  int process(int x);\n};\n";
        assert!(cpp_fields(src).is_empty(), "a method is not a data member");
    }

    #[test]
    fn a_cpp_function_pointer_member_is_not_a_field() {
        // `int (*target)();` is a member, but its declarator is a function
        // declarator: the ambiguous form is not indexed as a data member.
        let src = b"struct Obj { int (*target)(); };\n";
        assert!(
            cpp_fields(src).is_empty(),
            "a function-pointer declarator is not indexed"
        );
    }

    #[test]
    fn same_named_cpp_fields_stay_separate_symbols() {
        let src = b"struct A { int value; };\nstruct B { int value; };\n";
        let fields = cpp_fields(src);
        assert_eq!(fields.len(), 2, "{fields:?}");
        assert_eq!(fields[0].0, "value");
        assert_eq!(fields[1].0, "value");
        assert_ne!(fields[0].1, fields[1].1, "distinct declarations");
    }

    #[test]
    fn qualified_and_derived_cpp_members_are_fields() {
        // A static, a mutable, a bitfield and a member of an anonymous union
        // are data members; a template class member is one too.
        let src = b"class C {\n public:\n  static int counter;\n  mutable double ratio;\n  unsigned int flags : 3;\n  union { int raw; float as_float; };\n};\n";
        let fields = cpp_fields(src);
        let names: Vec<&str> = fields.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(
            names,
            vec!["counter", "ratio", "flags", "raw", "as_float"],
            "{fields:?}"
        );
        // One row per declaration: a member is never indexed twice.
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "no duplicate field rows");
    }

    #[test]
    fn extracts_java_definitions_imports_and_calls() {
        let src = "import java.util.Map;
record P(int x) {}
enum E { A }
@interface T {}
interface I { void f(); }
class C {
  C() {}
  void m() { a.b().c(); d(); new C(); }
}
record Q(int y) { Q {} }
";
        let facts = parse_source(Language::Java, src.as_bytes());
        assert_eq!(facts.errors, 0);
        let defs: Vec<_> = facts
            .definitions
            .iter()
            .map(|d| (d.kind, d.name.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            defs,
            vec![
                ("class", "P"),
                ("type", "E"),
                ("interface", "T"),
                ("interface", "I"),
                ("method", "f"),
                ("class", "C"),
                ("method", "C"),
                ("method", "m"),
                ("class", "Q"),
                ("method", "Q"),
            ]
        );
        assert_eq!(facts.imports.len(), 1);
        let calls: Vec<_> = facts
            .calls
            .iter()
            .map(|c| c.name.as_deref().unwrap_or(""))
            .collect();
        // Chained calls keep the receiver as written; `new C()` is no call.
        assert_eq!(calls, vec!["a.b().c", "a.b", "d"]);
    }

    fn def_pairs(lang: Language, src: &str) -> Vec<(&'static str, String)> {
        parse_source(lang, src.as_bytes())
            .definitions
            .iter()
            .map(|d| (d.kind, d.name.clone().unwrap_or_default()))
            .collect()
    }

    #[test]
    fn rust_enums_traits_and_type_aliases_are_definitions() {
        let src = "enum E { A }
trait T { fn t(&self); fn d(&self) {} }
type A = u8;
impl T for E { type Out = u8; fn t(&self) {} }
";
        let want = [
            ("type", "E"),
            ("interface", "T"),
            ("function", "t"),
            ("function", "d"),
            ("type", "A"),
            ("function", "t"),
        ];
        let got = def_pairs(Language::Rust, src);
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (g, w) in got.iter().zip(want) {
            assert_eq!((g.0, g.1.as_str()), w);
        }
    }

    #[test]
    fn c_and_cpp_enums_and_typedefs_are_definitions_but_uses_are_not() {
        let c = "enum Color { RED };
typedef int myint;
typedef struct { int y; } Q;
static struct P *gp;
enum Color c2;
typedef struct Node { int v; } Node;
typedef enum Color2 { G } Color2;
";
        let got = def_pairs(Language::C, c);
        let names: Vec<_> = got.iter().map(|g| (g.0, g.1.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("type", "Color"),
                ("type", "myint"),
                ("struct", "Q"),
                ("struct", "Node"),
                ("type", "Color2"),
            ]
        );
        let cpp = "enum class K { A };
using V = int;
class Fwd;
struct Fs;
Fs *fp;
class Real {};
";
        let got = def_pairs(Language::Cpp, cpp);
        let names: Vec<_> = got.iter().map(|g| (g.0, g.1.as_str())).collect();
        assert_eq!(names, vec![("type", "K"), ("type", "V"), ("class", "Real")]);
    }

    #[test]
    fn a_typedef_of_a_tagged_type_is_one_fact_or_two_by_alias_name() {
        // Same alias as the tag: one fact whose span covers the whole
        // declaration, so the trailing alias name is an anchor too.
        let src = "typedef struct Node { int v; } Node;
";
        let facts = parse_source(Language::C, src.as_bytes());
        assert_eq!(facts.definitions.len(), 1);
        let d = &facts.definitions[0];
        assert_eq!((d.kind, d.name.as_deref()), ("struct", Some("Node")));
        assert_eq!((d.span.start_col, d.span.end_col), (1, 37));
        // Different alias: the tag and the alias are separate definitions.
        let src = "typedef struct node_s { int v; } node_t;
";
        let facts = parse_source(Language::C, src.as_bytes());
        let mut got: Vec<_> = facts
            .definitions
            .iter()
            .map(|d| (d.kind, d.name.as_deref().unwrap_or("")))
            .collect();
        got.sort();
        assert_eq!(got, vec![("struct", "node_s"), ("type", "node_t")]);
        // Enums and C++ classes follow the same rule.
        let facts = parse_source(
            Language::C,
            b"typedef enum E1 { A } E1;
",
        );
        assert_eq!(facts.definitions.len(), 1);
        assert_eq!(facts.definitions[0].span.end_col, 26);
        let facts = parse_source(
            Language::Cpp,
            b"typedef class K { } K;
",
        );
        assert_eq!(facts.definitions.len(), 1);
        assert_eq!(facts.definitions[0].kind, "class");
        assert_eq!(facts.definitions[0].span.end_col, 23);
        let facts = parse_source(
            Language::Cpp,
            b"typedef class K2 { } H;
",
        );
        assert_eq!(facts.definitions.len(), 2);
    }

    #[test]
    fn a_macro_between_the_class_keyword_and_name_is_still_a_class() {
        let src = "class EXPORT Foo { int x; };
struct EXPORT S { int a; };
class EXPORT Fwd;
class Plain {};
int real(int v) { return v; }
";
        let got = def_pairs(Language::Cpp, src);
        let names: Vec<_> = got.iter().map(|g| (g.0, g.1.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("class", "Foo"),
                ("struct", "S"),
                ("class", "Plain"),
                ("function", "real"),
            ]
        );
    }

    #[test]
    fn typescript_enums_type_aliases_and_abstract_classes_are_definitions() {
        let src = "enum E { X }
type A = string;
abstract class AC {}
";
        let got = def_pairs(Language::TypeScript, src);
        let names: Vec<_> = got.iter().map(|g| (g.0, g.1.as_str())).collect();
        assert_eq!(names, vec![("type", "E"), ("type", "A"), ("class", "AC")]);
    }

    #[test]
    fn grammar_versions_match_lockfile_entries() {
        assert_eq!(grammar_version(Language::Python), "python:0.25.0");
        assert_eq!(grammar_version(Language::Cpp), "cpp:0.23.4");
        assert_eq!(grammar_version(Language::Go), "go:0.25.0");
        assert_eq!(grammar_version(Language::Java), "java:0.23.5");
    }

    #[test]
    fn extracts_go_definitions_imports_and_calls() {
        let src = concat!(
            "package main\n",
            "\n",
            "import (\n",
            "\t\"fmt\"\n",
            "\tstr \"strings\"\n",
            ")\n",
            "\n",
            "type Server struct{ name string }\n",
            "type Handler interface{ Serve() }\n",
            "type ID = int\n",
            "\n",
            "func helper(n int) int { return n + 1 }\n",
            "\n",
            "func (s *Server) Start() {\n",
            "\tfmt.Println(helper(1))\n",
            "\tstr.ToUpper(s.name)\n",
            "\tf := func() { helper(2) }\n",
            "\tf()\n",
            "}\n",
        );
        let facts = parse_source(Language::Go, src.as_bytes());
        assert_eq!(facts.errors, 0);
        let defs: Vec<_> = facts
            .definitions
            .iter()
            .map(|d| (d.kind, d.name.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            defs,
            vec![
                ("struct", "Server"),
                ("interface", "Handler"),
                ("type", "ID"),
                ("function", "helper"),
                ("method", "Start"),
            ]
        );
        // A grouped import is one statement, starting on its first line.
        assert_eq!(facts.imports.len(), 1);
        assert_eq!(facts.imports[0].span.start_line, 3);
        assert_eq!(facts.imports[0].span.end_line, 6);
        let calls: Vec<_> = facts
            .calls
            .iter()
            .map(|c| c.name.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            calls,
            vec!["fmt.Println", "helper", "str.ToUpper", "helper", "f"]
        );
        assert_eq!(
            classify(Path::new("main.go")),
            FileKind::Supported(Language::Go)
        );
    }

    #[test]
    fn go_generic_calls_are_recorded_under_the_function_name() {
        // `F[int](1)` parses as a type conversion and `F[int](1, 2)` as a call
        // whose callee is an index expression; neither may lose the call or
        // keep the type argument in its name. `T(3)` and `[]int(z)` are the
        // plain call and the real conversion.
        let src = concat!(
            "package p\n",
            "func a() {\n",
            "\tF[int](1)\n",
            "\tpkg.F[int](1)\n",
            "\tF[int](1, 2)\n",
            "\tpkg.F[int](1, 2)\n",
            "\tG[int, string](1, 2)\n",
            "\tT(3)\n",
            "\t_ = []int(z)\n",
            "\t_ = M[int]\n",
            "}\n",
        );
        let facts = parse_source(Language::Go, src.as_bytes());
        let got: Vec<_> = facts
            .calls
            .iter()
            .map(|c| (c.span.start_line, c.name.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(
            got,
            vec![
                (3, "F"),
                (4, "pkg.F"),
                (5, "F"),
                (6, "pkg.F"),
                (7, "G"),
                (8, "T")
            ]
        );
        let c = &facts.calls[1];
        let t = c.tcallee.as_ref().expect("terminal span recorded");
        assert_eq!(&src.as_bytes()[t.byte_start..t.byte_end], b"F");
    }

    #[test]
    fn relationship_proof_attributes_go_calls_to_functions_methods_and_closures() {
        let src = concat!(
            "package main\n",
            "func outer() {\n",
            "\tfirst()\n",
            "\tf := func() { second() }\n",
            "\tf()\n",
            "}\n",
            "func (s *S) m() {\n",
            "\tthird()\n",
            "}\n",
        );
        let proof = super::relationship_proof(Language::Go, src.as_bytes());
        let (owner, _) = scope_of(src, &proof, "first()");
        assert!(owner.unwrap().starts_with("func outer()"));
        // A closure ends the owner chain, as in the other languages.
        let (owner, _) = scope_of(src, &proof, "second()");
        assert_eq!(owner, None);
        let (owner, _) = scope_of(src, &proof, "third()");
        assert!(owner.unwrap().starts_with("func (s *S) m()"));
        let names: Vec<_> = proof
            .declarations
            .iter()
            .map(|&(from, to)| &src[from..to])
            .collect();
        assert_eq!(names, ["outer", "m"]);
    }

    #[test]
    fn relationship_proof_attributes_java_calls_to_methods_and_constructors() {
        let src = concat!(
            "class A {
",
            "  A() { init(); }
",
            "  void m() { first(); Runnable r = () -> second(); }
",
            "}
",
            "record R(int x) { R { check(); } }
",
        );
        let proof = super::relationship_proof(Language::Java, src.as_bytes());
        let (owner, _) = scope_of(src, &proof, "init()");
        assert!(owner.unwrap().starts_with("A() {"));
        let (owner, _) = scope_of(src, &proof, "first()");
        assert!(owner.unwrap().starts_with("void m()"));
        // A lambda ends the owner chain, as in the other languages.
        let (owner, _) = scope_of(src, &proof, "second()");
        assert_eq!(owner, None);
        let (owner, _) = scope_of(src, &proof, "check()");
        assert!(owner.unwrap().starts_with("R {"));
    }

    #[test]
    fn relationship_proof_keeps_calls_in_arrow_functions_unowned() {
        let src = concat!(
            "const outer = () => { first(); [1].map(() => second()); };
",
            "class K { m = () => third(); }
",
            "[1].map(() => fourth());
",
        );
        let proof = super::relationship_proof(Language::JavaScript, src.as_bytes());
        // A name bound to an arrow is a definition, but not an owner: an
        // anonymous callable has no persisted named owner (M6 contract).
        for call in ["first()", "second()", "third()", "fourth()"] {
            let (owner, _) = scope_of(src, &proof, call);
            assert_eq!(owner, None, "{call}");
        }
    }

    #[test]
    fn named_function_bindings_are_definitions_but_other_values_are_not() {
        let src = "const f = () => 1;
const g = function () {};
const { a } = b;
const n = 3;
const h = k();
class C { x = () => 1; y = 2; #p = function () {}; }
";
        let got = def_pairs(Language::JavaScript, src);
        let names: Vec<_> = got.iter().map(|g| (g.0, g.1.as_str())).collect();
        assert_eq!(
            names,
            vec![
                ("function", "f"),
                ("function", "g"),
                ("class", "C"),
                ("method", "x"),
                ("method", "#p"),
            ]
        );
    }

    #[test]
    fn cpp_direct_initialization_is_a_call() {
        // C++ has no `call_expression` for `T var(args);`: the grammar models
        // it as declaration -> init_declarator -> argument_list, so the
        // constructor call is invisible unless it is captured explicitly.
        let src = b"struct Widget { Widget(int a); };\nvoid caller() {\n  Widget w(1);\n  Widget w2 = Widget(2);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        let names: Vec<&str> = facts
            .calls
            .iter()
            .map(|c| c.name.as_deref().unwrap_or(""))
            .collect();
        assert_eq!(
            names,
            vec!["Widget", "Widget"],
            "direct initialization and copy initialization are both constructor calls"
        );
        let direct = &facts.calls[0];
        // The call range covers the called type through the argument list.
        assert_eq!(&src[direct.byte_start..direct.byte_end], b"Widget w(1)");
        let callee = direct.callee.as_ref().expect("callee span recorded");
        assert_eq!(&src[callee.byte_start..callee.byte_end], b"Widget");
        let terminal = direct.tcallee.as_ref().expect("terminal span recorded");
        assert_eq!(&src[terminal.byte_start..terminal.byte_end], b"Widget");
    }

    #[test]
    fn cpp_direct_initialization_keeps_the_qualified_terminal() {
        let src = b"void caller() {\n  ns::Widget w(1);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert_eq!(facts.calls.len(), 1);
        let call = &facts.calls[0];
        assert_eq!(call.name.as_deref(), Some("ns::Widget"));
        let terminal = call.tcallee.as_ref().expect("terminal span recorded");
        assert_eq!(&src[terminal.byte_start..terminal.byte_end], b"Widget");
    }

    #[test]
    fn cpp_primitive_direct_initialization_is_not_a_call() {
        // `int x(5);` is not a constructor call; a primitive type must not
        // become a call name.
        let src = b"void caller() {\n  int x(5);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "primitive initialization is not a call"
        );
    }

    #[test]
    fn cpp_most_vexing_parse_is_ambiguous_and_not_a_call() {
        // `T v(a, b);` with identifier-like arguments is parsed by the
        // grammar as a function declarator. C++ reads it as a function
        // declaration when `a`/`b` denote types and as a variable
        // definition when they denote variables; only name lookup can
        // decide, so the syntax layer must not assert a constructor call.
        let src = b"void caller() {\n  Widget w(options, problem);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "an ambiguous most-vexing-parse form must not become a call fact"
        );
    }

    #[test]
    fn cpp_block_scope_type_argument_is_not_a_call() {
        // `Widget w(int);` inside a function body declares the function
        // `w` taking an int (MSVC: the type of `w` is `Widget(int)`), not
        // a variable: a function declaration is legal C++ at block scope,
        // so the declaration is not a constructor call.
        let src = b"void caller() {\n  Widget w(int);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "a block-scope function declaration is not a constructor call"
        );
    }

    #[test]
    fn cpp_block_scope_function_declaration_is_not_a_call() {
        // The same shape with a named return type is the canonical
        // block-scope function declaration, which C++ accepts.
        let src = b"void caller() {\n  Widget helper(int);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "a block-scope function declaration is not a constructor call"
        );
    }

    #[test]
    fn cpp_block_scope_empty_parentheses_is_not_a_call() {
        // `Widget w();` declares the function `w` (MSVC: the type of `w` is
        // `Widget(void)`), never a default-constructed variable.
        let src = b"void caller() {\n  Widget w();\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "an empty argument list declares a function, not a call"
        );
    }

    #[test]
    fn cpp_block_scope_function_type_argument_is_not_a_call() {
        // `Widget w(T());` is the classic most-vexing parse: `T()` is a
        // function type, so the declaration is a function declaration
        // (MSVC: the type of `w` is `Widget(T(void))`).
        let src = b"struct T {};\nvoid caller() {\n  Widget w(T());\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "a function-type argument declares a function, not a call"
        );
    }

    #[test]
    fn cpp_prototype_at_namespace_scope_is_not_a_call() {
        // The same shape outside a function body is a prototype, not a call.
        let src = b"Widget w(options, problem);\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "a namespace-scope prototype is not a constructor call"
        );
    }

    #[test]
    fn cpp_local_declaration_in_a_preprocessor_branch_is_a_call() {
        // A preprocessor branch does not change the scope: the declaration is
        // still local, so a direct initialization is a constructor call.
        let src = b"void caller() {\n#if defined(X)\n  Widget w(1);\n#endif\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert_eq!(facts.calls.len(), 1);
        assert_eq!(facts.calls[0].name.as_deref(), Some("Widget"));
    }

    #[test]
    fn cpp_direct_initialization_records_the_first_declarator_only() {
        // Known limit: a declaration with several declarators yields the
        // fact for the first one only. The omission is conservative - a
        // missing fact, never a wrong one.
        let src = b"void caller() {\n  Widget a(1), b(2);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert_eq!(
            facts.calls.len(),
            1,
            "only the first declarator of a multi-declarator declaration is recorded"
        );
    }

    #[test]
    fn cpp_template_type_direct_initialization_is_a_call() {
        // A template type is a qualified_identifier in the type field, so
        // the call is recorded. The callee stays as written; the terminal is
        // the template name without its arguments.
        let src = b"void caller() {\n  std::vector<int> v(10);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert_eq!(facts.calls.len(), 1);
        let call = &facts.calls[0];
        assert_eq!(call.name.as_deref(), Some("std::vector<int>"));
        let callee = call.callee.as_ref().expect("callee span recorded");
        assert_eq!(
            &src[callee.byte_start..callee.byte_end],
            b"std::vector<int>"
        );
        assert_eq!(terminal_text(src, call), "vector");
    }

    #[test]
    fn cpp_template_type_direct_initialization_terminal_ignores_argument_separators() {
        let src = b"void caller() {\n  std::vector<ns::Item> items(10);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert_eq!(facts.calls.len(), 1);
        assert_eq!(terminal_text(src, &facts.calls[0]), "vector");
    }

    /// Text covered by the terminal span of a call fact.
    fn terminal_text<'a>(src: &'a [u8], call: &super::Fact) -> &'a str {
        let terminal = call.tcallee.as_ref().expect("terminal span recorded");
        std::str::from_utf8(&src[terminal.byte_start..terminal.byte_end]).unwrap()
    }

    /// Terminal of the single call in `void caller() { <expr>; }`.
    fn cpp_terminal_of(expr: &str) -> String {
        let src = format!("void caller() {{\n  {expr};\n}}\n");
        let facts = parse_source(Language::Cpp, src.as_bytes());
        assert_eq!(facts.calls.len(), 1, "one call in {expr:?}");
        terminal_text(src.as_bytes(), &facts.calls[0]).to_string()
    }

    #[test]
    fn cpp_template_call_terminal_is_the_function_not_the_type_argument() {
        // `mvs::PatchMatchController` is a template type argument of the
        // called function `make_shared`, not part of the called name. The
        // callee stays as written; the terminal is the function name.
        let src = b"void caller() {\n  std::make_shared<mvs::PatchMatchController>(options);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        assert_eq!(facts.calls.len(), 1);
        let call = &facts.calls[0];
        assert_eq!(
            call.name.as_deref(),
            Some("std::make_shared<mvs::PatchMatchController>")
        );
        let callee = call.callee.as_ref().expect("callee span recorded");
        assert_eq!(
            &src[callee.byte_start..callee.byte_end],
            b"std::make_shared<mvs::PatchMatchController>"
        );
        assert_eq!(terminal_text(src, call), "make_shared");
        // The line/column pair agrees with the byte range (1-based).
        let terminal = call.tcallee.as_ref().expect("terminal span recorded");
        assert_eq!(
            (
                terminal.start_line,
                terminal.start_col,
                terminal.end_line,
                terminal.end_col
            ),
            (2, 8, 2, 19)
        );
    }

    #[test]
    fn cpp_template_call_terminal_excludes_every_template_argument_list() {
        // The terminal is the called name for every shape that carries a
        // template argument list, whatever the arguments contain.
        let cases = [
            (
                "std::make_unique<std::vector<std::string>>(n)",
                "make_unique",
            ),
            ("make<ns::Thing>(x)", "make"),
            ("std::get<0>(t)", "get"),
            ("obj.get<ns::Thing>()", "get"),
            ("ptr->get<ns::Thing>()", "get"),
            ("ptr->template get<ns::Thing>()", "get"),
            ("Foo<ns::T>::template create<ns::U>(x)", "create"),
        ];
        for (expr, terminal) in cases {
            assert_eq!(cpp_terminal_of(expr), terminal, "terminal of {expr:?}");
        }
    }

    #[test]
    fn cpp_call_terminal_is_unchanged_without_a_template_argument_list() {
        // Calls that were already identified correctly keep their terminal,
        // including template arguments that only appear in a scope.
        let cases = [
            ("foo(1)", "foo"),
            ("ns::inner::run(x)", "run"),
            ("a.b.c(x)", "c"),
            ("ptr->run(x)", "run"),
            ("Foo<ns::T>::create(x)", "create"),
            ("ns::Foo<ns::Bar>::Baz<ns::Qux>::run(x)", "run"),
        ];
        for (expr, terminal) in cases {
            assert_eq!(cpp_terminal_of(expr), terminal, "terminal of {expr:?}");
        }
    }

    #[test]
    fn cpp_unqualified_template_call_has_no_receiver() {
        // Nothing precedes the called name, so the terminal starts where the
        // callee starts (the graph reads this as "no receiver").
        let src = b"void caller() {\n  make<ns::Thing>(x);\n}\n";
        let facts = parse_source(Language::Cpp, src);
        let call = &facts.calls[0];
        let callee = call.callee.as_ref().expect("callee span recorded");
        let terminal = call.tcallee.as_ref().expect("terminal span recorded");
        assert_eq!(terminal.byte_start, callee.byte_start);
    }

    #[test]
    fn cpp_class_member_declaration_is_not_a_call() {
        // A declaration inside a class body is a member declaration, not a
        // constructor call.
        let src = b"struct Holder { Holder(int a); };\n";
        let facts = parse_source(Language::Cpp, src);
        assert!(
            facts.calls.is_empty(),
            "a class member declaration is not a constructor call"
        );
    }

    /// The reference occurrences of a C++ source: (role, name, start_line,
    /// start_col, the text under the recorded byte range).
    fn cpp_occurrences(src: &[u8]) -> Vec<(String, String, u32, u32, String)> {
        parse_source(Language::Cpp, src)
            .reference_occurrences
            .iter()
            .map(|o| {
                (
                    o.kind.to_string(),
                    o.name.clone().unwrap_or_default(),
                    o.span.start_line,
                    o.span.start_col,
                    String::from_utf8_lossy(&src[o.byte_start..o.byte_end]).into_owned(),
                )
            })
            .collect()
    }

    fn occurrence(
        role: &str,
        name: &str,
        line: u32,
        col: u32,
    ) -> (String, String, u32, u32, String) {
        (role.into(), name.into(), line, col, name.into())
    }

    #[test]
    fn a_cpp_member_access_is_an_observed_occurrence_at_the_identifier() {
        // `.` and `->` are both member accesses; the row points at the field
        // identifier, and the bytes address exactly that identifier.
        let src = b"struct S { int b; };\nint f(S a, S* p) {\n  return a.b + p->b;\n}\n";
        assert_eq!(
            cpp_occurrences(src),
            vec![
                occurrence("member-access", "b", 3, 12),
                occurrence("member-access", "b", 3, 19),
            ]
        );
    }

    #[test]
    fn a_method_call_receiver_is_a_member_access_by_name_next_to_its_call() {
        // Any `field_expression` with a `field_identifier` is a member access;
        // the call fact is recorded separately and is unchanged.
        let src = b"struct W { void run(); };\nvoid g(W w) {\n  w.run();\n}\n";
        assert_eq!(
            cpp_occurrences(src),
            vec![occurrence("member-access", "run", 3, 5)]
        );
        assert_eq!(parse_source(Language::Cpp, src).calls.len(), 1);
    }

    #[test]
    fn the_address_of_a_member_is_one_address_of_binding_row() {
        // `&p->b` is a single row with the address-of role: the binding rule
        // replaces the member-access row, it does not add a second one.
        let src = b"struct S { int b; };\nvoid f(S* p) {\n  int* q = &p->b;\n}\n";
        assert_eq!(
            cpp_occurrences(src),
            vec![occurrence("address-of-binding", "b", 3, 16)]
        );
    }

    #[test]
    fn a_dereferenced_member_is_a_member_access_not_an_address_of_binding() {
        // Only the `&` operator binds: `*p->b` dereferences the member.
        let src = b"struct S { int* b; };\nint f(S* p) {\n  int v = *p->b;\n  return v;\n}\n";
        assert_eq!(
            cpp_occurrences(src),
            vec![occurrence("member-access", "b", 3, 15)]
        );
    }

    #[test]
    fn a_bare_argument_of_a_same_file_function_macro_is_a_candidate() {
        // The callee `PRINT` is a function-like macro defined in this file,
        // so the bare identifier argument is a macro-argument row. The macro
        // name itself is never an occurrence.
        let src = b"#define PRINT(x) (x)\nvoid f() {\n  int b = 0;\n  PRINT(b);\n}\n";
        assert_eq!(
            cpp_occurrences(src),
            vec![occurrence("macro-argument", "b", 4, 9)]
        );
    }

    #[test]
    fn a_function_macro_defined_after_its_use_is_still_a_candidate() {
        // The macro-name pre-pass is order-insensitive; the `candidate`
        // confidence is the honest ceiling for it.
        let src = b"void f() {\n  int b = 0;\n  M(b);\n}\n#define M(x) (x)\n";
        assert_eq!(
            cpp_occurrences(src),
            vec![occurrence("macro-argument", "b", 3, 5)]
        );
    }

    #[test]
    fn object_like_foreign_and_ordinary_callees_have_no_macro_arguments() {
        // `K` is an object-like macro (not callable), `FOREIGN` is defined
        // nowhere in this file and `real` is an ordinary call: none of their
        // arguments is a macro-argument row.
        let src = b"#define K 1\nvoid f() {\n  int b = 0;\n  K(b);\n  FOREIGN(b);\n  real(b);\n}\n";
        assert!(
            cpp_occurrences(src).is_empty(),
            "{:?}",
            cpp_occurrences(src)
        );
    }

    #[test]
    fn a_member_access_inside_a_macro_argument_is_a_member_access() {
        // Only a BARE identifier argument is a macro argument: `a.b` is a
        // member access of `b`, and `a` (the receiver) is not an occurrence.
        let src = b"#define M(x) (x)\nstruct S { int b; };\nvoid f(S a) {\n  M(a.b);\n}\n";
        assert_eq!(
            cpp_occurrences(src),
            vec![occurrence("member-access", "b", 4, 7)]
        );
    }

    #[test]
    fn a_string_literal_is_never_an_occurrence() {
        let src = b"#define M(x) (x)\nvoid f() {\n  M(\"b\");\n  const char* s = \"b\";\n}\n";
        assert!(
            cpp_occurrences(src).is_empty(),
            "{:?}",
            cpp_occurrences(src)
        );
    }

    #[test]
    fn declarations_are_not_occurrences() {
        // A data member declaration is the `field` symbol of Slice 1, never
        // an occurrence row.
        let src = b"struct S { int a; int b, c; };\n";
        assert!(
            cpp_occurrences(src).is_empty(),
            "{:?}",
            cpp_occurrences(src)
        );
        assert_eq!(cpp_fields(src).len(), 3);
    }

    #[test]
    fn a_qualified_member_name_is_not_an_occurrence() {
        // `Type::member` and `a.Type::member` have a qualified identifier,
        // not a field identifier, in the field position: a recorded coverage
        // limit, never a guess.
        let src = b"struct A { static int b; };\nint f(A a) {\n  return A::b + a.A::b;\n}\n";
        assert!(
            cpp_occurrences(src).is_empty(),
            "{:?}",
            cpp_occurrences(src)
        );
    }

    #[test]
    fn c_sources_produce_no_reference_occurrences() {
        // The C walker is untouched: a C struct member census is a separate
        // decision (the C++ arm only).
        let src = b"struct S { int b; };\nint f(struct S a) { return a.b; }\n";
        assert!(parse_source(Language::C, src)
            .reference_occurrences
            .is_empty());
    }

    #[test]
    fn the_confidence_of_an_occurrence_is_derived_from_its_role() {
        use super::{
            occurrence_confidence, ROLE_ADDRESS_OF_BINDING, ROLE_MACRO_ARGUMENT, ROLE_MEMBER_ACCESS,
        };
        assert_eq!(occurrence_confidence(ROLE_MEMBER_ACCESS), "observed");
        assert_eq!(occurrence_confidence(ROLE_ADDRESS_OF_BINDING), "observed");
        assert_eq!(occurrence_confidence(ROLE_MACRO_ARGUMENT), "candidate");
        // An unknown role never claims more than a candidate.
        assert_eq!(occurrence_confidence("anything-else"), "candidate");
    }

    #[test]
    fn deeply_nested_source_does_not_overflow_the_stack() {
        // About 400 KB, inside the 1 MiB file cap, and far deeper than a 2 MiB
        // stack can recurse through: extraction must not depend on tree depth.
        const DEPTH: usize = 200_000;
        let nest = |head: &str, open: &str, close: &str, tail: &str| {
            format!("{head}{}{}{tail}", open.repeat(DEPTH), close.repeat(DEPTH))
        };
        let cases = [
            (Language::Python, nest("x = ", "[", "]", "\n")),
            (Language::JavaScript, nest("x = ", "[", "]", ";\n")),
            (Language::Cpp, nest("int x[] = ", "{", "}", ";\n")),
        ];
        for (lang, src) in cases {
            let name = language_name(lang);
            let facts = std::thread::Builder::new()
                .stack_size(2 * 1024 * 1024)
                .spawn(move || parse_source(lang, src.as_bytes()))
                .unwrap()
                .join()
                .unwrap_or_else(|_| panic!("{name}: extraction panicked"));
            assert_eq!(facts.errors, 0, "{name}: nested source parses cleanly");
            assert!(facts.definitions.is_empty(), "{name}");
        }
    }

    /// Source text of the owner and block recorded for the call written as `text`.
    fn scope_of<'a>(
        src: &'a str,
        proof: &super::RelationshipProof,
        text: &str,
    ) -> (Option<&'a str>, Option<&'a str>) {
        let start = src.find(text).unwrap();
        let key = (start, start + text.len());
        let slice = |span: Option<(usize, usize)>| span.map(|(from, to)| &src[from..to]);
        (slice(proof.owners[&key]), slice(proof.call_blocks[&key]))
    }

    #[test]
    fn relationship_proof_attributes_rust_calls_to_the_nearest_definition_and_block() {
        let src = concat!(
            "fn outer() {\n",
            "    first();\n",
            "    fn inner() {\n",
            "        second();\n",
            "    }\n",
            "    let alias = inner;\n",
            "    let run = || { third(); };\n",
            "    run();\n",
            "}\n",
            "static S: u8 = side();\n",
        );
        let proof = super::relationship_proof(Language::Rust, src.as_bytes());
        let (owner, block) = scope_of(src, &proof, "first()");
        assert!(owner.unwrap().starts_with("fn outer()"));
        let outer_body = block.unwrap();
        assert!(outer_body.starts_with("{\n    first();") && outer_body.ends_with("\n}"));
        let (owner, block) = scope_of(src, &proof, "second()");
        assert!(owner.unwrap().starts_with("fn inner()"));
        assert_eq!(block, Some("{\n        second();\n    }"));
        // A closure ends the owner chain, but its body is still the nearest block.
        assert_eq!(
            scope_of(src, &proof, "third()"),
            (None, Some("{ third(); }"))
        );
        let (owner, block) = scope_of(src, &proof, "run()");
        assert!(owner.unwrap().starts_with("fn outer()"));
        assert_eq!(block, Some(outer_body));
        // Outside every definition and block there is neither.
        assert_eq!(scope_of(src, &proof, "side()"), (None, None));
        let names: Vec<_> = proof
            .declarations
            .iter()
            .map(|&(from, to)| &src[from..to])
            .collect();
        assert_eq!(names, ["outer", "inner"]);
        let aliases: Vec<_> = proof
            .local_function_aliases
            .iter()
            .map(|a| {
                (
                    &src[a.binding.0..a.binding.1],
                    &src[a.target.0..a.target.1],
                    &src[a.block.0..a.block.1],
                )
            })
            .collect();
        assert_eq!(aliases, [("alias", "inner", outer_body)]);
    }

    #[test]
    fn relationship_proof_attributes_python_calls_to_the_nearest_definition_and_block() {
        let src = concat!(
            "def outer():\n",
            "    first()\n",
            "    class K:\n",
            "        attr = second()\n",
            "        def method(self):\n",
            "            third()\n",
            "    f = lambda: fourth()\n",
            "fifth()\n",
        );
        let proof = super::relationship_proof(Language::Python, src.as_bytes());
        let (owner, block) = scope_of(src, &proof, "first()");
        assert!(owner.unwrap().starts_with("def outer():"));
        let outer_body = block.unwrap();
        assert!(outer_body.starts_with("first()") && outer_body.contains("fourth()"));
        // A class ends the owner chain; its body is the nearest block...
        let (owner, block) = scope_of(src, &proof, "second()");
        assert_eq!(owner, None);
        let class_body = block.unwrap();
        assert!(class_body.starts_with("attr = second()") && class_body.contains("third()"));
        // ...but a method inside it is a new owner with a block of its own.
        let (owner, block) = scope_of(src, &proof, "third()");
        assert!(owner.unwrap().starts_with("def method(self):"));
        assert_eq!(block.map(str::trim_end), Some("third()"));
        // A lambda ends the owner chain without being a block.
        assert_eq!(scope_of(src, &proof, "fourth()"), (None, Some(outer_body)));
        assert_eq!(scope_of(src, &proof, "fifth()"), (None, None));
        let names: Vec<_> = proof
            .declarations
            .iter()
            .map(|&(from, to)| &src[from..to])
            .collect();
        assert_eq!(names, ["outer", "method"]);
    }

    #[test]
    fn relationship_proof_treats_a_js_generator_declaration_as_the_owner_of_its_calls() {
        let src = concat!(
            "function outer() {\n",
            "  first();\n",
            "  function* gen() {\n",
            "    second();\n",
            "  }\n",
            "}\n",
        );
        let proof = super::relationship_proof(Language::JavaScript, src.as_bytes());
        let (owner, _) = scope_of(src, &proof, "first()");
        assert!(owner.unwrap().starts_with("function outer()"));
        let (owner, _) = scope_of(src, &proof, "second()");
        assert!(owner.unwrap().starts_with("function* gen()"));
        let names: Vec<_> = proof
            .declarations
            .iter()
            .map(|&(from, to)| &src[from..to])
            .collect();
        assert_eq!(names, ["outer", "gen"]);
    }

    #[test]
    fn relationship_proof_walks_deeply_nested_calls_within_a_time_bound() {
        // Guards the walk against per-call ancestor lookups. The walk over 300
        // nested calls takes milliseconds; the bound is generous for slow
        // runners.
        const CALLS: usize = 300;
        let src = format!(
            "fn f() {{ {}1{} }}\n",
            "g(".repeat(CALLS),
            ")".repeat(CALLS)
        );
        let start = std::time::Instant::now();
        let proof = super::relationship_proof(Language::Rust, src.as_bytes());
        let elapsed = start.elapsed();
        let end = src.trim_end().len();
        assert_eq!(proof.owners.len(), CALLS);
        assert!(proof.owners.values().all(|owner| *owner == Some((0, end))));
        assert!(proof
            .call_blocks
            .values()
            .all(|block| *block == Some((7, end))));
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "{CALLS} nested calls took {elapsed:?}"
        );
    }

    #[test]
    fn wide_call_block_extraction_completes_promptly() {
        // One function body holding many call statements, parsed in full.
        const STATEMENTS: usize = 20_000;
        const BOUND: std::time::Duration = std::time::Duration::from_secs(5);
        let mut source = String::from("fn main() {\n");
        for _ in 0..STATEMENTS {
            source.push_str("    g();\n");
        }
        source.push_str("}\n");
        let started = std::time::Instant::now();
        let facts = parse_source(Language::Rust, source.as_bytes());
        let elapsed = started.elapsed();
        assert_eq!(facts.calls.len(), STATEMENTS);
        assert!(elapsed < BOUND, "a wide call block took {elapsed:?}");
    }

    #[test]
    fn node_relative_positions_match_a_whole_file_count() {
        // The whole-file count that `position_from` replaces, as the reference.
        fn whole_file(src: &[u8], off: usize) -> (u32, u32) {
            let off = off.min(src.len());
            let line = src[..off].iter().filter(|b| **b == b'\n').count() as u32 + 1;
            let col = (off
                - src[..off]
                    .iter()
                    .rposition(|b| *b == b'\n')
                    .map(|p| p + 1)
                    .unwrap_or(0)) as u32
                + 1;
            (line, col)
        }
        let cases: [&[u8]; 6] = [
            b"fn f() {}\n",
            b"a\n  b.c()\r\n  d\n",
            "x = \"é\"\n  ü.g()\n".as_bytes(),
            b"",
            b"\n\n\n",
            b"tail without newline",
        ];
        for src in cases {
            for base in 0..=src.len() {
                let base_pos = whole_file(src, base);
                for off in base..=src.len() {
                    assert_eq!(
                        position_from(src, base, base_pos, off),
                        whole_file(src, off),
                        "base {base}, offset {off}, source {:?}",
                        String::from_utf8_lossy(src)
                    );
                }
            }
        }
    }

    // Heritage facts (PR 6): a type declaration names its supertypes. Every
    // expectation below is written from the fixture source.
    type HeritageRow = (String, &'static str, String, Option<String>);

    /// The heritage rows of a source, sorted. Each row is also checked against
    /// the source: its bytes are the supertype text, the declaring range holds
    /// the supertype, and (outside Rust, where an impl is no definition) the
    /// declaring range and subtype are those of a stored definition.
    fn heritage_rows(lang: Language, src: &str) -> Vec<HeritageRow> {
        let facts = parse_source(lang, src.as_bytes());
        let mut rows = Vec::new();
        for h in &facts.heritage {
            assert_eq!(
                &src.as_bytes()[h.byte_start..h.byte_end],
                h.supertype_text.as_bytes(),
                "{h:?}"
            );
            assert!(
                h.decl_byte_start <= h.byte_start && h.byte_end <= h.decl_byte_end,
                "{h:?}"
            );
            if lang == Language::Rust {
                assert!(src[h.decl_byte_start..].starts_with("impl"), "{h:?}");
            } else {
                assert!(
                    facts
                        .definitions
                        .iter()
                        .any(|d| d.byte_start == h.decl_byte_start
                            && d.byte_end == h.decl_byte_end
                            && d.name.as_deref() == Some(h.subtype.as_str())),
                    "no stored definition for {h:?}"
                );
            }
            rows.push((
                h.subtype.clone(),
                h.relation,
                h.supertype_text.clone(),
                h.supertype_name.clone(),
            ));
        }
        rows.sort();
        rows
    }

    fn heritage_expected(rows: &[(&str, &'static str, &str, Option<&str>)]) -> Vec<HeritageRow> {
        let mut out: Vec<HeritageRow> = rows
            .iter()
            .map(|(a, b, c, d)| (a.to_string(), *b, c.to_string(), d.map(str::to_string)))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn heritage_names_strip_type_arguments_and_pointer_marks_only() {
        for (text, name) in [
            ("B", Some("B")),
            ("mod.C", Some("mod.C")),
            ("ns::Base", Some("ns::Base")),
            ("pkg.Type", Some("pkg.Type")),
            ("Base<T>", Some("Base")),
            ("Generic[T]", Some("Generic")),
            ("Gen[int]", Some("Gen")),
            ("*Ptr", Some("Ptr")),
            ("*pkg.Type", Some("pkg.Type")),
            ("std::vector<int>", Some("std::vector")),
            ("List<Map<K, V>>", Some("List")),
            ("ns::Tpl<A, B>", Some("ns::Tpl")),
            ("$base", Some("$base")),
            ("Étendue", Some("Étendue")),
            ("mixin(F)", None),
            ("make_base()", None),
            ("a.b()", None),
            ("", None),
            ("<T>", None),
            ("A::", None),
            (".B", None),
        ] {
            assert_eq!(
                super::heritage_name(text, true).as_deref(),
                name,
                "{text:?}"
            );
        }
        // Square brackets are type arguments only in Python and Go (`brackets`):
        // elsewhere they index. A number, an index or a pack is never a name.
        for (text, brackets, name) in [
            ("Gen[int]", true, Some("Gen")),
            ("Gen[int]", false, None),
            ("Generic[T]", false, None),
            ("bases[0]", true, None),
            ("bases[-1]", true, None),
            ("bases[+1]", true, None),
            ("bases[ 0]", true, None),
            ("bases[:2]", true, None),
            ("bases[0]", false, None),
            ("m[\"k\"]", true, None),
            ("123", true, None),
            ("2x", true, None),
            ("pkg.v2", true, Some("pkg.v2")),
            ("Bs...", true, None),
            ("Base<T>", false, Some("Base")),
        ] {
            assert_eq!(
                super::heritage_name(text, brackets).as_deref(),
                name,
                "{text:?} brackets={brackets}"
            );
        }
    }

    #[test]
    fn python_heritage_lists_each_base_and_skips_keyword_arguments() {
        let src = "class B :\n    pass\nclass A(B, mod.C, metaclass=M):\n    pass\nclass F(Generic[T]):\n    pass\nclass Q(make_base()):\n    pass\nclass K():\n    pass\nclass N :\n    pass\nclass Z(*bases):\n    pass\nclass D(123):\n    pass\nclass PN(None):\n    pass\nclass PT(True):\n    pass\nclass S(bases[0]):\n    pass\nclass Outer:\n    class Inner(B):\n        pass\n";
        assert_eq!(
            heritage_rows(Language::Python, src),
            heritage_expected(&[
                ("A", "extends", "B", Some("B")),
                ("A", "extends", "mod.C", Some("mod.C")),
                ("F", "extends", "Generic[T]", Some("Generic")),
                ("Q", "extends", "make_base()", None),
                ("D", "extends", "123", None),
                ("PN", "extends", "None", None),
                ("PT", "extends", "True", None),
                ("S", "extends", "bases[0]", None),
                ("Inner", "extends", "B", Some("B")),
            ])
        );
    }

    #[test]
    fn javascript_heritage_keeps_an_expression_base_without_a_name() {
        let src = "class B {}\nclass D extends B {}\nclass E extends ns.D {}\nclass G extends mixin(B) {}\nclass H {}\nclass K extends /* note */ B {}\nclass M extends bases[0] {}\nclass N extends null {}\nclass U extends undefined {}\n";
        assert_eq!(
            heritage_rows(Language::JavaScript, src),
            heritage_expected(&[
                ("K", "extends", "B", Some("B")),
                ("M", "extends", "bases[0]", None),
                ("N", "extends", "null", None),
                ("U", "extends", "undefined", None),
                ("D", "extends", "B", Some("B")),
                ("E", "extends", "ns.D", Some("ns.D")),
                ("G", "extends", "mixin(B)", None),
            ])
        );
    }

    #[test]
    fn typescript_heritage_covers_extends_implements_and_interfaces() {
        let src = "interface I1 {}\ninterface I2 {}\nclass Base<T> {}\nclass A extends Base<number> implements I1, I2 {}\ninterface J extends I1, I2 {}\nabstract class Abs extends Base<string> {}\nclass Plain {}\n";
        assert_eq!(
            heritage_rows(Language::TypeScript, src),
            heritage_expected(&[
                ("A", "extends", "Base<number>", Some("Base")),
                ("A", "implements", "I1", Some("I1")),
                ("A", "implements", "I2", Some("I2")),
                ("J", "extends", "I1", Some("I1")),
                ("J", "extends", "I2", Some("I2")),
                ("Abs", "extends", "Base<string>", Some("Base")),
            ])
        );
        assert_eq!(
            heritage_rows(Language::TypeScript, "class N extends null {}\n"),
            heritage_expected(&[("N", "extends", "null", None)])
        );
        let tsx = "class B {}\nclass A extends B {}\n";
        assert_eq!(
            heritage_rows(Language::Tsx, tsx),
            heritage_expected(&[("A", "extends", "B", Some("B"))])
        );
    }

    #[test]
    fn java_heritage_covers_classes_interfaces_enums_and_records() {
        let src = "interface I1 {}\ninterface I2 {}\ninterface J extends I1, I2 {}\nclass B {}\nclass A extends B implements I1, I2 {}\nenum E implements I1 { X }\nrecord R(int x) implements I2 {}\nclass G extends B<String> {}\nclass P extends java.util.ArrayList<String> {}\nclass Q extends /* n */ B {}\nclass Plain {}\n";
        assert_eq!(
            heritage_rows(Language::Java, src),
            heritage_expected(&[
                ("Q", "extends", "B", Some("B")),
                ("J", "extends", "I1", Some("I1")),
                ("J", "extends", "I2", Some("I2")),
                ("A", "extends", "B", Some("B")),
                ("A", "implements", "I1", Some("I1")),
                ("A", "implements", "I2", Some("I2")),
                ("E", "implements", "I1", Some("I1")),
                ("R", "implements", "I2", Some("I2")),
                ("G", "extends", "B<String>", Some("B")),
                (
                    "P",
                    "extends",
                    "java.util.ArrayList<String>",
                    Some("java.util.ArrayList")
                ),
            ])
        );
    }

    #[test]
    fn rust_heritage_is_a_trait_impl_and_nothing_else() {
        // An inherent impl and a trait with supertraits give no row.
        let src = "trait Tr {}\ntrait Sup: Tr {}\nstruct Foo;\nstruct Bar;\nimpl std::fmt::Display for Foo {\n    fn fmt(&self) {}\n}\nimpl<T> From<T> for Bar {\n    fn from(t: T) -> Bar { Bar }\n}\nimpl Tr for Vec<u8> {}\nimpl !Send for Foo {}\nimpl Foo {\n    fn inherent() {}\n}\n";
        assert_eq!(
            heritage_rows(Language::Rust, src),
            heritage_expected(&[
                (
                    "Foo",
                    "implements",
                    "std::fmt::Display",
                    Some("std::fmt::Display")
                ),
                ("Bar", "implements", "From<T>", Some("From")),
                ("Vec<u8>", "implements", "Tr", Some("Tr")),
            ])
        );
    }

    #[test]
    fn cpp_heritage_lists_bases_without_access_specifiers() {
        let src = "namespace ns { struct C {}; }\nstruct B {};\nclass D {};\nclass A : public B, private ns::C, public D<int> {};\nstruct S : B {};\nclass V : virtual public B {};\ntemplate <class T> class X : public Base<T> {};\nclass AD : [[deprecated]] public B {};\ntemplate <class... Bs> struct PK : Bs... {};\ntemplate <class... Bs> struct PK2 : public B, Bs... {};\ntemplate <class... Bs> struct PK3 : Bs /* c */ ... {};\nclass Fwd;\nclass Plain {};\n";
        assert_eq!(
            heritage_rows(Language::Cpp, src),
            heritage_expected(&[
                ("AD", "extends", "B", Some("B")),
                // A pack stands for any number of bases: its text and range
                // keep the `...`, and it has no single lookup name.
                ("PK", "extends", "Bs...", None),
                ("PK2", "extends", "B", Some("B")),
                ("PK2", "extends", "Bs...", None),
                ("PK3", "extends", "Bs /* c */ ...", None),
                ("A", "extends", "B", Some("B")),
                ("A", "extends", "ns::C", Some("ns::C")),
                ("A", "extends", "D<int>", Some("D")),
                ("S", "extends", "B", Some("B")),
                ("V", "extends", "B", Some("B")),
                ("X", "extends", "Base<T>", Some("Base")),
            ])
        );
    }

    #[test]
    fn go_heritage_is_an_embedded_struct_field() {
        // A named field is no heritage, interface embedding is not recorded,
        // and a struct nested in a field gives its outer type no row.
        let src = "package p\ntype Base struct{}\ntype Ptr struct{}\ntype Other struct{}\ntype Gen[T any] struct{}\ntype S struct {\n\tBase\n\t*Ptr\n\tpkg.Other\n\tGen[int]\n\tname string\n}\ntype I interface {\n\tBase\n}\ntype A = Base\ntype T struct {\n\tInner struct {\n\t\tBase\n\t}\n}\n";
        assert_eq!(
            heritage_rows(Language::Go, src),
            heritage_expected(&[
                ("S", "embeds", "Base", Some("Base")),
                ("S", "embeds", "*Ptr", Some("Ptr")),
                ("S", "embeds", "pkg.Other", Some("pkg.Other")),
                ("S", "embeds", "Gen[int]", Some("Gen")),
            ])
        );
    }

    #[test]
    fn c_has_no_heritage() {
        let src = "struct B { int x; };\nstruct S { struct B b; };\n";
        assert!(heritage_rows(Language::C, src).is_empty());
    }

    // Containment facts (PR 6): the nearest enclosing container of a stored
    // definition or of a symbol-less container (namespace, impl, module).
    type ContainmentRow = (String, String, String, String);

    /// The containment rows of a source as (child kind, child name, parent
    /// kind, parent name), sorted. Each row is also checked: the child lies
    /// strictly inside the parent; a child or parent that is not symbol-less
    /// has exactly the kind, name and byte range of a stored definition; and
    /// a container used both as a parent and as a child has one identity.
    fn containment_rows(lang: Language, src: &str) -> Vec<ContainmentRow> {
        let facts = parse_source(lang, src.as_bytes());
        let symbol_less = ["namespace", "impl", "module"];
        let stored = |kind: &str, name: &Option<String>, start: usize, end: usize| {
            facts.definitions.iter().any(|d| {
                d.kind == kind && d.name == *name && d.byte_start == start && d.byte_end == end
            })
        };
        let mut identities = std::collections::BTreeMap::new();
        let mut rows = Vec::new();
        for c in &facts.containment {
            let (child, parent) = (
                (c.child_byte_start, c.child_byte_end),
                (c.parent_byte_start, c.parent_byte_end),
            );
            assert!(
                parent.0 <= child.0 && child.1 <= parent.1 && parent != child,
                "{c:?}"
            );
            if !symbol_less.contains(&c.child_kind) {
                assert!(
                    stored(c.child_kind, &c.child_name, child.0, child.1),
                    "{c:?}"
                );
            }
            if !symbol_less.contains(&c.parent_kind) {
                assert!(
                    stored(c.parent_kind, &c.parent_name, parent.0, parent.1),
                    "{c:?}"
                );
            }
            for (range, kind, name) in [
                (child, c.child_kind, &c.child_name),
                (parent, c.parent_kind, &c.parent_name),
            ] {
                let seen = identities.entry(range).or_insert((kind, name.clone()));
                assert_eq!(*seen, (kind, name.clone()), "one identity per range: {c:?}");
            }
            rows.push((
                c.child_kind.to_string(),
                c.child_name.clone().unwrap_or_default(),
                c.parent_kind.to_string(),
                c.parent_name.clone().unwrap_or_default(),
            ));
        }
        rows.sort();
        rows
    }

    fn containment_expected(rows: &[(&str, &str, &str, &str)]) -> Vec<ContainmentRow> {
        let mut out: Vec<ContainmentRow> = rows
            .iter()
            .map(|(a, b, c, d)| (a.to_string(), b.to_string(), c.to_string(), d.to_string()))
            .collect();
        out.sort();
        out
    }

    #[test]
    fn python_containment_is_the_nearest_enclosing_class() {
        // `deep` sits in a method, and a function is no container, so its
        // parent is the class around the method.
        let src = "class Outer:\n    class Inner:\n        def m(self):\n            def deep():\n                pass\n    def f(self):\n        pass\ndef top():\n    pass\n";
        assert_eq!(
            containment_rows(Language::Python, src),
            containment_expected(&[
                ("class", "Inner", "class", "Outer"),
                ("function", "m", "class", "Inner"),
                ("function", "deep", "class", "Inner"),
                ("function", "f", "class", "Outer"),
            ])
        );
    }

    #[test]
    fn javascript_containment_covers_methods_and_bound_functions_of_a_class() {
        let src = "class C {\n  m() {}\n  f = () => {};\n  static s() {}\n}\nfunction top() {}\nconst g = () => {};\nclass D {}\n";
        assert_eq!(
            containment_rows(Language::JavaScript, src),
            containment_expected(&[
                ("method", "m", "class", "C"),
                ("method", "f", "class", "C"),
                ("method", "s", "class", "C"),
            ])
        );
    }

    #[test]
    fn typescript_containment_chains_namespaces_and_keeps_a_dotted_name_whole() {
        let src = "namespace A {\n  namespace B {\n    class C {\n      m() {}\n    }\n  }\n}\nnamespace X.Y {\n  interface I {}\n  enum E { P }\n}\nfunction top() {}\n";
        assert_eq!(
            containment_rows(Language::TypeScript, src),
            containment_expected(&[
                ("namespace", "B", "namespace", "A"),
                ("class", "C", "namespace", "B"),
                ("method", "m", "class", "C"),
                ("interface", "I", "namespace", "X.Y"),
                ("type", "E", "namespace", "X.Y"),
            ])
        );
    }

    #[test]
    fn java_containment_covers_nested_types_constructors_and_enum_methods() {
        let src = "class Outer {\n  class Inner {\n    Inner() {}\n    void m() {}\n  }\n  void f() {}\n  enum E { X; void g() {} }\n}\ninterface I {\n  default void d() {}\n}\n";
        assert_eq!(
            containment_rows(Language::Java, src),
            containment_expected(&[
                ("class", "Inner", "class", "Outer"),
                ("method", "Inner", "class", "Inner"),
                ("method", "m", "class", "Inner"),
                ("method", "f", "class", "Outer"),
                ("type", "E", "class", "Outer"),
                ("method", "g", "type", "E"),
                ("method", "d", "interface", "I"),
            ])
        );
    }

    #[test]
    fn rust_containment_chains_modules_impls_and_traits() {
        let src = "mod outer {\n    mod inner {\n        struct Foo;\n        impl Foo {\n            fn m() {}\n        }\n        trait T {\n            fn sig();\n            fn dflt() {}\n        }\n    }\n    fn free() {}\n}\nfn top() {}\n";
        assert_eq!(
            containment_rows(Language::Rust, src),
            containment_expected(&[
                ("module", "inner", "module", "outer"),
                ("struct", "Foo", "module", "inner"),
                ("impl", "Foo", "module", "inner"),
                ("function", "m", "impl", "Foo"),
                ("interface", "T", "module", "inner"),
                ("function", "sig", "interface", "T"),
                ("function", "dflt", "interface", "T"),
                ("function", "free", "module", "outer"),
            ])
        );
    }

    #[test]
    fn cpp_containment_chains_namespaces_and_keeps_an_out_of_line_name_whole() {
        // `m` has a body, so it is a stored definition (a bare declaration is
        // not). `namespace c::d` is one container; the out-of-line
        // `Widget::run` is parented by its namespace only; the anonymous
        // namespace has no name.
        let src = "namespace a {\n  namespace b {\n    class C {\n      void m() {}\n    };\n  }\n}\nnamespace c::d {\n  struct S {\n    int f;\n  };\n}\nnamespace n {\n  void Widget::run() {}\n}\nnamespace {\n  void anon() {}\n}\nclass Top {};\nvoid free_fn() {}\n";
        assert_eq!(
            containment_rows(Language::Cpp, src),
            containment_expected(&[
                ("namespace", "b", "namespace", "a"),
                ("class", "C", "namespace", "b"),
                ("function", "m", "class", "C"),
                ("struct", "S", "namespace", "c::d"),
                ("field", "f", "struct", "S"),
                ("function", "Widget::run", "namespace", "n"),
                ("function", "anon", "namespace", ""),
            ])
        );
    }

    #[test]
    fn a_cpp_typedef_struct_is_a_parent_by_its_stored_definition_range() {
        // The stored definition of `typedef struct Pt {...} Pt;` covers the
        // whole typedef, so that range, not the struct node's, is the parent.
        let src = "typedef struct Pt {\n  int x;\n} Pt;\n";
        assert_eq!(
            containment_rows(Language::Cpp, src),
            containment_expected(&[("field", "x", "struct", "Pt")])
        );
    }

    #[test]
    fn go_and_c_record_no_containment() {
        let go = "package p\ntype S struct{}\nfunc (s S) M() {}\nfunc F() {}\n";
        assert!(containment_rows(Language::Go, go).is_empty());
        let c = "struct S { int f; };\nint g(void) { return 0; }\n";
        assert!(containment_rows(Language::C, c).is_empty());
    }

    #[test]
    fn a_macro_headed_cpp_class_is_the_parent_of_its_member_functions() {
        // `class EXPORT W {...}` is stored as a class through a function
        // definition node, and its member functions with bodies are stored as
        // plain functions: the stored class is their nearest container.
        let src = "class EXPORT W {\n  void run() {}\n};\n";
        assert_eq!(
            containment_rows(Language::Cpp, src),
            containment_expected(&[("function", "run", "class", "W")])
        );
    }

    // Doc comments (PR 6): the comment block above a definition, and a Python
    // docstring. Every expectation is written from the fixture source.
    type DocRow = (String, &'static str, String);

    /// The doc rows of a source as (definition name, form, text), sorted. Each
    /// row is also checked: it belongs to a stored definition (by byte range),
    /// a definition has at most one row per form, the block bytes are the
    /// source under the recorded range, and the text is that block, cut only
    /// when it is longer than the cap.
    fn doc_rows(lang: Language, src: &str) -> Vec<DocRow> {
        let facts = parse_source(lang, src.as_bytes());
        let mut seen = std::collections::BTreeSet::new();
        let mut rows = Vec::new();
        for d in &facts.doc_comments {
            let def = facts
                .definitions
                .iter()
                .find(|f| f.byte_start == d.def_byte_start && f.byte_end == d.def_byte_end)
                .unwrap_or_else(|| panic!("no stored definition for {d:?}"));
            assert!(
                seen.insert((d.def_byte_start, d.def_byte_end, d.form)),
                "two {} rows for one definition: {d:?}",
                d.form
            );
            let block = &src[d.byte_start..d.byte_end];
            if d.truncated {
                assert!(block.len() > super::DOC_TEXT_CAP && block.starts_with(&d.text));
                assert!(d.text.len() <= super::DOC_TEXT_CAP && d.text.len() < block.len());
            } else {
                assert_eq!(block, d.text, "{d:?}");
            }
            rows.push((def.name.clone().unwrap_or_default(), d.form, d.text.clone()));
        }
        rows.sort();
        rows
    }

    fn doc_expected(rows: &[(&str, &'static str, &str)]) -> Vec<DocRow> {
        let mut out: Vec<DocRow> = rows
            .iter()
            .map(|(a, b, c)| (a.to_string(), *b, c.to_string()))
            .collect();
        out.sort();
        out
    }

    const JS_DOCS: &str = "// dangling header\n\n/** Adds. */\n/** second line of block. */\nfunction add() {}\n\n// detached by a blank line\n\nfunction detached() {}\nrun(); // explains run\nfunction afterTrailing() {}\nconst x = 1; /* trailing block\n  ends above */\nfunction afterBlock() {}\n// before export\nexport function exported() {}\n// above const\nexport const bound = () => {};\nfunction sameLine() {} // after\n/* before, same line */ function sameLine2() {}\nclass K {\n  // member doc\n  m() {}\n}\n";

    #[test]
    fn javascript_doc_comments_attach_only_as_a_contiguous_block_on_its_own_lines() {
        assert_eq!(
            doc_rows(Language::JavaScript, JS_DOCS),
            doc_expected(&[
                ("add", "above", "/** Adds. */\n/** second line of block. */"),
                ("exported", "above", "// before export"),
                ("bound", "above", "// above const"),
                ("m", "above", "// member doc"),
            ])
        );
    }

    #[test]
    fn crlf_line_endings_attach_the_same_comments() {
        let crlf = JS_DOCS.replace('\n', "\r\n");
        let names = |src: &str| -> Vec<(String, &'static str)> {
            doc_rows(Language::JavaScript, src)
                .into_iter()
                .map(|(name, form, _)| (name, form))
                .collect()
        };
        assert_eq!(names(&crlf), names(JS_DOCS));
        assert_eq!(names(&crlf).len(), 4);
    }

    #[test]
    fn python_doc_comments_are_a_hash_block_and_a_docstring() {
        let src = "# header\n\n# a: first\n# a: second\ndef a():\n    \"\"\"Docstring of a.\"\"\"\n\n# only above\ndef b():\n    pass\n\nx = 1  # note\ndef c():\n    pass\n\n# deco doc\n@decorator\ndef d():\n    pass\n\n# both\nclass E :\n    \"\"\"Class doc.\"\"\"\n    def m(self):\n        '''method doc'''\n    def n(self):\n        # inside, not a doc\n        pass\n";
        assert_eq!(
            doc_rows(Language::Python, src),
            doc_expected(&[
                ("a", "above", "# a: first\n# a: second"),
                ("a", "docstring", "\"\"\"Docstring of a.\"\"\""),
                ("b", "above", "# only above"),
                ("d", "above", "# deco doc"),
                ("E", "above", "# both"),
                ("E", "docstring", "\"\"\"Class doc.\"\"\""),
                ("m", "docstring", "'''method doc'''"),
            ])
        );
    }

    #[test]
    fn a_python_docstring_may_follow_a_comment_and_a_non_string_first_statement_has_none() {
        // Comments are no statements, so the string is still the first one.
        let src = "def g():\n    # note\n    \"\"\"after a comment\"\"\"\n    pass\ndef h():\n    x = 1\n    \"\"\"not first\"\"\"\ndef i():\n    f\"\"\"{x}\"\"\"\n";
        assert_eq!(
            doc_rows(Language::Python, src),
            doc_expected(&[("g", "docstring", "\"\"\"after a comment\"\"\"")])
        );
    }

    #[test]
    fn typescript_doc_comments_pass_through_export_and_find_class_members() {
        let src = "/** I doc. */\nexport interface I {}\n/** Abs doc. */\nexport abstract class Abs {\n  /** field doc */\n  public f = () => {};\n}\n";
        assert_eq!(
            doc_rows(Language::TypeScript, src),
            doc_expected(&[
                ("I", "above", "/** I doc. */"),
                ("Abs", "above", "/** Abs doc. */"),
                ("f", "above", "/** field doc */"),
            ])
        );
    }

    #[test]
    fn rust_doc_comments_pass_over_every_attribute() {
        let src = "/// Doc of s.\n#[derive(Debug)]\n#[allow(dead_code)]\nstruct S;\n\n/// Detached doc.\n\n#[inline]\nfn detached() {}\n\n// plain comment\nfn plain() {}\n\n/** block doc */\npub fn blk() {}\n\nfn after() {} // trailing\nfn next() {}\n";
        assert_eq!(
            doc_rows(Language::Rust, src),
            doc_expected(&[
                ("S", "above", "/// Doc of s."),
                ("plain", "above", "// plain comment"),
                ("blk", "above", "/** block doc */"),
            ])
        );
    }

    #[test]
    fn cpp_doc_comments_pass_through_template_and_typedef() {
        let src = "// header\n\n/// Doc of Tpl.\ntemplate <class T>\nclass Tpl {};\n\n// doc of S\nstruct S {\n  // field f doc\n  int f;\n  int g; // trailing on g\n  int h;\n};\n// for typedef\ntypedef struct Pt { int x; } Pt;\nint k();\nvoid run() {} // trailing\nvoid next() {}\n";
        assert_eq!(
            doc_rows(Language::Cpp, src),
            doc_expected(&[
                ("Tpl", "above", "/// Doc of Tpl."),
                ("S", "above", "// doc of S"),
                ("f", "above", "// field f doc"),
                ("Pt", "above", "// for typedef"),
            ])
        );
    }

    #[test]
    fn c_doc_comments_attach_like_cpp_ones() {
        let src = "// c doc\nint f(void) { return 0; }\nint g(void) { return 1; } // trailing\nint h(void) { return 2; }\n";
        assert_eq!(
            doc_rows(Language::C, src),
            doc_expected(&[("f", "above", "// c doc")])
        );
    }

    #[test]
    fn java_doc_comments_sit_above_the_annotation_that_is_inside_the_declaration() {
        let src = "class A {\n    /** Javadoc of m. */\n    @Override\n    public void m() {}\n\n    // line doc\n    void n() {}\n}\n/** Class doc. */\n@Deprecated\nclass B {}\n";
        assert_eq!(
            doc_rows(Language::Java, src),
            doc_expected(&[
                ("m", "above", "/** Javadoc of m. */"),
                ("n", "above", "// line doc"),
                ("B", "above", "/** Class doc. */"),
            ])
        );
    }

    #[test]
    fn go_doc_comments_sit_above_the_type_keyword_or_the_spec_in_a_group() {
        // An ungrouped `type S ...` is wrapped by its `type` keyword on the same
        // line; inside `type ( ... )` the comment sits directly above the spec.
        let src = "package p\n\n// S is a type.\ntype S struct{}\n\n// F does f.\nfunc F() {}\n\n// Grouped.\ntype (\n\t// A doc.\n\tA int\n\t// B doc.\n\tB int\n)\n";
        assert_eq!(
            doc_rows(Language::Go, src),
            doc_expected(&[
                ("S", "above", "// S is a type."),
                ("F", "above", "// F does f."),
                ("A", "above", "// A doc."),
                ("B", "above", "// B doc."),
            ])
        );
    }

    #[test]
    fn a_doc_text_is_cut_at_4096_bytes_on_a_character_boundary() {
        let doc = |comment: &str| {
            let src = format!("{comment}\nfunction big() {{}}\n");
            let facts = parse_source(Language::JavaScript, src.as_bytes());
            assert_eq!(facts.doc_comments.len(), 1, "{}", comment.len());
            let d = facts.doc_comments[0].clone();
            (d.text, d.truncated, comment.to_string())
        };
        // Exactly the cap: whole, not truncated.
        let exact = format!("// {}", "a".repeat(4093));
        let (text, truncated, _) = doc(&exact);
        assert_eq!((text.len(), truncated), (4096, false));
        // One byte over: cut to the cap, flagged.
        let over = format!("// {}", "a".repeat(5000));
        let (text, truncated, comment) = doc(&over);
        assert_eq!((text.len(), truncated), (4096, true));
        assert_eq!(text, comment[..4096]);
        // A three-byte character straddles byte 4096 (it spans 4094..4097):
        // the cut falls before it.
        let straddle = format!("// {}€€ end", "a".repeat(4091));
        let (text, truncated, comment) = doc(&straddle);
        assert_eq!((text.len(), truncated), (4094, true));
        assert_eq!(text, comment[..4094]);
    }
}
