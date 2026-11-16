"""PR29 review finding 7: the web Settings key must reach the active client.

`factory.llm_api_key` prefers a config's endpoint-bound activation snapshot
(`llm_key_ref`) over the generic `llm_api_key`. The web Settings endpoint
(`PUT /api/secrets/llm_api_key` -> `api.update_secret`) only ever wrote the
generic secret, so after a provider was activated on Models & keys a key
replaced (or cleared) in Settings was never used, not even after a restart.

Expected behaviour (the tests below pin it down):

* normal: with an activated config, the web update rewrites that config's own
  snapshot slot in place, bound to the same endpoint; the running clients and
  a freshly loaded config both send the new key. No config write, no new
  snapshot, and the saved provider key is not touched.
* clear: clearing removes the active key; clients send no key, the status says
  `has_key: false`, and nothing ever falls back to the generic `llm_api_key`.
* cached endpoint security: the generic `llm_api_key` belongs to legacy
  configs. Rotating/clearing B's key on the web must never rewrite it, so a
  config cached before activation (endpoint A) never sends B's key to A, and
  B never picks up A's key.
* error: an unavailable keyring leaves the active key and endpoint as they
  were; an invalid `llm_key_ref` is refused without writing anything; the
  existing guards (unknown secret, non-string value, API token) are unchanged.

Everything is in memory or under tmp_path: no Keychain, no network. All key
values are fake test strings.
"""

from dataclasses import replace

import pytest
from fastapi.testclient import TestClient

from localflow import api, llm_activation, secrets, server
from localflow import provider_keys as keys
from localflow.config import Config, config_path, load_config, save_config
from localflow.meetings import MeetingSummarizer
from localflow.providers import factory

OPENAI = keys.get_preset("openai")
GROQ = keys.get_preset("groq")
KEY_A = "fake-key-openai-aaaa"
KEY_B = "fake-key-groq-bbbb"
KEY_B2 = "fake-key-groq-web-rotated-cccc"
KEY_B3 = "fake-key-groq-native-saved-dddd"
TOKEN = "test-token"
AUTH = {"X-Localflow-Token": TOKEN}


class Unavailable(secrets.SecretsUnavailable):
    pass


@pytest.fixture
def vault(monkeypatch, tmp_path):
    """In-memory secrets shared by provider_keys, api, the factory and server."""
    store = {}
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))

    def get_secret(name):
        return store.get(name, "")

    def set_secret(name, value):
        store[name] = value

    def delete_secret(name):
        store.pop(name, None)

    for target in (secrets, keys.secrets, api):
        monkeypatch.setattr(target, "get_secret", get_secret)
        monkeypatch.setattr(target, "set_secret", set_secret)
        monkeypatch.setattr(target, "delete_secret", delete_secret)
    monkeypatch.setattr(factory, "get_secret", get_secret)
    return store


@pytest.fixture
def web(monkeypatch, vault):
    """The real server app with a valid token; startup hooks are not run."""
    monkeypatch.setattr(server, "_api_token", TOKEN)
    monkeypatch.setattr(server, "load_or_create", lambda: TOKEN)
    monkeypatch.setattr(server, "_ollama", None)
    monkeypatch.setattr(server, "_cleaner", object())
    monkeypatch.setattr(server, "_summarizer", None)
    monkeypatch.setattr(server, "_prompt_gen", None)
    monkeypatch.setattr(server, "_config", base_config())
    return TestClient(
        server.app, base_url="http://127.0.0.1:8756", client=("127.0.0.1", 1234)
    )


def base_config():
    return Config(setup_version=1, meetings_enabled=False, meeting_watch=False)


def cloud_a():
    return replace(
        base_config(), llm_provider="openai-compatible",
        llm_base_url=OPENAI.base_url, llm_model="model-a",
    )


def pair(config):
    """What Ask/cleanup would send for `config` (cached or freshly loaded)."""
    llm = factory.build_llm(config)
    return llm.base_url, (getattr(llm, "api_key", None) or "")


def restart_pair():
    """A fresh process: load the config from disk and build the client."""
    return pair(load_config())


def running_pairs():
    """What the running server's existing clients send right now."""
    summarizer = server._summarizer
    assert isinstance(summarizer, MeetingSummarizer)
    return {
        (server._ollama.base_url, server._ollama.api_key or ""),
        (summarizer.llm.base_url, summarizer.llm.api_key or ""),
    }


def put_key(web, value, *, name="llm_api_key", headers=AUTH):
    return web.put(f"/api/secrets/{name}", json={"value": value}, headers=headers)


def snapshots(vault):
    return {n for n in vault if llm_activation.is_ref(n)}


def config_bytes():
    path = config_path()
    return path.read_bytes() if path.exists() else None


def activate_b(monkeypatch, *, legacy_a=False):
    """Provider B (groq) activated natively; optionally a legacy A before it.

    Returns (config cached before activation or None, activated config B).
    """
    cached_a = None
    if legacy_a:
        save_config(cloud_a())
        secrets.set_secret("llm_api_key", KEY_A)  # legacy generic key for A
        cached_a = load_config()
        assert cached_a.llm_key_ref == ""
        assert pair(cached_a) == (OPENAI.base_url, KEY_A)
    keys.save_key("groq", KEY_B)
    active = keys.activate("groq", model="model-b", config=cached_a or cloud_a())
    assert active.llm_key_ref
    assert restart_pair() == (GROQ.base_url, KEY_B)
    monkeypatch.setattr(server, "_config", active)
    return cached_a, active


# --------------------------------------------------------------- normal ---


def test_web_update_after_activation_reaches_running_clients_and_restart(
    web, vault, monkeypatch,
):
    _, active = activate_b(monkeypatch)
    disk_before = config_bytes()
    refs_before = snapshots(vault)
    index_before = vault.get(llm_activation.INDEX_NAME)

    response = put_key(web, KEY_B2)

    assert response.status_code == 200
    assert response.json()["llm_api_key"] is True
    assert running_pairs() == {(GROQ.base_url, KEY_B2)}
    assert server._cleaner is None  # rebuilt lazily from the new client
    assert pair(server._config) == (GROQ.base_url, KEY_B2)
    assert restart_pair() == (GROQ.base_url, KEY_B2)
    # Rotated in place: same binding, same endpoint, no new config or snapshot.
    assert load_config().llm_key_ref == active.llm_key_ref
    assert llm_activation.decode(vault[active.llm_key_ref]) == (GROQ.base_url, KEY_B2)
    assert config_bytes() == disk_before
    assert snapshots(vault) == refs_before
    assert vault.get(llm_activation.INDEX_NAME) == index_before
    assert vault["provider_key_groq"] == KEY_B  # the saved provider key is not the web key


def test_secret_status_reports_the_active_binding_not_the_unused_generic(
    web, vault, monkeypatch,
):
    activate_b(monkeypatch)
    assert "llm_api_key" not in vault  # only the snapshot holds B's key

    status = web.get("/api/secrets", headers=AUTH)

    assert status.status_code == 200
    assert status.json()["llm_api_key"] is True


def test_legacy_config_without_ref_web_update_still_uses_generic_key(web, vault, monkeypatch):
    save_config(cloud_a())
    monkeypatch.setattr(server, "_config", load_config())
    assert server._config.llm_key_ref == ""

    response = put_key(web, KEY_A)

    assert response.status_code == 200
    assert response.json()["llm_api_key"] is True
    assert vault["llm_api_key"] == KEY_A
    assert snapshots(vault) == set()
    assert running_pairs() == {(OPENAI.base_url, KEY_A)}
    assert restart_pair() == (OPENAI.base_url, KEY_A)

    cleared = put_key(web, "")

    assert cleared.status_code == 200
    assert cleared.json()["llm_api_key"] is False
    assert "llm_api_key" not in vault
    assert running_pairs() == {(OPENAI.base_url, "")}


def test_native_save_after_web_rotation_still_requires_use(web, vault, monkeypatch):
    _, active = activate_b(monkeypatch)
    assert put_key(web, KEY_B2).status_code == 200

    keys.save_key("groq", KEY_B3)  # Models & keys "Save" without "Use"

    assert restart_pair() == (GROQ.base_url, KEY_B2)
    assert pair(active) == (GROQ.base_url, KEY_B2)
    assert keys.key_needs_activation("groq", load_config()) is True

    used = keys.activate("groq", model="model-b")  # "Use"

    assert restart_pair() == (GROQ.base_url, KEY_B3)
    assert used.llm_key_ref != active.llm_key_ref  # a fresh immutable snapshot
    assert keys.key_needs_activation("groq", load_config()) is False


# ---------------------------------------------------------------- clear ---


def test_web_clear_after_activation_sends_no_key_and_reports_none(web, vault, monkeypatch):
    _, active = activate_b(monkeypatch)
    vault["llm_api_key"] = "fake-stale-generic-zzzz"  # must never be a fallback
    disk_before = config_bytes()

    response = put_key(web, "")

    assert response.status_code == 200
    assert response.json()["llm_api_key"] is False
    assert web.get("/api/secrets", headers=AUTH).json()["llm_api_key"] is False
    assert running_pairs() == {(GROQ.base_url, "")}
    assert restart_pair() == (GROQ.base_url, "")
    assert active.llm_key_ref not in vault
    assert vault["llm_api_key"] == "fake-stale-generic-zzzz"  # untouched, unused
    assert vault["provider_key_groq"] == KEY_B
    assert config_bytes() == disk_before


# ------------------------------------------- cached endpoint security ---


def test_web_rotation_of_b_never_sends_b_key_to_cached_legacy_a(web, vault, monkeypatch):
    cached_a, active = activate_b(monkeypatch, legacy_a=True)

    response = put_key(web, KEY_B2)

    assert response.status_code == 200
    # Legacy A's generic key and A's cached config are exactly as before.
    assert vault["llm_api_key"] == KEY_A
    assert pair(cached_a) == (OPENAI.base_url, KEY_A)
    # The updated running config and a fresh load use B2, only for B.
    assert running_pairs() == {(GROQ.base_url, KEY_B2)}
    assert pair(server._config) == (GROQ.base_url, KEY_B2)
    assert pair(active) == (GROQ.base_url, KEY_B2)
    assert restart_pair() == (GROQ.base_url, KEY_B2)


def test_web_clear_of_b_never_leaks_legacy_a_key_to_b(web, vault, monkeypatch):
    cached_a, active = activate_b(monkeypatch, legacy_a=True)

    response = put_key(web, "")

    assert response.status_code == 200
    assert response.json()["llm_api_key"] is False
    assert vault["llm_api_key"] == KEY_A
    assert pair(cached_a) == (OPENAI.base_url, KEY_A)
    assert running_pairs() == {(GROQ.base_url, "")}
    assert pair(active) == (GROQ.base_url, "")
    assert restart_pair() == (GROQ.base_url, "")


def test_removing_provider_key_revokes_web_rotated_slot_without_fallback(
    web, vault, monkeypatch,
):
    cached_a, active = activate_b(monkeypatch, legacy_a=True)
    assert put_key(web, KEY_B2).status_code == 200

    keys.remove_key("groq", config=load_config())

    assert restart_pair() == (GROQ.base_url, "")
    assert pair(active) == (GROQ.base_url, "")
    assert pair(cached_a) == (OPENAI.base_url, KEY_A)
    assert not any(KEY_B2 in value for value in vault.values())


def test_web_key_recreated_after_remove_is_indexed_and_revocable_without_config(
    web, vault, monkeypatch,
):
    cached_a, active = activate_b(monkeypatch, legacy_a=True)
    ref = active.llm_key_ref
    keys.remove_key("groq", config=active)  # native Remove
    assert llm_activation.INDEX_NAME not in vault
    assert ref not in vault
    disk_before = config_bytes()

    cleared = put_key(web, "")  # clearing never creates index entries

    assert cleared.status_code == 200
    assert llm_activation.INDEX_NAME not in vault
    assert ref not in vault

    recreated = put_key(web, KEY_B2)  # web recreates the config's own slot

    assert recreated.status_code == 200
    assert llm_activation.read_index(secrets.get_secret) == [ref]
    assert restart_pair() == (GROQ.base_url, KEY_B2)
    assert running_pairs() == {(GROQ.base_url, KEY_B2)}
    assert config_bytes() == disk_before

    keys.save_key("groq", KEY_B3)  # Save without Use keeps B2 active

    assert restart_pair() == (GROQ.base_url, KEY_B2)
    assert keys.key_needs_activation("groq", load_config()) is True

    keys.remove_key("groq")  # `lf keys remove`: no config argument

    assert ref not in vault
    assert restart_pair() == (GROQ.base_url, "")
    assert pair(active) == (GROQ.base_url, "")
    assert vault["llm_api_key"] == KEY_A  # no fallback, legacy A untouched
    assert pair(cached_a) == (OPENAI.base_url, KEY_A)
    assert not any(k in v for k in (KEY_B2, KEY_B3) for v in vault.values())


# ---------------------------------------------------------------- error ---


@pytest.mark.parametrize("value", [KEY_B2, ""], ids=["update", "clear"])
def test_unavailable_keyring_leaves_active_key_and_endpoint(web, vault, monkeypatch, value):
    cached_a, active = activate_b(monkeypatch, legacy_a=True)
    before = dict(vault)
    disk_before = config_bytes()
    clients_before = (server._ollama, server._summarizer)

    def refuse(*_args):
        raise Unavailable("locked")

    for target in (secrets, keys.secrets, api):
        monkeypatch.setattr(target, "set_secret", refuse)
        monkeypatch.setattr(target, "delete_secret", refuse)

    response = put_key(web, value)

    assert response.status_code == 503
    assert vault == before
    assert config_bytes() == disk_before
    assert server._config == active
    assert (server._ollama, server._summarizer) == clients_before
    assert restart_pair() == (GROQ.base_url, KEY_B)
    assert pair(active) == (GROQ.base_url, KEY_B)
    assert pair(cached_a) == (OPENAI.base_url, KEY_A)


@pytest.mark.parametrize(
    "bad_ref",
    ["llm_activation_invalid", "provider_key_openai", "fireworks_api_key", "llm_api_key"],
)
@pytest.mark.parametrize("value", [KEY_B2, ""], ids=["update", "clear"])
def test_invalid_key_ref_is_refused_without_any_write(web, vault, monkeypatch, bad_ref, value):
    save_config(cloud_a())
    vault.update({
        "llm_api_key": KEY_A,
        "provider_key_openai": KEY_A,
        "fireworks_api_key": "fake-fireworks-ffff",
    })
    monkeypatch.setattr(server, "_config", replace(load_config(), llm_key_ref=bad_ref))
    before = dict(vault)
    disk_before = config_bytes()

    response = put_key(web, value)

    assert response.status_code in (400, 403, 409)
    assert vault == before  # never overwrite (or delete) an arbitrary secret
    assert config_bytes() == disk_before


def test_unknown_secret_non_string_and_token_guards_are_unchanged(web, vault, monkeypatch):
    activate_b(monkeypatch, legacy_a=True)
    before = dict(vault)

    assert put_key(web, KEY_B2, name="provider_key_nope").status_code == 404
    assert put_key(web, KEY_B2, name="llm_activation_groq_" + "0" * 32).status_code == 404
    assert web.put(
        "/api/secrets/llm_api_key", json={"value": 5}, headers=AUTH
    ).status_code == 400
    assert web.put(
        "/api/secrets/llm_api_key", json={"value": None}, headers=AUTH
    ).status_code == 400
    assert put_key(web, KEY_B2, headers={}).status_code == 401
    assert put_key(web, KEY_B2, headers={"X-Localflow-Token": "wrong"}).status_code == 401
    assert web.get("/api/secrets").status_code == 401

    assert vault == before
    assert restart_pair() == (GROQ.base_url, KEY_B)
