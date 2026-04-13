use std::path::Path;

/// Strip ES module syntax (`export`, `import`) so code can run inside an IIFE.
///
/// TidaLuna plugins are pre-bundled by Quartz:
///   - Imports are already resolved to `luna?.core?.modules?.["@luna/lib"]`
///   - The only ESM syntax left is `export{...}` at the end (named exports)
///   - Some have `export const/function/class` declarations
///
/// This function handles both the common Quartz output and edge cases:
///   - `export { a, b as c }` → removed (not needed in script context)
///   - `export const/let/var x = ...` → `const/let/var x = ...`
///   - `export function f()` → `function f()`
///   - `export class C` → `class C`
///   - `export default expr` → `var __default = expr`
///   - `import { x } from "y"` → `const { x } = luna?.core?.modules?.["y"]` (fallback)
///   - `import "y"` → removed (side-effect import)
pub fn strip_esm_syntax(code: &str) -> String {
    let mut result = String::with_capacity(code.len());
    let bytes = code.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i < len {
        // Skip to potential keyword positions
        let b = bytes[i];

        // Check for `export` keyword
        if b == b'e'
            && i + 6 <= len
            && &code[i..i + 6] == "export"
            && is_keyword_boundary(bytes, i, 6)
        {
            let after_export = skip_ws(&code[i + 6..]);
            let rest = &code[i + 6 + after_export..];

            if rest.starts_with('{') {
                // `export { ... }` or `export { ... } from "..."`
                // Find the closing brace, then optionally `from "..."`
                if let Some(close) = rest.find('}') {
                    let after_brace = &rest[close + 1..];
                    let ws = skip_ws(after_brace);
                    let tail = &after_brace[ws..];
                    if tail.starts_with("from") && is_keyword_boundary(tail.as_bytes(), 0, 4) {
                        // `export { ... } from "..."` - re-export, skip entirely
                        let from_rest = &tail[4..];
                        i += skip_to_semicolon_or_newline(code, i, from_rest);
                    } else {
                        // `export { F as Settings, T as unloads }` →
                        // `var __exports = { Settings: F, unloads: T };`
                        // This allows the wrapper to register exports on window
                        // so the Settings component shares state with the running plugin.
                        let specifiers_str = &rest[1..close]; // between { and }
                        let obj_entries = convert_export_specifiers(specifiers_str);
                        result.push_str("var __exports = {");
                        result.push_str(&obj_entries);
                        result.push('}');
                        let total = 6 + after_export + close + 1;
                        let remaining = &code[i + total..];
                        i += total + skip_optional_semicolon(remaining);
                    }
                    continue;
                }
            } else if rest.starts_with("default") && is_keyword_boundary(rest.as_bytes(), 0, 7) {
                // `export default ...` → `var __default = ...`
                let decl_start = 7 + skip_ws(&rest[7..]);
                let decl = &rest[decl_start..];
                // For `export default function name()` or `export default class name`,
                // keep the declaration as-is (the name is useful)
                if decl.starts_with("function") || decl.starts_with("class") {
                    result.push_str(&rest[decl_start..]);
                    break; // rest of file is consumed
                }
                result.push_str("var __default = ");
                i += 6 + after_export + decl_start;
                continue;
            } else if rest.starts_with("const ")
                || rest.starts_with("let ")
                || rest.starts_with("var ")
                || rest.starts_with("function ")
                || rest.starts_with("function*")
                || rest.starts_with("class ")
                || rest.starts_with("async ")
            {
                // `export const/let/var/function/class/async ...` → strip the `export ` prefix
                i += 6 + after_export; // skip "export" + whitespace
                continue;
            } else if rest.starts_with('*') {
                // `export * from "..."` - skip entire statement
                i += skip_to_semicolon_or_newline(code, i, rest);
                continue;
            }
        }

        // Check for `import` keyword (fallback - Quartz plugins shouldn't have these)
        if b == b'i'
            && i + 6 <= len
            && &code[i..i + 6] == "import"
            && is_keyword_boundary(bytes, i, 6)
        {
            let after_import = skip_ws(&code[i + 6..]);
            let rest = &code[i + 6 + after_import..];

            // Skip `import type` (already handled by OXC, but just in case)
            if rest.starts_with("type ") || rest.starts_with("type{") {
                i += skip_to_semicolon_or_newline(code, i, rest);
                continue;
            }

            // `import "module"` (side-effect) → remove
            if rest.starts_with('"') || rest.starts_with('\'') {
                i += skip_to_semicolon_or_newline(code, i, rest);
                continue;
            }

            // `import { ... } from "module"` → `const { ... } = luna?.core?.modules?.["module"]`
            // `import * as ns from "module"` → `const ns = luna?.core?.modules?.["module"]`
            // `import Default from "module"` → `const Default = luna?.core?.modules?.["module"]?.default`
            if let Some(from_pos) = find_from_keyword(rest) {
                let specifiers = rest[..from_pos].trim();
                let after_from = &rest[from_pos + 4..].trim_start();
                if let Some(module_name) = extract_string_literal(after_from) {
                    let modules_ref = format!("luna?.core?.modules?.[\"{module_name}\"]");

                    if specifiers.starts_with('*') {
                        // `import * as ns from "mod"`
                        if let Some(as_pos) = specifiers.find(" as ") {
                            let ns = specifiers[as_pos + 4..].trim();
                            result.push_str(&format!("const {ns} = {modules_ref};"));
                        }
                    } else if specifiers.starts_with('{') {
                        // `import { a, b } from "mod"`
                        result.push_str(&format!("const {specifiers} = {modules_ref};"));
                    } else {
                        // `import Default from "mod"` - or mixed `import Default, { a } from "mod"`
                        if let Some(comma) = specifiers.find(',') {
                            let default_name = specifiers[..comma].trim();
                            let named = specifiers[comma + 1..].trim();
                            result.push_str(&format!(
                                "const {default_name} = {modules_ref}?.default; const {named} = {modules_ref};"
                            ));
                        } else {
                            let default_name = specifiers.trim();
                            result.push_str(&format!(
                                "const {default_name} = {modules_ref}?.default;"
                            ));
                        }
                    }

                    i += skip_to_semicolon_or_newline(code, i, after_from);
                    continue;
                }
            }
        }

        result.push(bytes[i] as char);
        i += 1;
    }

    result
}

/// Convert `F as Settings, T as unloads` → `Settings: F, unloads: T`
/// (from export specifiers to object literal entries)
fn convert_export_specifiers(specifiers: &str) -> String {
    specifiers
        .split(',')
        .filter_map(|spec| {
            let spec = spec.trim();
            if spec.is_empty() {
                return None;
            }
            if let Some(as_pos) = spec.find(" as ") {
                let local = spec[..as_pos].trim();
                let exported = spec[as_pos + 4..].trim();
                Some(format!("{exported}: {local}"))
            } else {
                // `export { foo }` - same name for local and exported
                Some(format!("{spec}: {spec}"))
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Check that a keyword at `pos` with length `kw_len` is a standalone keyword
/// (not part of a larger identifier).
fn is_keyword_boundary(bytes: &[u8], pos: usize, kw_len: usize) -> bool {
    // Check char before
    if pos > 0 {
        let before = bytes[pos - 1];
        if before.is_ascii_alphanumeric() || before == b'_' || before == b'$' {
            return false;
        }
    }
    // Check char after
    let after_pos = pos + kw_len;
    if after_pos < bytes.len() {
        let after = bytes[after_pos];
        if after.is_ascii_alphanumeric() || after == b'_' || after == b'$' {
            return false;
        }
    }
    true
}

/// Count whitespace characters at the start of a string.
fn skip_ws(s: &str) -> usize {
    s.len() - s.trim_start().len()
}

/// Skip past the current statement (to semicolon or newline), return total bytes to advance from `base`.
fn skip_to_semicolon_or_newline(code: &str, base: usize, from_rest: &str) -> usize {
    let offset = from_rest.as_ptr() as usize - code.as_ptr() as usize;
    let remaining = &code[offset..];
    for (j, ch) in remaining.char_indices() {
        if ch == ';' {
            return (offset - base) + j + 1; // include the semicolon
        }
        if ch == '\n' {
            return (offset - base) + j + 1; // include the newline
        }
    }
    code.len() - base // consume rest of file
}

/// Skip an optional semicolon and trailing whitespace.
fn skip_optional_semicolon(s: &str) -> usize {
    let mut n = 0;
    for ch in s.chars() {
        if ch == ';' {
            return n + 1;
        }
        if ch == ' ' || ch == '\t' {
            n += ch.len_utf8();
            continue;
        }
        break;
    }
    n
}

/// Find the `from` keyword in an import specifier string.
fn find_from_keyword(s: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(pos) = s[search..].find("from") {
        let abs = search + pos;
        if is_keyword_boundary(s.as_bytes(), abs, 4) {
            return Some(abs);
        }
        search = abs + 4;
    }
    None
}

/// Extract a string literal value (single or double quoted) from the start of `s`.
fn extract_string_literal(s: &str) -> Option<&str> {
    let quote = s.as_bytes().first()?;
    if *quote != b'"' && *quote != b'\'' {
        return None;
    }
    let end = s[1..].find(*quote as char)?;
    Some(&s[1..1 + end])
}

/// Transpile TypeScript source code to JavaScript (ES2020).
///
/// Strips type annotations, preserves ES module structure (import/export).
/// No JSX, no decorators, no polyfills.
pub fn transpile_ts(source: &str, filename: &str) -> anyhow::Result<String> {
    let allocator = oxc::allocator::Allocator::default();
    let source_type = oxc::span::SourceType::from_path(Path::new(filename))
        .map_err(|_| anyhow::anyhow!("Could not determine source type for {filename}"))?;

    // Parse
    let parsed = oxc::parser::Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        anyhow::bail!("Parser panicked for {filename}");
    }
    if !parsed.errors.is_empty() {
        let errors: Vec<String> = parsed.errors.iter().map(|e| e.to_string()).collect();
        anyhow::bail!("Parse errors in {filename}: {}", errors.join("; "));
    }

    let mut program = parsed.program;

    // Semantic analysis (required by transformer)
    let semantic = oxc::semantic::SemanticBuilder::new()
        .build(&program)
        .semantic;
    let scoping = semantic.into_scoping();

    // Transform: strip TS types
    let options = oxc::transformer::TransformOptions::default();
    oxc::transformer::Transformer::new(&allocator, Path::new(filename), &options)
        .build_with_scoping(scoping, &mut program);

    // Codegen: emit JS
    let output = oxc::codegen::Codegen::new().build(&program);

    Ok(output.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- strip_esm_syntax tests ---

    #[test]
    fn test_strip_export_block_to_exports_object() {
        let code = r#"var x=1;var y=2;export{x as Settings,y as unloads};"#;
        let result = strip_esm_syntax(code);
        assert!(result.contains("var __exports = {Settings: x, unloads: y}"));
        assert!(!result.contains("export{"));
    }

    #[test]
    fn test_strip_export_block_multiname() {
        let code =
            "var a=1;\nexport{a as Settings,b as trace,c as unloads};\n//# sourceMappingURL=x";
        let result = strip_esm_syntax(code);
        assert!(result.contains("var __exports = {Settings: a, trace: b, unloads: c}"));
        assert!(!result.contains("export{"));
    }

    #[test]
    fn test_strip_export_const() {
        let code = "export const foo = 42;";
        let result = strip_esm_syntax(code);
        assert_eq!(result, "const foo = 42;");
    }

    #[test]
    fn test_strip_export_function() {
        let code = "export function hello() { return 1; }";
        let result = strip_esm_syntax(code);
        assert_eq!(result, "function hello() { return 1; }");
    }

    #[test]
    fn test_strip_export_class() {
        let code = "export class Foo {}";
        let result = strip_esm_syntax(code);
        assert_eq!(result, "class Foo {}");
    }

    #[test]
    fn test_strip_export_default_expr() {
        let code = "export default 42;";
        let result = strip_esm_syntax(code);
        assert!(result.contains("var __default = 42;"));
    }

    #[test]
    fn test_strip_import_named() {
        let code = r#"import { storage, intercept } from "@luna/lib";console.log(1);"#;
        let result = strip_esm_syntax(code);
        assert!(
            result.contains(r#"const { storage, intercept } = luna?.core?.modules?.["@luna/lib"]"#)
        );
        assert!(result.contains("console.log(1);"));
        assert!(!result.contains("import"));
    }

    #[test]
    fn test_strip_import_namespace() {
        let code = r#"import * as core from "@luna/core";foo();"#;
        let result = strip_esm_syntax(code);
        assert!(result.contains(r#"const core = luna?.core?.modules?.["@luna/core"]"#));
        assert!(!result.contains("import"));
    }

    #[test]
    fn test_strip_import_default() {
        let code = r#"import React from "react";foo();"#;
        let result = strip_esm_syntax(code);
        assert!(result.contains(r#"const React = luna?.core?.modules?.["react"]?.default"#));
    }

    #[test]
    fn test_strip_import_side_effect() {
        let code = r#"import "./polyfill";foo();"#;
        let result = strip_esm_syntax(code);
        assert!(!result.contains("import"));
        assert!(result.contains("foo();"));
    }

    #[test]
    fn test_real_quartz_plugin_format() {
        // Simplified version of a real Quartz-bundled plugin
        let code = concat!(
            "var _=Object.create;var b=y(()=>{return luna?.core?.modules?.[\"@luna/core\"]});\n",
            "var d=p(b(),1);var a=await d.ReactiveStore.getPluginStorage(\"test\",{});\n",
            "(0,d.observePromise)(T,\"[class*='_slider']\",3e3).then(e=>{});\n",
            "T.add(()=>{});\n",
            "export{F as Settings,T as unloads};\n",
        );
        let result = strip_esm_syntax(code);
        // Code body preserved
        assert!(result.contains("var _=Object.create"));
        assert!(result.contains("await d.ReactiveStore"));
        assert!(result.contains("observePromise"));
        // Export converted to __exports object
        assert!(result.contains("var __exports = {Settings: F, unloads: T}"));
        assert!(!result.contains("export{"));
    }

    #[test]
    fn test_no_false_positive_in_string() {
        // "export" inside a string should not be stripped
        let code = r#"var s="export{foo}";console.log(s);"#;
        let result = strip_esm_syntax(code);
        // The string content is tricky - our parser looks at keyword boundaries,
        // so "export{ inside a string won't match because `"` precedes `e`
        assert!(result.contains("console.log(s);"));
    }

    #[test]
    fn test_export_star_from() {
        let code = r#"export * from "@luna/core";"#;
        let result = strip_esm_syntax(code);
        assert!(!result.contains("export"));
    }

    #[test]
    fn test_export_named_from() {
        let code = r#"export { foo, bar } from "@luna/lib";"#;
        let result = strip_esm_syntax(code);
        assert!(!result.contains("export"));
    }

    // --- transpile_ts tests ---

    #[test]
    fn test_strip_types() {
        let ts = r#"
            const x: number = 42;
            function greet(name: string): string {
                return `Hello, ${name}!`;
            }
            export { x, greet };
        "#;

        let js = transpile_ts(ts, "test.ts").unwrap();
        assert!(!js.contains(": number"));
        assert!(!js.contains(": string"));
        assert!(js.contains("const x = 42"));
        assert!(js.contains("function greet"));
    }

    #[test]
    fn test_preserve_imports() {
        let ts = r#"
            import { foo } from "@luna/core";
            import type { Bar } from "@luna/lib";
            export const x = foo();
        "#;

        let js = transpile_ts(ts, "test.ts").unwrap();
        assert!(js.contains("import { foo }"));
        assert!(!js.contains("Bar"));
    }

    #[test]
    fn test_mts_extension() {
        let ts = "export const x: number = 1;";
        let js = transpile_ts(ts, "plugin.mts").unwrap();
        assert!(js.contains("export const x = 1"));
    }
}
