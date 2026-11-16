//! The parts of Python `urllib.parse.urlsplit` that `provider_keys._url`
//! inspects: scheme, hostname, username, password, query and fragment.

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Split {
    pub scheme: String,
    pub hostname: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub query: String,
    pub fragment: String,
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// `Err("Invalid IPv6 URL")` where Python's urlsplit raises ValueError.
pub(crate) fn urlsplit(url: &str) -> Result<Split, &'static str> {
    // Python strips leading C0 controls/space and removes tab/CR/LF.
    let cleaned: String = url
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = cleaned.as_str();
    let mut split = Split::default();
    if let Some(i) = rest.find(':') {
        let candidate = &rest[..i];
        if i > 0
            && candidate.starts_with(|c: char| c.is_ascii_alphabetic())
            && candidate.chars().all(is_scheme_char)
        {
            split.scheme = candidate.to_ascii_lowercase();
            rest = &rest[i + 1..];
        }
    }
    let mut netloc = "";
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = &after[..end];
        rest = &after[end..];
        if netloc.contains('[') != netloc.contains(']') {
            return Err("Invalid IPv6 URL");
        }
    }
    if let Some((before, fragment)) = rest.split_once('#') {
        split.fragment = fragment.to_string();
        rest = before;
    }
    if let Some((_, query)) = rest.split_once('?') {
        split.query = query.to_string();
    }
    let (userinfo, hostinfo) = match netloc.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, netloc),
    };
    if let Some(userinfo) = userinfo {
        match userinfo.split_once(':') {
            Some((user, password)) => {
                split.username = Some(user.to_string());
                split.password = Some(password.to_string());
            }
            None => split.username = Some(userinfo.to_string()),
        }
    }
    let hostname = if let Some(bracketed) = hostinfo.strip_prefix('[') {
        let inside = bracketed.split(']').next().unwrap_or("");
        let address = inside.split('%').next().unwrap_or("");
        if !address.starts_with(['v', 'V']) && address.parse::<std::net::Ipv6Addr>().is_err() {
            return Err("Invalid IPv6 URL");
        }
        inside.to_string()
    } else {
        hostinfo.split(':').next().unwrap_or("").to_string()
    };
    if !hostname.is_empty() {
        split.hostname = Some(hostname.to_lowercase());
    }
    Ok(split)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_like_python() {
        let s = urlsplit("HTTPS://User:Pw@Example.COM:8443/v1?x=1#frag").unwrap();
        assert_eq!(s.scheme, "https");
        assert_eq!(s.hostname.as_deref(), Some("example.com"));
        assert_eq!(s.username.as_deref(), Some("User"));
        assert_eq!(s.password.as_deref(), Some("Pw"));
        assert_eq!((s.query.as_str(), s.fragment.as_str()), ("x=1", "frag"));

        let s = urlsplit("http://[::1]:11434").unwrap();
        assert_eq!(s.hostname.as_deref(), Some("::1"));
        assert_eq!(urlsplit("http://[::1").unwrap_err(), "Invalid IPv6 URL");
        assert_eq!(
            urlsplit("http://[nothost]").unwrap_err(),
            "Invalid IPv6 URL"
        );

        let s = urlsplit("localhost:11434").unwrap();
        assert_eq!(s.scheme, "localhost");
        assert_eq!(s.hostname, None);

        let s = urlsplit("http://@host").unwrap();
        assert_eq!(s.username.as_deref(), Some(""));
        assert_eq!(s.hostname.as_deref(), Some("host"));

        assert_eq!(urlsplit("").unwrap(), Split::default());
        assert_eq!(urlsplit("http://").unwrap().hostname, None);
        assert_eq!(urlsplit("http://h/?").unwrap().query, "");
        assert_eq!(
            urlsplit(" \thttp://h\n/x").unwrap().hostname.as_deref(),
            Some("h")
        );
    }
}
