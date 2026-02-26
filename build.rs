use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    // --- FRONTEND BUILD (BUN) ---
    // Only rebuild if frontend source or deps actually change
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("bundle.js");
    let frontend_dir = Path::new("frontend");

    // Make sure we have the node_modules ready
    let status = Command::new("bun")
        .args(&["install"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run bun install");

    if !status.success() {
        panic!("bun install failed");
    }

    // Run the build script defined in package.json
    let status = Command::new("bun")
        .args(&["run", "build"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run bun build");

    if !status.success() {
        panic!("bun build failed");
    }

    // Move the final bundle to where Rust can find it
    let bundle_path = frontend_dir.join("dist").join("bundle.js");
    std::fs::copy(&bundle_path, &dest_path).expect("Failed to copy bundle.js to OUT_DIR");
}
