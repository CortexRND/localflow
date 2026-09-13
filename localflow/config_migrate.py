import logging

log = logging.getLogger("localflow.config")


def migrate_v1(data: dict) -> tuple[dict, str | None]:
    """Convert flat v1 config data to nested v2 data."""
    from localflow.config import _SECTIONS

    reverse = {
        field_name: (section, file_key)
        for section, fields in _SECTIONS.items()
        for file_key, field_name in fields.items()
    }
    reverse.update(
        {
            "ollama_url": ("llm", "base_url"),
            "ollama_model": ("llm", "model"),
        }
    )
    migrated: dict = {"config_version": 2}
    for key, value in data.items():
        if key == "fireworks_api_key":
            continue
        location = reverse.get(key)
        if location is None:
            log.warning("ignoring unknown v1 config key %r", key)
            continue
        section, file_key = location
        migrated.setdefault(section, {})[file_key] = value
    if "stt" in migrated and migrated["stt"].get("provider") == "mlx":
        migrated["stt"]["provider"] = "mlx-whisper"
    return migrated, data.get("fireworks_api_key")
