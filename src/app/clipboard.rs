//! Platform-conditional clipboard write. Best-effort: errors from
//! spawning or writing to the helper process are swallowed because
//! there's nothing actionable the UI can do about them.
//!
//! Helper processes run with stdout/stderr nulled — the TUI owns the
//! terminal, and a helper's error output (e.g. xclip's "Can't open
//! display" when headless) would otherwise print straight into the
//! alternate screen. When no helper can take the text (no display, or
//! the tools aren't installed), we fall back to OSC 52, which asks the
//! terminal emulator itself to set the clipboard and therefore works
//! over SSH with no display at all.

pub fn yank_to_clipboard(text: &str) {
    // Unit tests exercise the yank paths (mouse drag-copy, `y`); a
    // `cargo test` run must not clobber the developer's real clipboard.
    if cfg!(test) {
        return;
    }

    #[cfg(target_os = "macos")]
    {
        if pipe_to_helper("pbcopy", &[], text) {
            return;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let display_set = |var| std::env::var_os(var).is_some_and(|v| !v.is_empty());
        if display_set("WAYLAND_DISPLAY") && pipe_to_helper("wl-copy", &[], text) {
            return;
        }
        if display_set("DISPLAY")
            && (pipe_to_helper("xclip", &["-selection", "clipboard"], text)
                || pipe_to_helper("xsel", &["--clipboard", "--input"], text))
        {
            return;
        }
    }
    osc52_to_terminal(text);
}

/// Spawn `cmd` with the text on stdin and stdout/stderr discarded.
/// Returns whether the helper ran and exited successfully.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn pipe_to_helper(cmd: &str, args: &[&str], text: &str) -> bool {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    let Ok(mut child) =
        Command::new(cmd).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
    else {
        return false;
    };
    if let Some(stdin) = child.stdin.as_mut() {
        _ = stdin.write_all(text.as_bytes());
    }
    drop(child.stdin.take());
    child.wait().is_ok_and(|status| status.success())
}

/// OSC 52: hand the text to the terminal emulator itself
/// (`ESC ] 52 ; c ; <base64> BEL`). The sequence produces no visible
/// output, so writing it mid-session is safe; terminals that don't
/// support it (or have clipboard access disabled) ignore it.
fn osc52_to_terminal(text: &str) {
    use std::io::Write;

    let mut stdout = std::io::stdout().lock();
    _ = stdout.write_all(b"\x1b]52;c;");
    _ = stdout.write_all(base64(text.as_bytes()).as_bytes());
    _ = stdout.write_all(b"\x07");
    _ = stdout.flush();
}

fn base64(data: &[u8]) -> String {
    fn digit(v: u32) -> char {
        let v = (v & 0x3f) as u8;
        char::from(match v {
            0..=25 => b'A' + v,
            26..=51 => b'a' + (v - 26),
            52..=61 => b'0' + (v - 52),
            62 => b'+',
            _ => b'/',
        })
    }

    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    let mut buf: u32 = 0;
    let mut bits = 0u32;
    for &byte in data {
        buf = (buf << 8) | u32::from(byte);
        bits += 8;
        while bits >= 6 {
            bits -= 6;
            out.push(digit(buf >> bits));
        }
    }
    if bits > 0 {
        out.push(digit(buf << (6 - bits)));
    }
    while !out.len().is_multiple_of(4) {
        out.push('=');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected, "input {input:?}");
        }
    }
}
