use std::env;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/package.json");
    println!("cargo:rerun-if-changed=frontend/plugins");
    println!("cargo:rerun-if-changed=frontend/render");
    println!("cargo:rerun-if-changed=frontend/build");
    println!("cargo:rerun-if-changed=frontend/esbuild.config.ts");

    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("bundle.js");
    let frontend_dir = Path::new("frontend");
    let bundle_path = frontend_dir.join("dist").join("bundle.js");

    // Skip bun install if node_modules exists (avoids Bun segfault on Windows via 9P)
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

    // On Windows via 9P, Bun segfaults. Pre-build from WSL then skip here.
    if cfg!(target_os = "windows") && bundle_path.exists() {
        eprintln!("Windows: skipping bun scripts (using pre-built outputs)");
    } else {
        let status = Command::new("bun")
            .args(["esbuild.config.ts"])
            .current_dir(frontend_dir)
            .status()
            .expect("Failed to run esbuild.config.ts");
        if !status.success() {
            panic!("esbuild.config.ts failed");
        }
    }

    std::fs::copy(&bundle_path, &dest_path).expect("Failed to copy bundle.js to OUT_DIR");

    // Bake the plugin ES modules alongside the bundle for luna_modules.rs to include_bytes! them
    // and serve them via a ResourceHandler, instead of bundle.js carrying them as inline strings.
    for (src, name) in [
        (
            frontend_dir.join("plugins/ui/dist/luna-ui.mjs"),
            "luna-ui.mjs",
        ),
        (
            frontend_dir.join("plugins/dev/dist/luna-dev.mjs"),
            "luna-dev.mjs",
        ),
    ] {
        let dest = Path::new(&out_dir).join(name);
        std::fs::copy(&src, &dest)
            .unwrap_or_else(|e| panic!("Failed to copy {} to OUT_DIR: {e}", src.display()));
    }

    // `include_bytes!` resolves before any Rust code runs, which leaves the Connect
    // receiver unable to test for its own TLS key; the probe belongs here instead. That
    // key is a credential kept out of the repo, and a clone without it is the normal case.
    println!("cargo:rerun-if-changed=src/connect/ws/certs");
    println!("cargo:rustc-check-cfg=cfg(has_connect_server_key)");
    if Path::new("src/connect/ws/certs/tidal_server_key.pem").exists() {
        println!("cargo:rustc-cfg=has_connect_server_key");
    } else {
        println!(
            "cargo:warning=tidal_server_key.pem is absent, so the TIDAL Connect receiver is compiled out (the controller is unaffected)"
        );
    }

    // Windows icon
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
