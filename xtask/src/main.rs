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
        Some("package") => package(&args[1..]),
        Some("delta") => delta(&args[1..]),
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
    eprintln!("  package          Build a platform installer from dist/");
    eprintln!("                   --release  Build payload in release mode (recommended)");
    eprintln!(
        "                   --target <windows-nsis|linux-deb|linux-tarball>  Format (default: windows-nsis)"
    );
    eprintln!(
        "                   --arch <amd64|arm64>  Required: payload arch (matches matrix.target in CI)"
    );
    eprintln!("  delta            Build a consecutive-delta archive from dist/ vs an old manifest");
    eprintln!("                   --target <win32|linux>  Required: archive flavor");
    eprintln!("                   --arch <amd64|arm64>    Required");
    eprintln!(
        "                   --old-manifest <path>   Required: previous release's manifest.json"
    );
    eprintln!("                   --dist <dir>            New bundle dir (default: dist)");
    eprintln!("  generate-keypair Generate an Ed25519 keypair for update signing");
    eprintln!("  sign-manifest    Sign dist/manifest.json using $UPDATE_SIGNING_KEY");
}

fn clippy() -> Result<(), String> {
    // The updater is a separate workspace member that the default invocation
    // skips, so lint it explicitly with the same flags.
    for pkg in ["tidalunar", "updater"] {
        run(
            "cargo",
            &[
                "clippy",
                "--package",
                pkg,
                "--all-targets",
                "--",
                "-D",
                "warnings",
                "-D",
                "clippy::all",
            ],
        )?;
    }
    Ok(())
}

fn fmt() -> Result<(), String> {
    run("cargo", &["fmt", "--all", "--", "--check"])
}

// ---------------------------------------------------------------------------
// Manifest types
// ---------------------------------------------------------------------------

/// Minimum `SANDBOX_PROTOCOL_VERSION` the Linux .deb's system bootstrap must
/// have for in-app updates produced by this xtask to be safe to apply.
///
/// Bump this value when CEF's major version changes or libcef changes the
/// SUID-sandbox helper protocol. The corresponding
/// `/usr/lib/tidalunar/SANDBOX_PROTOCOL_VERSION` file is written from this
/// constant by the .deb packaging pipeline, and the in-app updater compares
/// the value here (read via the manifest field) against the system file.
const LINUX_SANDBOX_PROTOCOL_REQUIRED: u32 = 1;

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: String,
    min_version: String,
    target: String,
    files: BTreeMap<String, FileEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sandbox_protocol_required: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delta_from: Option<String>,
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

    let target = target_triple();

    // Linux: declare the sandbox-helper protocol this payload requires. The
    // .deb launcher reads it from $USER_DIR/SANDBOX_PROTOCOL_REQUIRED to gate
    // against a stale system bootstrap. Written before collect_files so it
    // ships in the manifest, the update archives, and the payload tarball, and
    // is re-applied by in-app updates.
    if target.starts_with("linux-") {
        fs::write(
            bundle_dir.join("SANDBOX_PROTOCOL_REQUIRED"),
            format!("{LINUX_SANDBOX_PROTOCOL_REQUIRED}\n"),
        )
        .map_err(|e| format!("write SANDBOX_PROTOCOL_REQUIRED: {e}"))?;
    }

    let mut files = BTreeMap::new();
    collect_files(bundle_dir, bundle_dir, &mut files)?;

    let sandbox_protocol_required = if target.starts_with("linux-") {
        Some(LINUX_SANDBOX_PROTOCOL_REQUIRED)
    } else {
        None
    };
    let manifest = Manifest {
        version,
        min_version,
        target,
        files,
        sandbox_protocol_required,
        // CI stamps the real previous version into the published manifest.
        delta_from: None,
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

    // Apple's CFBundleVersion / CFBundleShortVersionString reject prerelease
    // suffixes during signing/notarization. Strip `-alpha` etc. and pad to 3
    // dotted-numeric parts (the conventional Apple form).
    let version = numeric_version(&read_workspace_version()?, 3);

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
    write_info_plist(&contents, exe_name, &version, false)?;

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
        write_info_plist(&helper_contents, &helper_name, &version, true)?;
    }
    println!("  Created {} helper apps", helpers.len());

    Ok(())
}

fn write_info_plist(
    contents_dir: &Path,
    name: &str,
    version: &str,
    is_helper: bool,
) -> Result<(), String> {
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
    <key>CFBundleShortVersionString</key>
    <string>{version}</string>
    <key>CFBundleVersion</key>
    <string>{version}</string>
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

/// Sum the byte size of every regular file under `dir`, recursively.
/// Used for the installer's manual `AddSize` estimate: NSIS can't see inside
/// the pre-compressed payload.7z, so it needs the decompressed total.
fn dir_size_bytes(dir: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let ft = entry.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            total += dir_size_bytes(&entry.path())?;
        } else if ft.is_file() {
            total += entry.metadata().map_err(|e| e.to_string())?.len();
        }
    }
    Ok(total)
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

    const BUN_VERSION: &str = "1.3.14";

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

// ---------------------------------------------------------------------------
// package - Build a Windows NSIS installer from dist/
// ---------------------------------------------------------------------------

/// Map a semver string like `0.0.4-alpha` to a fixed-arity dotted-numeric form
/// for installer/bundle metadata that rejects prerelease suffixes:
///   * NSIS `VIProductVersion` requires exactly 4 numeric parts
///   * Apple `CFBundleVersion` / `CFBundleShortVersionString` reject `-alpha`
///     and conventionally use 3 parts
///
/// Drops any prerelease/build suffix, keeps only leading digits per part,
/// pads missing components with `0`, truncates to `parts`.
fn numeric_version(version: &str, parts: usize) -> String {
    let core = version.split(['-', '+']).next().unwrap_or(version);
    let mut out: Vec<String> = core
        .split('.')
        .map(|p| {
            p.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .filter(|p| !p.is_empty())
        .collect();
    while out.len() < parts {
        out.push("0".into());
    }
    out.truncate(parts);
    out.join(".")
}

#[cfg(test)]
mod tests {
    use super::numeric_version;

    #[test]
    fn drops_prerelease_suffix() {
        assert_eq!(numeric_version("0.0.4-alpha", 4), "0.0.4.0");
        assert_eq!(numeric_version("1.2.3-rc.1", 4), "1.2.3.0");
        assert_eq!(numeric_version("0.0.4-alpha", 3), "0.0.4");
    }

    #[test]
    fn drops_build_metadata() {
        assert_eq!(numeric_version("1.2.3+build.42", 4), "1.2.3.0");
    }

    #[test]
    fn pads_missing_parts() {
        assert_eq!(numeric_version("1", 4), "1.0.0.0");
        assert_eq!(numeric_version("1.2", 4), "1.2.0.0");
        assert_eq!(numeric_version("1.2", 3), "1.2.0");
    }

    #[test]
    fn truncates_extra_parts() {
        assert_eq!(numeric_version("1.2.3.4.5", 4), "1.2.3.4");
        assert_eq!(numeric_version("1.2.3.4", 3), "1.2.3");
    }

    #[test]
    fn passes_through_already_normal() {
        assert_eq!(numeric_version("1.2.3.4", 4), "1.2.3.4");
        assert_eq!(numeric_version("1.2.3", 3), "1.2.3");
    }

    #[test]
    fn handles_garbage_suffix_per_part() {
        assert_eq!(numeric_version("1abc.2def.3ghi", 4), "1.2.3.0");
    }
}

#[cfg(test)]
mod manifest_emission_tests {
    use super::*;

    #[test]
    fn linux_manifest_emits_protocol_required() {
        let manifest = Manifest {
            version: "0.0.5-alpha".to_string(),
            min_version: "0.0.4-alpha".to_string(),
            target: "linux-amd64".to_string(),
            files: BTreeMap::new(),
            sandbox_protocol_required: Some(LINUX_SANDBOX_PROTOCOL_REQUIRED),
            delta_from: None,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            json.contains("\"sandbox_protocol_required\":1"),
            "Linux manifest must carry the protocol field; got: {json}"
        );
    }

    #[test]
    fn windows_manifest_omits_protocol_required() {
        let manifest = Manifest {
            version: "0.0.5-alpha".to_string(),
            min_version: "0.0.4-alpha".to_string(),
            target: "windows-amd64".to_string(),
            files: BTreeMap::new(),
            sandbox_protocol_required: None,
            delta_from: None,
        };
        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            !json.contains("sandbox_protocol_required"),
            "Windows manifest must omit the protocol field; got: {json}"
        );
    }
}

/// Files in `new` that are new or whose sha256 differs from `old`. Files only
/// in `old` (removed) are not returned; deletions are handled by the updater's
/// manifest diff at apply time, not by the delta archive.
fn delta_changed_files(old: &Manifest, new: &Manifest) -> Vec<String> {
    new.files
        .iter()
        .filter(|(path, entry)| {
            old.files
                .get(path.as_str())
                .map(|o| o.sha256 != entry.sha256)
                .unwrap_or(true)
        })
        .map(|(path, _)| path.clone())
        .collect()
}

fn delta(flags: &[String]) -> Result<(), String> {
    let mut target: Option<String> = None;
    let mut arch: Option<String> = None;
    let mut old_manifest: Option<String> = None;
    let mut dist: Option<String> = None;
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--target" => {
                i += 1;
                target = Some(flags.get(i).ok_or("--target requires a value")?.clone());
            }
            "--arch" => {
                i += 1;
                arch = Some(flags.get(i).ok_or("--arch requires a value")?.clone());
            }
            "--old-manifest" => {
                i += 1;
                old_manifest = Some(
                    flags
                        .get(i)
                        .ok_or("--old-manifest requires a value")?
                        .clone(),
                );
            }
            "--dist" => {
                i += 1;
                dist = Some(flags.get(i).ok_or("--dist requires a value")?.clone());
            }
            other => return Err(format!("unknown delta flag: {other}")),
        }
        i += 1;
    }
    let target = target.ok_or("--target is required (win32|linux)")?;
    let arch = arch.ok_or("--arch is required (amd64|arm64)")?;
    if arch != "amd64" && arch != "arm64" {
        return Err(format!("--arch must be amd64 or arm64, got: {arch}"));
    }
    let old_manifest_path = old_manifest.ok_or("--old-manifest is required")?;

    let project_root = project_root()?;
    let dist_dir = match dist {
        Some(d) => PathBuf::from(d),
        None => project_root.join("dist"),
    };

    let new: Manifest = serde_json::from_str(
        &fs::read_to_string(dist_dir.join("manifest.json"))
            .map_err(|e| format!("read new manifest: {e}"))?,
    )
    .map_err(|e| format!("parse new manifest: {e}"))?;
    let old: Manifest = serde_json::from_str(
        &fs::read_to_string(&old_manifest_path).map_err(|e| format!("read old manifest: {e}"))?,
    )
    .map_err(|e| format!("parse old manifest: {e}"))?;

    let changed = delta_changed_files(&old, &new);
    println!(
        "delta: {} changed/new files vs {}",
        changed.len(),
        old.version
    );

    // Token + extension mirror src/updater/mod.rs::ARCHIVE_SUFFIX.
    let (token, ext) = match (target.as_str(), arch.as_str()) {
        ("win32", "amd64") => ("win32_x64", "zip"),
        ("win32", "arm64") => ("win32_arm64", "zip"),
        ("linux", "amd64") => ("linux_amd64", "tar.gz"),
        ("linux", "arm64") => ("linux_arm64", "tar.gz"),
        _ => {
            return Err(format!(
                "unsupported --target/--arch combo: {target}/{arch}"
            ));
        }
    };
    let version = new.version.clone();
    let out_dir = project_root.join("target").join("installer");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let out = out_dir.join(format!("tidalunar_{version}_update_{token}.{ext}"));

    match target.as_str() {
        "linux" => {
            // Wrapped top-level dir, matching the full tarball so the updater's
            // strip-1 extraction works unchanged.
            let wrap = format!("tidalunar_{version}_update_{token}");
            let stage_root = out_dir.join(format!("delta-build-{token}"));
            if stage_root.exists() {
                fs::remove_dir_all(&stage_root).map_err(|e| e.to_string())?;
            }
            let stage = stage_root.join(&wrap);
            for rel in &changed {
                let from = dist_dir.join(rel);
                let to = stage.join(rel);
                if let Some(p) = to.parent() {
                    fs::create_dir_all(p).map_err(|e| e.to_string())?;
                }
                fs::copy(&from, &to).map_err(|e| format!("copy {rel}: {e}"))?;
            }
            let status = Command::new("tar")
                .args(["-czf"])
                .arg(&out)
                .args(["-C"])
                .arg(&stage_root)
                .arg(&wrap)
                .status()
                .map_err(|e| format!("tar spawn: {e}"))?;
            if !status.success() {
                return Err(format!("tar failed: {status}"));
            }
        }
        "win32" => {
            // Flat zip, matching the full windows bundle.
            use std::io::Write as _;
            let file = fs::File::create(&out).map_err(|e| format!("create zip: {e}"))?;
            let mut zip = zip::ZipWriter::new(file);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for rel in &changed {
                let data = fs::read(dist_dir.join(rel)).map_err(|e| format!("read {rel}: {e}"))?;
                zip.start_file(rel.replace('\\', "/"), opts)
                    .map_err(|e| e.to_string())?;
                zip.write_all(&data).map_err(|e| e.to_string())?;
            }
            zip.finish().map_err(|e| e.to_string())?;
        }
        _ => unreachable!(),
    }
    println!("delta archive created: {}", out.display());
    Ok(())
}

#[cfg(test)]
mod delta_diff_tests {
    use super::*;
    use std::collections::BTreeMap;

    fn entry(sha: &str) -> FileEntry {
        FileEntry {
            sha256: sha.to_string(),
            size: 1,
        }
    }

    fn manifest_with(files: &[(&str, &str)]) -> Manifest {
        Manifest {
            version: "0.0.5-alpha".into(),
            min_version: "0.0.4-alpha".into(),
            target: "linux-amd64".into(),
            files: files
                .iter()
                .map(|(p, s)| (p.to_string(), entry(s)))
                .collect::<BTreeMap<_, _>>(),
            sandbox_protocol_required: None,
            delta_from: None,
        }
    }

    #[test]
    fn changed_set_includes_new_and_modified_only() {
        let old = manifest_with(&[("a", "h1"), ("b", "h2"), ("gone", "h3")]);
        let new = manifest_with(&[("a", "h1"), ("b", "h2_changed"), ("c", "h4")]);
        let mut changed = delta_changed_files(&old, &new);
        changed.sort();
        assert_eq!(changed, vec!["b".to_string(), "c".to_string()]);
    }
}

fn package(flags: &[String]) -> Result<(), String> {
    let mut arch: Option<String> = None;
    let mut target: String = "windows-nsis".into();
    let mut i = 0;
    while i < flags.len() {
        match flags[i].as_str() {
            "--release" => {} // payload was built by `bundle`
            "--target" => {
                i += 1;
                target = flags.get(i).ok_or("--target requires a value")?.clone();
            }
            "--arch" => {
                i += 1;
                arch = Some(flags.get(i).ok_or("--arch requires a value")?.clone());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    let arch = arch.ok_or(
        "--arch <amd64|arm64> is required (no host-default to avoid cross-build mismatch)",
    )?;
    if arch != "amd64" && arch != "arm64" {
        return Err(format!("--arch must be amd64 or arm64, got: {arch}"));
    }

    match target.as_str() {
        "windows-nsis" => package_windows_nsis(&arch),
        "linux-deb" => package_linux_deb(&arch),
        "linux-tarball" => package_linux_tarball(&arch),
        other => Err(format!(
            "unknown --target: {other} (expected windows-nsis | linux-deb | linux-tarball)"
        )),
    }
}

fn package_windows_nsis(arch: &str) -> Result<(), String> {
    let project_root = project_root()?;
    let dist = project_root.join("dist");
    if !dist.is_dir()
        || fs::read_dir(&dist)
            .map_err(|e| e.to_string())?
            .next()
            .is_none()
    {
        return Err("dist/ is empty or missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join("manifest.json").is_file() {
        return Err("dist/manifest.json missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join("updater.exe").is_file() {
        return Err("dist/updater.exe missing - run `cargo xtask build-updater` first".into());
    }

    // Validate dist payload arch matches --arch.
    let manifest_data =
        fs::read_to_string(dist.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&manifest_data).map_err(|e| e.to_string())?;
    let expected = format!("windows-{arch}");
    if manifest.target != expected {
        return Err(format!(
            "dist/ payload target {} does not match --arch {} (expected {})",
            manifest.target, arch, expected
        ));
    }

    // Prefer makensis on PATH, but fall back to the standard NSIS install
    // location on Windows: `choco install nsis` drops makensis.exe under
    // Program Files without reliably adding it to PATH.
    fn resolve_makensis() -> Option<PathBuf> {
        if Command::new("makensis").arg("/VERSION").output().is_ok() {
            return Some(PathBuf::from("makensis"));
        }
        #[cfg(windows)]
        {
            for var in ["ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"] {
                if let Some(base) = std::env::var_os(var) {
                    let candidate = Path::new(&base).join("NSIS").join("makensis.exe");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }
    let makensis = resolve_makensis().ok_or_else(|| {
        "makensis not found - install via `apt install nsis` (Linux) or `choco install nsis` (Windows)"
            .to_string()
    })?;

    let out_dir = project_root.join("target").join("installer");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    // Windows arch token for filenames (x64 instead of Debian's amd64).
    let win_arch = if arch == "amd64" { "x64" } else { "arm64" };

    // Remove any prior installers for this arch so older builds (typically a
    // version-suffixed .exe restored from cache or left from a local version
    // bump) cannot be picked up by downstream globbers in CI (signtool,
    // Get-ChildItem | Select-Object -First 1, upload-artifact path glob).
    let stale_suffix = format!("_{win_arch}.exe");
    for entry in fs::read_dir(&out_dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with("tidalunar_") && name_str.ends_with(&stale_suffix) {
            fs::remove_file(entry.path())
                .map_err(|e| format!("remove stale installer {name_str}: {e}"))?;
        }
    }

    let version = read_workspace_version()?;
    // NSIS VIProductVersion requires X.X.X.X with all numeric components, so
    // strip any semver prerelease/build suffix and pad to 4 parts. The
    // human-readable version still ships in DisplayVersion + filename.
    let version_numeric = numeric_version(&version, 4);
    let nsi = project_root
        .join("installer")
        .join("windows")
        .join("tidalunar.nsi");

    // Pre-compress the payload OUTSIDE makensis. makensis' builtin LZMA is
    // single-threaded and uses an outdated codec with no executable filter; on
    // the CEF payload that measured ~14% larger and ~2x slower than 7-Zip's
    // multithreaded LZMA2. So we build the solid .7z here and have the installer
    // extract it at run time via the bundled official 7zr.exe (see tidalunar.nsi).
    fn resolve_7z() -> Option<PathBuf> {
        for cand in ["7z", "7za"] {
            if Command::new(cand).output().is_ok() {
                return Some(PathBuf::from(cand));
            }
        }
        #[cfg(windows)]
        {
            for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
                if let Some(base) = std::env::var_os(var) {
                    let candidate = Path::new(&base).join("7-Zip").join("7z.exe");
                    if candidate.is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
        None
    }
    let sevenzip =
        resolve_7z().ok_or("7z/7za not found - install p7zip-full (Linux) or 7-Zip (Windows)")?;

    // Official 7-Zip standalone extractor shipped INSIDE the installer to
    // decompress payload.7z on the user's machine. Sourced via env (CI
    // downloads it and pins its SHA-256) so no binary blob lives in git.
    let sevenzr = std::env::var_os("TIDALUNAR_7ZR_EXE")
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .ok_or(
            "7zr.exe not found - set TIDALUNAR_7ZR_EXE to the official 7-Zip standalone \
             extractor (CI downloads it; locally point it at a trusted 7zr.exe)",
        )?;

    // Build the solid, max-compression payload archive. cwd = dist so entries
    // store at archive root (no `dist/` prefix), matching extraction straight
    // into $INSTDIR. -mmt=on uses all runner cores.
    let payload = out_dir.join("payload.7z");
    let _ = fs::remove_file(&payload);
    println!("Compressing payload with 7-Zip (LZMA2, solid, multithreaded)...");
    let status = Command::new(&sevenzip)
        .current_dir(&dist)
        .args(["a", "-t7z", "-mx=9", "-ms=on", "-mmt=on"])
        .arg(&payload)
        .arg(".")
        .status()
        .map_err(|e| format!("7z spawn: {e}"))?;
    if !status.success() {
        return Err(format!("7z failed: {status}"));
    }
    if !payload.is_file() {
        return Err("7z reported success but payload.7z is missing".into());
    }

    // AddSize wants the decompressed total in KB (NSIS can't see inside the .7z).
    let payload_kb = dir_size_bytes(&dist)?.div_ceil(1024);

    println!(
        "Packaging installer: target={expected} version={version} (numeric={version_numeric})"
    );
    let status = Command::new(&makensis)
        .arg(format!("-DVERSION={version}"))
        .arg(format!("-DVERSION_NUMERIC={version_numeric}"))
        .arg(format!("-DARCH={win_arch}"))
        .arg(format!("-DPAYLOAD_7Z={}", payload.display()))
        .arg(format!("-DSEVENZR_EXE={}", sevenzr.display()))
        .arg(format!("-DPAYLOAD_KB={payload_kb}"))
        .arg(format!("-DOUT_DIR={}", out_dir.display()))
        .arg(&nsi)
        .status()
        .map_err(|e| format!("makensis spawn: {e}"))?;
    if !status.success() {
        return Err(format!("makensis failed: {status}"));
    }

    let exe = out_dir.join(format!("tidalunar_{version}_{win_arch}.exe"));
    if !exe.is_file() {
        return Err(format!(
            "expected installer at {} but file is missing",
            exe.display()
        ));
    }
    println!("Installer created: {}", exe.display());
    Ok(())
}

fn package_linux_deb(arch: &str) -> Result<(), String> {
    let project_root = project_root()?;
    let dist = project_root.join("dist");
    if !dist.is_dir()
        || fs::read_dir(&dist)
            .map_err(|e| e.to_string())?
            .next()
            .is_none()
    {
        return Err("dist/ is empty or missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join("manifest.json").is_file() {
        return Err("dist/manifest.json missing - run `cargo xtask bundle` first".into());
    }
    if !dist.join("updater").is_file() {
        return Err("dist/updater missing - run `cargo xtask build-updater` first".into());
    }

    // Validate dist payload arch matches --arch.
    let manifest_data =
        fs::read_to_string(dist.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&manifest_data).map_err(|e| e.to_string())?;
    let expected = format!("linux-{arch}");
    if manifest.target != expected {
        return Err(format!(
            "dist/ payload target {} does not match --arch {} (expected {})",
            manifest.target, arch, expected
        ));
    }

    if Command::new("dpkg-deb").arg("--version").output().is_err() {
        return Err("dpkg-deb not on PATH - install dpkg-dev (apt install dpkg-dev)".into());
    }

    let version = read_workspace_version()?;
    let out_dir = project_root.join("target").join("installer");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    // Wipe any prior deb-build for this arch.
    let stage = out_dir.join(format!("deb-build-{arch}"));
    if stage.exists() {
        fs::remove_dir_all(&stage).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(&stage).map_err(|e| e.to_string())?;

    // 1. DEBIAN/ control + scripts
    let debian_dir = stage.join("DEBIAN");
    fs::create_dir_all(&debian_dir).map_err(|e| e.to_string())?;

    let installer_deb = project_root.join("installer").join("linux").join("deb");
    let control_template = fs::read_to_string(installer_deb.join("control.in"))
        .map_err(|e| format!("read control.in: {e}"))?;
    let control = control_template
        .replace("{VERSION}", &version)
        .replace("{ARCH}", arch);
    fs::write(debian_dir.join("control"), control).map_err(|e| e.to_string())?;

    for script in ["postinst", "postrm"] {
        let src = installer_deb.join(script);
        let dst = debian_dir.join(script);
        fs::copy(&src, &dst).map_err(|e| format!("copy {script}: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dst, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("chmod {script}: {e}"))?;
        }
    }

    // 2. /usr/bin/tidalunar.real launcher
    let usr_bin = stage.join("usr").join("bin");
    fs::create_dir_all(&usr_bin).map_err(|e| e.to_string())?;
    let launcher_dst = usr_bin.join("tidalunar.real");
    fs::copy(installer_deb.join("tidalunar-launcher.sh"), &launcher_dst)
        .map_err(|e| format!("copy launcher: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&launcher_dst, fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }

    // 3. /usr/share/applications/tidalunar.desktop
    let usr_apps = stage.join("usr").join("share").join("applications");
    fs::create_dir_all(&usr_apps).map_err(|e| e.to_string())?;
    fs::copy(
        installer_deb.join("tidalunar.desktop.in"),
        usr_apps.join("tidalunar.desktop"),
    )
    .map_err(|e| format!("copy desktop: {e}"))?;

    // 4. /usr/share/icons/hicolor/{size}/apps/tidalunar.png via convert.
    let icon_src = project_root.join("tidaluna.png");
    if !icon_src.is_file() {
        return Err("tidaluna.png missing at project root".into());
    }
    for size in [16u32, 32, 64, 128, 256, 512] {
        let icon_dir = stage
            .join("usr")
            .join("share")
            .join("icons")
            .join("hicolor")
            .join(format!("{size}x{size}"))
            .join("apps");
        fs::create_dir_all(&icon_dir).map_err(|e| e.to_string())?;
        let dst = icon_dir.join("tidalunar.png");
        let status = Command::new("convert")
            .arg(&icon_src)
            .args(["-resize", &format!("{size}x{size}")])
            .arg(&dst)
            .status()
            .map_err(|e| format!("convert spawn: {e}"))?;
        if !status.success() {
            return Err(format!("convert failed for size {size}"));
        }
    }

    // 5. /etc/apparmor.d/tidalunar
    let etc_apparmor = stage.join("etc").join("apparmor.d");
    fs::create_dir_all(&etc_apparmor).map_err(|e| e.to_string())?;
    fs::copy(
        installer_deb.join("apparmor-profile"),
        etc_apparmor.join("tidalunar"),
    )
    .map_err(|e| format!("copy apparmor profile: {e}"))?;

    // 6. /opt/tidalunar/bin/cef/chrome-sandbox (from dist/)
    let opt_cef = stage.join("opt").join("tidalunar").join("bin").join("cef");
    fs::create_dir_all(&opt_cef).map_err(|e| e.to_string())?;
    let cs_src = dist.join("bin").join("cef").join("chrome-sandbox");
    if !cs_src.is_file() {
        return Err(format!(
            "{} missing - run `cargo xtask bundle` first",
            cs_src.display()
        ));
    }
    fs::copy(&cs_src, opt_cef.join("chrome-sandbox"))
        .map_err(|e| format!("copy chrome-sandbox: {e}"))?;
    // Default mode 0755; postinst probes and chmods to 4755 if needed.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(
            opt_cef.join("chrome-sandbox"),
            fs::Permissions::from_mode(0o755),
        )
        .map_err(|e| e.to_string())?;
    }

    // 7. /usr/lib/tidalunar/payload.tar.zst + SANDBOX_PROTOCOL_VERSION
    let usr_lib = stage.join("usr").join("lib").join("tidalunar");
    fs::create_dir_all(&usr_lib).map_err(|e| e.to_string())?;
    let payload = usr_lib.join("payload.tar.zst");

    println!("Compressing payload tarball (this may take a minute)...");
    let tar_status = Command::new("tar")
        .args(["-I", "zstd -10 -T0", "-cf"])
        .arg(&payload)
        .args(["-C"])
        .arg(&dist)
        .arg(".")
        .status()
        .map_err(|e| format!("tar spawn: {e}"))?;
    if !tar_status.success() {
        return Err(format!("tar failed: {tar_status}"));
    }

    fs::write(
        usr_lib.join("SANDBOX_PROTOCOL_VERSION"),
        format!("{}\n", LINUX_SANDBOX_PROTOCOL_REQUIRED),
    )
    .map_err(|e| e.to_string())?;

    // 8. dpkg-deb --build. -Znone: the data.tar's bulk is the already-zstd
    //    payload.tar.zst (incompressible), so dpkg's default recompression
    //    would burn time re-squeezing it for no gain. Skipping it keeps the
    //    build fast; the few MB of uncompressed control files are negligible
    //    next to the payload. (Same rationale as `SetCompress off` on Windows.)
    let out_deb = out_dir.join(format!("tidalunar_{version}_{arch}.deb"));
    println!("Building .deb at {}", out_deb.display());
    let status = Command::new("dpkg-deb")
        .args(["--build", "--root-owner-group", "-Znone"])
        .arg(&stage)
        .arg(&out_deb)
        .status()
        .map_err(|e| format!("dpkg-deb spawn: {e}"))?;
    if !status.success() {
        return Err(format!("dpkg-deb failed: {status}"));
    }

    println!(".deb created: {}", out_deb.display());
    Ok(())
}

fn package_linux_tarball(arch: &str) -> Result<(), String> {
    let project_root = project_root()?;
    let dist = project_root.join("dist");
    if !dist.is_dir() {
        return Err("dist/ missing - run `cargo xtask bundle` first".into());
    }

    let manifest_data =
        fs::read_to_string(dist.join("manifest.json")).map_err(|e| e.to_string())?;
    let manifest: Manifest = serde_json::from_str(&manifest_data).map_err(|e| e.to_string())?;
    let expected = format!("linux-{arch}");
    if manifest.target != expected {
        return Err(format!(
            "dist/ payload target {} does not match --arch {} (expected {})",
            manifest.target, arch, expected
        ));
    }

    let version = read_workspace_version()?;
    let out_dir = project_root.join("target").join("installer");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;

    let stage_root = out_dir.join(format!("tarball-build-{arch}"));
    if stage_root.exists() {
        fs::remove_dir_all(&stage_root).map_err(|e| e.to_string())?;
    }
    let bundle_name = format!("tidalunar_{version}_linux_{arch}");
    let stage = stage_root.join(&bundle_name);
    fs::create_dir_all(&stage).map_err(|e| e.to_string())?;

    // Recursively copy dist/ into stage/.
    copy_dir_all(&dist, &stage)?;

    // Drop in the README.
    fs::copy(
        project_root
            .join("installer")
            .join("linux")
            .join("tarball")
            .join("README"),
        stage.join("README"),
    )
    .map_err(|e| format!("copy README: {e}"))?;

    let out_tarball = out_dir.join(format!("{bundle_name}.tar.gz"));
    let status = Command::new("tar")
        .arg("-czf")
        .arg(&out_tarball)
        .arg("-C")
        .arg(&stage_root)
        .arg(&bundle_name)
        .status()
        .map_err(|e| format!("tar spawn: {e}"))?;
    if !status.success() {
        return Err(format!("tar failed: {status}"));
    }

    println!(".tar.gz created: {}", out_tarball.display());
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), String> {
    fs::create_dir_all(dst).map_err(|e| e.to_string())?;
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}
