//! Finding and starting Chrome, with a throwaway profile and a DevTools port.

use super::cdp::Connection;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

pub struct Chrome {
    pub conn: Connection,
    child: Child,
    profile: PathBuf,
}

impl Drop for Chrome {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

/// `CHROME` if set, else the usual install locations, else Chrome or Chromium on the `PATH`.
fn executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CHROME") {
        return Ok(PathBuf::from(path));
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        for app in ["Google Chrome", "Google Chrome for Testing", "Chromium"] {
            candidates.push(format!("/Applications/{app}.app/Contents/MacOS/{app}").into());
        }
    }
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Some(dir) = std::env::var_os(var) {
                candidates.push(PathBuf::from(dir).join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
    }
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
        "chrome",
    ];
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in names {
                candidates.push(dir.join(name));
                if cfg!(windows) {
                    candidates.push(dir.join(format!("{name}.exe")));
                }
            }
        }
    }
    candidates.into_iter().find(|p| p.is_file()).ok_or_else(|| {
        "couldn't find Chrome or Chromium; install one, or set CHROME to its path".into()
    })
}

pub fn launch(headed: bool) -> Result<Chrome, String> {
    let exe = executable()?;
    let profile = std::env::temp_dir().join(format!(
        "klausur-chrome-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let mut cmd = Command::new(&exe);
    cmd.arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            // Ubuntu 24.04, and so GitHub's runners, blocks the user namespaces the sandbox
            // needs. Playwright turns it off too; the pages are the user's own app.
            "--no-sandbox",
            "--window-size=1280,800",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-default-apps",
            "--disable-extensions",
            "--disable-sync",
            "--disable-features=Translate,MediaRouter,OptimizationHints",
            "--metrics-recording-only",
            "--mute-audio",
            "--password-store=basic",
            "--use-mock-keychain",
        ]);
    if !headed {
        cmd.arg("--headless");
    }
    cmd.arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("couldn't start {}: {e}", exe.display()))?;

    // Chrome prints the DevTools URL on stderr. Keep draining stderr afterwards: a full pipe
    // would block Chrome.
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut tail = Vec::new();
        let mut tx = Some(tx);
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if let Some(url) = line.strip_prefix("DevTools listening on ") {
                if let Some(tx) = tx.take() {
                    let _ = tx.send(Ok(url.trim().to_string()));
                }
            } else if tx.is_some() {
                tail.push(line);
            }
        }
        if let Some(tx) = tx {
            let last = tail.into_iter().rev().take(3).collect::<Vec<_>>();
            let _ = tx.send(Err(last.into_iter().rev().collect::<Vec<_>>().join("\n")));
        }
    });
    let started = match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(url)) => Connection::connect(&url),
        Ok(Err(output)) => Err(format!(
            "{} exited before it was ready: {output}",
            exe.display()
        )),
        Err(_) => Err(format!("{} didn't start within 30 s", exe.display())),
    };
    match started {
        Ok(conn) => Ok(Chrome {
            conn,
            child,
            profile,
        }),
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&profile);
            Err(e)
        }
    }
}
