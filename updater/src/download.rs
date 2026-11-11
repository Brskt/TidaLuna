use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::manifest::verify_manifest_signature;
use crate::platform::TARGET;
use crate::repo::{GITHUB_OWNER, GITHUB_REPO};
use crate::{GhRelease, Manifest};

pub(crate) fn fetch_release(
    client: &reqwest::blocking::Client,
    version: &str,
) -> Result<GhRelease> {
    let url = format!(
        "https://api.github.com/repos/{GITHUB_OWNER}/{GITHUB_REPO}/releases/tags/{version}"
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .context("failed to fetch release")?;

    if !resp.status().is_success() {
        bail!(
            "GitHub API returned {}: {}",
            resp.status(),
            resp.text().unwrap_or_default()
        );
    }

    resp.json::<GhRelease>()
        .context("failed to parse release JSON")
}

/// Cap on the metadata downloads (manifest JSON + detached signature). Small
/// first-party files; the bound stops an oversized body from being buffered
/// before the signature can be verified.
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// Read a response body into memory, failing if it exceeds `max` bytes.
fn read_capped(mut reader: impl Read, max: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    reader
        .by_ref()
        .take(max + 1)
        .read_to_end(&mut buf)
        .context("read response body")?;
    if buf.len() as u64 > max {
        bail!("download exceeds the {max}-byte cap");
    }
    Ok(buf)
}

pub(crate) fn download_and_verify_manifest(
    client: &reqwest::blocking::Client,
    release: &GhRelease,
) -> Result<Manifest> {
    let manifest_name = format!("manifest-{TARGET}.json");
    let sig_name = format!("manifest-{TARGET}.json.sig");

    let manifest_url = release
        .assets
        .iter()
        .find(|a| a.name == manifest_name)
        .context(format!("release missing {manifest_name}"))?
        .browser_download_url
        .clone();

    let sig_url = release
        .assets
        .iter()
        .find(|a| a.name == sig_name)
        .context(format!("release missing {sig_name}"))?
        .browser_download_url
        .clone();

    let manifest_resp = client
        .get(&manifest_url)
        .send()
        .context("download manifest")?;
    let manifest_bytes =
        read_capped(manifest_resp, MAX_MANIFEST_BYTES).context("read manifest bytes")?;

    let sig_resp = client.get(&sig_url).send().context("download signature")?;
    let sig_b64 =
        String::from_utf8(read_capped(sig_resp, MAX_MANIFEST_BYTES).context("read signature")?)
            .context("signature not utf8")?;

    verify_manifest_signature(&manifest_bytes, &sig_b64)?;

    // Parse manifest
    let manifest: Manifest =
        serde_json::from_slice(&manifest_bytes).context("invalid manifest JSON")?;

    Ok(manifest)
}

/// Hard ceiling on an update archive download. Far above any real release;
/// bounds memory and disk if a tampered or misbehaving server streams an
/// oversized body before the post-extraction hash check can reject it.
const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(crate) fn download_file(
    client: &reqwest::blocking::Client,
    url: &str,
    dest: &Path,
) -> Result<()> {
    let resp = client.get(url).send().context("download failed")?;
    if !resp.status().is_success() {
        bail!("download returned {}", resp.status());
    }
    stream_to_file(resp, dest, MAX_ARCHIVE_BYTES)
}

/// Stream `reader` to `dest`, failing if more than `max` bytes arrive.
fn stream_to_file(mut reader: impl Read, dest: &Path, max: u64) -> Result<()> {
    let mut file = fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    // take(max + 1): an over-cap body trips the check after writing at most
    // one byte past the limit, never buffering or storing the whole stream.
    let copied = std::io::copy(&mut reader.by_ref().take(max + 1), &mut file)
        .with_context(|| format!("write to {}", dest.display()))?;
    if copied > max {
        drop(file);
        fs::remove_file(dest).ok();
        bail!("download exceeds the {max}-byte archive cap");
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/download/download_tests.rs"]
mod download_tests;
