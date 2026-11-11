use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::{Args, cleanup};

pub(crate) fn parse_args() -> Result<Args> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pid = None;
    let mut version = None;
    let mut app_dir = None;
    let mut skip_download = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pid" => {
                i += 1;
                pid = Some(
                    args.get(i)
                        .context("--pid requires a value")?
                        .parse::<u32>()?,
                );
            }
            "--version" => {
                i += 1;
                version = Some(args.get(i).context("--version requires a value")?.clone());
            }
            "--app-dir" => {
                i += 1;
                app_dir = Some(PathBuf::from(
                    args.get(i).context("--app-dir requires a value")?,
                ));
            }
            "--skip-download" => {
                skip_download = true;
            }
            other => bail!("unknown argument: {other}"),
        }
        i += 1;
    }

    Ok(Args {
        pid: pid.context("--pid is required")?,
        version: version.context("--version is required")?,
        app_dir: app_dir.context("--app-dir is required")?,
        skip_download,
    })
}

pub(crate) fn run_cleanup_stale() -> i32 {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let mut app_dir: Option<PathBuf> = None;
    let mut old_manifest: Option<PathBuf> = None;
    let mut new_manifest: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--app-dir" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    say!("[cleanup-stale] --app-dir requires a value");
                    return 2;
                };
                app_dir = Some(PathBuf::from(v));
            }
            "--old-manifest" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    say!("[cleanup-stale] --old-manifest requires a value");
                    return 2;
                };
                old_manifest = Some(PathBuf::from(v));
            }
            "--new-manifest" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    say!("[cleanup-stale] --new-manifest requires a value");
                    return 2;
                };
                new_manifest = Some(PathBuf::from(v));
            }
            other => {
                say!("[cleanup-stale] unknown argument: {other}");
                return 2;
            }
        }
        i += 1;
    }
    let (Some(app), Some(old), Some(new)) = (app_dir, old_manifest, new_manifest) else {
        say!("[cleanup-stale] need --app-dir, --old-manifest, --new-manifest");
        return 2;
    };
    match cleanup::cleanup_stale(&app, &old, &new) {
        Ok(n) => {
            say!("[cleanup-stale] removed {n} stale file(s)");
            0
        }
        Err(e) => {
            say!("[cleanup-stale] failed: {e:#}");
            1
        }
    }
}
