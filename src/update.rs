//! Staying up to date: `klausur update`, and a hint after `test` and `run` when a newer release
//! is out, checked at most once a day.
//!
//! The latest version comes from GitHub's redirect of `/releases/latest` to
//! `/releases/tag/vX.Y.Z`: no API, so no rate limit and no JSON.

use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const CURRENT: &str = env!("CARGO_PKG_VERSION");
const DAY: u64 = 24 * 60 * 60;

/// Where releases live. `KLAUSUR_RELEASES_URL` points elsewhere, for tests.
fn releases_url() -> String {
    std::env::var("KLAUSUR_RELEASES_URL")
        .unwrap_or_else(|_| "https://github.com/arseeeeN/klausur/releases".into())
}

fn agent(redirects: u32, timeout: Duration) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(redirects)
        .max_redirects_will_error(false)
        .timeout_global(Some(timeout))
        .user_agent(concat!("klausur/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::new_with_config(config)
}

/// `0.3.0` / `v0.3.0` as numbers, for comparing.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next()?.parse().ok();
    Some((next()?, next()?, next()?))
}

fn newer(latest: &str) -> bool {
    matches!((parse(latest), parse(CURRENT)), (Some(l), Some(c)) if l > c)
}

/// The version of the latest release.
fn latest(timeout: Duration) -> Result<String, String> {
    let url = format!("{}/latest", releases_url());
    let response = agent(0, timeout)
        .get(&url)
        .call()
        .map_err(|e| format!("couldn't reach {url}: {e}"))?;
    let location = response
        .headers()
        .get("location")
        .and_then(|l| l.to_str().ok())
        .ok_or_else(|| format!("{url} didn't redirect to a release"))?;
    let version = location.rsplit("/tag/").next().unwrap_or_default();
    parse(version)
        .map(|_| version.trim_start_matches('v').to_string())
        .ok_or_else(|| format!("{url} redirected to {location}, which names no version"))
}

// --- The hint -----------------------------------------------------------------------------

/// Where the date of the last check and its result are kept.
fn cache_file() -> Option<PathBuf> {
    let dir = match std::env::var_os("KLAUSUR_CACHE_DIR") {
        Some(d) => PathBuf::from(d),
        None if cfg!(windows) => PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("klausur"),
        None if cfg!(target_os = "macos") => {
            PathBuf::from(std::env::var_os("HOME")?).join("Library/Caches/klausur")
        }
        None => match std::env::var_os("XDG_CACHE_HOME") {
            Some(d) => PathBuf::from(d).join("klausur"),
            None => PathBuf::from(std::env::var_os("HOME")?).join(".cache/klausur"),
        },
    };
    Some(dir.join("latest-version"))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

/// A check started by [`start_check`]: the cached answer, or one on its way.
pub enum Check {
    Off,
    Known(Option<String>),
    Pending(mpsc::Receiver<Option<String>>),
}

/// Start finding out whether a newer release is out, without delaying anything: from the cache
/// if it's less than a day old, otherwise in the background. Off in CI and with
/// `KLAUSUR_NO_UPDATE_CHECK`.
pub fn start_check() -> Check {
    if std::env::var_os("KLAUSUR_NO_UPDATE_CHECK").is_some() || std::env::var_os("CI").is_some() {
        return Check::Off;
    }
    let Some(cache) = cache_file() else {
        return Check::Off;
    };
    if let Ok(text) = std::fs::read_to_string(&cache)
        && let Some((when, version)) = text.trim().split_once(' ')
        && now().saturating_sub(when.parse().unwrap_or(0)) < DAY
    {
        return Check::Known(Some(version.to_string()));
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let found = latest(Duration::from_secs(5)).ok();
        if let Some(version) = &found {
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&cache, format!("{} {version}\n", now()));
        }
        let _ = tx.send(found);
    });
    Check::Pending(rx)
}

/// At the end of a command: print a hint if a newer release is out. Waits a moment at most
/// for a check still in flight; a slow network just means no hint this time.
pub fn finish_check(check: Check) {
    let latest = match check {
        Check::Off => None,
        Check::Known(v) => v,
        Check::Pending(rx) => rx.recv_timeout(Duration::from_millis(300)).ok().flatten(),
    };
    if let Some(latest) = latest
        && newer(&latest)
    {
        eprintln!(
            "{}",
            crate::style::dim(format!(
                "klausur {latest} is out (you have {CURRENT}): run `klausur update`"
            ))
        );
    }
}

// --- `klausur update` ---------------------------------------------------------------------

/// The release asset built for this platform.
fn asset() -> Result<&'static str, String> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "klausur-x86_64-unknown-linux-musl",
        ("linux", "aarch64") => "klausur-aarch64-unknown-linux-musl",
        ("macos", "aarch64") => "klausur-aarch64-apple-darwin",
        ("macos", "x86_64") => "klausur-x86_64-apple-darwin",
        ("windows", "x86_64") => "klausur-x86_64-pc-windows-msvc.exe",
        (os, arch) => return Err(format!("there's no release build for {os} on {arch}")),
    })
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = agent(10, Duration::from_secs(300))
        .get(url)
        .call()
        .map_err(|e| format!("couldn't download {url}: {e}"))?;
    if response.status() != 200 {
        return Err(format!("couldn't download {url}: {}", response.status()));
    }
    let mut bytes = Vec::new();
    response
        .into_body()
        .into_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| format!("couldn't download {url}: {e}"))?;
    Ok(bytes)
}

/// Replace this binary with the latest release, after checking it against the release's
/// `SHA256SUMS`. Returns the exit code.
pub fn update() -> i32 {
    match run_update() {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(e) => {
            eprintln!("{}", crate::style::red(format!("update failed: {e}")));
            eprintln!(
                "  the install script does the same: \
                 https://github.com/arseeeeN/klausur#install"
            );
            1
        }
    }
}

fn run_update() -> Result<String, String> {
    let latest = latest(Duration::from_secs(15))?;
    if !newer(&latest) {
        return Ok(format!("klausur {CURRENT} is the latest release"));
    }
    let asset = asset()?;
    let base = format!("{}/download/v{latest}", releases_url());
    eprintln!("Downloading {asset} {latest} …");
    let binary = download(&format!("{base}/{asset}"))?;
    let sums = String::from_utf8(download(&format!("{base}/SHA256SUMS"))?)
        .map_err(|_| "SHA256SUMS isn't text".to_string())?;
    let expected = sums
        .lines()
        .filter_map(|l| l.split_once(char::is_whitespace))
        .find(|(_, name)| name.trim().trim_start_matches('*') == asset)
        .map(|(sum, _)| sum.to_lowercase())
        .ok_or_else(|| format!("SHA256SUMS has no entry for {asset}"))?;
    let actual: String = Sha256::digest(&binary)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != expected {
        return Err(format!(
            "the download doesn't match its checksum; nothing was changed ({asset}: expected {expected}, got {actual})"
        ));
    }
    let exe = std::env::current_exe().map_err(|e| format!("can't find this binary: {e}"))?;
    let staged = exe.with_file_name(format!(".klausur-{latest}.download"));
    std::fs::write(&staged, &binary)
        .map_err(|e| format!("can't write next to {}: {e}", exe.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755));
    }
    let replaced = self_replace::self_replace(&staged)
        .map_err(|e| format!("can't replace {}: {e}", exe.display()));
    let _ = std::fs::remove_file(&staged);
    replaced?;
    Ok(format!(
        "Updated klausur {CURRENT} → {latest} ({})",
        exe.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert_eq!(parse("v0.10.2"), Some((0, 10, 2)));
        assert!(parse("v1.2").is_none());
        assert!(parse("9.0.0") > parse("0.10.0"));
        assert!(newer("999.0.0"));
        assert!(!newer(CURRENT));
        assert!(!newer("0.0.1"));
    }
}
