//! Platform-conditional clipboard write. Best-effort: errors from
//! spawning or writing to the helper process are swallowed because
//! there's nothing actionable the UI can do about them.

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn yank_to_clipboard(text: &str) {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    // Unit tests exercise the yank paths (mouse drag-copy, `y`); a
    // `cargo test` run must not clobber the developer's real clipboard.
    if cfg!(test) {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                _ = stdin.write_all(text.as_bytes());
            }
            _ = child.wait();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut done = false;
        if let Ok(mut child) = Command::new("xclip").args(["-selection", "clipboard"]).stdin(Stdio::piped()).spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok() {
                done = true;
            }
        }
        if !done
            && let Ok(mut child) = Command::new("xsel").args(["--clipboard", "--input"]).stdin(Stdio::piped()).spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                _ = stdin.write_all(text.as_bytes());
            }
            _ = child.wait();
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn yank_to_clipboard(_text: &str) {}
