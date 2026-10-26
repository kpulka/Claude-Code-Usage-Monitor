use semver::Version;
use serde::Deserialize;

use super::download::AssetIntegrity;
use super::github_repo;

pub(super) const RELEASE_ASSET_NAME: &str = "claude-code-usage-monitor.exe";

#[derive(Clone, Debug)]
pub struct ReleaseDescriptor {
    pub latest_version: String,
    pub(super) asset_url: String,
    pub(super) integrity: AssetIntegrity,
}

#[derive(Deserialize)]
pub(super) struct GitHubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

pub(super) fn release_descriptor(
    release: GitHubRelease,
    current: &str,
) -> Result<Option<ReleaseDescriptor>, String> {
    let latest = parse_version(&release.tag_name)?;
    let current = parse_version(current)?;
    // This updater follows stable releases, even if a tag was mislabelled in GitHub.
    if release.draft || release.prerelease || !latest.pre.is_empty() {
        return Ok(None);
    }
    if !latest.cmp_precedence(&current).is_gt() {
        return Ok(None);
    }

    let mut matches = release
        .assets
        .iter()
        .filter(|asset| asset.name == RELEASE_ASSET_NAME);
    let asset = matches
        .next()
        .ok_or_else(|| format!("The latest release is missing {RELEASE_ASSET_NAME}."))?;
    if matches.next().is_some() {
        return Err(format!(
            "The latest release has duplicate {RELEASE_ASSET_NAME} assets."
        ));
    }

    let (owner, repo) = github_repo()?;
    // A release from a foreign owner or repository must never be offered, even
    // if its asset is named like ours. Forks would otherwise silently replace
    // themselves with upstream binaries.
    if !super::github_url_belongs_to_repository(&asset.browser_download_url, owner, repo) {
        return Err(format!(
            "The update asset URL does not belong to https://github.com/{owner}/{repo}."
        ));
    }
    let expected_url = format!(
        "https://github.com/{owner}/{repo}/releases/download/{}/{RELEASE_ASSET_NAME}",
        release.tag_name
    );
    if asset.browser_download_url != expected_url {
        return Err("The update asset URL does not match the expected GitHub release.".into());
    }
    let integrity = AssetIntegrity::new(asset.size, asset.digest.as_deref())?;

    Ok(Some(ReleaseDescriptor {
        latest_version: latest.to_string(),
        asset_url: expected_url,
        integrity,
    }))
}

/// Refuses to install a release that was not published by this build's own
/// GitHub repository. Checked again right before the download starts.
pub(super) fn ensure_release_belongs_to_build(release: &ReleaseDescriptor) -> Result<(), String> {
    let (owner, repo) = github_repo()?;
    if super::github_url_belongs_to_repository(&release.asset_url, owner, repo) {
        return Ok(());
    }

    Err(format!(
        "Refusing to install a release that does not belong to https://github.com/{owner}/{repo}."
    ))
}

pub(super) fn parse_version(version: &str) -> Result<Version, String> {
    Version::parse(version.strip_prefix('v').unwrap_or(version))
        .map_err(|e| format!("Invalid release version {version:?}: {e}"))
}

#[cfg(test)]
mod tests;
