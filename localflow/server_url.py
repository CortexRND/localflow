"""Where local clients reach the LocalFlow server.

The server binds `server_host:server_port` from the config. Clients (the
app's health probe, `lf`, the meeting client, the menu bar) connect to the
same port; a wildcard bind is reached over loopback and IPv6 literals are
bracketed. Import-light on purpose: no server, no HTTP library.
"""

_IPV4_WILDCARDS = frozenset({"", "0.0.0.0", "*"})
_IPV6_WILDCARDS = frozenset({"::", "::0"})


def client_host(host) -> str:
    """Host a local client should connect to for a server bound to `host`."""
    host = (host if isinstance(host, str) else "").strip()
    if host.startswith("[") and host.endswith("]"):
        host = host[1:-1].strip()
    if host in _IPV4_WILDCARDS:
        return "127.0.0.1"
    if host in _IPV6_WILDCARDS:
        return "::1"
    return host


def base_url(host, port) -> str:
    """`http://host:port` for the server bound to `host`/`port`."""
    host = client_host(host)
    if ":" in host:
        # IPv6 literal; a zone id ("fe80::1%en0") must be percent-encoded.
        host = "[" + host.replace("%", "%25") + "]"
    return f"http://{host}:{port}"


def configured_base_url(config=None) -> str:
    """Base URL of the server described by `config` (default: saved config)."""
    if config is None:
        from localflow.config import load_config

        config = load_config()
    return base_url(config.server_host, config.server_port)
