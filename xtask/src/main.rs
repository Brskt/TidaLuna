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

    // 5. Copy exe
    let exe_src = target_dir.join(EXE_NAME);
    let exe_dst = bundle_dir.join(EXE_NAME);
    fs::copy(&exe_src, &exe_dst).map_err(|e| format!("failed to copy exe: {e}"))?;
    println!("  Copied {EXE_NAME}");

    // 6. Copy all CEF files (DLLs, .pak, .dat, .bin, .json — skip .exe, .lib, dirs)
    copy_cef_files(&cef_dir, &bundle_dir)?;

    // 7. Copy locales/
    let locales_src = cef_dir.join("locales");
    let locales_dst = bundle_dir.join("locales");
    if locales_src.is_dir() {
        copy_dir_flat(&locales_src, &locales_dst)?;
        println!("  Copied locales/");
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
                        // Verify it contains libcef
                        let has_libcef = sub.path().join("libcef.dll").exists()
                            || sub.path().join("libcef.so").exists();
                        if has_libcef {
                            return Ok(sub.path());
                        }
                    }
                }
            }
        }
    }
    Err("CEF directory not found — run `cargo build --release` first".to_string())
}

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
        if let Some(ext) = path.extension() {
            if skip_extensions.contains(&ext.to_string_lossy().as_ref()) {
                continue;
            }
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
