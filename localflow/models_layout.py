"""Pure state rules and geometry for the Models & keys page (no AppKit).

Status, "which provider is active", import-tick defaults and every frame live
here so they can be tested without macOS; `hub_mac` only renders them.
"""

from localflow import provider_keys
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
SUMMARY_BUTTON_W = 188
SUMMARY_ROW_H = 38  # caption line + value line + gap
SUMMARY_HEIGHT = 2 * CARD_PADDING + 2 * SUMMARY_ROW_H
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
    otherwise active and unchanged (no secret lookup for other rows)."""
    if show_use_button(preset, config, url=url, model=model):
        return True
    return show_use_button(
        preset, config, url=url, model=model,
        key_changed=provider_keys.key_needs_activation(preset.id),
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
    """(speech text, cleanup & Ask text) for the summary card."""
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


def summary_layout(width):
    inner = width - 2 * CARD_PADDING
    text_w = inner - SUMMARY_BUTTON_W - CONTROL_GAP
    button_x = width - CARD_PADDING - SUMMARY_BUTTON_W
    stack = 2 * ACTION_H + MODELS_ROW_GAP
    top = (SUMMARY_HEIGHT - stack) // 2
    return {
        "height": SUMMARY_HEIGHT,
        "rows": [
            {"caption": (CARD_PADDING, CARD_PADDING + i * SUMMARY_ROW_H, text_w, 16),
             "value": (CARD_PADDING, CARD_PADDING + i * SUMMARY_ROW_H + 16, text_w, 18)}
            for i in range(2)
        ],
        "change": (button_x, top, SUMMARY_BUTTON_W, ACTION_H),
        "import": (button_x, top + ACTION_H + MODELS_ROW_GAP, SUMMARY_BUTTON_W, ACTION_H),
    }


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
