import requests

from localflow.providers.base import Health, ModelInfo, ProviderError


class OpenAICompatLLM:
    id = "openai-compatible"
    offline = False

    def __init__(
        self,
        base_url: str,
        model: str,
        api_key: str = "",
        timeout: int = 120,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.api_key = api_key
        self.timeout = timeout

    def complete(
        self, system: str, user: str, *, max_tokens: int | None = None
    ) -> str:
        headers = {"Content-Type": "application/json"}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        body = {
            "model": self.model,
            "messages": [
                {"role": "system", "content": system},
                {"role": "user", "content": user},
            ],
        }
        if max_tokens is not None:
            body["max_tokens"] = max_tokens
        try:
            response = requests.post(
                f"{self.base_url}/chat/completions",
                headers=headers,
                json=body,
                timeout=self.timeout,
            )
            response.raise_for_status()
            text = response.json()["choices"][0]["message"]["content"].strip()
            if not text:
                raise ProviderError("OpenAI-compatible provider returned an empty response")
            return text
        except ProviderError:
            raise
        except Exception as exc:
            raise ProviderError(f"OpenAI-compatible completion failed: {exc}") from exc

    def list_models(self) -> list[ModelInfo]:
        headers = {}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        try:
            response = requests.get(
                f"{self.base_url}/models",
                headers=headers,
                timeout=self.timeout,
            )
            response.raise_for_status()
            return [
                ModelInfo(id=model["id"], label=model.get("id", ""))
                for model in response.json().get("data", [])
            ]
        except Exception as exc:
            raise ProviderError(f"OpenAI-compatible model listing failed: {exc}") from exc

    def healthcheck(self) -> Health:
        try:
            self.list_models()
            return Health(ok=True)
        except Exception as exc:  # noqa: BLE001
            return Health(ok=False, detail=str(exc))
