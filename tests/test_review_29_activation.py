"""PR29 review finding 1: provider activation must keep key and endpoint paired.

`provider_keys.activate` used to write the new generic `llm_api_key` and only
then save the config. When the save failed, the old endpoint was left paired
with the new provider's key, and Ask/cleanup (`factory.build_llm`) would send
provider B's key to provider A. Changing the order only moves the mismatch
(the new endpoint with the old key), so the tests below watch the effective
(endpoint, key) pair after every single write, not just the end state.

Activation now writes a new, immutable, endpoint-bound snapshot and then
atomically saves a config naming it (`llm.api_key_ref`); the shared
`llm_api_key` is never rewritten, so a config cached before activation keeps
its own key.

Everything is in memory or under tmp_path: no Keychain, no network.
"""

import logging
from dataclasses import replace

import pytest

from localflow import config as config_module
from localflow import llm_activation, secrets
from localflow import provider_keys as keys
from localflow.config import Config, ConfigError, load_config, save_config
from localflow.providers import factory

OPENAI = keys.get_preset("openai")
GROQ = keys.get_preset("groq")
KEY_A = "fake-key-openai-aaaa"
KEY_B = "fake-key-groq-bbbb"
KEY_B2 = "fake-key-groq-rotated-cccc"
ALL_KEYS = (KEY_A, KEY_B, KEY_B2)


@pytest.fixture
def vault(monkeypatch, tmp_path):
    """In-memory secrets shared by provider_keys and the LLM factory."""
    store = {}
    monkeypatch.setenv("LOCALFLOW_CONFIG", str(tmp_path / "config.toml"))

    def get_secret(name):
        return store.get(name, "")

    def set_secret(name, value):
        store[name] = value

    def delete_secret(name):
        store.pop(name, None)

    for target in (secrets, keys.secrets):
        monkeypatch.setattr(target, "get_secret", get_secret)
        monkeypatch.setattr(target, "set_secret", set_secret)
        monkeypatch.setattr(target, "delete_secret", delete_secret)
    monkeypatch.setattr(factory, "get_secret", get_secret)
    return store


def cloud_a():
    return replace(
        Config(setup_version=1), llm_provider="openai-compatible",
        llm_base_url=OPENAI.base_url, llm_model="model-a",
    )


def pair(config):
    """What Ask/cleanup would send for `config` (cached or freshly loaded)."""
    llm = factory.build_llm(config)
    return llm.base_url, getattr(llm, "api_key", None)


def effective_pair():
    return pair(load_config())


def snapshots(vault):
    return {name for name in vault if name.startswith(llm_activation.PREFIX)}


def install_observer(monkeypatch, *, fail_save=False):
    """Record the effective pair after every secret write and config save."""
    seen = []
    real_set, real_delete = keys.secrets.set_secret, keys.secrets.delete_secret

    def set_secret(name, value):
        real_set(name, value)
        seen.append(("set_secret", name, effective_pair()))

    def delete_secret(name):
        real_delete(name)
        seen.append(("delete_secret", name, effective_pair()))

    def failing_or_real_save(config, *args, **kwargs):
        if fail_save:
            raise OSError("disk full")
        result = save_config(config, *args, **kwargs)
        seen.append(("save_config", None, effective_pair()))
        return result

    for target in (secrets, keys.secrets):
        monkeypatch.setattr(target, "set_secret", set_secret)
        monkeypatch.setattr(target, "delete_secret", delete_secret)
    monkeypatch.setattr(keys, "save_config", failing_or_real_save)
    return seen


@pytest.fixture(params=["legacy", "activated"])
def on_cloud_a(request, vault):
    """Provider A active; provider B has a saved key.

    legacy: a config from before snapshots (no ref, generic `llm_api_key`).
    activated: A was activated by this code (config names A's snapshot)."""
    vault.update({"provider_key_openai": KEY_A, "provider_key_groq": KEY_B})
    if request.param == "legacy":
        vault["llm_api_key"] = KEY_A
        save_config(cloud_a())
    else:
        keys.activate("openai", model="model-a", config=cloud_a())
    assert effective_pair() == (OPENAI.base_url, KEY_A)
    return request.param


# --------------------------------------------------------- failed saves ---


def test_failed_config_save_keeps_old_endpoint_with_old_key(vault, on_cloud_a, monkeypatch):
    before = dict(vault)
    install_observer(monkeypatch, fail_save=True)

    with pytest.raises(OSError, match="disk full"):
        keys.activate("groq", model="model-b")

    on_disk = load_config()
    assert (on_disk.llm_provider, on_disk.llm_base_url, on_disk.llm_model) == (
        "openai-compatible", OPENAI.base_url, "model-a",
    )
    assert vault.get("llm_api_key") == before.get("llm_api_key")  # never B's key
    assert vault["provider_key_openai"] == KEY_A
    assert vault["provider_key_groq"] == KEY_B  # saved keys are never touched
    assert vault == before  # only the new snapshot (and index entry) were undone
    assert effective_pair() == (OPENAI.base_url, KEY_A)


def test_failed_config_save_restores_a_missing_original_key(vault, monkeypatch):
    # Local Ollama active: no generic cloud key exists yet.
    save_config(Config(setup_version=1))
    vault["provider_key_groq"] = KEY_B
    install_observer(monkeypatch, fail_save=True)

    with pytest.raises(OSError, match="disk full"):
        keys.activate("groq", model="model-b")

    on_disk = load_config()
    assert (on_disk.llm_provider, on_disk.llm_base_url) == (
        "ollama", "http://localhost:11434",
    )
    # Still absent, not left as B's key (nor an empty stored secret).
    assert "llm_api_key" not in vault
    assert vault == {"provider_key_groq": KEY_B}


def test_failed_activation_never_exposes_a_mismatched_pair(vault, on_cloud_a, monkeypatch):
    seen = install_observer(monkeypatch, fail_save=True)

    with pytest.raises(OSError):
        keys.activate("groq", model="model-b")

    mismatched = [s for s in seen if s[2] != (OPENAI.base_url, KEY_A)]
    assert mismatched == [], (
        "a reader between writes would send the wrong key to the wrong endpoint"
    )


def test_failed_snapshot_write_keeps_the_prior_pair(vault, on_cloud_a, monkeypatch):
    """The key copy itself fails (Keychain refuses the new item)."""
    before = dict(vault)
    real_set = keys.secrets.set_secret

    def refuse_snapshots(name, value):
        if llm_activation.is_ref(name):
            raise secrets.SecretsUnavailable("locked")
        real_set(name, value)

    monkeypatch.setattr(keys.secrets, "set_secret", refuse_snapshots)

    with pytest.raises(secrets.SecretsUnavailable):
        keys.activate("groq", model="model-b")

    assert vault == before
    assert effective_pair() == (OPENAI.base_url, KEY_A)


def test_keychain_unavailable_leaves_everything_untouched(vault, on_cloud_a, monkeypatch):
    before = dict(vault)

    def unavailable(name, value):
        raise secrets.SecretsUnavailable("locked")

    monkeypatch.setattr(keys.secrets, "set_secret", unavailable)

    with pytest.raises(secrets.SecretsUnavailable):
        keys.activate("groq", model="model-b")

    assert load_config().llm_base_url == OPENAI.base_url
    assert vault == before
    assert effective_pair() == (OPENAI.base_url, KEY_A)


def test_failed_cleanup_is_reported_without_values_and_never_referenced(
    vault, on_cloud_a, monkeypatch, caplog,
):
    """Save fails *and* deleting the new snapshot fails: the original error
    surfaces, the log says what happened without any key, no config names
    the orphan, and removing B's key later still revokes it."""
    install_observer(monkeypatch, fail_save=True)
    real_delete = keys.secrets.delete_secret

    def refuse_delete(name):
        raise secrets.SecretsUnavailable("locked")

    monkeypatch.setattr(keys.secrets, "delete_secret", refuse_delete)
    caplog.set_level(logging.DEBUG)

    with pytest.raises(OSError, match="disk full"):
        keys.activate("groq", model="model-b")

    assert "could not be removed" in caplog.text
    for key in ALL_KEYS:
        assert key not in caplog.text
    [orphan] = [s for s in snapshots(vault) if llm_activation.ref_provider(s) == "groq"]
    assert load_config().llm_key_ref != orphan
    assert effective_pair() == (OPENAI.base_url, KEY_A)
    assert orphan in llm_activation.read_index(vault.get)

    monkeypatch.setattr(keys.secrets, "delete_secret", real_delete)
    keys.remove_key("groq")
    assert orphan not in vault


# ---------------------------------------------------- successful paths ---


def test_successful_activation_never_exposes_a_mismatched_pair(
    vault, on_cloud_a, monkeypatch,
):
    """Every intermediate state is either fully A or fully B.

    Fails for 'key then config' (A's endpoint with B's key) and equally for
    'config then key' (B's endpoint with A's key)."""
    legacy_key = vault.get("llm_api_key")
    seen = install_observer(monkeypatch)

    updated = keys.activate("groq", model="model-b")

    allowed = {(OPENAI.base_url, KEY_A), (GROQ.base_url, KEY_B)}
    assert [s for s in seen if s[2] not in allowed] == []
    assert (updated.llm_base_url, updated.llm_model) == (GROQ.base_url, "model-b")
    assert effective_pair() == (GROQ.base_url, KEY_B)  # what the factory sends
    # The shared legacy key is not rewritten: cached A configs depend on it.
    assert vault.get("llm_api_key") == legacy_key
    assert keys.key_needs_activation("groq", updated) is False


def test_cached_config_a_stays_paired_after_activating_b(vault, on_cloud_a):
    cached_a = load_config()  # e.g. the running dictation process's config

    keys.activate("groq", model="model-b")

    assert pair(cached_a) == (OPENAI.base_url, KEY_A)
    assert effective_pair() == (GROQ.base_url, KEY_B)


def test_saving_a_replacement_key_does_not_activate_it(vault, on_cloud_a):
    active = keys.activate("groq", model="model-b")

    keys.save_key("groq", KEY_B2)

    assert pair(active) == (GROQ.base_url, KEY_B)  # snapshot is immutable
    assert effective_pair() == (GROQ.base_url, KEY_B)
    assert keys.key_needs_activation("groq", active) is True  # Use is offered

    rotated = keys.activate("groq", model="model-b", config=active)

    assert rotated.llm_key_ref != active.llm_key_ref
    assert effective_pair() == (GROQ.base_url, KEY_B2)
    assert pair(active) == (GROQ.base_url, KEY_B)  # a cached config is unchanged
    assert keys.key_needs_activation("groq", rotated) is False


def test_removing_the_active_key_disables_every_client_using_it(vault, on_cloud_a):
    first = keys.activate("groq", model="model-b")
    keys.save_key("groq", KEY_B2)
    second = keys.activate("groq", model="model-b", config=first)

    keys.remove_key("groq", config=second)

    assert effective_pair() == (GROQ.base_url, "")  # no key, and never A's key
    assert pair(first) == (GROQ.base_url, "")  # older cached activation too
    assert [s for s in snapshots(vault) if llm_activation.ref_provider(s) == "groq"] == []
    assert not any(KEY_B in value or KEY_B2 in value for value in vault.values())
    assert keys.key_needs_activation("groq", second) is False
    assert "provider_key_groq" not in vault


def test_removing_another_providers_key_leaves_the_active_one(vault, on_cloud_a):
    active = keys.activate("groq", model="model-b")

    keys.remove_key("openai", config=active)

    assert effective_pair() == (GROQ.base_url, KEY_B)


def test_reactivating_the_same_key_reuses_its_snapshot(vault, on_cloud_a):
    first = keys.activate("groq", model="model-b")
    count = len(snapshots(vault))

    again = keys.activate("groq", model="model-c", config=first)

    assert again.llm_key_ref == first.llm_key_ref
    assert len(snapshots(vault)) == count
    assert effective_pair() == (GROQ.base_url, KEY_B)


def test_snapshot_names_are_unique_and_in_our_namespace(vault, on_cloud_a):
    first = keys.activate("groq", model="model-b")
    keys.save_key("groq", KEY_B2)
    second = keys.activate("groq", model="model-b", config=first)

    for ref in (first.llm_key_ref, second.llm_key_ref):
        assert llm_activation.is_ref(ref)
        assert llm_activation.ref_provider(ref) == "groq"
        assert ref != "provider_key_groq"
    assert first.llm_key_ref != second.llm_key_ref


def test_ollama_stays_keyless(vault, on_cloud_a):
    before = snapshots(vault)

    local = keys.activate("ollama", model="llama3.2:3b")

    assert local.llm_key_ref == ""
    assert snapshots(vault) == before
    assert effective_pair() == ("http://localhost:11434", None)


def test_no_key_value_is_logged_or_written_to_the_config(vault, on_cloud_a, caplog, tmp_path):
    caplog.set_level(logging.DEBUG)
    keys.activate("groq", model="model-b")
    keys.save_key("groq", KEY_B2)
    keys.activate("groq", model="model-b")
    keys.remove_key("groq")

    text = (tmp_path / "config.toml").read_text(encoding="utf-8")
    for key in ALL_KEYS:
        assert key not in caplog.text
        assert key not in text


# --------------------------------------------- binding and legacy rules ---


def test_a_snapshot_is_never_sent_to_another_endpoint(vault, on_cloud_a):
    active = keys.activate("groq", model="model-b")
    moved = replace(active, llm_base_url="https://attacker.example/v1")  # hand-edited

    assert pair(moved) == ("https://attacker.example/v1", "")


def test_choosing_another_endpoint_by_hand_drops_the_binding(vault, on_cloud_a):
    active = keys.activate("groq", model="model-b")

    moved = config_module.apply_config_update(
        active, {"llm": {"base_url": "https://llm.example.test/v1"}},
    )
    same = config_module.apply_config_update(active, {"llm": {"model": "model-c"}})

    assert moved.llm_key_ref == ""  # legacy llm_api_key flow, as before
    assert same.llm_key_ref == active.llm_key_ref


def test_the_ref_can_only_be_cleared_through_config_updates(vault, on_cloud_a):
    active = keys.activate("groq", model="model-b")
    other = llm_activation.new_ref("groq")

    with pytest.raises(ConfigError):
        config_module.apply_config_update(active, {"llm": {"api_key_ref": other}})
    with pytest.raises(ConfigError):
        config_module.apply_config_update(
            active, {"llm": {"api_key_ref": "provider_key_groq"}},
        )
    unchanged = config_module.apply_config_update(
        active, {"llm": {"api_key_ref": active.llm_key_ref}},
    )
    cleared = config_module.apply_config_update(active, {"llm": {"api_key_ref": ""}})
    assert unchanged.llm_key_ref == active.llm_key_ref
    assert cleared.llm_key_ref == ""


@pytest.mark.parametrize("ref", ["provider_key_groq", "llm_api_key", "llm_activation_x"])
def test_a_ref_outside_our_namespace_fails_closed_on_load(vault, tmp_path, ref):
    path = tmp_path / "config.toml"
    path.write_text(
        'config_version = 2\n[llm]\nprovider = "openai-compatible"\n'
        f'base_url = "{GROQ.base_url}"\napi_key_ref = "{ref}"\n',
        encoding="utf-8",
    )
    vault.update({ref: KEY_B, "llm_api_key": KEY_A})

    loaded = load_config()

    assert loaded.llm_key_ref  # not a legacy config: never use its generic key
    assert pair(loaded) == (GROQ.base_url, "")
    assert pair(replace(loaded, llm_key_ref=ref)) == (GROQ.base_url, "")


@pytest.mark.parametrize("raw", ['[null, {}, [], 5]', 'null', '{}', '[]'])
def test_malformed_activation_index_entries_are_ignored(raw):
    assert llm_activation.read_index(lambda _name: raw) == []


def test_ref_round_trips_through_the_config_file(vault, on_cloud_a):
    active = keys.activate("groq", model="model-b")

    assert load_config().llm_key_ref == active.llm_key_ref
    assert f'api_key_ref = "{active.llm_key_ref}"' in config_module.config_toml(active)


def test_legacy_config_without_ref_still_uses_the_generic_key(vault, monkeypatch):
    from localflow import api

    # api binds the secret functions at import: route them to the fake vault.
    monkeypatch.setattr(api, "set_secret", secrets.set_secret)
    monkeypatch.setattr(api, "delete_secret", secrets.delete_secret)
    monkeypatch.setattr(api, "get_secret", secrets.get_secret)
    save_config(cloud_a())
    api.update_secret("llm_api_key", KEY_A)  # `lf secret set` / web secret API

    assert load_config().llm_key_ref == ""
    assert effective_pair() == (OPENAI.base_url, KEY_A)
