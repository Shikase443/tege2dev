import copy
import json
import math
import os
import sys
import tempfile
from pathlib import Path
from urllib.parse import urlsplit


SETTINGS_PATH = Path(os.environ.get("CODEX_HOME", "/data/codex")) / "llm-settings.json"
ENV_KEYS = {
    "main": {
        "base_url": "CODEX_OSS_BASE_URL",
        "model": "CODEX_AGENT_MODEL",
        "context_window": "CODEX_AGENT_CONTEXT_WINDOW",
        "max_output_tokens": "CODEX_AGENT_MAX_OUTPUT_TOKENS",
        "temperature": "CODEX_AGENT_TEMPERATURE",
        "stream_idle_timeout_ms": "CODEX_AGENT_STREAM_IDLE_TIMEOUT_MS",
    },
}


def env_number(name, default, converter=int):
    value = os.environ.get(name)
    return default if value is None else converter(value)


def defaults():
    base_url = os.environ.get("CODEX_OSS_BASE_URL", "http://127.0.0.1:1234/v1")
    model = os.environ.get("CODEX_AGENT_MODEL", "mymodel")
    return {
        "main": {
            "base_url": base_url,
            "model": model,
            "context_window": env_number("CODEX_AGENT_CONTEXT_WINDOW", 32768),
            "max_output_tokens": env_number("CODEX_AGENT_MAX_OUTPUT_TOKENS", 8192),
            "temperature": env_number("CODEX_AGENT_TEMPERATURE", None, float),
            "stream_idle_timeout_ms": env_number("CODEX_AGENT_STREAM_IDLE_TIMEOUT_MS", None),
        },
    }


def validate(settings):
    if not isinstance(settings, dict) or set(settings) != set(ENV_KEYS):
        raise ValueError("main を指定してください。")
    result = copy.deepcopy(settings)
    for group, fields in ENV_KEYS.items():
        section = result[group]
        if not isinstance(section, dict) or set(section) != set(fields):
            raise ValueError(f"{group} の設定項目が一致しません。")
        for key, value in section.items():
            label = f"{group}.{key}"
            if key == "base_url":
                if not isinstance(value, str):
                    raise ValueError(f"{label}: URLを指定してください。")
                parsed = urlsplit(value)
                try:
                    parsed.port
                except ValueError as error:
                    raise ValueError(f"{label}: ポート番号が不正です。") from error
                if parsed.scheme not in {"http", "https"} or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
                    raise ValueError(f"{label}: HTTP/HTTPSのベースURLを指定してください。")
                if any(char.isspace() for char in value):
                    raise ValueError(f"{label}: URLに空白は使用できません。")
                section[key] = value.rstrip("/")
            elif key == "model":
                if not isinstance(value, str) or not value.strip() or any(ord(char) < 32 for char in value):
                    raise ValueError(f"{label}: モデル名を指定してください。")
                section[key] = value.strip()
            elif key == "temperature":
                if value is None and group == "main":
                    continue
                if type(value) not in {int, float} or not math.isfinite(value) or not 0 <= value <= 2:
                    raise ValueError(f"{label}: 0〜2を指定してください。")
            else:
                if value is None and key == "stream_idle_timeout_ms":
                    continue
                minimum = 0 if key == "stream_idle_timeout_ms" else 1
                if type(value) is not int or not minimum <= value <= 2**63 - 1:
                    raise ValueError(f"{label}: {minimum}以上の整数を指定してください。")
    return result


def save(settings):
    settings = validate(settings)
    SETTINGS_PATH.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary = tempfile.mkstemp(prefix=".llm-settings-", dir=SETTINGS_PATH.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="\n") as output:
            json.dump(settings, output, ensure_ascii=False, indent=2, allow_nan=False)
            output.write("\n")
            output.flush()
            os.fsync(output.fileno())
        os.replace(temporary, SETTINGS_PATH)
    finally:
        Path(temporary).unlink(missing_ok=True)
    return settings


def load():
    if not SETTINGS_PATH.exists():
        return save(defaults())
    with SETTINGS_PATH.open(encoding="utf-8") as source:
        return validate(json.load(source))


def environment(settings, base=None):
    result = dict(os.environ if base is None else base)
    for group, fields in ENV_KEYS.items():
        for key, env_key in fields.items():
            value = settings[group][key]
            if value is None:
                result.pop(env_key, None)
            else:
                result[env_key] = str(value).lower() if type(value) is bool else str(value)
    return result


async def test_connection(request):
    import asyncio
    import time
    from aiohttp import ClientError, ClientTimeout, web

    try:
        body = await request.json()
        group = body.get("group")
        if group not in ENV_KEYS:
            raise ValueError("接続テストの対象が不正です。")
        settings = defaults()
        settings[group]["base_url"] = body.get("base_url")
        settings[group]["model"] = body.get("model")
        section = validate(settings)[group]
    except (ValueError, TypeError, AttributeError) as error:
        return web.json_response({"error": str(error)}, status=400)

    started = time.monotonic()
    payload = {
        "model": section["model"],
        "messages": [{"role": "user", "content": "Reply with OK only."}],
        "max_tokens": 64,
        "stream": False,
        "temperature": 0,
        "chat_template_kwargs": {"enable_thinking": False},
    }
    try:
        async with request.app["client"].post(
            section["base_url"] + "/chat/completions",
            json=payload, timeout=ClientTimeout(total=60, connect=10),
        ) as response:
            if response.status != 200:
                detail = (await response.content.read(2048)).decode("utf-8", errors="replace")
                return web.json_response({"error": f"HTTP {response.status}: {detail}"}, status=502)
            result = await response.json()
            choices = result.get("choices") if isinstance(result, dict) else None
            if not isinstance(choices, list) or not choices or not isinstance(choices[0], dict):
                raise ValueError("LLMの応答形式が不正です。")
            message = choices[0].get("message")
            if not isinstance(message, dict) or not isinstance(message.get("content"), str) or not message["content"].strip():
                raise ValueError("LLMから回答本文が返りませんでした。")
    except asyncio.TimeoutError:
        return web.json_response({"error": "接続テストがタイムアウトしました（60秒）。"}, status=504)
    except (ClientError, ValueError, TypeError) as error:
        return web.json_response({"error": f"接続テスト失敗: {error}"}, status=502)
    return web.json_response({"ok": True, "elapsed_seconds": round(time.monotonic() - started, 2)})


if __name__ == "__main__":
    if len(sys.argv) < 3 or sys.argv[1] != "--exec":
        raise SystemExit("usage: llm_settings.py --exec COMMAND [ARG ...]")
    try:
        settings = load()
    except (OSError, ValueError) as error:
        raise SystemExit(f"LLM設定の読み込みに失敗しました: {error}") from error
    os.execvpe(sys.argv[2], sys.argv[2:], environment(settings))
