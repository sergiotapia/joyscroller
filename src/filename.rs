//! Turns text from the web (a video's title) into a file or folder name
//! that's valid and harmless on Linux, macOS and Windows alike. Windows is
//! the strictest: <https://learn.microsoft.com/en-us/windows/win32/fileio/naming-a-file>.
//! The result can be empty (a title of only dots), and different titles can
//! end up the same, so callers add something of their own, like the video
//! id, to every name.

/// Characters Windows forbids in names (`/` is also Unix's separator, `:`
/// special to macOS), swapped for full-width lookalikes as yt-dlp does, so
/// a title like "Part 1: Intro" stays readable.
const LOOKALIKES: [(char, char); 9] = [
    ('<', '＜'),
    ('>', '＞'),
    (':', '：'),
    ('"', '＂'),
    ('/', '／'),
    ('\\', '＼'),
    ('|', '｜'),
    ('?', '？'),
    ('*', '＊'),
];

/// Device names Windows reserves in every folder, even with an extension
/// (`NUL.txt` is `NUL`). It counts the superscript digits as digits too.
const RESERVED: [&str; 28] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "COM¹", "COM²", "COM³", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8",
    "LPT9", "LPT¹", "LPT²", "LPT³",
];

/// Invisible characters that hide or reorder text: zero-width ones, and
/// direction overrides, which can make a name ending in `4pm.exe` display
/// as one ending in `exe.mp4`.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// `text` as a name of at most `max_bytes` bytes (Linux and macOS count a
/// name's limit of 255 in bytes, and emoji take four). A cut is made at a
/// word boundary where there is one, and marked with "…".
pub fn safe_name(text: &str, max_bytes: usize) -> String {
    let cleaned: String = text
        .chars()
        .filter(|&c| !is_invisible(c))
        .map(|c| match LOOKALIKES.iter().find(|(bad, _)| *bad == c) {
            Some(&(_, lookalike)) => lookalike,
            // Newlines, tabs and other control characters.
            None if c.is_control() => ' ',
            None => c,
        })
        .collect();
    let mut name = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.len() > max_bytes {
        name = shorten(&name, max_bytes);
    }
    // A leading dot hides the folder, and `.` and `..` are path tricks; a
    // leading dash reads as an option to command-line tools. Windows drops
    // trailing dots and spaces, so a name ending in them can't be opened.
    let name = name
        .trim_start_matches(['.', '-', ' '])
        .trim_end_matches(['.', ' ']);
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        format!("_{name}")
    } else {
        name.to_string()
    }
}

/// `name` cut to fit `max_bytes` with "…" after it: at the last space, unless
/// that would lose more than half of what fits.
fn shorten(name: &str, max_bytes: usize) -> String {
    const MORE: &str = "…";
    let mut end = max_bytes.saturating_sub(MORE.len());
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    let cut = &name[..end];
    let cut = match cut.rfind(' ') {
        Some(space) if space > end / 2 => &cut[..space],
        _ => cut,
    };
    format!("{}{MORE}", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_characters_windows_forbids() {
        assert_eq!(
            safe_name(r#"Part 1: "Why?" <a/b\c|d*>"#, 255),
            "Part 1： ＂Why？＂ ＜a／b＼c｜d＊＞"
        );
    }

    #[test]
    fn flattens_whitespace_and_drops_invisible_characters() {
        assert_eq!(
            safe_name("line one\n\n\tline\u{0}two\u{202E}  end\u{200B}", 255),
            "line one line two end"
        );
    }

    #[test]
    fn trims_dots_dashes_and_spaces_at_the_ends() {
        assert_eq!(safe_name("  ..-hidden. . ", 255), "hidden");
        assert_eq!(safe_name("..", 255), "");
        assert_eq!(safe_name("wait...", 255), "wait");
        assert_eq!(safe_name("a.b", 255), "a.b");
    }

    #[test]
    fn guards_windows_device_names() {
        assert_eq!(safe_name("con", 255), "_con");
        assert_eq!(safe_name("NUL.txt", 255), "_NUL.txt");
        assert_eq!(safe_name("com¹", 255), "_com¹");
        assert_eq!(safe_name("COM10", 255), "COM10");
        assert_eq!(safe_name("console", 255), "console");
    }

    #[test]
    fn shortens_at_a_word_within_the_byte_limit() {
        assert_eq!(safe_name("the quick brown fox", 15), "the quick…");
        // One long word is cut mid-word.
        assert_eq!(safe_name("abcdefghijklmnop", 10), "abcdefg…");
        // Never inside a multi-byte character: each 🙂 is 4 bytes.
        let emoji = safe_name(&"🙂".repeat(10), 13);
        assert_eq!(emoji, "🙂🙂…");
        assert!(emoji.len() <= 13);
        assert_eq!(safe_name("short", 10), "short");
    }
}
