//! `web_fetch` helpers: strict https target parsing, bounded body reading and
//! deterministic HTML-to-text conversion. Policy (which hosts are allowed,
//! approval, read-only mode) is decided by the registry, not here.

use std::io::Read;

/// Largest response body read from the network, before conversion.
pub const MAX_FETCH_BYTES: usize = 2 * 1024 * 1024;
/// Longest URL accepted, to keep arguments and logs bounded.
pub const MAX_URL_BYTES: usize = 2048;

/// A validated `https` target: lowercase host, and the URL as sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebTarget {
    pub host: String,
    pub url: String,
}

/// Parses and validates a URL. Rejects anything but plain `https`, userinfo
/// (credentials in the URL), non-default ports, whitespace/control characters
/// and hosts with characters outside `[a-z0-9.-]` (IP literals included).
pub fn parse_https_target(raw: &str) -> Result<WebTarget, String> {
    if raw.len() > MAX_URL_BYTES {
        return Err(format!("url is longer than {MAX_URL_BYTES} bytes"));
    }
    if raw.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return Err("url must not contain whitespace or control characters".into());
    }
    let lower = raw.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .ok_or_else(|| "only https:// URLs are supported".to_owned())?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.contains('@') {
        return Err("URLs with credentials are not allowed".into());
    }
    let host = match authority.split_once(':') {
        Some((host, "443")) => host,
        Some(_) => return Err("only the default https port is supported".into()),
        None => authority,
    };
    // An all-numeric host is an IPv4 literal; the allowlist is by DNS name.
    if host.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
        return Err("IP address hosts are not supported; use a DNS name".into());
    }
    if host.is_empty()
        || host.starts_with('.')
        || host.ends_with('.')
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '.' || ch == '-')
    {
        return Err("host name is not a plain DNS name".into());
    }
    Ok(WebTarget {
        host: host.to_owned(),
        url: raw.to_owned(),
    })
}

/// Reads at most [`MAX_FETCH_BYTES`] from `reader`. Returns the bytes and
/// whether the body was longer than the limit.
pub fn read_bounded(reader: impl Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_FETCH_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_FETCH_BYTES;
    bytes.truncate(MAX_FETCH_BYTES);
    Ok((bytes, truncated))
}

/// Media types we convert to text. Anything else (images, archives, binaries)
/// is refused rather than decoded.
pub fn is_textual(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
        || mime == "application/xhtml+xml"
}

const SKIPPED_ELEMENTS: [&str; 3] = ["script", "style", "noscript"];
const BLOCK_ELEMENTS: [&str; 17] = [
    "p", "div", "br", "li", "tr", "table", "section", "article", "header", "footer", "h1", "h2",
    "h3", "h4", "h5", "h6", "pre",
];

/// Converts HTML to readable text: drops scripts, styles and comments, turns
/// block elements into line breaks, decodes common entities and collapses
/// whitespace. Plain text passes through with the same whitespace rules.
pub fn html_to_text(input: &str) -> String {
    // ASCII lowercasing keeps byte offsets identical, so positions found in
    // `lower` are valid for `input`.
    let lower = input.to_ascii_lowercase();
    let mut out = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let rest = &lower[index..];
        if rest.starts_with("<!--") {
            index = lower[index..]
                .find("-->")
                .map_or(input.len(), |end| index + end + 3);
            continue;
        }
        if let Some(stripped) = rest.strip_prefix('<') {
            let Some(close) = input[index..].find('>') else {
                break;
            };
            let tag_end = index + close;
            let name: String = stripped
                .trim_start_matches('/')
                .chars()
                .take_while(|ch| ch.is_ascii_alphanumeric())
                .collect();
            if SKIPPED_ELEMENTS.contains(&name.as_str()) && !rest.starts_with("</") {
                let closing = format!("</{name}");
                index = lower[tag_end..]
                    .find(&closing)
                    .and_then(|found| {
                        lower[tag_end + found..]
                            .find('>')
                            .map(|end| tag_end + found + end + 1)
                    })
                    .unwrap_or(input.len());
                out.push(' ');
                continue;
            }
            if BLOCK_ELEMENTS.contains(&name.as_str()) {
                out.push('\n');
            } else {
                out.push(' ');
            }
            index = tag_end + 1;
            continue;
        }
        let ch = input[index..].chars().next().unwrap_or('\0');
        index += ch.len_utf8().max(1);
        out.push(ch);
    }
    collapse(&decode_entities(&out))
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

fn collapse(text: &str) -> String {
    // Blank lines carry no information for a reader and cost tokens, so the
    // result is one non-empty line per block.
    text.lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_https_with_default_port() {
        let target = parse_https_target("https://Docs.RS/serde/latest?x=1").expect("ok");
        assert_eq!(target.host, "docs.rs");
        assert_eq!(
            parse_https_target("https://example.com:443/")
                .expect("ok")
                .host,
            "example.com"
        );
    }

    #[test]
    fn rejects_unsafe_or_ambiguous_targets() {
        for raw in [
            "http://example.com",
            "file:///etc/passwd",
            "https://user:pass@example.com",
            "https://example.com:8443/",
            "https://127.0.0.1/",
            "https://[::1]/",
            "https://exa mple.com",
            "https://example.com\n/x",
            "https://.example.com",
            "ftp://example.com",
        ] {
            assert!(parse_https_target(raw).is_err(), "{raw} should be rejected");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_BYTES));
        assert!(parse_https_target(&long).is_err());
    }

    #[test]
    fn read_bounded_caps_the_body() {
        let body = vec![b'x'; MAX_FETCH_BYTES + 10];
        let (bytes, truncated) = read_bounded(&body[..]).expect("read");
        assert_eq!(bytes.len(), MAX_FETCH_BYTES);
        assert!(truncated);
        let (short, truncated) = read_bounded(&b"hi"[..]).expect("read");
        assert_eq!(short, b"hi");
        assert!(!truncated);
    }

    #[test]
    fn html_is_reduced_to_readable_text() {
        let html = "<html><head><style>p{color:red}</style><script>var x = '<p>';</script></head>\
<body><!-- hidden --><h1>Title &amp; more</h1><p>First   paragraph.</p>\
<ul><li>one</li><li>two&nbsp;words</li></ul></body></html>";
        let text = html_to_text(html);
        assert_eq!(text, "Title & more\nFirst paragraph.\none\ntwo words");
        assert!(!text.contains("color"));
        assert!(!text.contains("var x"));
        assert!(!text.contains("hidden"));
    }

    #[test]
    fn unterminated_markup_does_not_loop_or_panic() {
        assert_eq!(html_to_text("text <b"), "text");
        assert_eq!(html_to_text("<script>never closed"), "");
        assert_eq!(html_to_text("é<p>ü</p>"), "é\nü");
    }

    #[test]
    fn only_textual_media_types_are_converted() {
        assert!(is_textual("text/html; charset=utf-8"));
        assert!(is_textual("application/json"));
        assert!(!is_textual("image/png"));
        assert!(!is_textual("application/octet-stream"));
    }
}
