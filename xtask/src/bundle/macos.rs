use std::fs;
use std::path::Path;

use crate::{numeric_version, read_workspace_version};

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
///       Resources/tidaluna.icns
///       Info.plist
pub(super) fn bundle_macos(
    exe_name: &str,
    project_root: &Path,
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

    // Bundle icon, read by Finder, Spotlight and the Dock while the app is not
    // running: no runtime code can stand in for it. Absent it, the .app ships
    // with the generic placeholder, hence the hard failure over a silent skip.
    let icon_name = "tidaluna.icns";
    let icon_src = project_root.join(icon_name);
    if !icon_src.is_file() {
        return Err(format!("missing app icon: {}", icon_src.display()));
    }
    fs::copy(&icon_src, resources_dir.join(icon_name))
        .map_err(|e| format!("copy app icon: {e}"))?;
    println!("  Copied {icon_name}");

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

    // Helpers are LSUIElement, surfacing in neither Dock nor Finder, and need
    // no bundle icon. CFBundleIconFile names a file in Resources/, unlike
    // CFBundleIconName which would want an actool-compiled asset catalog.
    let icon_file = if is_helper {
        ""
    } else {
        "\n    <key>CFBundleIconFile</key>\n    <string>tidaluna.icns</string>"
    };

    // Only the main bundle claims the scheme. A helper declaring it too would
    // give Launch Services five candidates for one click.
    let url_types = if is_helper {
        ""
    } else {
        r#"
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key>
            <string>com.tidalunar.tidal</string>
            <key>CFBundleURLSchemes</key>
            <array>
                <string>tidal</string>
            </array>
        </dict>
    </array>"#
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
    <true/>{ui_element}{icon_file}{url_types}
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
