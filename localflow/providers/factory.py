import time
from dataclasses import replace

from localflow import llm_activation, perf_ledger
from localflow.config import Config
from localflow.providers import registry
from localflow.providers.base import LLMProvider
from localflow.providers.llm_ollama import OllamaLLM
from localflow.providers.llm_openai_compat import OpenAICompatLLM
from localflow.secrets import get_secret
from localflow.stt import Transcriber


def llm_api_key(config: Config) -> str:
    """The key this config's OpenAI-compatible client may send.

    A config activated on Models & keys names an endpoint-bound snapshot
    (`llm_key_ref`) and gets that key only for that endpoint, never a
    fallback; a legacy config without one uses the generic `llm_api_key`.
    """
    ref = getattr(config, "llm_key_ref", "")
    if ref:
        return llm_activation.resolve(ref, config.llm_base_url, get_secret)
    return get_secret("llm_api_key")


def build_llm(
    config: Config,
    *,
    timeout: int | None = None,
    num_ctx: int | None = None,
    temperature: float | None = None,
    with_api_key: bool = True,
) -> LLMProvider:
    """Build the configured provider.

    Third-party providers discovered through the registry must accept
    ``base_url=`` and ``model=`` keyword arguments.
    """
    actual_timeout = timeout or config.cleanup_timeout
    actual_num_ctx = num_ctx or config.cleanup_num_ctx
    if config.llm_provider == "ollama":
        return OllamaLLM(
            config.llm_base_url,
            config.llm_model,
            timeout=actual_timeout,
            num_ctx=actual_num_ctx,
        )
    if config.llm_provider == "openai-compatible":
        return OpenAICompatLLM(
            config.llm_base_url,
            config.llm_model,
            api_key=llm_api_key(config) if with_api_key else None,
            timeout=actual_timeout,
            temperature=temperature,
        )
    provider_class = registry.llm_provider_class(config.llm_provider)
    # Third-party providers must accept base_url= and model= keyword arguments.
    return provider_class(base_url=config.llm_base_url, model=config.llm_model)


def fast_model_config(config: Config) -> Config | None:
    model = config.llm_fast_model.strip()
    if not model or model == config.llm_model:
        return None
    return replace(config, llm_model=model)


def build_fast_llm(config: Config, *, builder=build_llm) -> LLMProvider | None:
    fast = fast_model_config(config)
    return builder(fast) if fast is not None else None


def build_stt(config: Config) -> Transcriber:
    """Build and load the speech model, timing the load into the perf ledger
    (`stt_load`) unless `perf_metrics` is off."""
    started = time.monotonic()
    transcriber = None
    try:
        transcriber = Transcriber(
            config.model_size,
            config.language,
            backend=config.stt_backend,
            device=config.stt_device,
        )
        return transcriber
    finally:
        elapsed = (time.monotonic() - started) * 1000
        perf_ledger.record_for(
            config,
            "stt_load",
            status="ok" if transcriber is not None else "error",
            stt_ms=elapsed,
            total_ms=elapsed,
            stt=perf_ledger.SttLabels(
                backend=str(getattr(transcriber, "backend", config.stt_backend)),
                model=str(config.model_size),
                provider=str(config.stt_device),
            ),
        )


def build_workprompts_llm(config: Config) -> LLMProvider | None:
    key = get_secret("fireworks_api_key")
    if not key:
        return None
    return OpenAICompatLLM(
        base_url="https://api.fireworks.ai/inference/v1",
        model=config.fireworks_model,
        api_key=key,
        temperature=0.3,
    )
