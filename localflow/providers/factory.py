from localflow.config import Config
from localflow.providers import registry
from localflow.providers.base import LLMProvider
from localflow.providers.llm_ollama import OllamaLLM
from localflow.providers.llm_openai_compat import OpenAICompatLLM
from localflow.secrets import get_secret
from localflow.stt import Transcriber


def build_llm(
    config: Config,
    *,
    timeout: int | None = None,
    num_ctx: int | None = None,
    temperature: float | None = None,
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
            api_key=get_secret("llm_api_key"),
            timeout=actual_timeout,
            temperature=temperature,
        )
    provider_class = registry.llm_provider_class(config.llm_provider)
    # Third-party providers must accept base_url= and model= keyword arguments.
    return provider_class(base_url=config.llm_base_url, model=config.llm_model)


def build_stt(config: Config) -> Transcriber:
    return Transcriber(
        config.model_size,
        config.language,
        backend=config.stt_backend,
        device=config.stt_device,
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
