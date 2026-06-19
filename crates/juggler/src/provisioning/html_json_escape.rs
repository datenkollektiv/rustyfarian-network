//! HTML and JSON string escaping helpers shared by both provisioning tiers.
//!
//! These functions work in `no_std` environments because they use a write
//! callback rather than allocating a `String`.  Each caller accumulates the
//! output in whatever buffer suits its tier — the ESP-IDF tier uses
//! `String::push_str`; the bare-metal tier writes directly into its response
//! buffer.
//!
//! # Correctness invariants
//!
//! - [`html_escape_to`] escapes the five HTML-significant characters
//!   (`<`, `>`, `&`, `"`, `'`) and passes all other characters through
//!   unchanged.  The output is safe to embed in HTML attribute values and text
//!   nodes.
//!
//! - [`json_escape_to`] escapes `"`, `\`, and the common control characters
//!   (`\n`, `\r`, `\t`) using the standard JSON backslash sequences.  Any
//!   remaining control character below `U+0020` is written as `\uXXXX`.  The
//!   output is safe to embed inside a JSON string literal.

/// HTML-escape `input`, writing chunks to `write`.
///
/// Escapes the five HTML-significant characters:
///
/// | Character | Replacement |
/// |-----------|-------------|
/// | `&`       | `&amp;`     |
/// | `<`       | `&lt;`      |
/// | `>`       | `&gt;`      |
/// | `"`       | `&quot;`    |
/// | `'`       | `&#39;`     |
///
/// All other characters are forwarded to `write` unchanged.
///
/// # Examples
///
/// ```rust
/// use juggler::provisioning::html_json_escape::html_escape_to;
///
/// let mut out = String::new();
/// html_escape_to("<b>\"rock\" & 'roll'</b>", |s| out.push_str(s));
/// assert_eq!(out, "&lt;b&gt;&quot;rock&quot; &amp; &#39;roll&#39;&lt;/b&gt;");
/// ```
pub fn html_escape_to<F: FnMut(&str)>(input: &str, mut write: F) {
    for c in input.chars() {
        match c {
            '&' => write("&amp;"),
            '<' => write("&lt;"),
            '>' => write("&gt;"),
            '"' => write("&quot;"),
            '\'' => write("&#39;"),
            other => {
                // Write single-char slices directly from the input to avoid
                // any allocation.
                let mut buf = [0u8; 4];
                let s = other.encode_utf8(&mut buf);
                write(s);
            }
        }
    }
}

/// JSON-escape `input`, writing chunks to `write`.
///
/// Escapes characters that are not safe inside a JSON string literal:
///
/// | Character          | Replacement  |
/// |--------------------|--------------|
/// | `"`                | `\"`         |
/// | `\`                | `\\`         |
/// | newline (`\n`)     | `\n`         |
/// | carriage return    | `\r`         |
/// | tab (`\t`)         | `\t`         |
/// | other < U+0020     | `\uXXXX`    |
///
/// All other characters are forwarded to `write` unchanged.
///
/// # Examples
///
/// ```rust
/// use juggler::provisioning::html_json_escape::json_escape_to;
///
/// let mut out = String::new();
/// json_escape_to("say \"hi\"\nbye", |s| out.push_str(s));
/// assert_eq!(out, "say \\\"hi\\\"\\nbye");
/// ```
pub fn json_escape_to<F: FnMut(&str)>(input: &str, mut write: F) {
    for c in input.chars() {
        match c {
            '"' => write("\\\""),
            '\\' => write("\\\\"),
            '\n' => write("\\n"),
            '\r' => write("\\r"),
            '\t' => write("\\t"),
            c if (c as u32) < 0x20 => {
                // Format control characters as \uXXXX.  Use a small stack buffer
                // so this function stays allocation-free.
                let code = c as u32;
                let hex = [
                    b"0123456789abcdef"[((code >> 12) & 0xF) as usize],
                    b"0123456789abcdef"[((code >> 8) & 0xF) as usize],
                    b"0123456789abcdef"[((code >> 4) & 0xF) as usize],
                    b"0123456789abcdef"[(code & 0xF) as usize],
                ];
                // SAFETY: hex digits are all valid ASCII which is valid UTF-8.
                write("\\u");
                write(core::str::from_utf8(&hex).unwrap_or("0000"));
            }
            other => {
                let mut buf = [0u8; 4];
                let s = other.encode_utf8(&mut buf);
                write(s);
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    extern crate alloc;
    use alloc::string::String;

    fn html_escape(input: &str) -> String {
        let mut out = String::new();
        html_escape_to(input, |s| out.push_str(s));
        out
    }

    fn json_escape(input: &str) -> String {
        let mut out = String::new();
        json_escape_to(input, |s| out.push_str(s));
        out
    }

    #[test]
    fn html_escape_covers_all_five_significant_chars() {
        // Exercises all five HTML-significant characters in one shot so that
        // removing any branch from `html_escape_to` fails this test.
        assert_eq!(
            html_escape("<a href=\"x\">&'"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&#39;"
        );
    }

    #[test]
    fn html_escape_passes_plain_text_unchanged() {
        let plain = "hello world 123";
        assert_eq!(html_escape(plain), plain);
    }

    #[test]
    fn html_escape_handles_unicode() {
        // Non-ASCII characters must pass through without escaping.
        // Input: "cafe\u{e9} & \u{2665}" = "cafeé & ♥"
        let input = "cafe\u{e9} & \u{2665}";
        let result = html_escape(input);
        // The é (U+00E9) and ♥ (U+2665) pass through unchanged.
        assert!(
            result.contains("cafe\u{e9}"),
            "codepoint U+00E9 not preserved: {result}"
        );
        assert!(result.contains("&amp;"), "& not escaped: {result}");
        assert!(
            result.contains('\u{2665}'),
            "heart U+2665 not preserved: {result}"
        );
    }

    #[test]
    fn json_escape_handles_quote_backslash_and_control_chars() {
        // Quote and backslash — the two most common escapes.
        // json_escape(say "hi") → say \"hi\"
        assert_eq!(json_escape("say \"hi\""), "say \\\"hi\\\"");
        // json_escape(path\to\file) → path\\to\\file
        assert_eq!(json_escape("path\\to\\file"), "path\\\\to\\\\file");

        // Common control characters produce JSON backslash-escape sequences.
        assert_eq!(json_escape("line1\nline2"), "line1\\nline2");
        assert_eq!(json_escape("a\rb"), "a\\rb");
        assert_eq!(json_escape("col1\tcol2"), "col1\\tcol2");

        // Arbitrary control chars below U+0020 produce \uXXXX sequences.
        // U+0001 (SOH) → the 6-byte ASCII sequence
        assert_eq!(json_escape("\x01"), "\\u0001");

        // U+001F (US) → the 6-byte ASCII sequence
        assert_eq!(json_escape("\x1f"), "\\u001f");
    }

    #[test]
    fn json_escape_passes_plain_text_unchanged() {
        let plain = "hello world 123 / ? : @";
        assert_eq!(json_escape(plain), plain);
    }
}
