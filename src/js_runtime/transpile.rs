use std::path::Path;

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

        // Types should be gone
        assert!(!js.contains(": number"));
        assert!(!js.contains(": string"));

        // Logic should remain
        assert!(js.contains("const x = 42"));
        assert!(js.contains("function greet"));
        assert!(js.contains("export"));
    }

    #[test]
    fn test_preserve_imports() {
        let ts = r#"
            import { foo } from "@luna/core";
            import type { Bar } from "@luna/lib";
            export const x = foo();
        "#;

        let js = transpile_ts(ts, "test.ts").unwrap();

        // Value import preserved
        assert!(js.contains("import { foo }"));

        // Type-only import removed
        assert!(!js.contains("Bar"));

        // Export preserved
        assert!(js.contains("export const x"));
    }

    #[test]
    fn test_interfaces_removed() {
        let ts = r#"
            interface User {
                name: string;
                age: number;
            }
            type ID = string | number;
            const user: User = { name: "test", age: 25 };
        "#;

        let js = transpile_ts(ts, "test.ts").unwrap();

        // Interface and type alias gone
        assert!(!js.contains("interface"));
        assert!(!js.contains("type ID"));

        // Runtime code preserved
        assert!(js.contains("const user"));
        assert!(js.contains("name: \"test\""));
    }

    #[test]
    fn test_enum_preserved() {
        let ts = r#"
            enum Color {
                Red,
                Green,
                Blue,
            }
            const c: Color = Color.Red;
        "#;

        let js = transpile_ts(ts, "test.ts").unwrap();

        // Enum should be compiled to JS (not stripped)
        assert!(js.contains("Color"));
        assert!(js.contains("Red"));
    }

    #[test]
    fn test_mts_extension() {
        let ts = "export const x: number = 1;";
        let js = transpile_ts(ts, "plugin.mts").unwrap();
        assert!(js.contains("export const x = 1"));
    }
}
