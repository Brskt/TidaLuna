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
    let bundle_path = frontend_dir.join("dist").join("bundle.js");

    // Make sure we have the node_modules ready
    // Skip bun install if node_modules already exists (avoids Bun segfault on Windows via 9P)
    let node_modules = frontend_dir.join("node_modules");
    if !node_modules.exists() {
        let status = Command::new("bun")
            .args(["install"])
            .current_dir(frontend_dir)
            .status()
            .expect("Failed to run bun install");

        if !status.success() {
            panic!("bun install failed");
        }
    }

    // Build @luna/ui plugin
    let status = Command::new("node")
        .args(["scripts/build-luna-ui.mjs"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run build-luna-ui");

    if !status.success() {
        panic!("build-luna-ui failed");
    }

    // Build @luna/* module bundles for rquickjs runtime
    let status = Command::new("node")
        .args(["scripts/build-luna-modules.mjs"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run build-luna-modules");

    if !status.success() {
        panic!("build-luna-modules failed");
    }

    // Build main bundle
    let status = Command::new("node")
        .args(["scripts/build-bundle.mjs"])
        .current_dir(frontend_dir)
        .status()
        .expect("Failed to run build-bundle");

    if !status.success() {
        panic!("build-bundle failed");
    }

    std::fs::copy(&bundle_path, &dest_path).expect("Failed to copy bundle.js to OUT_DIR");

    // --- WINDOWS ICON ---
    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("tidaluna.ico");
        res.set_icon_with_id("tidaluna.ico", "101");
        res.set_manifest_file("tidalunar.exe.manifest");
        res.set("FileDescription", "TidaLunar");
        res.set("ProductName", "TidaLunar");
        res.compile().expect("Failed to compile Windows resources");
    }
    println!("cargo:rerun-if-changed=tidalunar.exe.manifest");
}
