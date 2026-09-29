//! Text helpers shared by the recording, inspection and export layers.

/// Strip ANSI/VT escape sequences from a byte string, keeping the printable
/// text. Used for search, transcript export and `inspect` previews.
///
/// This is a deliberately pragmatic scanner, not a VT parser: it recognises
/// CSI, OSC, DCS and single-character escapes well enough to keep the visible
/// text clean. Anything it does not recognise is dropped, never leaked.
pub fn strip_ansi(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            0x1b => {
                i += 1;
                i = skip_escape(bytes, i, &mut out);
            }
            b'\n' | b'\r' => {
                out.push('\n');
                i += 1;
            }
            b'\t' => {
                out.push('\t');
                i += 1;
            }
            0x08 => {
                out.pop();
                i += 1;
            }
            0x00..=0x07 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f => i += 1,
            _ => {
                // Copy one UTF-8 scalar, keeping the output valid.
                let start = i;
                i += 1;
                while i < bytes.len() && (bytes[i] & 0xc0) == 0x80 {
                    i += 1;
                }
                match std::str::from_utf8(&bytes[start..i]) {
                    Ok(s) => out.push_str(s),
                    Err(_) => out.push('\u{fffd}'),
                }
            }
        }
    }
    out
}

/// Consume one escape sequence starting at `i` (just past `ESC`).
/// Returns the index just past the sequence.
fn skip_escape(bytes: &[u8], mut i: usize, out: &mut String) -> usize {
    if i >= bytes.len() {
        return i;
    }
    match bytes[i] {
        b'[' => {
            // CSI: parameter and intermediate bytes, then a final byte.
            i += 1;
            while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                i += 1;
            }
            (i + 1).min(bytes.len())
        }
        b']' => {
            // OSC: terminated by BEL or ST (ESC \).
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            i
        }
        b'P' | b'X' | b'^' | b'_' => {
            // DCS / SOS / PM / APC: string terminated by ST.
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
            i
        }
        // ESC ( B, ESC # 8, and friends: one more intermediate then a final.
        0x20..=0x2f => {
            i += 1;
            while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            i
        }
        _ => {
            let _ = out;
            i + 1
        }
    }
}

/// Truncate to `max` characters, appending an ellipsis if it had to cut.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Collapse whitespace so a multi-line chunk fits on one timeline row.
pub fn first_line(s: &str) -> String {
    let line = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim_end();
    truncate_chars(line, 200)
}

/// Replace control characters so untrusted bytes are safe to print to a
/// terminal that is not in raw mode.
pub fn sanitize_for_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\t' => out.push(c),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push('·'),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_are_stripped_but_text_survives() {
        assert_eq!(strip_ansi(b"\x1b[31merror:\x1b[0m something"), "error: something");
        assert_eq!(strip_ansi(b"\x1b[1;32mok\x1b[m"), "ok");
        assert_eq!(strip_ansi(b"\x1b[38;5;208m256-colour\x1b[0m"), "256-colour");
        assert_eq!(strip_ansi(b"\x1b[?1049h\x1b[2J\x1b[Hclean"), "clean");
    }

    #[test]
    fn osc_titles_are_dropped_not_leaked() {
        assert_eq!(strip_ansi(b"\x1b]0;a title\x07visible"), "visible");
        assert_eq!(strip_ansi(b"\x1b]0;a title\x1b\\visible"), "visible");
    }

    #[test]
    fn utf8_is_preserved_and_invalid_bytes_are_replaced() {
        assert_eq!(strip_ansi("héllo → 🌍".as_bytes()), "héllo → 🌍");
        assert_eq!(strip_ansi(&[b'a', 0xff, b'b']), "a\u{fffd}b");
    }

    #[test]
    fn carriage_returns_become_newlines() {
        assert_eq!(strip_ansi(b"one\rtwo"), "one\ntwo");
    }

    #[test]
    fn truncated_sequences_do_not_hang_or_leak() {
        assert_eq!(strip_ansi(b"\x1b"), "");
        assert_eq!(strip_ansi(b"\x1b["), "");
        assert_eq!(strip_ansi(b"\x1b]0;unterminated"), "");
    }

    #[test]
    fn bell_and_other_controls_vanish() {
        assert_eq!(strip_ansi(b"a\x07b\x00c"), "abc");
    }

    #[test]
    fn previews_are_single_line() {
        assert_eq!(first_line("  \n\nhello\nworld"), "hello");
        assert_eq!(first_line(""), "");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }
}
