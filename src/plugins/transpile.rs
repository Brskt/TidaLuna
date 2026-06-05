use std::path::Path;

use oxc::ast::ast::*;
use oxc::span::GetSpan;

/// Transpile TypeScript plugin source to IIFE-ready JavaScript.
///
/// Two passes:
///   1. [`lower_es_modules`] parses the source and rewrites top-level ESM
///      module declarations (`import`/`export`) into script-compatible
///      statements, since the result is concatenated inside an async IIFE
///      where module syntax is illegal.
///   2. The ESM-free source is parsed again and run through the oxc TypeScript
///      transform (strip types) and codegen.
///
/// The lowering is driven entirely by the parsed AST: it only edits real
/// `ModuleDeclaration` nodes, addressed by their byte spans. Because oxc spans
/// always fall on UTF-8 char boundaries and the parser has already classified
/// strings/comments/regex, this never corrupts multi-byte characters and never
/// mistakes a keyword inside a string literal for real module syntax - both of
/// which the previous hand-rolled byte scanner got wrong.
pub fn transpile_ts(source: &str, filename: &str) -> anyhow::Result<String> {
    let stripped = lower_es_modules(source, filename)?;

    let allocator = oxc::allocator::Allocator::default();
    let source_type = oxc::span::SourceType::from_path(Path::new(filename))
        .map_err(|_| anyhow::anyhow!("Could not determine source type for {filename}"))?;

    let parsed = oxc::parser::Parser::new(&allocator, &stripped, source_type).parse();
    if parsed.panicked {
        anyhow::bail!("Parser panicked for {filename}");
    }
    if !parsed.errors.is_empty() {
        let errors: Vec<String> = parsed.errors.iter().map(|e| e.to_string()).collect();
        anyhow::bail!("Parse errors in {filename}: {}", errors.join("; "));
    }

    let mut program = parsed.program;

    let scoping = oxc::semantic::SemanticBuilder::new()
        .build(&program)
        .semantic
        .into_scoping();

    let options = oxc::transformer::TransformOptions::default();
    oxc::transformer::Transformer::new(&allocator, Path::new(filename), &options)
        .build_with_scoping(scoping, &mut program);

    Ok(oxc::codegen::Codegen::new().build(&program).code)
}

/// Rewrite top-level ESM module declarations into script statements, returning
/// the rewritten source. See [`transpile_ts`] for the contract:
///   - `export { a as X }`            -> `var __exports = { X: a };`
///   - `export const|fn|class ...`      -> strip the `export` keyword
///   - `export default <expr>` / anon fn|class -> `var __default = <expr>;`
///   - `export default fn|class` (named)        -> bare declaration
///   - `export ... from "m"` / `export *`-> dropped (re-exports, unused in IIFE)
///   - `import { a, b as c } from "m"` -> `const { a, b: c } = luna?.core?.modules?.["m"];`
///   - `import * as ns from "m"`       -> `const ns = luna?.core?.modules?.["m"];`
///   - `import d from "m"`             -> `const d = luna?.core?.modules?.["m"]?.default;`
///   - `import "m"` / `import type ...`  -> dropped
fn lower_es_modules(source: &str, filename: &str) -> anyhow::Result<String> {
    let allocator = oxc::allocator::Allocator::default();
    let source_type = oxc::span::SourceType::from_path(Path::new(filename))
        .map_err(|_| anyhow::anyhow!("Could not determine source type for {filename}"))?;

    let parsed = oxc::parser::Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        anyhow::bail!("Parser panicked for {filename}");
    }
    if !parsed.errors.is_empty() {
        let errors: Vec<String> = parsed.errors.iter().map(|e| e.to_string()).collect();
        anyhow::bail!("Parse errors in {filename}: {}", errors.join("; "));
    }

    // (start, end, replacement) byte-span edits into `source`.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ImportDeclaration(d) => {
                edits.push((d.span.start as usize, d.span.end as usize, lower_import(d)));
            }
            Statement::ExportNamedDeclaration(d) => {
                if let Some(decl) = &d.declaration {
                    // `export const|let|var|function|class ...` -> drop just `export `.
                    edits.push((
                        d.span.start as usize,
                        decl.span().start as usize,
                        String::new(),
                    ));
                } else if d.source.is_some() {
                    // `export { ... } from "m"` re-export -> drop.
                    edits.push((d.span.start as usize, d.span.end as usize, String::new()));
                } else if d.export_kind.is_type() {
                    // `export type { Foo }` - type-only, stripped by the TS
                    // transform; drop it so we don't emit a dangling reference.
                    edits.push((d.span.start as usize, d.span.end as usize, String::new()));
                } else {
                    // `export { a as X }` -> `var __exports = { X: a };`
                    edits.push((
                        d.span.start as usize,
                        d.span.end as usize,
                        build_exports_object(&d.specifiers),
                    ));
                }
            }
            Statement::ExportDefaultDeclaration(d) => {
                let decl_start = d.declaration.span().start as usize;
                let keep_declaration = match &d.declaration {
                    // Keep only *named* fn/class as a bare declaration; an anonymous
                    // one (no `id`) is a value, not a valid bare statement.
                    ExportDefaultDeclarationKind::FunctionDeclaration(f) => f.id.is_some(),
                    ExportDefaultDeclarationKind::ClassDeclaration(c) => c.id.is_some(),
                    // Type-only; erased by the TS transform. Must stay a bare strip -
                    // `var __default = interface ...` would be invalid JS.
                    ExportDefaultDeclarationKind::TSInterfaceDeclaration(_) => true,
                    _ => false,
                };
                let replacement = if keep_declaration {
                    // Named fn/class: keep the declaration, drop `export default `.
                    String::new()
                } else {
                    // Expression default (incl. anonymous fn/class) -> `var __default = <expr>`.
                    "var __default = ".to_string()
                };
                edits.push((d.span.start as usize, decl_start, replacement));
            }
            Statement::ExportAllDeclaration(d) => {
                edits.push((d.span.start as usize, d.span.end as usize, String::new()));
            }
            _ => {}
        }
    }

    edits.sort_by_key(|(start, _, _)| *start);
    let mut out = String::with_capacity(source.len());
    let mut cursor = 0usize;
    for (start, end, repl) in edits {
        if start < cursor {
            // Overlapping/nested module decl (shouldn't happen at top level); skip.
            continue;
        }
        out.push_str(&source[cursor..start]);
        out.push_str(&repl);
        cursor = end;
    }
    out.push_str(&source[cursor..]);
    Ok(out)
}

/// `import ...` -> the `luna?.core?.modules?.["m"]` binding(s), or empty for
/// side-effect and type-only imports.
fn lower_import(decl: &ImportDeclaration) -> String {
    if matches!(decl.import_kind, ImportOrExportKind::Type) {
        return String::new();
    }
    let Some(specifiers) = &decl.specifiers else {
        return String::new(); // side-effect import
    };

    let module =
        serde_json::to_string(decl.source.value.as_str()).unwrap_or_else(|_| "\"\"".to_string());
    let modules_ref = format!("luna?.core?.modules?.[{module}]");

    let mut statements: Vec<String> = Vec::new();
    let mut named: Vec<String> = Vec::new();

    for spec in specifiers {
        match spec {
            ImportDeclarationSpecifier::ImportSpecifier(s) => {
                if matches!(s.import_kind, ImportOrExportKind::Type) {
                    continue;
                }
                let imported = module_export_name(&s.imported);
                let local = s.local.name.as_str();
                if imported == local {
                    named.push(local.to_string());
                } else {
                    named.push(format!("{imported}: {local}"));
                }
            }
            ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                statements.push(format!("const {} = {modules_ref}?.default;", s.local.name));
            }
            ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                statements.push(format!("const {} = {modules_ref};", s.local.name));
            }
        }
    }

    if !named.is_empty() {
        statements.push(format!("const {{ {} }} = {modules_ref};", named.join(", ")));
    }

    statements.join(" ")
}

/// `export { a as X, type T, b }` -> `var __exports = { X: a, b: b };`
///
/// Type-only specifiers (`export { type T }`) are dropped: the TS transform
/// removes their binding, so emitting them would throw `ReferenceError`.
fn build_exports_object(specifiers: &[ExportSpecifier]) -> String {
    let entries: Vec<String> = specifiers
        .iter()
        .filter(|s| !s.export_kind.is_type())
        .map(|s| {
            let exported = module_export_name(&s.exported);
            let local = module_export_name(&s.local);
            format!("{exported}: {local}")
        })
        .collect();
    format!("var __exports = {{ {} }};", entries.join(", "))
}

pub(crate) fn module_export_name(name: &ModuleExportName) -> String {
    match name {
        ModuleExportName::IdentifierName(i) => i.name.to_string(),
        ModuleExportName::IdentifierReference(i) => i.name.to_string(),
        ModuleExportName::StringLiteral(s) => s.value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lower then transpile, asserting the result is ESM-free and parses.
    fn lower(code: &str) -> String {
        transpile_ts(code, "plugin.ts").expect("transpile")
    }

    #[test]
    fn export_block_becomes_exports_object() {
        let js = lower("var x=1;var y=2;export{x as Settings,y as unloads};");
        assert!(js.contains("__exports"));
        assert!(js.contains("Settings"));
        assert!(js.contains("unloads"));
        // `__exports` legitimately contains the substring "export"; ensure no
        // real `export` statement survives once that token is removed.
        assert!(!js.replace("__exports", "").contains("export"));
    }

    #[test]
    fn export_const_strips_keyword() {
        let js = lower("export const foo = 42;");
        assert!(js.contains("const foo = 42"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn export_function_strips_keyword() {
        let js = lower("export function hello() { return 1; }");
        assert!(js.contains("function hello"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn export_class_strips_keyword() {
        let js = lower("export class Foo {}");
        assert!(js.contains("class Foo"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn export_default_expr_becomes_default_var() {
        let js = lower("export default 42;");
        assert!(js.contains("__default"));
        assert!(js.contains("42"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn export_default_class_keeps_declaration() {
        let js = lower("export default class Foo {}");
        assert!(js.contains("class Foo"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn export_default_anonymous_function_becomes_default_var() {
        // Anon default must lower to `var __default = ...`, not a bare invalid decl.
        let js = lower("export default function() { return 1; }");
        assert!(js.contains("__default"));
        assert!(js.contains("function"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn export_default_anonymous_class_becomes_default_var() {
        let js = lower("export default class { method() {} }");
        assert!(js.contains("__default"));
        assert!(js.contains("class"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn export_default_named_function_keeps_declaration() {
        // A named default fn stays a hoisted declaration (`id` is present).
        let js = lower("export default function Foo() { return 1; }");
        assert!(js.contains("function Foo"));
        assert!(!js.contains("export"));
    }

    #[test]
    fn import_named_maps_to_modules() {
        let js = lower(r#"import { storage, intercept } from "@luna/lib";console.log(1);"#);
        assert!(js.contains(r#"luna?.core?.modules?.["@luna/lib"]"#));
        assert!(js.contains("storage"));
        assert!(js.contains("intercept"));
        assert!(js.contains("console.log(1)"));
        assert!(!js.contains("import"));
    }

    #[test]
    fn import_named_alias_uses_colon_not_as() {
        let js = lower(r#"import { a as b } from "m";b();"#);
        // Valid destructuring renames with `:`, never the invalid `{ a as b }`.
        assert!(js.contains("a: b"));
        assert!(!js.contains(" as "));
        assert!(!js.contains("import"));
    }

    #[test]
    fn import_namespace_maps_to_modules() {
        let js = lower(r#"import * as core from "@luna/core";foo();"#);
        assert!(js.contains(r#"const core = luna?.core?.modules?.["@luna/core"]"#));
        assert!(!js.contains("import"));
    }

    #[test]
    fn import_default_maps_to_modules_default() {
        let js = lower(r#"import React from "react";foo();"#);
        assert!(js.contains(r#"luna?.core?.modules?.["react"]?.default"#));
        assert!(!js.contains("import"));
    }

    #[test]
    fn import_side_effect_is_dropped() {
        let js = lower(r#"import "./polyfill";foo();"#);
        assert!(!js.contains("import"));
        assert!(js.contains("foo()"));
    }

    #[test]
    fn export_star_from_is_dropped() {
        let js = lower(r#"export * from "@luna/core";var z = 1;"#);
        assert!(!js.contains("export"));
        assert!(js.contains("var z = 1") || js.contains("z = 1"));
    }

    #[test]
    fn export_type_block_is_dropped() {
        let js = lower("type T = number; export type { T };");
        assert!(!js.contains("__exports"));
        assert!(!js.replace("__exports", "").contains("export"));
    }

    #[test]
    fn export_inline_type_specifier_is_dropped() {
        let js = lower("var x = 1; type T = number; export { x as X, type T };");
        assert!(js.contains("X: x"));
        assert!(!js.contains("T:"));
    }

    #[test]
    fn export_named_from_is_dropped() {
        let js = lower(r#"export { foo, bar } from "@luna/lib";var z = 1;"#);
        assert!(!js.contains("export"));
    }

    #[test]
    fn multibyte_utf8_survives() {
        // The old byte scanner corrupted multi-byte chars via `bytes[i] as char`.
        let js = lower(r#"const s = "héllo wörld 日本語 🎵"; export { s as Settings };"#);
        assert!(js.contains("héllo wörld 日本語 🎵"));
        assert!(js.contains("__exports"));
    }

    #[test]
    fn export_keyword_inside_string_is_untouched() {
        // Only real module declarations are lowered, not text inside literals.
        let js = lower(r#"var s = "export{foo}"; export { s as Settings };"#);
        assert!(js.contains(r#""export{foo}""#));
        assert!(js.contains("__exports"));
    }

    #[test]
    fn strips_ts_types() {
        let ts = r#"
            const x: number = 42;
            function greet(name: string): string { return `Hello, ${name}!`; }
            export { x, greet };
        "#;
        let js = lower(ts);
        assert!(!js.contains(": number"));
        assert!(!js.contains(": string"));
        assert!(js.contains("const x = 42"));
        assert!(js.contains("function greet"));
    }

    #[test]
    fn mts_extension_is_supported() {
        let js = transpile_ts("export const x: number = 1;", "plugin.mts").expect("transpile");
        assert!(js.contains("const x = 1"));
        assert!(!js.contains("export"));
    }
}
