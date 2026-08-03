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
    if !parsed.diagnostics.is_empty() {
        let errors: Vec<String> = parsed.diagnostics.iter().map(|e| e.to_string()).collect();
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
    if !parsed.diagnostics.is_empty() {
        let errors: Vec<String> = parsed.diagnostics.iter().map(|e| e.to_string()).collect();
        anyhow::bail!("Parse errors in {filename}: {}", errors.join("; "));
    }

    // (start, end, replacement) byte-span edits into `source`.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    for stmt in &parsed.program.body {
        match stmt {
            Statement::ImportDeclaration(d) => {
                edits.push((d.span.start as usize, d.span.end as usize, lower_import(d)));
            }
            Statement::ExportDeclaration(d) => {
                // `export const|let|var|function|class ...` -> drop just `export `.
                edits.push((
                    d.span.start as usize,
                    d.declaration.span().start as usize,
                    String::new(),
                ));
            }
            Statement::ExportFromDeclaration(d) => {
                // `export { ... } from "m"` re-export -> drop.
                edits.push((d.span.start as usize, d.span.end as usize, String::new()));
            }
            Statement::ExportNamedDeclaration(d) => {
                if d.export_kind.is_type() {
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
#[path = "../../tests/unit/plugins/transpile.rs"]
mod tests;
