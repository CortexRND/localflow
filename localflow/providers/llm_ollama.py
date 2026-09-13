import requests

from localflow.providers.base import Health, ModelInfo, ProviderError


class OllamaLLM:
    id = "ollama"
    offline = True

    def __init__(
        self,
        base_url: str,
        model: str,
        timeout: int = 120,
        num_ctx: int = 8192,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.timeout = timeout
        self.num_ctx = num_ctx

    def complete(
        self, system: str, user: str, *, max_tokens: int | None = None
    ) -> str:
        try:
            prompt = f"{system}\n\n{user}" if system else user
            response = requests.post(
                f"{self.base_url}/api/generate",
                json={
                    "model": self.model,
                    "prompt": prompt,
                    "options": {
                        "num_ctx": self.num_ctx,
                        "num_predict": max_tokens if max_tokens is not None else -1,
                    },
                    "stream": False,
                },
                timeout=self.timeout,
            )
            response.raise_for_status()
            text = response.json().get("response", "")
            text = text.strip().strip('"').strip("'").strip()
            if not text:
                raise ProviderError("Ollama returned an empty response")
            return text
        except ProviderError:
            raise
        except Exception as exc:
            raise ProviderError(f"Ollama completion failed: {exc}") from exc

    def list_models(self) -> list[ModelInfo]:
        try:
            response = requests.get(
                f"{self.base_url}/api/tags",
                timeout=self.timeout,
            )
            response.raise_for_status()
            models = response.json().get("models", [])
            return [
                ModelInfo(id=model["name"], label=model["name"], installed=True)
                for model in models
            ]
        except Exception as exc:
            raise ProviderError(f"Ollama model listing failed: {exc}") from exc

    def healthcheck(self) -> Health:
        try:
            response = requests.get(f"{self.base_url}/api/tags", timeout=3)
            response.raise_for_status()
            return Health(ok=True)
        except Exception as exc:
            return Health(ok=False, detail=str(exc))
