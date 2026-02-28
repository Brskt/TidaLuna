use std::process::{Command, exit};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let result = match args.first().map(|s| s.as_str()) {
        Some("clippy") => clippy(),
        Some("fmt") => fmt(),
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
