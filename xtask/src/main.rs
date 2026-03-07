use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let result = match args.first().map(|s| s.as_str()) {
        Some("clippy") => clippy(),
        Some("fmt") => fmt(),
        Some("bundle") => bundle(&args[1..]),
        Some(cmd) => {
            eprintln!("Unknown command: {cmd}");
            eprintln!();
            usage();
            exit(1);
        }
        None => {
            usage();
            exit(1);
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {e}");
        exit(1);
    }
}

fn usage() {
    eprintln!("Usage: cargo xtask <command>");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  clippy    Run clippy with strict warnings");
    eprintln!("  fmt       Check formatting");
    eprintln!("  bundle    Build release and create distributable bundle");
    eprintln!("            --console  Enable console window (shows logs)");
}

fn clippy() -> Result<(), String> {
    run(
        "cargo",
        &[
            "clippy",
            "--all-targets",
            "--",
            "-D",
            "warnings",
            "-D",
            "clippy::all",
        ],
    )
}

fn fmt() -> Result<(), String> {
    run("cargo", &["fmt", "--all", "--", "--check"])
}

fn bundle(flags: &[String]) -> Result<(), String> {
    const EXE_NAME: &str = if cfg!(target_os = "windows") {
        "tidalunar.exe"
    } else {
        "tidalunar"
    };

    let console = flags.iter().any(|f| f == "--console");

    // 1. Build release
    println!(
        "Building release{}...",
        if console { " (console)" } else { "" }
    );
    if console {
        run("cargo", &["build", "--release", "--features", "console"])?;
    } else {
        run("cargo", &["build", "--release"])?;
    }

    // 2. Locate project root and target dir
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or("cannot find project root")?;
    let target_dir = project_root.join("target").join("release");
    let bundle_dir = project_root.join("dist");

    // 3. Find CEF directory from build output
    let cef_dir = find_cef_dir(&target_dir)?;
    println!("CEF dir: {}", cef_dir.display());

    // 4. Create clean bundle directory
    if bundle_dir.exists() {
        fs::remove_dir_all(&bundle_dir).map_err(|e| format!("failed to clean dist/: {e}"))?;
    }
    fs::create_dir_all(&bundle_dir).map_err(|e| format!("failed to create dist/: {e}"))?;

    // 5. Platform-specific bundling
    if cfg!(target_os = "macos") {
        bundle_macos(EXE_NAME, &target_dir, &cef_dir, &bundle_dir)?;
    } else {
        // Linux/Windows: flat directory with exe + CEF files
        let exe_src = target_dir.join(EXE_NAME);
        let exe_dst = bundle_dir.join(EXE_NAME);
        fs::copy(&exe_src, &exe_dst).map_err(|e| format!("failed to copy exe: {e}"))?;
        println!("  Copied {EXE_NAME}");

        copy_cef_files(&cef_dir, &bundle_dir)?;

        let locales_src = cef_dir.join("locales");
        let locales_dst = bundle_dir.join("locales");
        if locales_src.is_dir() {
            copy_dir_flat(&locales_src, &locales_dst)?;
            println!("  Copied locales/");
        }
    }

    println!("Bundle created in: {}", bundle_dir.display());
    Ok(())
}

/// Find the CEF distribution directory inside target/release/build/cef-dll-sys-*/out/
fn find_cef_dir(target_dir: &Path) -> Result<PathBuf, String> {
    let build_dir = target_dir.join("build");
    let entries = fs::read_dir(&build_dir)
        .map_err(|e| format!("cannot read {}: {e}", build_dir.display()))?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("cef-dll-sys-") {
            let out_dir = entry.path().join("out");
            if out_dir.is_dir() {
                // Find the platform-specific CEF directory (e.g., cef_windows_x86_64)
                for sub in fs::read_dir(&out_dir).into_iter().flatten().flatten() {
                    let sub_name = sub.file_name().to_string_lossy().to_string();
                    if sub_name.starts_with("cef_") && sub.path().is_dir() {
                        let has_cef = sub.path().join("libcef.dll").exists()
                            || sub.path().join("libcef.so").exists()
                            || sub
                                .path()
                                .join("Chromium Embedded Framework.framework")
                                .is_dir();
                        if has_cef {
                            return Ok(sub.path());
                        }
                    }
                }
            }
        }
    }
    Err("CEF directory not found — run `cargo build --release` first".to_string())
}

// ---------------------------------------------------------------------------
// macOS .app bundle
// ---------------------------------------------------------------------------

/// Create a macOS .app bundle with CEF framework and helper apps.
///
/// Structure:
///   dist/tidalunar.app/
///     Contents/
///       MacOS/tidalunar
///       Frameworks/
///         Chromium Embedded Framework.framework/
///         tidalunar Helper.app/
///         tidalunar Helper (GPU).app/
///         tidalunar Helper (Renderer).app/
///         tidalunar Helper (Plugin).app/
///         tidalunar Helper (Alerts).app/
///       Resources/
///       Info.plist
fn bundle_macos(
    exe_name: &str,
    target_dir: &Path,
    cef_dir: &Path,
    bundle_dir: &Path,
) -> Result<(), String> {
    let app_dir = bundle_dir.join(format!("{exe_name}.app"));
    let contents = app_dir.join("Contents");
    let macos_dir = contents.join("MacOS");
    let frameworks_dir = contents.join("Frameworks");
    let resources_dir = contents.join("Resources");

    for dir in [&macos_dir, &frameworks_dir, &resources_dir] {
        fs::create_dir_all(dir).map_err(|e| format!("create dir: {e}"))?;
    }

    // Copy main binary
    let exe_src = target_dir.join(exe_name);
    fs::copy(&exe_src, macos_dir.join(exe_name)).map_err(|e| format!("copy main binary: {e}"))?;
    println!("  Copied {exe_name}");

    // Copy CEF framework (recursive, preserving symlinks)
    let framework = "Chromium Embedded Framework.framework";
    let fw_src = cef_dir.join(framework);
    let fw_dst = frameworks_dir.join(framework);
    copy_dir_recursive(&fw_src, &fw_dst)?;
    println!("  Copied {framework}");

    // Write main Info.plist
    write_info_plist(&contents, exe_name, false)?;

    // Create helper apps — CEF subprocess helpers reuse the main binary.
    // CEF identifies the subprocess role via --type= argument.
    let helpers = [
        "Helper",
        "Helper (GPU)",
        "Helper (Renderer)",
        "Helper (Plugin)",
        "Helper (Alerts)",
    ];
    for suffix in helpers {
        let helper_name = format!("{exe_name} {suffix}");
        let helper_app = frameworks_dir.join(format!("{helper_name}.app"));
        let helper_contents = helper_app.join("Contents");
        let helper_macos = helper_contents.join("MacOS");

        fs::create_dir_all(&helper_macos).map_err(|e| format!("create helper dir: {e}"))?;
        fs::copy(&exe_src, helper_macos.join(&helper_name))
            .map_err(|e| format!("copy helper binary: {e}"))?;
        write_info_plist(&helper_contents, &helper_name, true)?;
    }
    println!("  Created {} helper apps", helpers.len());

    Ok(())
}

fn write_info_plist(contents_dir: &Path, name: &str, is_helper: bool) -> Result<(), String> {
    let identifier = name
        .to_lowercase()
        .replace(' ', "-")
        .replace(['(', ')'], "");

    let ui_element = if is_helper {
        "\n    <key>LSUIElement</key>\n    <string>1</string>"
    } else {
        ""
    };

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>{name}</string>
    <key>CFBundleIdentifier</key>
    <string>com.tidalunar.{identifier}</string>
    <key>CFBundleName</key>
    <string>{name}</string>
    <key>CFBundleVersion</key>
    <string>1.0.0</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>{ui_element}
</dict>
</plist>
"#
    );

    fs::write(contents_dir.join("Info.plist"), plist).map_err(|e| format!("write Info.plist: {e}"))
}

/// Recursively copy a directory, preserving symlinks (needed for macOS framework structure).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("create dir {}: {e}", dst.display()))?;

    for entry in fs::read_dir(src).map_err(|e| format!("read dir {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| format!("dir entry: {e}"))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let ft = entry
            .file_type()
            .map_err(|e| format!("file type {}: {e}", src_path.display()))?;

        if ft.is_symlink() {
            let target = fs::read_link(&src_path)
                .map_err(|e| format!("read symlink {}: {e}", src_path.display()))?;
            create_symlink(&target, &dst_path)?;
        } else if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("copy {}: {e}", src_path.display()))?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> Result<(), String> {
    std::os::unix::fs::symlink(target, link).map_err(|e| format!("symlink {}: {e}", link.display()))
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link: &Path) -> Result<(), String> {
    Ok(())
}

// ---------------------------------------------------------------------------
// Linux / Windows helpers (flat bundle)
// ---------------------------------------------------------------------------

/// Copy files from CEF dir to bundle (skip .exe, .lib, directories, build files)
fn copy_cef_files(cef_dir: &Path, bundle_dir: &Path) -> Result<(), String> {
    let skip_extensions = ["exe", "lib"];
    let skip_names = ["CMakeLists.txt", "cmake", "include", "libcef_dll"];

    let entries = fs::read_dir(cef_dir).map_err(|e| format!("cannot read CEF dir: {e}"))?;

    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Skip directories (locales handled separately), build artifacts, headers
        if path.is_dir() || skip_names.contains(&name.as_str()) {
            continue;
        }

        // Skip .exe and .lib files
        if let Some(ext) = path.extension()
            && skip_extensions.contains(&ext.to_string_lossy().as_ref())
        {
            continue;
        }

        let dst = bundle_dir.join(&name);
        fs::copy(&path, &dst).map_err(|e| format!("failed to copy {name}: {e}"))?;
        count += 1;
    }
    println!("  Copied {count} CEF files");
    Ok(())
}

/// Copy all files from a directory (flat, no recursion)
fn copy_dir_flat(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("failed to create {}: {e}", dst.display()))?;

    for entry in fs::read_dir(src).into_iter().flatten().flatten() {
        if entry.path().is_file() {
            let dest = dst.join(entry.file_name());
            fs::copy(entry.path(), &dest)
                .map_err(|e| format!("failed to copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let status = Command::new(cmd)
        .args(args)
        .status()
        .map_err(|e| format!("failed to run {cmd}: {e}"))?;

    if !status.success() {
        exit(status.code().unwrap_or(1));
    }

    Ok(())
}
