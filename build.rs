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

    // macOS resolves the framework at launch instead of linking it; something has to hand
    // the loader a path. Inside the shipped `.app` that path is bundle-relative and always
    // right; nothing outside a bundle has that layout (not `cargo run`, not any test binary),
    // and the framework's real home carries a build-script hash that changes on every rebuild,
    // which is why it cannot be searched for at runtime. `cef-dll-sys` publishes it, resolved
    // by the very build that produced this binary: bake it in and let the loader fall back to it.
    println!("cargo:rustc-check-cfg=cfg(has_cef_build_dir)");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        match env::var("DEP_CEF_DLL_WRAPPER_CEF_DIR") {
            Ok(cef_dir) => {
                println!("cargo:rustc-env=CEF_BUILD_FRAMEWORK_DIR={cef_dir}");
                println!("cargo:rustc-cfg=has_cef_build_dir");
            }
            // Not fatal: a bundled build never reads it. Said out loud because a developer
            // running the tests without it gets a null dispatch table and a bare SIGSEGV.
            Err(e) => println!(
                "cargo:warning=DEP_CEF_DLL_WRAPPER_CEF_DIR is unavailable ({e}), so anything run outside the .app will not find the CEF framework"
            ),
        }
    }

    // CEF lives in bin/cef/: libcef.dll is delay-loaded to let AddDllDirectory run before
    // the first call. Emitted here rather than as a target-wide rustflag, which also
    // reached xtask and updater; neither imports libcef, and the linker said so (LNK4199).
    // Unqualified, which covers this package's test harness too. The triple keeps the
    // reach the rustflag had; the arm64 target never carried it.
    if env::var("TARGET").as_deref() == Ok("x86_64-pc-windows-msvc") {
        println!("cargo:rustc-link-arg=/DELAYLOAD:libcef.dll");
        println!("cargo:rustc-link-arg=delayimp.lib");
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
