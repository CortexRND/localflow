"""Pure state rules and geometry for the Models & keys page (no AppKit).

Status, "which provider is active", import-tick defaults and every frame live
here so they can be tested without macOS; `hub_mac` only renders them.
"""

import re
from dataclasses import replace

from localflow import provider_keys
from localflow.config import save_config
from localflow.text_wrap import wrap_lines
from localflow.ui_tokens import (
    CARD_PADDING,
    CONTROL_GAP,
    MODELS_FIELD_HEIGHT,
    MODELS_LABEL_HEIGHT,
    MODELS_ROW_GAP,
    MODELS_ROW_HEIGHT,
)

ACTION_H = 28
SAVE_W = 64
REMOVE_W = 80
USE_W = 190
TEST_W = 140
ROW_BUTTON_W = 72
EDITOR_TOP = 8
EDITOR_BOTTOM = 12
MESSAGE_LINE = 16
IMPORT_ROW_H = 28
IMPORT_HEADER_H = 48  # card title + detail line


def _same_url(left, right):
    return (left or "").strip().rstrip("/") == (right or "").strip().rstrip("/")


def is_active(preset, config, *, url=None):
    """True only when the configured provider AND endpoint are this preset's.

    `url` is the endpoint to compare against (the typed one for the custom
    preset); by default the preset's own URL. The custom preset is active when
    the configured endpoint is not one of the named presets'.
    """
    if config.llm_provider != preset.llm_provider:
        return False
    configured = config.llm_base_url
    if preset.id == "custom":
        if url is not None:
            return bool(url.strip()) and _same_url(configured, url)
        named = {
            p.base_url.rstrip("/") for p in provider_keys.PRESETS if p.id != "custom"
        }
        return bool(configured.strip()) and configured.strip().rstrip("/") not in named
    return _same_url(configured, preset.base_url if url is None else url)


def active_preset(config):
    return next(
        (p for p in provider_keys.PRESETS if is_active(p, config)), None
    )


def provider_status(preset, config, has_key):
    """(label, (tint token, text token)) for a row's status pill."""
    if is_active(preset, config):
        return "Active", ("cobalt", "cobalt")
    if not preset.needs_key:
        return "Local", ("line", "ink")
    if has_key:
        return "Key saved", ("success", "success")
    return "No key", ("muted", "muted")


def show_use_button(preset, config, *, url, model, key_changed=False):
    """The single primary action: hidden once the editor matches what is active.

    `key_changed`: a saved key is newer than the one in use (see
    provider_keys.key_needs_activation); it keeps Use available."""
    return key_changed or not (
        is_active(preset, config, url=url)
        and (model or "").strip() == (config.llm_model or "").strip()
    )


def use_button_visible(preset, config, *, url, model):
    """show_use_button plus the key comparison, made only when the provider is
    otherwise active and unchanged (no secret lookup for other rows).

    A blank model is never offered: activation would refuse it (or quietly
    pick a default the user did not choose), so Use stays hidden until a
    model is typed or picked."""
    if not (model or "").strip():
        return False
    if show_use_button(preset, config, url=url, model=model):
        return True
    return show_use_button(
        preset, config, url=url, model=model,
        key_changed=provider_keys.key_needs_activation(preset.id, config),
    )


# Friendly names for the configured speech provider (`[stt] provider`, i.e.
# Config.stt_backend; ids as in providers.registry.STT_BUILTINS plus the
# `auto` / `mlx` aliases). Display only: nothing is resolved or loaded.
STT_LABELS = {
    "auto": "Automatic",
    "faster-whisper": "Faster Whisper",
    "mlx-whisper": "MLX Whisper",
    "mlx": "MLX Whisper",
    "parakeet": "Parakeet",
}
# Parakeet runs one fixed model and ignores [stt] model (see config.py), so
# its size is not shown as if it were chosen.
STT_FIXED_MODEL = {"parakeet": "TDT 0.6B v2"}


def speech_summary(config):
    """'Parakeet / TDT 0.6B v2', 'MLX Whisper / small', 'Automatic / base'.

    Reads only the configured provider and model size. `auto` stays
    "Automatic": which engine auto picks depends on what is installed and is
    only known once the model loads, so it is never guessed here. `stt_device`
    is not the provider and plays no part. Unknown (entry-point) ids are shown
    as they are.
    """
    provider = (getattr(config, "stt_backend", "") or "auto").strip()
    label = STT_LABELS.get(provider.lower(), provider)
    model = STT_FIXED_MODEL.get(provider.lower()) or config.model_size
    return f"{label} / {model}" if model else label


def summary_values(config):
    """(speech text, cleanup & Ask text): plain-text names of the models in
    use (the Models card shows them in its pickers and status line)."""
    speech = speech_summary(config)
    preset = active_preset(config)
    provider = preset.label if preset else config.llm_provider
    return speech, f"{provider} / {config.llm_model or 'not selected'}"


def import_states(candidates, has_key, overrides=None):
    """Tick state per candidate: ON when its provider has no saved key, OFF when it
    already has one; a tick the user changed (`overrides`) always wins."""
    overrides = overrides or {}
    states = {}
    for candidate in candidates:
        key = (candidate.provider_id, candidate.source)
        states[key] = (
            bool(overrides[key]) if key in overrides
            else not has_key(candidate.provider_id)
        )
    return states


def row_columns(width, pill_w):
    """Name+URL column, status pill and Set up/Edit button of one list row."""
    button_x = width - CARD_PADDING - ROW_BUTTON_W
    pill_x = button_x - CONTROL_GAP - pill_w
    return {
        "text": (CARD_PADDING, max(pill_x - CONTROL_GAP - CARD_PADDING, 0)),
        "pill": pill_x,
        "button": (button_x, ROW_BUTTON_W),
    }


def editor_layout(inner, *, custom, needs_key, show_use, message_lines=0):
    """Frames (x, y, w, h) relative to the editor's top-left, only for the
    controls this provider needs; nothing reserves room for a hidden one."""
    rows, y = {}, EDITOR_TOP
    x = CARD_PADDING
    if custom:
        rows["url_label"] = (x, y, inner, MODELS_LABEL_HEIGHT)
        rows["url_field"] = (x, y + 18, inner, MODELS_FIELD_HEIGHT)
        y += 18 + MODELS_FIELD_HEIGHT + MODELS_ROW_GAP
    if needs_key:
        rows["key_label"] = (x, y, inner, MODELS_LABEL_HEIGHT)
        field_w = inner - SAVE_W - REMOVE_W - 2 * CONTROL_GAP
        rows["key_field"] = (x, y + 18, field_w, MODELS_FIELD_HEIGHT)
        rows["key_save"] = (x + field_w + CONTROL_GAP, y + 18, SAVE_W, MODELS_FIELD_HEIGHT)
        rows["key_remove"] = (
            x + field_w + 2 * CONTROL_GAP + SAVE_W, y + 18, REMOVE_W, MODELS_FIELD_HEIGHT,
        )
        y += 18 + MODELS_FIELD_HEIGHT + MODELS_ROW_GAP
    rows["model_label"] = (x, y, inner, MODELS_LABEL_HEIGHT)
    rows["model_field"] = (x, y + 18, inner, MODELS_FIELD_HEIGHT)
    y += 18 + MODELS_FIELD_HEIGHT + MODELS_ROW_GAP
    action_x = x
    if show_use:
        rows["use"] = (action_x, y, USE_W, ACTION_H)
        action_x += USE_W + CONTROL_GAP
    rows["test"] = (action_x, y, TEST_W, ACTION_H)
    y += ACTION_H
    if message_lines:
        y += MODELS_ROW_GAP
        rows["message"] = (x, y, inner, message_lines * MESSAGE_LINE)
        y += message_lines * MESSAGE_LINE
    return rows, y + EDITOR_BOTTOM


def action_frames(rows, show_use):
    """(use frame, test frame) of the shared action row.

    `rows` comes from editor_layout(..., show_use=True): the Use slot always
    exists there. When Use is hidden, Test moves left into its place; the row
    height never changes, so nothing is reserved or reflowed."""
    use, test = rows["use"], rows["test"]
    if show_use:
        return use, test
    return use, (use[0], test[1], test[2], test[3])


def list_layout(width, count, expanded_index=None, editor_height=0):
    """Row tops inside one list card, with the editor inserted under the expanded row."""
    rows, y, editor_y = [], 0, None
    for index in range(count):
        rows.append(y)
        y += MODELS_ROW_HEIGHT
        if index == expanded_index:
            editor_y = y
            y += editor_height
    return {"rows": rows, "editor_y": editor_y, "height": y}


def import_height(count):
    return (
        CARD_PADDING + IMPORT_HEADER_H + count * IMPORT_ROW_H
        + MODELS_ROW_GAP + ACTION_H + CARD_PADDING
    )


# ---------------------------------------------------- change models directly ---
#
# The "Change models" card picks the cleanup & Ask model of the *active*
# provider and the speech provider/model in place. Rules live here (no
# AppKit); hub_mac renders them and runs the Ollama listing off the main
# thread. Saving only writes the config: the running app and server reload
# their clients from it (live_config RUNTIME_GROUPS) without a restart.

MAX_MODEL_ID_CHARS = 200
_MODEL_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9._:/@+\-]*")
OLLAMA_LIST_TIMEOUT = 5

# Speech providers LocalFlow ships, in menu order. Only those that can run
# on this machine are offered (providers.registry.stt_available); `auto`
# always can.
SPEECH_PROVIDER_IDS = ("auto", "mlx-whisper", "faster-whisper", "parakeet")


class ModelChoiceError(ValueError):
    """A refused model choice; the message is user-facing."""


def validate_model_id(text):
    """A cloud or Ollama model id as typed/picked: one plain token."""
    model = text.strip() if isinstance(text, str) else ""
    if not model:
        raise ModelChoiceError("Enter a model ID.")
    if len(model) > MAX_MODEL_ID_CHARS or not _MODEL_ID.fullmatch(model):
        raise ModelChoiceError(
            "Use the provider's model ID: letters, numbers and . _ - : / @ + only."
        )
    return model


def set_active_model(model, config, *, expected=None, save=save_config):
    """Change only the active provider's model and save; returns the config.

    Provider, endpoint and the endpoint-bound key reference are left exactly
    as they are, so a stored key never has to be entered again and no key is
    re-paired with another endpoint (switching providers still needs that
    provider's explicit "Use"). `expected` is the (provider, base_url) the
    control was drawn for: if the saved config moved to another provider
    since, nothing is written.
    """
    chosen = validate_model_id(model)
    if expected is not None and (config.llm_provider, _norm(config.llm_base_url)) != (
        expected[0], _norm(expected[1])
    ):
        raise ModelChoiceError("Your cleanup provider changed meanwhile; pick the model again.")
    if chosen == config.llm_model:
        return config
    updated = replace(config, llm_model=chosen)
    save(updated)
    return updated


def _norm(url):
    return (url or "").strip().rstrip("/")


def _stt_available(provider_id):
    from localflow.providers import registry

    return registry.stt_available(provider_id)


def current_speech_provider(config):
    """The configured speech provider id (`mlx` is the same as `mlx-whisper`)."""
    current = (getattr(config, "stt_backend", "") or "auto").strip()
    return "mlx-whisper" if current == "mlx" else current


def speech_provider_choices(config, available=None):
    """[(id, label)] the speech popup offers. The configured provider is
    listed even when unavailable here, so the popup never shows a lie."""
    available = available or _stt_available
    current = current_speech_provider(config)
    choices = [
        (pid, STT_LABELS[pid]) for pid in SPEECH_PROVIDER_IDS
        if pid == "auto" or pid == current or available(pid)
    ]
    if current not in [pid for pid, _label in choices]:
        choices.append((current, f"{STT_LABELS.get(current.lower(), current)} (not offered)"))
    return choices


def speech_model_choices(provider):
    """[(size, label)] for a speech provider; () for Parakeet (fixed model)."""
    from localflow.model_storage import _FW_CATALOG, _MLX_CATALOG, _SIZE_LABELS

    if provider in STT_FIXED_MODEL:
        return ()
    sizes = _MLX_CATALOG if provider in ("mlx-whisper", "mlx") else _FW_CATALOG
    return tuple((size, _SIZE_LABELS[size]) for size in sizes)


def set_speech(provider, model, config, *, available=None, save=save_config):
    """Save the speech provider and model together; returns the config.

    Only providers this machine can run (plus the configured one) and only
    their catalog sizes are accepted. Parakeet runs one fixed model, so its
    `model` is ignored and the saved size is left alone. Providers that only
    run on the default device get `stt_device = "auto"` so the pair can load.
    Nothing is loaded here: the app and server load it in the background.
    An unchanged choice is saved too: re-saving is how picking the same model
    again asks the running app to retry it after a failed load (a healthy
    model is not reloaded).
    """
    offered = [pid for pid, _label in speech_provider_choices(config, available)]
    if provider not in offered or provider not in SPEECH_PROVIDER_IDS:
        raise ModelChoiceError("That speech engine isn't available on this computer.")
    if provider in STT_FIXED_MODEL:
        size = config.model_size
    else:
        sizes = [size for size, _label in speech_model_choices(provider)]
        size = model if model in sizes else (
            config.model_size if config.model_size in sizes else "base"
        )
    device = config.stt_device
    if provider in ("mlx-whisper", "parakeet"):
        device = "auto"
    updated = replace(config, stt_backend=provider, model_size=size, stt_device=device)
    save(updated)
    return updated


def ollama_models(base_url, *, client=None):
    """(installed model names, state) for the cleanup picker; never raises.

    state: "ready" (some installed), "empty" (Ollama answered, nothing
    installed) or "unavailable" (no answer). Blocking network call: run it
    off the main thread.
    """
    from localflow.providers.llm_ollama import OllamaLLM

    try:
        llm = client or OllamaLLM(base_url, "", timeout=OLLAMA_LIST_TIMEOUT)
        names = [
            info.id for info in llm.list_models()
            if isinstance(info.id, str) and _MODEL_ID.fullmatch(info.id)
            and len(info.id) <= MAX_MODEL_ID_CHARS
        ]
    except Exception:  # noqa: BLE001 - any failure is "unavailable"
        return [], "unavailable"
    return names, ("ready" if names else "empty")


def cleanup_picker(config, state, models):
    """What the cleanup model control shows: (items, selected, note).

    Ollama lists only installed models; a configured one that is not
    installed is still shown (marked) with what to do, because Ollama answers
    404 for a model it does not have.
    """
    configured = (config.llm_model or "").strip()
    if state == "loading":
        return [configured] if configured else [], configured, "Loading installed models…"
    if state == "unavailable":
        note = "Ollama isn't answering. Start Ollama, then Refresh."
        return [configured] if configured else [], configured, note
    if state == "empty":
        return [configured] if configured else [], configured, (
            "No models installed. Run `ollama pull gemma4`, then Refresh."
        )
    items = list(models)
    note = ""
    if configured and configured not in items:
        items.insert(0, configured)
        note = (
            f"{configured} isn't installed in Ollama (it would fail). Pick an "
            f"installed model, or run `ollama pull {configured}` and Refresh."
        )
    return items, configured, note


QUICK_LABEL_W = 150
QUICK_CONTROL_H = 26
QUICK_ROW_H = 34
REFRESH_W = 96
IMPORT_W = 120
STATUS_H = 16
CLEANUP_LABEL = "Cleanup & Ask"  # the row label; the provider has its own status line


def provider_status_text(config):
    """The active cleanup provider, shown under the model control."""
    preset = active_preset(config)
    return f"Active provider: {preset.label if preset else config.llm_provider}"


def note_lines(texts, width, measure):
    """Most lines any of `texts` wraps to at `width` (0 when all are empty)."""
    return max(
        (len(wrap_lines(text, max(width, 40), measure)) for text in texts if text),
        default=0,
    )


def quick_layout(width, *, ollama, fixed_speech, message_lines=0):
    """Frames (x, y, w, h) of the one Models card; nothing overlaps.

    Title row: "Change models" and Import keys…. Row 1: speech engine popup
    and model popup (or the fixed-model label). Row 2: "Cleanup & Ask": an
    Ollama popup plus Refresh, or a model-ID field for a cloud provider, with
    the active provider on a status line under it. Then a wrapped note sized
    by the caller (`message_lines`); none is reserved when there is none.
    """
    inner = width - 2 * CARD_PADDING
    x = CARD_PADDING
    controls_x = x + QUICK_LABEL_W + CONTROL_GAP
    controls_w = inner - QUICK_LABEL_W - CONTROL_GAP
    half = (controls_w - CONTROL_GAP) // 2
    y = CARD_PADDING
    frames = {
        "title": (x, y + (ACTION_H - 20) // 2, inner - IMPORT_W - CONTROL_GAP, 20),
        "import": (x + inner - IMPORT_W, y, IMPORT_W, ACTION_H),
    }
    y += ACTION_H + 2
    frames["detail"] = (x, y, inner, 16)
    y += 16 + MODELS_ROW_GAP
    frames["speech_label"] = (x, y + 5, QUICK_LABEL_W, 16)
    frames["speech_provider"] = (controls_x, y, half, QUICK_CONTROL_H)
    key = "speech_fixed" if fixed_speech else "speech_model"
    frames[key] = (controls_x + half + CONTROL_GAP, y + (5 if fixed_speech else 0),
                   controls_w - half - CONTROL_GAP, 16 if fixed_speech else QUICK_CONTROL_H)
    y += QUICK_ROW_H
    frames["cleanup_label"] = (x, y + 5, QUICK_LABEL_W, 16)
    if ollama:
        frames["cleanup_model"] = (
            controls_x, y, controls_w - REFRESH_W - CONTROL_GAP, QUICK_CONTROL_H,
        )
        frames["refresh"] = (x + inner - REFRESH_W, y, REFRESH_W, QUICK_CONTROL_H)
    else:
        frames["cleanup_model"] = (controls_x, y, controls_w, QUICK_CONTROL_H)
    y += QUICK_CONTROL_H + 4
    frames["provider_status"] = (controls_x, y, controls_w, STATUS_H)
    y += STATUS_H
    if message_lines:
        y += MODELS_ROW_GAP
        frames["message"] = (x, y, inner, message_lines * MESSAGE_LINE)
        y += message_lines * MESSAGE_LINE
    frames["height"] = y + CARD_PADDING
    return frames
