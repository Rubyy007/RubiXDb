//! Best-effort default-browser launch. Never fatal: a headless
//! environment (CI, a remote shell) must not crash `rubixdb gui` --
//! it should print the URL and keep serving. No new dependency: three
//! well-known OS launcher commands cover every supported platform, the
//! same approach the `open`/`webbrowser` crates themselves use
//! internally, without pulling in a crate for three `Command` calls.

use std::process::Stdio;

pub fn open(url: &str) -> bool {
    let result = {
        #[cfg(target_os = "windows")]
        {
            // `cmd /C start "" <url>` -- the empty `""` is the window
            // title `start` expects as its first argument; without it
            // a URL containing spaces or special characters would be
            // misparsed as the title.
            std::process::Command::new("cmd")
                .args(["/C", "start", "", url])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        }
        #[cfg(target_os = "macos")]
        {
            std::process::Command::new("open")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            std::process::Command::new("xdg-open")
                .arg(url)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        }
    };
    matches!(result, Ok(status) if status.success())
}
