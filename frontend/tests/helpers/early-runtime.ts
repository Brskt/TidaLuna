// Loads and runs a fragment from `src/ui/early_runtime/` against stubbed globals, so a test
// exercises the file the app actually ships rather than a reimplementation of it. Three test
// files hand-rolled this, each repeating the escape out of `frontend/` into the Rust tree;
// one copy here means a directory move breaks one place instead of three.

export function fragmentSource(name: string): Promise<string> {
	return Bun.file(new URL(`../../../src/ui/early_runtime/${name}.js`, import.meta.url)).text();
}

// The fragments are bare top-level code that the Rust assembler concatenates inside one
// IIFE, so they resolve `navigator`, `location` and the rest as free variables. Passing those
// as parameters mirrors that and keeps each run independent of the real globals.
export function runFragment(source: string, globals: Record<string, unknown> = {}): void {
	const names = Object.keys(globals);
	new Function(...names, source)(...names.map((name) => globals[name]));
}
