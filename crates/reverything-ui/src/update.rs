//! Updates from GitHub releases: checks for a newer release, downloads its installer, verifies
//! it against the SHA256 published with the release and runs it. The installer replaces the app
//! and the service and starts the app again; it asks for administrator rights once.
//!
//! Only installed builds update.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use windows::core::w;
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};

const LATEST_RELEASE: &str = "https://api.github.com/repos/tth05/reverything/releases/latest";
/// Automatic checks happen at most this often
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Larger downloads are refused
const MAX_DOWNLOAD: u64 = 100 * 1024 * 1024;

/// A release newer than the running version.
#[derive(Debug, Clone)]
pub struct Update {
    pub version: String,
    installer_url: String,
    checksum_url: String,
    installer_name: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// Whether this is an installed build that updates itself. Development builds never do, and
/// installs by winget or Scoop leave updates to them.
pub fn enabled() -> bool {
    installed() && managed_by().is_none()
}

fn installed() -> bool {
    !cfg!(debug_assertions)
        && std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("unins000.exe").exists()))
            .unwrap_or(false)
}

/// The package manager that installed the app and keeps it up to date, as the installer noted
/// it (`/MANAGED=winget`).
pub fn managed_by() -> Option<&'static str> {
    static MANAGED_BY: OnceLock<Option<String>> = OnceLock::new();
    MANAGED_BY
        .get_or_init(|| {
            let mut buf = [0u16; 64];
            let mut len = size_of_val(&buf) as u32;
            let result = unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    w!(r"Software\Reverything"),
                    w!("ManagedBy"),
                    RRF_RT_REG_SZ,
                    None,
                    Some(buf.as_mut_ptr() as *mut _),
                    Some(&mut len),
                )
            };
            let value = String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]);
            (result.is_ok() && !value.is_empty()).then_some(value)
        })
        .as_deref()
}

/// Why updates are off, for the settings and the About dialog.
pub fn disabled_reason() -> &'static str {
    match managed_by() {
        Some(m) if m.eq_ignore_ascii_case("winget") => {
            "Installed with winget, which also updates Reverything (winget upgrade)."
        }
        Some(m) if m.eq_ignore_ascii_case("scoop") => {
            "Installed with Scoop, which also updates Reverything (scoop update)."
        }
        Some(_) => "Installed by a package manager, which also updates Reverything.",
        None => "Development build: updates are turned off.",
    }
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The latest release if it is newer than this build. Blocks, run it in the background.
pub fn check() -> Result<Option<Update>, String> {
    let body = match fetch(LATEST_RELEASE) {
        Ok(body) => body,
        // No release published yet
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        Err(e) => return Err(format!("Request to {} failed: {}", LATEST_RELEASE, e)),
    };
    let release: GitHubRelease = serde_json::from_slice(&body)
        .map_err(|e| format!("Unexpected answer from GitHub: {}", e))?;
    if release.draft || release.prerelease {
        return Ok(None);
    }
    let version = release.tag_name.trim_start_matches('v').to_string();
    if !is_newer(&version, env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let installer_name = format!("reverything-setup-{}.exe", version);
    let asset = |name: &str| {
        release
            .assets
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
            .map(|a| a.browser_download_url.clone())
    };
    let (Some(installer_url), Some(checksum_url)) = (
        asset(&installer_name),
        asset(&format!("{}.sha256", installer_name)),
    ) else {
        return Err(format!(
            "Release {} has no installer with a checksum",
            version
        ));
    };
    Ok(Some(Update {
        version,
        installer_url,
        checksum_url,
        installer_name,
    }))
}

/// Downloads the installer, checks its SHA256 and starts it. It closes the app and the
/// service, updates both and starts the app again.
pub fn install(update: &Update) -> Result<(), String> {
    let expected = String::from_utf8_lossy(&get(&update.checksum_url)?)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if expected.len() != 64 {
        return Err("The release has an invalid checksum".into());
    }
    let installer = get(&update.installer_url)?;
    if sha256_hex(&installer) != expected {
        return Err("The downloaded installer does not match its checksum".into());
    }

    let path = std::env::temp_dir().join(&update.installer_name);
    std::fs::write(&path, &installer)
        .map_err(|e| format!("Failed to save the installer: {}", e))?;
    // The installer asks for administrator rights by itself, closes the app and starts it again
    std::process::Command::new(&path)
        .args(["/SILENT", "/SUPPRESSMSGBOXES", "/NORESTART"])
        .spawn()
        .map_err(|e| format!("The installer could not be started: {}", e))?;
    Ok(())
}

/// Whether version `a` (like `0.2.0`) is newer than `b`.
fn is_newer(a: &str, b: &str) -> bool {
    let parse = |v: &str| {
        v.split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|n| n.parse::<u64>().unwrap_or(0))
            .collect::<Vec<_>>()
    };
    parse(a) > parse(b)
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// An HTTPS GET, following redirects. Fails for error status codes.
fn get(url: &str) -> Result<Vec<u8>, String> {
    fetch(url).map_err(|e| format!("Request to {} failed: {}", url, e))
}

fn fetch(url: &str) -> Result<Vec<u8>, ureq::Error> {
    let agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(120)))
        .build()
        .new_agent();
    let mut response = agent
        .get(url)
        .header(
            "User-Agent",
            &format!("Reverything/{}", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json, */*")
        .call()?;
    response
        .body_mut()
        .with_config()
        .limit(MAX_DOWNLOAD)
        .read_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0-beta", "0.1.0"));
    }

    #[test]
    fn hashes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
