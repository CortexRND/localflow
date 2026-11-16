"""Activation snapshots: the LLM key Ask and cleanup use, bound to one endpoint.

Activating a provider on Models & keys never rewrites a shared key or an
existing snapshot. It stores a new secret `llm_activation_<provider>_<32 hex>`
holding `{"base_url": ..., "key": ...}` and then atomically saves a config
whose `[llm] api_key_ref` names it. The only in-place change is an explicit
key rotation in web Settings (`api.update_secret`), which rewrites (or, on
clear, deletes) the active config's own slot for the same bound endpoint. A
reader that loaded the old config keeps the old key (or the legacy
`llm_api_key`); one that loaded the new config finds the snapshot already
written. The key is only ever used for the
endpoint it was activated for, so no reader can pair A's endpoint with B's
key or B's endpoint with A's key.

`llm_activation_index` lists snapshot names (never values) so removing a
provider key also revokes every snapshot taken from it, including ones an
older cached config still names.

Pure: no keyring, no config import. Secret values are never logged.
"""

from __future__ import annotations

import json
import logging
import re
import uuid
from collections.abc import Callable, Iterable

log = logging.getLogger("localflow.llm_activation")

PREFIX = "llm_activation_"
INDEX_NAME = "llm_activation_index"
_REF = re.compile(r"llm_activation_([a-z][a-z0-9]{0,31})_([0-9a-f]{32})")


def is_ref(value: object) -> bool:
    """True for a well-formed snapshot name in our own namespace."""
    return isinstance(value, str) and _REF.fullmatch(value) is not None


def ref_provider(ref: object) -> str | None:
    """The provider preset id a snapshot was taken from, or None."""
    match = _REF.fullmatch(ref) if isinstance(ref, str) else None
    return match.group(1) if match else None


def new_ref(provider_id: str) -> str:
    ref = f"{PREFIX}{provider_id}_{uuid.uuid4().hex}"
    if not is_ref(ref):
        raise ValueError("Unsupported provider id for an activation")
    return ref


def endpoint(url: object) -> str:
    return url.strip().rstrip("/") if isinstance(url, str) else ""


def encode(base_url: str, key: str) -> str:
    return json.dumps(
        {"base_url": endpoint(base_url), "key": key}, separators=(",", ":"),
    )


def decode(value: str) -> tuple[str, str] | None:
    """(endpoint, key) from a stored snapshot, or None when malformed/empty."""
    if not value:
        return None
    try:
        data = json.loads(value)
    except ValueError:
        return None
    if not isinstance(data, dict):
        return None
    url, key = data.get("base_url"), data.get("key")
    if not isinstance(url, str) or not isinstance(key, str) or not url or not key:
        return None
    return url, key


def resolve(ref: str, base_url: str, get_secret: Callable[[str], str]) -> str:
    """The snapshot key named by `ref`, only for the endpoint it is bound to.

    Anything else (bad name, missing or malformed snapshot, another endpoint)
    yields "" — never a fallback to some other key.
    """
    if not is_ref(ref):
        log.warning("ignoring an invalid llm.api_key_ref; activate the provider again")
        return ""
    snapshot = decode(get_secret(ref))
    if snapshot is None:
        log.warning("the activated provider key is missing; activate the provider again")
        return ""
    bound_url, key = snapshot
    if bound_url != endpoint(base_url):
        log.warning(
            "the activated provider key belongs to another endpoint; "
            "activate the provider again"
        )
        return ""
    return key


def read_index(get_secret: Callable[[str], str]) -> list[str]:
    raw = get_secret(INDEX_NAME)
    if not raw:
        return []
    try:
        names = json.loads(raw)
    except ValueError:
        log.warning("ignoring a malformed provider activation index")
        return []
    if not isinstance(names, list):
        return []
    return list(dict.fromkeys(name for name in names if is_ref(name)))


def encode_index(refs: Iterable[str]) -> str:
    return json.dumps([ref for ref in dict.fromkeys(refs) if is_ref(ref)])
