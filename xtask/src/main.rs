use std::fs;
use std::path::Path;
use std::process::{Command, exit};

mod bundle;
mod delta;
mod manifest;
mod package;

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
        Some("bundle") => bundle::bundle(&args[1..]),
        Some("build-updater") => bundle::build_updater(&args[1..]),
        Some("package") => package::package(&args[1..]),
        Some("delta") => delta::delta(&args[1..]),
        Some("generate-keypair") => manifest::generate_keypair(),
        Some("sign-manifest") => manifest::sign_manifest(),
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
    eprintln!("                   --prebuilt-front  Reuse frontend/dist instead of running bun");
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
    // A bare invocation lints the root package alone. A new member is linted without
    // being named here: `--workspace` follows the members list.
    run(
        "cargo",
        &[
            "clippy",
            "--workspace",
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

/// `files.rs` as xtask reads it: the list of files it ships.
macro_rules! emit_top_files {
    ($($name:ident = $file:literal,)*) => {
        pub(crate) const TOP_FILES: [&str; [$($file),*].len()] = [$($file),*];
    };
}
/// The allowlist `src/frontend/load.rs` reads: a copy of all of `frontend/dist` would ship
/// whatever a past build left there, hashed into the manifest.
#[path = "../../src/frontend/files.rs"]
mod front_files;

/// The repository the app updates from, for the texts that send users to its releases.
#[path = "../../src/updater/repo.rs"]
mod repo;

fn run_in(cmd: &Path, args: &[&str], dir: &Path) -> Result<(), String> {
    let status = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .status()
        .map_err(|e| format!("failed to run {}: {e}", cmd.display()))?;
    if !status.success() {
        return Err(format!("{} {} failed", cmd.display(), args.join(" ")));
    }
    Ok(())
}

/// Runs `cmd` to completion; `name` prefixes the spawn error and the failed-exit error.
fn run_status(cmd: &mut Command, name: &str) -> Result<(), String> {
    let status = cmd.status().map_err(|e| format!("{name} spawn: {e}"))?;
    if !status.success() {
        return Err(format!("{name} failed: {status}"));
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
        if trimmed.starts_with("version")
            && let Some(v) = trimmed.split('"').nth(1)
        {
            return Ok(v.to_string());
        }
    }
    Err("version not found in root Cargo.toml".to_string())
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

/// The value after the flag at `*i`, moving `*i` onto it.
fn flag_value(flags: &[String], i: &mut usize) -> Result<String, String> {
    let flag = &flags[*i];
    *i += 1;
    flags
        .get(*i)
        .cloned()
        .ok_or_else(|| format!("{flag} requires a value"))
}

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
