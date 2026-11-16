//! Import-light equivalent of Python's `server_url.py`.
pub fn client_host(host: &str) -> &str {
    let host = host.trim();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .trim();
    match host {
        "" | "0.0.0.0" | "*" => "127.0.0.1",
        "::" | "::0" => "::1",
        _ => host,
    }
}

pub fn base_url(host: &str, port: u16) -> String {
    let host = client_host(host);
    if host.contains(':') {
        format!("http://[{}]:{port}", host.replace('%', "%25"))
    } else {
        format!("http://{host}:{port}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wildcards_ipv6_and_zones() {
        for host in ["", " ", "*", "0.0.0.0"] {
            assert_eq!(base_url(host, 8756), "http://127.0.0.1:8756");
        }
        for host in ["::", "::0", "[::]", "::1", "[::1]"] {
            assert_eq!(base_url(host, 9000), "http://[::1]:9000");
        }
        assert_eq!(base_url("fe80::1%en0", 9000), "http://[fe80::1%25en0]:9000");
        assert_eq!(base_url("localhost", 8756), "http://localhost:8756");
    }
}
