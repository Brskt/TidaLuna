use std::fs;
use std::process::Command;

use super::render_template;
use crate::manifest::{LINUX_SANDBOX_PROTOCOL_REQUIRED, Manifest};
use crate::{project_root, read_workspace_version, repo, run_status};

fn releases_url() -> String {
    format!(
        "https://github.com/{}/{}/releases",
        repo::GITHUB_OWNER,
        repo::GITHUB_REPO
    )
}

/// The .deb's file name, which the tarball's README also gives.
pub(super) fn deb_file_name(version: &str, arch: &str) -> String {
    format!("tidalunar_{version}_{arch}.deb")
}

pub(super) fn package_linux_deb(arch: &str) -> Result<(), String> {
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
    let control = render_template(&control_template, &[("VERSION", &version), ("ARCH", arch)])
        .map_err(|e| format!("control.in: {e}"))?;
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
    let launcher_template = fs::read_to_string(installer_deb.join("tidalunar-launcher.sh"))
        .map_err(|e| format!("read launcher: {e}"))?;
    let launcher = render_template(&launcher_template, &[("RELEASES_URL", &releases_url())])
        .map_err(|e| format!("tidalunar-launcher.sh: {e}"))?;
    fs::write(&launcher_dst, launcher).map_err(|e| format!("write launcher: {e}"))?;
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

    // 4. /usr/share/icons/hicolor/{size}/apps/tidalunar.png - pre-rendered assets.
    for size in [16u32, 32, 64, 128, 256] {
        let rel = format!("icons/hicolor/{size}x{size}/apps/tidalunar.png");
        let icon_src = installer_deb.join(&rel);
        if !icon_src.is_file() {
            return Err(format!("missing icon asset: {}", icon_src.display()));
        }
        let icon_dir = stage
            .join("usr")
            .join("share")
            .join("icons")
            .join("hicolor")
            .join(format!("{size}x{size}"))
            .join("apps");
        fs::create_dir_all(&icon_dir).map_err(|e| e.to_string())?;
        fs::copy(&icon_src, icon_dir.join("tidalunar.png"))
            .map_err(|e| format!("copy icon {size}: {e}"))?;
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
    run_status(
        Command::new("tar")
            .args(["-I", "zstd -10 -T0", "-cf"])
            .arg(&payload)
            .args(["-C"])
            .arg(&dist)
            .arg("."),
        "tar",
    )?;

    fs::write(
        usr_lib.join("SANDBOX_PROTOCOL_VERSION"),
        format!("{}\n", LINUX_SANDBOX_PROTOCOL_REQUIRED),
    )
    .map_err(|e| e.to_string())?;

    // 8. dpkg-deb --build. -Znone: the data.tar's bulk is the already-zstd
    //    payload.tar.zst (incompressible). dpkg's default recompression would
    //    burn time re-squeezing it for no gain. Skipping it keeps the build
    //    fast; the few MB of uncompressed control files are negligible next
    //    to the payload. (Same rationale as `SetCompress off` on Windows.)
    let out_deb = out_dir.join(deb_file_name(&version, arch));
    println!("Building .deb at {}", out_deb.display());
    run_status(
        Command::new("dpkg-deb")
            .args(["--build", "--root-owner-group", "-Znone"])
            .arg(&stage)
            .arg(&out_deb),
        "dpkg-deb",
    )?;

    println!(".deb created: {}", out_deb.display());
    Ok(())
}
