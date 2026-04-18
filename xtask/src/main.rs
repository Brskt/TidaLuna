use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, exit};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

fn install_executable(src: &Path, dst: &Path) -> Result<(), String> {
    fs::copy(src, dst).map_err(|e| format!("failed to copy {}: {e}", src.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dst, fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

fn project_root() -> Result<&'static Path, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| "cannot find project root".to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let result = match args.first().map(|s| s.as_str()) {
        Some("clippy") => clippy(),
        Some("fmt") => fmt(),
        Some("bundle") => bundle(&args[1..]),
        Some("build-updater") => build_updater(&args[1..]),
        Some("generate-keypair") => generate_keypair(),
        Some("sign-manifest") => sign_manifest(),
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
    eprintln!("  clippy           Run clippy with strict warnings");
    eprintln!("  fmt              Check formatting");
    eprintln!("  bundle           Build and create distributable bundle (dev by default)");
    eprintln!("                   --release  Build in release mode (optimized, slower)");
    eprintln!("  build-updater    Build the updater crate and copy to dist/updater/");
    eprintln!("                   --release  Build in release mode");
    eprintln!("  generate-keypair Generate an Ed25519 keypair for update signing");
    eprintln!("  sign-manifest    Sign dist/manifest.json using $UPDATE_SIGNING_KEY");
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

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: String,
    min_version: String,
    target: String,
    files: BTreeMap<String, FileEntry>,
}

#[derive(Serialize, Deserialize)]
struct FileEntry {
    sha256: String,
    size: u64,
}

// ---------------------------------------------------------------------------
// bundle
// ---------------------------------------------------------------------------

fn bundle(flags: &[String]) -> Result<(), String> {
    const EXE_NAME: &str = if cfg!(target_os = "windows") {
        "tidalunar.exe"
    } else {
        "tidalunar"
    };

    let release = flags.iter().any(|f| f == "--release");
    let profile = if release { "release" } else { "dev" };

    // 1. Build
    println!("Building {profile}...");
    let mut args = vec!["build"];
    if release {
        args.push("--release");
    }
    run("cargo", &args)?;

    // 2. Locate project root and target dir
    let project_root = project_root()?;
    let target_dir = project_root
        .join("target")
        .join(if release { "release" } else { "debug" });
    let bundle_dir = project_root.join("dist");

    // 3. Find CEF directory from build output
    let cef_dir = find_cef_dir(&target_dir)?;
    println!("CEF dir: {}", cef_dir.display());

    // 4. Ensure bundle directory exists (incremental - no full wipe)
    fs::create_dir_all(&bundle_dir).map_err(|e| format!("failed to create dist/: {e}"))?;

    // 4b. Pre-clean: delete files from previous manifest that won't be in the new build.
    //     This runs BEFORE copying new files so stale artifacts don't contaminate the output.
    {
        let old_manifest_path = bundle_dir.join("manifest.json");
        if old_manifest_path.exists() {
            let data = fs::read_to_string(&old_manifest_path).unwrap_or_default();
            if let Some(old_files) = serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|v| v.get("files")?.as_object().cloned())
            {
                // Known files in the new layout (anything outside these paths is stale)
                let new_layout_prefixes: &[&str] = if cfg!(target_os = "macos") {
                    &[] // macOS uses .app bundle, no flat layout
                } else {
                    &["bin/cef/", "bin/bun", "bin/native-host"]
                };
                let new_layout_roots: &[&str] = if cfg!(target_os = "macos") {
                    &[]
                } else {
                    &[
                        EXE_NAME,
                        "updater.exe",
                        "updater",
                        "manifest.json",
                        "archive.json",
                    ]
                };

                let mut cleaned = 0u32;
                for old_path in old_files.keys() {
                    let dominated = new_layout_roots.iter().any(|r| old_path == *r)
                        || new_layout_prefixes.iter().any(|p| old_path.starts_with(p));
                    if !dominated {
                        if fs::remove_file(bundle_dir.join(old_path)).is_ok() {
                            cleaned += 1;
                        }
                    }
                }
                // Remove empty dirs left behind
                for old_path in old_files.keys() {
                    if let Some(parent) = Path::new(old_path).parent() {
                        if !parent.as_os_str().is_empty() {
                            let _ = fs::remove_dir(bundle_dir.join(parent));
                        }
                    }
                }
                if cleaned > 0 {
                    println!("  Pre-cleaned {cleaned} stale files from previous layout");
                }
            }
        }
    }

    // 5. Platform-specific bundling
    let bin_dir = bundle_dir.join("bin");
    if cfg!(target_os = "macos") {
        bundle_macos(EXE_NAME, &target_dir, &cef_dir, &bundle_dir)?;
    } else {
        // Linux/Windows: structured layout
        // Root: exe + updater + manifest
        let exe_src = target_dir.join(EXE_NAME);
        let exe_dst = bundle_dir.join(EXE_NAME);
        link_or_copy(&exe_src, &exe_dst)?;
        println!("  Copied {EXE_NAME}");

        // bin/cef/: all CEF runtime files
        let cef_bundle_dir = bin_dir.join("cef");
        fs::create_dir_all(&cef_bundle_dir)
            .map_err(|e| format!("failed to create bin/cef/: {e}"))?;

        copy_cef_files(&cef_dir, &cef_bundle_dir)?;

        let locales_src = cef_dir.join("locales");
        let locales_dst = cef_bundle_dir.join("locales");
        if locales_src.is_dir() {
            copy_dir_flat(&locales_src, &locales_dst)?;
            println!("  Copied bin/cef/locales/");
        }
    }

    // 6. Download Bun binary into bin/
    fs::create_dir_all(&bin_dir).map_err(|e| format!("failed to create bin/: {e}"))?;
    download_bun(&bin_dir)?;

    if release {
        strip_binaries(&bundle_dir)?;
    }

    // 8. Generate manifest.json
    generate_manifest(&bundle_dir)?;

    println!("Bundle created in: {}", bundle_dir.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// Manifest generation
// ---------------------------------------------------------------------------

fn target_triple() -> String {
    let os = if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };

    let arch = if cfg!(target_arch = "x86_64") {
        "amd64"
    } else if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "unknown"
    };

    format!("{os}-{arch}")
}

fn sha256_file(path: &Path) -> Result<(String, u64), String> {
    let mut file =
        fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?;
    let size = metadata.len();

    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read error {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let hash = base16ct::lower::encode_string(&hasher.finalize());
    Ok((hash, size))
}

fn collect_files(
    dir: &Path,
    base: &Path,
    files: &mut BTreeMap<String, FileEntry>,
) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;

    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .strip_prefix(base)
            .map_err(|e| format!("strip prefix: {e}"))?
            .to_string_lossy()
            .replace('\\', "/");

        if path.is_dir() {
            collect_files(&path, base, files)?;
        } else if name != "manifest.json" && name != "manifest.json.sig" {
            let (sha256, size) = sha256_file(&path)?;
            files.insert(name, FileEntry { sha256, size });
        }
    }
    Ok(())
}

fn read_workspace_version() -> Result<String, String> {
    let project_root = project_root()?;
    let cargo_toml = fs::read_to_string(project_root.join("Cargo.toml"))
        .map_err(|e| format!("cannot read root Cargo.toml: {e}"))?;

    // Simple parse - look for version = "x.y.z" in [package] section
    for line in cargo_toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("version") {
            if let Some(v) = trimmed.split('"').nth(1) {
                return Ok(v.to_string());
            }
        }
    }
    Err("version not found in root Cargo.toml".to_string())
}

fn generate_manifest(bundle_dir: &Path) -> Result<(), String> {
    let version = read_workspace_version()?;

    // min_version defaults to same as version for now - adjust per release
    let min_version = version.clone();

    let mut files = BTreeMap::new();
    collect_files(bundle_dir, bundle_dir, &mut files)?;

    let manifest = Manifest {
        version,
        min_version,
        target: target_triple(),
        files,
    };

    let json =
        serde_json::to_string_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;
    fs::write(bundle_dir.join("manifest.json"), &json)
        .map_err(|e| format!("write manifest.json: {e}"))?;

    println!("  Generated manifest.json ({} files)", manifest.files.len());
    Ok(())
}

// ---------------------------------------------------------------------------
// generate-keypair
// ---------------------------------------------------------------------------

fn generate_keypair() -> Result<(), String> {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(|e| format!("getrandom failed: {e}"))?;
    let signing_key = SigningKey::from_bytes(&secret);
    let verifying_key = VerifyingKey::from(&signing_key);

    let private_b64 = BASE64.encode(signing_key.to_bytes());
    let public_bytes = verifying_key.to_bytes();

    println!("=== Ed25519 Update Signing Keypair ===");
    println!();
    println!("PRIVATE KEY (store in GitHub Secret UPDATE_SIGNING_KEY):");
    println!("{private_b64}");
    println!();
    println!("PUBLIC KEY (embed in updater binary):");
    println!("const UPDATE_PUBLIC_KEY: [u8; 32] = {public_bytes:?};");
    println!();
    println!("PUBLIC KEY (base64):");
    println!("{}", BASE64.encode(public_bytes));

    Ok(())
}

// ---------------------------------------------------------------------------
// sign-manifest
// ---------------------------------------------------------------------------

fn sign_manifest() -> Result<(), String> {
    let key_b64 = std::env::var("UPDATE_SIGNING_KEY")
        .map_err(|_| "UPDATE_SIGNING_KEY environment variable not set".to_string())?;

    let key_bytes = BASE64
        .decode(key_b64.trim())
        .map_err(|e| format!("invalid base64 in UPDATE_SIGNING_KEY: {e}"))?;

    let key_array: [u8; 32] = key_bytes
        .try_into()
        .map_err(|_| "UPDATE_SIGNING_KEY must be exactly 32 bytes (base64-encoded)".to_string())?;

    let signing_key = SigningKey::from_bytes(&key_array);

    let project_root = project_root()?;
    let manifest_path = project_root.join("dist/manifest.json");
    let sig_path = project_root.join("dist/manifest.json.sig");

    let manifest_bytes = fs::read(&manifest_path)
        .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))?;

    let signature = signing_key.sign(&manifest_bytes);
    let sig_b64 = BASE64.encode(signature.to_bytes());

    fs::write(&sig_path, &sig_b64)
        .map_err(|e| format!("cannot write {}: {e}", sig_path.display()))?;

    println!("Signed manifest.json -> manifest.json.sig");
    Ok(())
}

// ---------------------------------------------------------------------------
// build-updater
// ---------------------------------------------------------------------------

fn build_updater(flags: &[String]) -> Result<(), String> {
    let release = flags.iter().any(|f| f == "--release");

    let mut args = vec!["build", "--package", "updater"];
    if release {
        args.push("--release");
    }
    run("cargo", &args)?;

    let project_root = project_root()?;
    let target_dir = project_root
        .join("target")
        .join(if release { "release" } else { "debug" });
    let bundle_dir = project_root.join("dist");

    let updater_name = if cfg!(target_os = "windows") {
        "updater.exe"
    } else {
        "updater"
    };

    let src = target_dir.join(updater_name);
    let dst = bundle_dir.join(updater_name);
    fs::copy(&src, &dst).map_err(|e| format!("copy updater binary: {e}"))?;

    println!("Updater binary copied to {}", dst.display());

    // Regenerate manifest to include the updater binary
    generate_manifest(&bundle_dir)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// CEF directory finder
// ---------------------------------------------------------------------------

/// Find the CEF distribution directory inside target/release/build/cef-dll-sys-*/out/
fn find_cef_dir(target_dir: &Path) -> Result<PathBuf, String> {
    let build_dir = target_dir.join("build");
    let entries = fs::read_dir(&build_dir)
        .map_err(|e| format!("cannot read {}: {e}", build_dir.display()))?;

    // Only match the CEF library for the current target (avoids picking Linux dirs on Windows)
    let cef_marker = if cfg!(target_os = "windows") {
        "libcef.dll"
    } else if cfg!(target_os = "macos") {
        "Chromium Embedded Framework.framework"
    } else {
        "libcef.so"
    };

    let mut candidates: Vec<PathBuf> = Vec::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("cef-dll-sys-") {
            let out_dir = entry.path().join("out");
            if out_dir.is_dir() {
                for sub in fs::read_dir(&out_dir).into_iter().flatten().flatten() {
                    let sub_name = sub.file_name().to_string_lossy().to_string();
                    if sub_name.starts_with("cef_") && sub.path().is_dir() {
                        let marker_path = sub.path().join(cef_marker);
                        if marker_path.exists() || marker_path.is_dir() {
                            candidates.push(sub.path());
                        }
                    }
                }
            }
        }
    }

    candidates.sort_by(|a, b| {
        let mtime = |p: &Path| fs::metadata(p).and_then(|m| m.modified()).ok();
        mtime(b).cmp(&mtime(a))
    });

    if let Some(best) = candidates.into_iter().next() {
        return Ok(best);
    }
    Err("CEF directory not found - run `cargo build --release` first".to_string())
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

    // Create helper apps - CEF subprocess helpers reuse the main binary.
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

/// Link or copy files from CEF dir to bundle (skip .exe, .lib, directories, build files).
/// Hard-link or copy a file, removing any existing destination first.
fn link_or_copy(src: &Path, dst: &Path) -> Result<(), String> {
    let _ = fs::remove_file(dst);
    if fs::hard_link(src, dst).is_err() {
        fs::copy(src, dst).map_err(|e| format!("failed to copy {}: {e}", src.display()))?;
    }
    Ok(())
}

fn copy_cef_files(cef_dir: &Path, bundle_dir: &Path) -> Result<(), String> {
    let skip_extensions = ["exe", "lib"];
    let skip_names = ["CMakeLists.txt", "cmake", "include", "libcef_dll"];

    let entries = fs::read_dir(cef_dir).map_err(|e| format!("cannot read CEF dir: {e}"))?;

    let mut count = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        if path.is_dir() || skip_names.contains(&name.as_str()) {
            continue;
        }

        if let Some(ext) = path.extension()
            && skip_extensions.contains(&ext.to_string_lossy().as_ref())
        {
            continue;
        }

        let dst = bundle_dir.join(&name);
        link_or_copy(&path, &dst)?;
        count += 1;
    }
    println!("  Copied {count} CEF files");
    Ok(())
}

/// Link or copy all files from a directory (flat, no recursion).
fn copy_dir_flat(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| format!("failed to create {}: {e}", dst.display()))?;

    for entry in fs::read_dir(src).into_iter().flatten().flatten() {
        if entry.path().is_file() {
            let dest = dst.join(entry.file_name());
            link_or_copy(&entry.path(), &dest)?;
        }
    }
    Ok(())
}

/// Download Bun binary from GitHub releases into the bundle directory.
/// Uses a cache directory to avoid re-downloading on every build.
fn download_bun(bundle_dir: &Path) -> Result<(), String> {
    let bun_name = if cfg!(target_os = "windows") {
        "bun.exe"
    } else {
        "bun"
    };
    let bun_dst = bundle_dir.join(bun_name);

    const BUN_VERSION: &str = "1.3.12";

    // Cache dir persists across builds (dist/ is cleaned each time)
    let cache_dir = project_root()?.join(".cache").join("bun");
    fs::create_dir_all(&cache_dir).map_err(|e| format!("failed to create cache dir: {e}"))?;

    let cached_bun = cache_dir.join(format!(
        "bun-v{BUN_VERSION}{}",
        if cfg!(target_os = "windows") {
            ".exe"
        } else {
            ""
        }
    ));

    // If cached binary exists for this version, just copy it
    if cached_bun.exists()
        && fs::metadata(&cached_bun)
            .map(|m| m.len() > 1_000_000)
            .unwrap_or(false)
    {
        install_executable(&cached_bun, &bun_dst)?;
        println!("  Bun v{BUN_VERSION} (cached)");
        return Ok(());
    }

    let (archive_name, inner_dir) = if cfg!(target_os = "windows") {
        ("bun-windows-x64.zip", "bun-windows-x64")
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            ("bun-darwin-aarch64.zip", "bun-darwin-aarch64")
        } else {
            ("bun-darwin-x64.zip", "bun-darwin-x64")
        }
    } else {
        ("bun-linux-x64.zip", "bun-linux-x64")
    };

    let url = format!(
        "https://github.com/oven-sh/bun/releases/download/bun-v{BUN_VERSION}/{archive_name}"
    );
    let zip_path = cache_dir.join(archive_name);

    println!("  Downloading Bun v{BUN_VERSION} from {url}...");
    let status = Command::new("curl")
        .args(["-fSL", "-o"])
        .arg(&zip_path)
        .arg(&url)
        .status()
        .map_err(|e| format!("curl failed: {e}"))?;
    if !status.success() {
        return Err(format!(
            "Failed to download Bun (curl exit: {:?})",
            status.code()
        ));
    }

    println!("  Extracting {bun_name}...");
    if cfg!(target_os = "windows") {
        let ps_cmd = format!(
            "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
            zip_path.display(),
            cache_dir.display()
        );
        let status = Command::new("powershell")
            .args(["-NoProfile", "-Command", &ps_cmd])
            .status()
            .map_err(|e| format!("powershell extract failed: {e}"))?;
        if !status.success() {
            return Err("Failed to extract Bun zip".to_string());
        }
    } else {
        let status = Command::new("unzip")
            .args(["-o", "-q"])
            .arg(&zip_path)
            .arg("-d")
            .arg(&cache_dir)
            .status()
            .map_err(|e| format!("unzip failed: {e}"))?;
        if !status.success() {
            return Err("Failed to extract Bun zip".to_string());
        }
    }

    // Move bun binary from inner directory to cache
    let inner_bun = cache_dir.join(inner_dir).join(bun_name);
    if inner_bun.exists() {
        fs::rename(&inner_bun, &cached_bun).map_err(|e| format!("failed to move bun: {e}"))?;
        let _ = fs::remove_dir_all(cache_dir.join(inner_dir));
    }
    let _ = fs::remove_file(&zip_path);

    if cached_bun.exists() {
        install_executable(&cached_bun, &bun_dst)?;
        println!("  Bun v{BUN_VERSION} installed");
    } else {
        println!("  Warning: Bun binary not found after extraction");
    }

    Ok(())
}

fn strip_binaries(bundle_dir: &Path) -> Result<(), String> {
    if cfg!(target_os = "windows") {
        return Ok(());
    }

    let strip_args: &[&str] = if cfg!(target_os = "macos") {
        &["-x"]
    } else {
        &["--strip-debug", "--strip-unneeded"]
    };

    let should_strip = |name: &str| -> bool {
        name.ends_with(".so") || name.contains(".so.") || name.ends_with(".dylib") || name == "bun"
    };

    let mut stripped = 0u32;
    // Scan root + bin/ for strippable binaries
    let dirs_to_scan = [bundle_dir.to_path_buf(), bundle_dir.join("bin")];
    for scan_dir in &dirs_to_scan {
        for entry in fs::read_dir(scan_dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            if path.is_file() && should_strip(&name) {
                let size_before = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if run_strip(&path, strip_args)? {
                    let size_after = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    let saved_mb = (size_before.saturating_sub(size_after)) as f64 / 1_048_576.0;
                    println!("  Stripped {name} ({saved_mb:.1} MB saved)");
                    stripped += 1;
                } else {
                    println!("  Warning: strip failed for {name}");
                }
            }
        }
    }

    if cfg!(target_os = "macos") {
        strip_macos_app(bundle_dir, strip_args)?;
    }

    if stripped > 0 {
        println!("  Stripped {stripped} binaries");
    }
    Ok(())
}

fn run_strip(path: &Path, strip_args: &[&str]) -> Result<bool, String> {
    let path_str = path.to_string_lossy().to_string();
    let mut args: Vec<&str> = strip_args.to_vec();
    args.push(&path_str);
    let status = Command::new("strip")
        .args(&args)
        .status()
        .map_err(|e| format!("strip failed for {}: {e}", path.display()))?;
    Ok(status.success())
}

fn strip_macos_app(bundle_dir: &Path, strip_args: &[&str]) -> Result<(), String> {
    fn walk_strip(dir: &Path, strip_args: &[&str]) -> Result<(), String> {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_strip(&path, strip_args)?;
            } else if path.is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".dylib") || name == "Chromium Embedded Framework" {
                    if run_strip(&path, strip_args)? {
                        println!("  Stripped {name}");
                    }
                }
            }
        }
        Ok(())
    }
    walk_strip(bundle_dir, strip_args)
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
