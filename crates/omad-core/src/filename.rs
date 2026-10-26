//! Turning a server's suggestion into a file name that is safe on every platform.
//!
//! Two rules, both from the review (R2/R12):
//!
//! 1. The name never contains a path separator, a control character, or a Windows
//!    reserved device name. This is the only barrier between a hostile
//!    `Content-Disposition` and the rest of the disk.
//! 2. The result is never empty, never `.` or `..`, and never longer than
//!    [`MAX_BYTES`] bytes on a character boundary.

/// Windows refuses these names with or without an extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Leaves room for ` (2)` and an extension under the usual 255-byte limit.
pub const MAX_BYTES: usize = 180;

pub const FALLBACK: &str = "download";

/// Strips every path component, then every character a file system refuses.
///
/// A name that would collide with a reserved device name is prefixed with `_`
/// (`con.txt` becomes `_con.txt`), which keeps the extension and stays readable.
pub fn sanitize(raw: &str) -> String {
    // Take the last component of either separator before anything else, so
    // `../../etc/passwd` and `C:\Windows\x` both lose their directories.
    let last = raw
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("");

    let mut cleaned = String::with_capacity(last.len());
    for ch in last.chars() {
        let replaced = match ch {
            // Illegal on Windows and dangerous everywhere.
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            // Control characters, including the ones that break a log line.
            c if (c as u32) < 0x20 || c == '\u{7f}' => '_',
            c => c,
        };
        cleaned.push(replaced);
    }

    // Windows strips trailing dots and spaces; do it here so both platforms agree.
    let trimmed = cleaned.trim_matches(|c: char| c == ' ' || c == '.').trim();
    let trimmed = if trimmed.is_empty() {
        FALLBACK
    } else {
        trimmed
    };

    // `..` and `.` are gone above (all dots trimmed); a lone dash is not a name.
    let mut name = if trimmed == "-" {
        FALLBACK.to_string()
    } else {
        trimmed.to_string()
    };

    if is_reserved(&name) {
        name.insert(0, '_');
    }

    truncate(&name)
}

fn is_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    RESERVED.contains(&stem.as_str())
}

fn truncate(name: &str) -> String {
    if name.len() <= MAX_BYTES {
        return name.to_string();
    }
    // Keep the extension: `report.pdf` truncated is `rep...pdf`, not `rep…`.
    let (stem, ext) = match name.rfind('.') {
        Some(i) if name.len() - i <= 16 && i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    let budget = MAX_BYTES.saturating_sub(ext.len());
    let mut end = budget.min(stem.len());
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(MAX_BYTES);
    out.push_str(&stem[..end]);
    out.push_str(ext);
    if out.is_empty() {
        FALLBACK.to_string()
    } else {
        out
    }
}

/// Makes `raw` unique against the names already taken, the way a file manager does:
/// `a.txt`, `a (2).txt`, `a (3).txt`.
pub fn deduplicate(raw: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(raw) {
        return raw.to_string();
    }
    let (stem, ext) = match raw.rfind('.') {
        Some(i) if i > 0 => (&raw[..i], &raw[i..]),
        _ => (raw, ""),
    };
    for n in 2..10_000u32 {
        let candidate = format!("{stem} ({n}){ext}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{stem} ({}){ext}", uuid_suffix())
}

fn uuid_suffix() -> u64 {
    // A clock-derived suffix is enough to end the search; it is not an identifier.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 % 1_000_000)
        .unwrap_or(0)
}

/// Reads a name out of a `Content-Disposition` header.
///
/// `filename*=UTF-8''a%20b` wins over `filename="a b"` per RFC 6266; both are
/// treated as untrusted text and come out of [`sanitize`].
pub fn from_content_disposition(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();

    if let Some(at) = lower.find("filename*=") {
        let raw = &header[at + "filename*=".len()..];
        let raw = raw.split(';').next().unwrap_or(raw).trim();
        // charset'lang'percent-encoded
        let value = raw.splitn(3, '\'').nth(2).unwrap_or(raw);
        let decoded = percent_decode(value);
        if !decoded.trim().is_empty() {
            return Some(sanitize(&decoded));
        }
    }

    if let Some(at) = lower.find("filename=") {
        let raw = &header[at + "filename=".len()..];
        let raw = raw.split(';').next().unwrap_or(raw).trim();
        let unquoted = raw.trim_matches('"').trim_matches('\'');
        if !unquoted.trim().is_empty() {
            return Some(sanitize(unquoted));
        }
    }

    None
}

/// A file name for a URL whose server said nothing: the last path segment, or the
/// host when the path ends at the root.
pub fn from_url(url: &url::Url) -> String {
    // A magnet link has no path: its name is `dn`, and failing that the infohash, which is
    // still better than the word "download".
    if url.scheme() == "magnet" {
        let mut name = None;
        let mut hash = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "dn" => name = Some(value.into_owned()),
                "xt" => {
                    if let Some(rest) = value.strip_prefix("urn:btih:") {
                        hash = Some(rest.to_string());
                    }
                }
                _ => {}
            }
        }
        if let Some(name) = name.filter(|n| !n.trim().is_empty()) {
            return sanitize(&name);
        }
        if let Some(hash) = hash {
            return sanitize(&format!("torrent-{}", &hash[..hash.len().min(40)]));
        }
        return sanitize("torrent");
    }
    let last = url
        .path_segments()
        .and_then(|mut segments| segments.rfind(|s: &&str| !s.is_empty()))
        .map(percent_decode)
        .unwrap_or_default();
    let candidate = if last.trim().is_empty() {
        url.host_str().unwrap_or(FALLBACK).to_string()
    } else {
        last
    };
    sanitize(&candidate)
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_directories_and_separators() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("..\\..\\windows\\system32\\x"), "x");
        assert_eq!(sanitize("/absolute/path/a.txt"), "a.txt");
        assert_eq!(sanitize("a/b"), "b");
    }

    #[test]
    fn replaces_characters_windows_refuses() {
        assert_eq!(sanitize("re:port<1>|?.txt"), "re_port_1___.txt");
        assert_eq!(sanitize("tab\there"), "tab_here");
        assert_eq!(sanitize("line\nbreak"), "line_break");
    }

    #[test]
    fn refuses_dot_names_and_empty_names() {
        assert_eq!(sanitize("."), FALLBACK);
        assert_eq!(sanitize(".."), FALLBACK);
        assert_eq!(sanitize(""), FALLBACK);
        assert_eq!(sanitize("   "), FALLBACK);
        assert_eq!(sanitize("/"), FALLBACK);
    }

    #[test]
    fn leads_reserved_device_names() {
        assert_eq!(sanitize("con"), "_con");
        assert_eq!(sanitize("CON.txt"), "_CON.txt");
        assert_eq!(sanitize("Lpt9.tar.gz"), "_Lpt9.tar.gz");
        assert_eq!(sanitize("console.txt"), "console.txt");
    }

    #[test]
    fn trailing_dots_and_spaces_go() {
        assert_eq!(sanitize("a.txt..."), "a.txt");
        assert_eq!(sanitize("a.txt   "), "a.txt");
    }

    #[test]
    fn truncates_on_a_character_boundary_and_keeps_the_extension() {
        let long = format!("{}.pdf", "é".repeat(400));
        let out = sanitize(&long);
        assert!(out.len() <= MAX_BYTES);
        assert!(out.ends_with(".pdf"));
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }

    #[test]
    fn reads_content_disposition_both_ways() {
        assert_eq!(
            from_content_disposition("attachment; filename=\"report.pdf\""),
            Some("report.pdf".into())
        );
        assert_eq!(
            from_content_disposition("attachment; filename*=UTF-8''%E6%96%87%E4%BB%B6.txt"),
            Some("文件.txt".into())
        );
        // The extended form wins.
        assert_eq!(
            from_content_disposition("attachment; filename=\"a.txt\"; filename*=UTF-8''b.txt"),
            Some("b.txt".into())
        );
        assert_eq!(
            from_content_disposition("attachment; filename=\"../../x\""),
            Some("x".into())
        );
    }

    #[test]
    fn deduplicates_like_a_file_manager() {
        let taken = |n: &str| n == "a.txt" || n == "a (2).txt";
        assert_eq!(deduplicate("a.txt", taken), "a (3).txt");
        assert_eq!(deduplicate("b.txt", taken), "b.txt");
    }

    #[test]
    fn derives_a_name_from_a_url() {
        let url = url::Url::parse("https://example.com/files/report%202024.pdf?x=1").unwrap();
        assert_eq!(from_url(&url), "report 2024.pdf");
        let root = url::Url::parse("https://example.com/").unwrap();
        assert_eq!(from_url(&root), "example.com");
    }
}
