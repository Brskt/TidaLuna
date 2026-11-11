mod deb;
mod nsis;
mod tarball;

/// Fills each `{KEY}` of an installer template. A key the template does not hold is an error: the
/// value would be shipped nowhere, and a template edited by hand drifts from what the code knows.
pub(super) fn render_template(template: &str, pairs: &[(&str, &str)]) -> Result<String, String> {
    let mut out = template.to_string();
    for (key, value) in pairs {
        let token = format!("{{{key}}}");
        if !out.contains(&token) {
            return Err(format!("template has no {token}"));
        }
        out = out.replace(&token, value);
    }
    Ok(out)
}

pub(crate) fn package(flags: &[String]) -> Result<(), String> {
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
        "windows-nsis" => nsis::package_windows_nsis(&arch),
        "linux-deb" => deb::package_linux_deb(&arch),
        "linux-tarball" => tarball::package_linux_tarball(&arch),
        other => Err(format!(
            "unknown --target: {other} (expected windows-nsis | linux-deb | linux-tarball)"
        )),
    }
}
