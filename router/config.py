import math
import os
import re
from dotenv import load_dotenv

load_dotenv()

try:
    import h2

    HTTP2 = True
except ImportError:
    HTTP2 = False


def _load_api_keys() -> list[str]:
    keys = []
    if raw_keys := os.environ.get("GEMINI_API_KEYS"):
        keys.extend(k.strip() for k in raw_keys.split(",") if k.strip())
    numbered = []
    for var, val in os.environ.items():
        m = re.match(r"^GEMINI_API_KEY_?(\d+)$", var, re.IGNORECASE)
        if m and val.strip():
            numbered.append((int(m.group(1)), val.strip()))
    for _, val in sorted(numbered, key=lambda x: x[0]):
        keys.extend(k.strip() for k in val.split(",") if k.strip())
    if single_key := os.environ.get("GEMINI_API_KEY"):
        keys.extend(k.strip() for k in single_key.split(",") if k.strip())
    return list(dict.fromkeys(keys))


API_KEYS = _load_api_keys()
if not API_KEYS:
    raise ValueError("No keys found! Set GEMINI_API_KEYS in .env")

MODEL_CONFIGS = {
    "gemini-3.8-flash": {
        "rpm": 5,
        "rpd": 20,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-3.7-flash": {
        "rpm": 5,
        "rpd": 20,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-3.6-flash": {
        "rpm": 5,
        "rpd": 20,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-3.5-flash": {
        "rpm": 5,
        "rpd": 20,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-3-flash": {
        "rpm": 5,
        "rpd": 20,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-2.5-flash": {
        "rpm": 5,
        "rpd": 20,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-3.5-flash-lite": {
        "rpm": 15,
        "rpd": 500,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-2.5-flash-lite": {
        "rpm": 10,
        "rpd": 20,
        "context_length": 1048576,
        "max_completion_tokens": 65536,
        "capabilities": ["vision", "tools", "thinking"],
    },
    "gemini-embedding-001": {
        "rpm": 100,
        "rpd": 1000,
        "context_length": 2048,
        "max_completion_tokens": 0,
        "is_embedding": True,
        "capabilities": [],
    },
}

PROFILES = {
    "flashspam": {
        "models": [
            "gemini-3.8-flash",
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-3.5-flash",
            "gemini-2.5-flash",
            "gemini-3.5-flash-lite",
            "gemini-2.5-flash-lite",
        ],
        "overload_quarantine_percent": 50,
        "rpm_cooldown": 60.0,
    },
    "flashlitespam": {
        "models": [
            "gemini-3.5-flash-lite",
            "gemini-2.5-flash-lite",
        ],
        "overload_quarantine_percent": 50,
        "rpm_cooldown": 60.0,
    },
}

DEFAULT_PROFILE = "flashspam"

MODEL_OVERLOAD_COOLDOWN = 30.0
DEFAULT_RPM_COOLDOWN = 60.0
BLACKLIST_COOLDOWN = 86400.0 * 30


def get_profile_config(profile: str) -> dict:
    return PROFILES.get(profile, PROFILES[DEFAULT_PROFILE])


def get_profile_models(profile: str) -> list[str]:
    return get_profile_config(profile)["models"]


def _validate_profile_configs():
    if DEFAULT_PROFILE not in PROFILES:
        raise ValueError(f"Default profile not found: {DEFAULT_PROFILE}")
    for name, config in PROFILES.items():
        if not isinstance(name, str) or not name:
            raise ValueError("Profile names must be non-empty strings")
        if not isinstance(config, dict):
            raise ValueError(f"Profile {name} must be a dictionary")
        models = config.get("models")
        if not isinstance(models, list) or not models:
            raise ValueError(f"Profile {name} must have a non-empty model list")
        if any(not isinstance(model, str) for model in models):
            raise ValueError(f"Profile {name} model names must be strings")
        if len(models) != len(set(models)):
            raise ValueError(f"Profile {name} contains duplicate models")
        if any(
            model not in MODEL_CONFIGS or MODEL_CONFIGS[model].get("is_embedding")
            for model in models
        ):
            raise ValueError(f"Profile {name} contains an invalid model")
        percent = config.get("overload_quarantine_percent")
        if (
            isinstance(percent, bool)
            or not isinstance(percent, (int, float))
            or not math.isfinite(percent)
            or not 0 <= percent <= 100
        ):
            raise ValueError(
                f"Profile {name} overload_quarantine_percent must be between 0 and 100"
            )
        rpm_cooldown = config.get("rpm_cooldown")
        if (
            isinstance(rpm_cooldown, bool)
            or not isinstance(rpm_cooldown, (int, float))
            or not math.isfinite(rpm_cooldown)
            or rpm_cooldown < 0
        ):
            raise ValueError(f"Profile {name} rpm_cooldown must be non-negative")


_validate_profile_configs()

DEFAULT_MODEL_CONFIG = {
    "rpm": 5,
    "rpd": 20,
    "context_length": 1048576,
    "max_completion_tokens": 65536,
    "capabilities": ["vision", "tools", "thinking"],
}


def get_model_config(model: str) -> dict:
    clean = model.replace("models/", "")
    return MODEL_CONFIGS.get(clean, DEFAULT_MODEL_CONFIG)


FLASH_FALLBACK_CHAIN = [
    m for m, cfg in MODEL_CONFIGS.items() if not cfg.get("is_embedding")
]

DEFAULT_EMBEDDING_MODEL = "gemini-embedding-001"
ALL_DISCOVERED_MODELS = list(MODEL_CONFIGS.keys())

RPM_LIMIT = DEFAULT_MODEL_CONFIG["rpm"]
RPD_LIMIT = DEFAULT_MODEL_CONFIG["rpd"]
EMBED_RPM_LIMIT = MODEL_CONFIGS["gemini-embedding-001"]["rpm"]
EMBED_RPD_LIMIT = MODEL_CONFIGS["gemini-embedding-001"]["rpd"]

STATE_FLUSH_INTERVAL = float(os.environ.get("STATE_FLUSH_INTERVAL", "5"))

GEMINI_BASE = "https://generativelanguage.googleapis.com/v1beta"
CHAT_URL = f"{GEMINI_BASE}/openai/chat/completions"
EMBED_URL = f"{GEMINI_BASE}/models/{DEFAULT_EMBEDDING_MODEL}:embedContent"
RATE_LIMITED = {
    "error": {
        "message": "All keys/models maxed out or cooling down. Retry shortly.",
        "type": "rate_limit_error",
        "code": 429,
    }
}
TIMEOUT_ERROR = {
    "error": {
        "message": "Upstream request timed out.",
        "type": "timeout_error",
        "code": 504,
    }
}
RETRY_STATUSES = {429, 404, 401, 403} | set(range(500, 600))

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
STATE_FILE = os.environ.get(
    "ROUTER_STATE_FILE", os.path.join(BASE_DIR, ".router_state.json")
)
DASHBOARD_FILE = os.environ.get(
    "DASHBOARD_FILE", os.path.join(BASE_DIR, "dashboard.html")
)
