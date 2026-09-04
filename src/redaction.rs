/// Redacts credential-bearing headers and URLs before text is surfaced in logs
/// or operator-facing error messages.
///
/// Source backend stderr is untrusted and can echo signed media URLs, cookies,
/// or authorization headers. Redaction happens before that text is stored in an
/// error value so downstream callers cannot accidentally log the original.
#[must_use]
pub fn redact_sensitive(input: &str) -> String {
    input
        .lines()
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_line(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if let Some(index) = lower.find("authorization:") {
        let end = index + "authorization:".len();
        return format!("{} [REDACTED]", &line[..end]);
    }
    if let Some(index) = lower.find("cookie:") {
        let end = index + "cookie:".len();
        return format!("{} [REDACTED]", &line[..end]);
    }

    redact_urls(line)
}

fn redact_urls(line: &str) -> String {
    let mut output = String::with_capacity(line.len());
    let mut remainder = line;

    loop {
        let http = remainder.find("http://");
        let https = remainder.find("https://");
        let next = match (http, https) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(index), None) | (None, Some(index)) => Some(index),
            (None, None) => None,
        };
        let Some(index) = next else {
            output.push_str(remainder);
            break;
        };

        output.push_str(&remainder[..index]);
        output.push_str("[REDACTED_URL]");
        let url = &remainder[index..];
        let end = url
            .find(char::is_whitespace)
            .unwrap_or(url.len());
        remainder = &url[end..];
    }

    output
}

#[cfg(test)]
mod tests {
    use super::redact_sensitive;

    #[test]
    fn removes_signed_media_urls() {
        let input = "backend failed for https://cdn.example.test/audio?expire=123&sig=secret retrying";
        let redacted = redact_sensitive(input);
        assert_eq!(
            redacted,
            "backend failed for [REDACTED_URL] retrying"
        );
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("cdn.example.test"));
    }

    #[test]
    fn removes_authorization_and_cookie_credentials() {
        let authorization = redact_sensitive("Authorization: Bot super-secret-token");
        let cookie = redact_sensitive("Cookie: session=super-secret-cookie");

        assert_eq!(authorization, "Authorization: [REDACTED]");
        assert_eq!(cookie, "Cookie: [REDACTED]");
        assert!(!authorization.contains("super-secret-token"));
        assert!(!cookie.contains("super-secret-cookie"));
    }

    #[test]
    fn preserves_non_sensitive_diagnostics() {
        assert_eq!(
            redact_sensitive("ERROR: media source returned 403"),
            "ERROR: media source returned 403"
        );
    }
}
