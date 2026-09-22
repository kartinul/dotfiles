import os
import re
import json
import time
import asyncio
import logging
from contextlib import asynccontextmanager
from datetime import datetime, timezone, timedelta
from fastapi import FastAPI, Request, HTTPException
from fastapi.responses import StreamingResponse, Response, JSONResponse, HTMLResponse
from fastapi.middleware.cors import CORSMiddleware
import httpx

from config import (
    API_KEYS,
    MODEL_CONFIGS,
    PROFILES,
    DEFAULT_PROFILE,
    MODEL_OVERLOAD_COOLDOWN,
    RPM_COOLDOWN,
    BLACKLIST_COOLDOWN,
    DEFAULT_MODEL_CONFIG,
    get_model_config,
    FLASH_FALLBACK_CHAIN,
    DEFAULT_EMBEDDING_MODEL,
    ALL_DISCOVERED_MODELS,
    RPM_LIMIT,
    RPD_LIMIT,
    EMBED_RPM_LIMIT,
    EMBED_RPD_LIMIT,
    STATE_FLUSH_INTERVAL,
    GEMINI_BASE,
    CHAT_URL,
    EMBED_URL,
    RATE_LIMITED,
    RETRY_STATUSES,
    BASE_DIR,
    STATE_FILE,
    DASHBOARD_FILE,
    HTTP2,
)

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s | %(levelname)-8s | %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
logger = logging.getLogger("GeminiRouter")
logging.getLogger("httpx").setLevel(logging.WARNING)

try:
    from zoneinfo import ZoneInfo
    PT = ZoneInfo("America/Los_Angeles")
except Exception:
    PT = timezone(timedelta(hours=-8))

def get_pacific_date_str() -> str:
    return datetime.now(PT).strftime("%Y-%m-%d")

def seconds_until_pacific_midnight() -> float:
    now = datetime.now(PT)
    midnight = datetime.combine(
        now.date() + timedelta(days=1), datetime.min.time(), tzinfo=PT
    )
    return max(60.0, midnight.timestamp() - now.timestamp())

def mask_key(k: str) -> str:
    return f"...{k[-4:]}" if len(k) >= 4 else "..."

class SSEBroadcaster:
    def __init__(self):
        self.subscribers: set[asyncio.Queue] = set()

    async def subscribe(self) -> asyncio.Queue:
        queue: asyncio.Queue = asyncio.Queue(maxsize=100)
        self.subscribers.add(queue)
        return queue

    def unsubscribe(self, queue: asyncio.Queue):
        self.subscribers.discard(queue)

    async def broadcast(self, event_name: str, data: dict):
        payload = {"event": event_name, "data": json.dumps(data)}
        for queue in list(self.subscribers):
            try:
                queue.put_nowait(payload)
            except asyncio.QueueFull:
                self.unsubscribe(queue)

broadcaster = SSEBroadcaster()

def broadcast_bg(event_name: str, data: dict):
    try:
        loop = asyncio.get_running_loop()
        if loop.is_running():
            loop.create_task(broadcaster.broadcast(event_name, data))
    except RuntimeError:
        pass

slot_lock = asyncio.Lock()
_state_dirty = False
_flush_task: asyncio.Task | None = None
_next_request_id = 0
START_TIME = time.time()
active_profile = DEFAULT_PROFILE
model_cooldowns: dict[str, float] = {}

def mark_state_dirty():
    global _state_dirty
    _state_dirty = True

def load_persistent_state() -> dict:
    fresh = {"pacific_date": get_pacific_date_str(), "active_profile": DEFAULT_PROFILE, "keys": {}, "model_cooldowns": {}}
    try:
        with open(STATE_FILE, "r", encoding="utf-8") as f:
            data = json.load(f)
        if isinstance(data, dict) and isinstance(data.get("keys"), dict) and isinstance(data.get("pacific_date"), str):
            return data
    except Exception:
        pass
    return fresh

def write_state_to_disk(data: dict):
    tmp_file = f"{STATE_FILE}.tmp.{os.getpid()}"
    with open(tmp_file, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp_file, STATE_FILE)

saved_state = load_persistent_state()
current_day = saved_state["pacific_date"]
if saved_state.get("active_profile") in PROFILES:
    active_profile = saved_state["active_profile"]

now_init = time.time()
for _m, _cd in saved_state.get("model_cooldowns", {}).items():
    if _m in ALL_DISCOVERED_MODELS and float(_cd) > now_init:
        model_cooldowns[_m] = float(_cd)

key_stats: dict[str, dict] = {}
unsupported_slots: dict[str, set[str]] = {}
key_cooldowns: dict[str, float] = {}
usage_state: dict[str, dict[str, dict]] = {}

for _key in API_KEYS:
    _sk = saved_state["keys"].get(mask_key(_key), {})
    if not isinstance(_sk, dict):
        _sk = {}
    key_stats[_key] = {
        n: int(_sk.get(n, 0))
        for n in ("total_requests", "successful_requests", "failed_requests")
    }
    unsupported_slots[_key] = {
        m for m in _sk.get("unsupported_models", []) if m in ALL_DISCOVERED_MODELS
    }
    key_cooldowns[_key] = float(_sk.get("blacklisted_until", 0.0))
    usage_state[_key] = {
        m: {
            "timestamps": [],
            "cooldown_until": float(s.get("cooldown_until", 0.0)),
            "day_count": int(s.get("day_count", 0)),
        }
        for m, s in _sk.get("slots", {}).items()
        if m in ALL_DISCOVERED_MODELS
    }

def get_slot_data(key: str, model: str) -> dict:
    return usage_state.setdefault(key, {}).setdefault(
        model, {"timestamps": [], "cooldown_until": 0.0, "day_count": 0}
    )

def get_next_request_id() -> int:
    global _next_request_id
    _next_request_id += 1
    return _next_request_id

def limits_for(model: str) -> tuple[int, int]:
    cfg = get_model_config(model)
    return cfg.get("rpm", RPM_LIMIT), cfg.get("rpd", RPD_LIMIT)

def get_slot_payload(key: str, model: str) -> dict:
    now = time.time()
    d = get_slot_data(key, model)
    rpm, rpd = limits_for(model)
    effective_cooldown = max(d["cooldown_until"], model_cooldowns.get(model, 0.0))
    return {
        "used": d["day_count"],
        "rpm": len([t for t in d["timestamps"] if now - t < 60]),
        "rpm_limit": rpm,
        "rpd_limit": rpd,
        "cooldown": max(0, round(effective_cooldown - now)),
        "cooldown_ends_at": effective_cooldown if effective_cooldown > now else 0.0,
        "server_time": now,
        "unsupported": model in unsupported_slots[key],
    }

async def broadcast_reset_and_snapshot():
    await broadcaster.broadcast("reset", {})
    await broadcaster.broadcast("snapshot", get_stats_payload())

def roll_day():
    global current_day
    today = get_pacific_date_str()
    if today != current_day:
        current_day = today
        for slots in usage_state.values():
            for d in slots.values():
                d["day_count"] = 0
        mark_state_dirty()
        try:
            loop = asyncio.get_running_loop()
            if loop.is_running():
                loop.create_task(broadcast_reset_and_snapshot())
        except RuntimeError:
            pass

roll_day()

def build_state_snapshot() -> dict:
    roll_day()
    now = time.time()
    keys = {}
    for k in API_KEYS:
        slots = {
            m: {
                "day_count": d["day_count"],
                "cooldown_until": d["cooldown_until"] if d["cooldown_until"] > now else 0.0,
            }
            for m, d in usage_state[k].items()
            if d["day_count"] or d["cooldown_until"] > now
        }
        keys[mask_key(k)] = {
            **key_stats[k],
            "unsupported_models": sorted(unsupported_slots[k]),
            "blacklisted_until": key_cooldowns[k],
            "slots": slots,
        }
    active_model_cooldowns = {
        m: float(cd) for m, cd in model_cooldowns.items() if float(cd) > now
    }
    return {
        "pacific_date": current_day,
        "active_profile": active_profile,
        "model_cooldowns": active_model_cooldowns,
        "keys": keys,
    }

def get_stats_payload() -> dict:
    roll_day()
    now = time.time()
    reset_at = now + seconds_until_pacific_midnight()
    keys = []
    display_chain = PROFILES.get(active_profile, FLASH_FALLBACK_CHAIN)
    for k in API_KEYS:
        slots = {}
        for m in display_chain:
            d = get_slot_data(k, m)
            rpm, rpd = limits_for(m)
            effective_cooldown = max(d["cooldown_until"], model_cooldowns.get(m, 0.0))
            slots[m] = {
                "used": d["day_count"],
                "rpm": len([t for t in d["timestamps"] if now - t < 60]),
                "rpm_limit": rpm,
                "rpd_limit": rpd,
                "cooldown": max(0, round(effective_cooldown - now)),
                "cooldown_ends_at": effective_cooldown if effective_cooldown > now else 0.0,
                "unsupported": m in unsupported_slots[k],
            }
        keys.append({
            "key": mask_key(k),
            "blacklisted": key_cooldowns[k] > now,
            **key_stats[k],
            "slots": slots,
        })
    return {
        "server_time": now,
        "pacific_date": current_day,
        "seconds_to_midnight_reset": round(seconds_until_pacific_midnight()),
        "reset_at": reset_at,
        "active_profile": active_profile,
        "available_profiles": list(PROFILES.keys()),
        "models": display_chain,
        "models_config": {m: get_model_config(m) for m in display_chain},
        "rpm_limit": RPM_LIMIT,
        "rpd_limit": RPD_LIMIT,
        "keys": keys,
    }

async def flush_state_loop():
    global _state_dirty
    while True:
        await asyncio.sleep(STATE_FLUSH_INTERVAL)
        if _state_dirty:
            _state_dirty = False
            try:
                await asyncio.to_thread(write_state_to_disk, build_state_snapshot())
            except Exception as e:
                _state_dirty = True
                logger.error(f"Failed to persist state: {e}")

async def periodic_reset_check_loop():
    while True:
        await asyncio.sleep(60.0)
        roll_day()

async def acquire_slot(
    chain: list[str], excluded_slots: set | None = None
) -> tuple[str | None, str | None]:
    excluded_slots = excluded_slots or set()
    async with slot_lock:
        roll_day()
        now = time.time()
        for model in chain:
            if now < model_cooldowns.get(model, 0.0):
                continue
            rpm, rpd = limits_for(model)
            for key in API_KEYS:
                if (
                    (key, model) in excluded_slots
                    or now < key_cooldowns[key]
                    or model in unsupported_slots[key]
                ):
                    continue
                data = get_slot_data(key, model)
                if now < data["cooldown_until"] or data["day_count"] >= rpd:
                    continue
                data["timestamps"] = [t for t in data["timestamps"] if now - t < 60]
                if len(data["timestamps"]) < rpm:
                    data["timestamps"].append(now)
                    key_stats[key]["total_requests"] += 1
                    return key, model
        return None, None

def record_success(key: str, model: str):
    roll_day()
    get_slot_data(key, model)["day_count"] += 1
    key_stats[key]["successful_requests"] += 1
    mark_state_dirty()
    m_key = mask_key(key)
    broadcast_bg("key_changed", {
        "key": m_key,
        "blacklisted": key_cooldowns[key] > time.time(),
        "successful_requests": key_stats[key]["successful_requests"],
        "failed_requests": key_stats[key]["failed_requests"],
    })
    broadcast_bg("slot_changed", {
        "key": m_key,
        "model": model,
        "slot": get_slot_payload(key, model),
    })

def record_failure(key: str):
    key_stats[key]["failed_requests"] += 1
    mark_state_dirty()
    m_key = mask_key(key)
    broadcast_bg("key_changed", {
        "key": m_key,
        "blacklisted": key_cooldowns[key] > time.time(),
        "successful_requests": key_stats[key]["successful_requests"],
        "failed_requests": key_stats[key]["failed_requests"],
    })

def is_retryable(status: int, text: str) -> bool:
    return status in RETRY_STATUSES or (status == 400 and "API_KEY_INVALID" in text)

def err_msg(text: str) -> str:
    try:
        j = json.loads(text)
        if isinstance(j, list) and j:
            j = j[0]
        text = j["error"]["message"]
    except Exception:
        pass
    return " ".join(str(text).split())[:200]

def apply_cooldown(
    key: str, model: str, status_code: int, error_text: str, headers: dict
):
    m_key = mask_key(key)
    delay = 0.0
    retry_after = headers.get("retry-after")
    if retry_after:
        try:
            delay = float(retry_after)
        except ValueError:
            pass
    if not delay and error_text:
        m = re.search(r'"retryDelay":\s*"(\d+(?:\.\d+)?)s"', error_text)
        if m:
            delay = float(m.group(1))

    now = time.time()
    data = get_slot_data(key, model)

    if status_code in (500, 502, 503, 504) or "overloaded" in error_text.lower():
        model_cooldown = now + max(delay, MODEL_OVERLOAD_COOLDOWN)
        model_cooldowns[model] = model_cooldown
        for k in API_KEYS:
            get_slot_data(k, model)["cooldown_until"] = max(
                get_slot_data(k, model)["cooldown_until"], model_cooldown
            )
        logger.warning(
            f"Overload [{status_code}] for {model}. Model blocked across ALL keys for {int(max(delay, MODEL_OVERLOAD_COOLDOWN))}s."
        )
        mark_state_dirty()
        for k in API_KEYS:
            broadcast_bg("slot_changed", {
                "key": mask_key(k),
                "model": model,
                "slot": get_slot_payload(k, model),
            })
    elif status_code == 429:
        _, rpd = limits_for(model)
        if "PerDay" in error_text or (
            "RESOURCE_EXHAUSTED" in error_text and "day" in error_text.lower()
        ):
            cooldown = max(delay, seconds_until_pacific_midnight())
            data["day_count"] = max(data["day_count"], rpd)
            logger.warning(f"Daily quota hit for {model} on {m_key}. Cooldown ~{int(cooldown)}s.")
        else:
            cooldown = max(delay, RPM_COOLDOWN)
            logger.warning(f"RPM limit hit for {model} on {m_key}. Cooldown {cooldown:.1f}s.")
        data["cooldown_until"] = now + cooldown
        mark_state_dirty()
        broadcast_bg("slot_changed", {
            "key": m_key,
            "model": model,
            "slot": get_slot_payload(key, model),
        })
    elif status_code == 404:
        if "no longer available" in error_text.lower() or "not found" in error_text.lower():
            logger.warning(f"Model {model} unsupported on {m_key}. Disabling for key.")
            unsupported_slots.setdefault(key, set()).add(model)
            mark_state_dirty()
            broadcast_bg("slot_changed", {
                "key": m_key,
                "model": model,
                "slot": get_slot_payload(key, model),
            })
    elif status_code in (400, 401, 403) and any(
        e in error_text
        for e in ["API_KEY_INVALID", "PERMISSION_DENIED", "CONSUMER_SUSPENDED", "UNAUTHENTICATED"]
    ):
        key_cooldowns[key] = now + BLACKLIST_COOLDOWN
        logger.critical(f"Key {m_key} returned {status_code}. Blacklisted.")
        mark_state_dirty()
        broadcast_bg("slot_changed", {
            "key": m_key,
            "model": model,
            "slot": get_slot_payload(key, model),
        })

    ks = key_stats.setdefault(key, {"total_requests": 0, "successful_requests": 0, "failed_requests": 0})
    kc = key_cooldowns.get(key, 0.0)

    broadcast_bg("key_changed", {
        "key": m_key,
        "blacklisted": kc > time.time(),
        "successful_requests": ks["successful_requests"],
        "failed_requests": ks["failed_requests"],
    })

ALLOWED_GEMINI_CHAT_KEYS = {
    "messages",
    "model",
    "frequency_penalty",
    "logit_bias",
    "logprobs",
    "top_logprobs",
    "max_tokens",
    "max_completion_tokens",
    "n",
    "presence_penalty",
    "response_format",
    "seed",
    "stop",
    "stream",
    "stream_options",
    "temperature",
    "top_p",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "user",
    "reasoning_effort",
}

VALID_REASONING_EFFORTS = {"high", "low", "medium", "minimal", "none"}

def sanitize_chat_payload(body: dict, target_model: str) -> dict:
    payload = dict(body)
    payload["model"] = target_model
    reasoning_effort = None

    raw_effort = payload.get("reasoning_effort")
    if isinstance(raw_effort, str):
        val = raw_effort.strip().lower()
        if val in ("off", "none", "0"):
            reasoning_effort = "none"
        elif val in ("max", "max_thinking"):
            reasoning_effort = "high"
        elif val in VALID_REASONING_EFFORTS:
            reasoning_effort = val

    reasoning = payload.get("reasoning")
    if isinstance(reasoning, dict):
        if reasoning.get("enabled") is False:
            reasoning_effort = "none"
        elif "effort" in reasoning:
            effort_str = str(reasoning["effort"]).strip().lower()
            if effort_str in ("off", "none", "0"):
                reasoning_effort = "none"
            elif effort_str in ("high", "max"):
                reasoning_effort = "high"
            elif effort_str in ("low", "minimal"):
                reasoning_effort = "low"
            elif effort_str == "medium":
                reasoning_effort = "medium"

    thinking = payload.get("thinking")
    if isinstance(thinking, dict):
        if thinking.get("type") == "disabled" or thinking.get("enabled") is False:
            reasoning_effort = "none"
        elif thinking.get("budget_tokens") == 0:
            reasoning_effort = "none"

    thinking_config = payload.get("thinking_config")
    if isinstance(thinking_config, dict):
        budget = thinking_config.get("budget_tokens") or thinking_config.get("thinking_budget")
        if budget == 0:
            reasoning_effort = "none"
        elif budget and budget > 4000:
            reasoning_effort = "high"
        elif budget:
            reasoning_effort = "medium"

    extra_body = payload.get("extra_body")
    if isinstance(extra_body, dict):
        if "reasoning" in extra_body and isinstance(extra_body["reasoning"], dict):
            r = extra_body["reasoning"]
            if r.get("enabled") is False or str(r.get("effort", "")).lower() in ("off", "none", "0"):
                reasoning_effort = "none"
            elif str(r.get("effort", "")).lower() in ("high", "max"):
                reasoning_effort = "high"
            elif str(r.get("effort", "")).lower() in ("low", "minimal"):
                reasoning_effort = "low"
            elif str(r.get("effort", "")).lower() == "medium":
                reasoning_effort = "medium"
        if "google" in extra_body and isinstance(extra_body["google"], dict):
            gtc = extra_body["google"].get("thinking_config", {})
            if isinstance(gtc, dict):
                budget = gtc.get("budget_tokens") or gtc.get("thinking_budget")
                if budget == 0:
                    reasoning_effort = "none"
                elif budget and budget > 4000:
                    reasoning_effort = "high"
                elif budget:
                    reasoning_effort = "medium"

    if reasoning_effort is not None:
        payload["reasoning_effort"] = reasoning_effort

    if "messages" in payload and isinstance(payload["messages"], list):
        sanitized_messages = []
        for msg in payload["messages"]:
            if isinstance(msg, dict):
                m = dict(msg)
                if m.get("role") == "developer":
                    m["role"] = "system"
                if m.get("content") is None and not m.get("tool_calls"):
                    m["content"] = ""
                if m.get("role") == "assistant" and m.get("tool_calls"):
                    patched_tool_calls = []
                    for tc in m["tool_calls"]:
                        if not isinstance(tc, dict):
                            patched_tool_calls.append(tc)
                            continue
                        tc = dict(tc)
                        has_sig = (
                            tc.get("thought_signature")
                            or (isinstance(tc.get("extra_content"), dict)
                                and isinstance(tc["extra_content"].get("google"), dict)
                                and tc["extra_content"]["google"].get("thought_signature"))
                        )
                        if not has_sig:
                            tc["thought_signature"] = "skip_thought_signature_validator"
                            tc["extra_content"] = {
                                "google": {"thought_signature": "skip_thought_signature_validator"}
                            }
                        patched_tool_calls.append(tc)
                    m["tool_calls"] = patched_tool_calls
                sanitized_messages.append(m)
            else:
                sanitized_messages.append(msg)
        payload["messages"] = sanitized_messages

    if "n" in payload and payload["n"] is not None and payload["n"] != 1:
        payload["n"] = 1

    return {k: v for k, v in payload.items() if k in ALLOWED_GEMINI_CHAT_KEYS}

@asynccontextmanager
async def lifespan(app: FastAPI):
    app.state.client = httpx.AsyncClient(
        http2=HTTP2,
        limits=httpx.Limits(max_connections=120, max_keepalive_connections=30, keepalive_expiry=60.0),
        timeout=httpx.Timeout(connect=10.0, read=180.0, write=20.0, pool=10.0),
    )
    flush_task = asyncio.create_task(flush_state_loop())
    reset_check_task = asyncio.create_task(periodic_reset_check_loop())
    logger.info(f"Router up. {len(API_KEYS)} keys, profile={active_profile}, http2={HTTP2}.")
    yield
    flush_task.cancel()
    reset_check_task.cancel()
    try:
        await asyncio.gather(flush_task, reset_check_task, return_exceptions=True)
    except asyncio.CancelledError:
        pass
    try:
        write_state_to_disk(build_state_snapshot())
    except Exception as e:
        logger.error(f"Final state flush failed: {e}")
    await app.state.client.aclose()

app = FastAPI(title="Gemini Router", lifespan=lifespan)
app.add_middleware(
    CORSMiddleware,
    allow_origins=["*"],
    allow_credentials=False,
    allow_methods=["*"],
    allow_headers=["*"],
)

@app.exception_handler(Exception)
async def global_exception_handler(request: Request, exc: Exception):
    logger.error(f"CRITICAL ERROR on {request.method} {request.url.path}: {exc}", exc_info=True)
    return JSONResponse(
        status_code=500,
        content={"error": {"message": f"Internal router error: {exc}", "type": "internal_error", "code": 500}},
    )

@app.get("/health")
async def health_check():
    return {
        "status": "healthy",
        "keys_loaded": len(API_KEYS),
        "active_profile": active_profile,
        "uptime_seconds": round(time.time() - START_TIME, 1),
        "pacific_date": get_pacific_date_str(),
        "seconds_to_midnight_reset": round(seconds_until_pacific_midnight()),
    }

@app.get("/stats")
async def get_stats():
    return get_stats_payload()

@app.get("/events")
async def events_endpoint(request: Request):
    queue = await broadcaster.subscribe()
    async def event_generator():
        snapshot_data = get_stats_payload()
        yield f"event: snapshot\ndata: {json.dumps(snapshot_data)}\n\n"
        try:
            while True:
                try:
                    payload = await asyncio.wait_for(queue.get(), timeout=15.0)
                    yield f"event: {payload['event']}\ndata: {payload['data']}\n\n"
                except asyncio.TimeoutError:
                    yield ": heartbeat\n\n"
        except asyncio.CancelledError:
            pass
        finally:
            broadcaster.unsubscribe(queue)

    return StreamingResponse(
        event_generator(),
        media_type="text/event-stream",
        headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
    )

@app.get("/", response_class=HTMLResponse)
async def dashboard():
    try:
        with open(DASHBOARD_FILE, "r", encoding="utf-8") as f:
            return HTMLResponse(content=f.read())
    except FileNotFoundError:
        return HTMLResponse(content=f"<p>dashboard.html not found at {DASHBOARD_FILE}.</p>", status_code=404)

@app.get("/profile")
async def get_active_profile():
    return {"active_profile": active_profile, "available_profiles": list(PROFILES.keys())}

@app.post("/profile")
async def set_active_profile(request: Request):
    global active_profile
    try:
        data = await request.json()
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid JSON")
    profile = data.get("profile")
    if profile not in PROFILES:
        raise HTTPException(status_code=400, detail=f"Unknown profile: {profile}. Available: {list(PROFILES.keys())}")
    active_profile = profile
    mark_state_dirty()
    await broadcast_reset_and_snapshot()
    logger.info(f"Active profile switched to: {active_profile}")
    return {"status": "ok", "active_profile": active_profile}

@app.get("/v1/models")
@app.get("/models")
@app.get("/api/v1/models")
@app.get("/api/models")
async def list_models():
    models = list(ALL_DISCOVERED_MODELS) + ["auto-router", "auto", "default", "flashspam", "flashlitespam"]
    data = []
    for m in models:
        cfg = get_model_config(m)
        data.append({
            "id": m,
            "object": "model",
            "created": 1700000000,
            "owned_by": "google",
            "context_length": cfg.get("context_length", 1048576),
            "max_completion_tokens": cfg.get("max_completion_tokens", 65536),
        })
    return {"object": "list", "data": data}

@app.get("/v1/models/{model_id:path}")
@app.get("/models/{model_id:path}")
@app.get("/api/v1/models/{model_id:path}")
@app.get("/api/models/{model_id:path}")
async def get_model(model_id: str):
    clean_id = model_id.replace("models/", "")
    cfg = get_model_config(clean_id)
    return {
        "id": clean_id,
        "object": "model",
        "created": 1700000000,
        "owned_by": "google",
        "context_length": cfg.get("context_length", 1048576),
        "max_completion_tokens": cfg.get("max_completion_tokens", 65536),
    }

@app.post("/api/show")
async def ollama_show(request: Request):
    try:
        body = await request.json()
    except Exception:
        body = {}
    model_name = str(body.get("name") or body.get("model") or "gemini-flash").replace("models/", "")
    cfg = get_model_config(model_name)
    ctx = cfg.get("context_length", 1048576)
    caps = cfg.get("capabilities", ["vision", "tools", "thinking"])
    return {
        "license": "Google Gemini Terms of Service",
        "modelfile": f"FROM {model_name}",
        "parameters": f"num_ctx {ctx}",
        "template": "{{ .Prompt }}",
        "details": {
            "parent_model": "",
            "format": "gguf",
            "family": "gemini",
            "families": ["gemini"],
            "parameter_size": "flash",
            "quantization_level": "none",
        },
        "model_info": {
            "general.architecture": "gemini",
            "gemini.context_length": ctx,
            "context_length": ctx,
        },
        "capabilities": caps,
    }

@app.get("/api/tags")
async def ollama_tags():
    models = list(ALL_DISCOVERED_MODELS) + ["auto-router", "auto", "default", "flashspam", "flashlitespam"]
    return {
        "models": [
            {
                "name": m,
                "model": m,
                "modified_at": "2026-09-23T00:00:00Z",
                "size": 0,
                "digest": "gemini-flash",
                "details": {
                    "parent_model": "",
                    "format": "gguf",
                    "family": "gemini",
                    "families": ["gemini"],
                    "parameter_size": "flash",
                    "quantization_level": "none",
                },
            }
            for m in models
        ]
    }

@app.get("/api/version")
@app.get("/version")
async def server_version():
    return {"version": "0.6.0"}

@app.get("/v1/props")
@app.get("/props")
async def server_props():
    return {
        "default_generation_settings": {
            "n_ctx": 1048576,
            "params": {},
        },
        "total_slots": len(API_KEYS),
    }

def resolve_fallback_chain(requested: str) -> list[str]:
    active_chain = PROFILES.get(active_profile, FLASH_FALLBACK_CHAIN)
    if requested in PROFILES:
        return PROFILES[requested]
    if requested in ("auto-router", "auto", "default", ""):
        return active_chain
    if requested in MODEL_CONFIGS and not MODEL_CONFIGS[requested].get("is_embedding"):
        return [requested] + [m for m in active_chain if m != requested]
    return active_chain

@app.post("/v1/chat/completions")
@app.post("/chat/completions")
@app.post("/api/v1/chat/completions")
@app.post("/api/chat/completions")
async def chat_completions(request: Request):
    try:
        body_json = await request.json()
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid JSON body")
    if not isinstance(body_json, dict):
        raise HTTPException(status_code=400, detail="Body must be a JSON object")

    requested = str(body_json.get("model", "")).replace("models/", "")
    chain = resolve_fallback_chain(requested)
    is_stream = bool(body_json.get("stream", False))
    max_retries = min(len(API_KEYS) * len(chain), 30)
    excluded_slots: set = set()
    last_status, last_content = 429, json.dumps(RATE_LIMITED).encode()
    client: httpx.AsyncClient = request.app.state.client

    for attempt in range(max_retries):
        key, model = await acquire_slot(chain=chain, excluded_slots=excluded_slots)
        if not key:
            return JSONResponse(status_code=429, content=RATE_LIMITED)

        m_key = mask_key(key)
        slot = get_slot_data(key, model)
        rpm, rpd = limits_for(model)
        logger.info(
            f"Attempt {attempt + 1}/{max_retries} -> {model} | {m_key} | RPM {len(slot['timestamps'])}/{rpm} | RPD {slot['day_count']}/{rpd}"
        )

        req_id = get_next_request_id()
        await broadcaster.broadcast(
            "request_started", {"id": req_id, "key": m_key, "model": model}
        )
        request_start_time = time.time()

        payload = sanitize_chat_payload(body_json, target_model=model)
        headers = {"authorization": f"Bearer {key}", "content-type": "application/json"}

        try:
            req = client.build_request("POST", CHAT_URL, content=json.dumps(payload).encode("utf-8"), headers=headers)
            resp = await client.send(req, stream=True)

            if resp.status_code >= 400 or not is_stream:
                try:
                    data = await resp.aread()
                finally:
                    await resp.aclose()
                latency_ms = round((time.time() - request_start_time) * 1000)
                if resp.status_code < 400:
                    record_success(key, model)
                    await broadcaster.broadcast(
                        "request_finished",
                        {
                            "id": req_id,
                            "key": m_key,
                            "model": model,
                            "status": resp.status_code,
                            "latency_ms": latency_ms,
                            "slot": get_slot_payload(key, model),
                        },
                    )
                    return Response(
                        content=data,
                        status_code=resp.status_code,
                        media_type=resp.headers.get("content-type", "application/json"),
                    )

                err_text = data.decode("utf-8", errors="replace")
                record_failure(key)
                logger.warning(f"Upstream [{resp.status_code}] {m_key}/{model}: {err_msg(err_text)}")
                apply_cooldown(key, model, resp.status_code, err_text, dict(resp.headers))

                await broadcaster.broadcast(
                    "request_finished",
                    {
                        "id": req_id,
                        "key": m_key,
                        "model": model,
                        "status": resp.status_code,
                        "latency_ms": latency_ms,
                        "slot": get_slot_payload(key, model),
                    },
                )
                if not is_retryable(resp.status_code, err_text):
                    return Response(content=data, status_code=resp.status_code, media_type="application/json")

                excluded_slots.add((key, model))
                last_status, last_content = resp.status_code, data
                continue

            record_success(key, model)

            async def stream_forward(r=resp, r_id=req_id, r_start=request_start_time):
                try:
                    async for chunk in r.aiter_bytes():
                        yield chunk
                finally:
                    await r.aclose()
                    latency_ms = round((time.time() - r_start) * 1000)
                    await broadcaster.broadcast(
                        "request_finished",
                        {
                            "id": r_id,
                            "key": m_key,
                            "model": model,
                            "status": 200,
                            "latency_ms": latency_ms,
                            "slot": get_slot_payload(key, model),
                        },
                    )

            return StreamingResponse(
                stream_forward(),
                status_code=resp.status_code,
                media_type="text/event-stream",
                headers={"Cache-Control": "no-cache", "Connection": "keep-alive", "X-Accel-Buffering": "no"},
            )
        except Exception as e:
            logger.error(f"Request error {m_key}/{model}: {e or type(e).__name__}")
            record_failure(key)
            apply_cooldown(key, model, 500, str(e), {})
            latency_ms = round((time.time() - request_start_time) * 1000)
            await broadcaster.broadcast(
                "request_finished",
                {
                    "id": req_id,
                    "key": m_key,
                    "model": model,
                    "status": 500,
                    "latency_ms": latency_ms,
                    "slot": get_slot_payload(key, model),
                },
            )
            excluded_slots.add((key, model))

    return Response(content=last_content, status_code=last_status, media_type="application/json")

@app.post("/v1/embeddings")
@app.post("/embeddings")
@app.post("/api/v1/embeddings")
@app.post("/api/embeddings")
async def create_embeddings(request: Request):
    try:
        body = await request.json()
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid JSON body")
    raw_input = body.get("input", "") if isinstance(body, dict) else ""
    if isinstance(raw_input, str):
        inputs = [raw_input]
    elif isinstance(raw_input, list):
        inputs = raw_input
    else:
        inputs = []
    if not inputs or not raw_input:
        raise HTTPException(status_code=400, detail="Missing or empty 'input' field")

    client: httpx.AsyncClient = request.app.state.client
    data_items, total_tokens = [], 0

    for idx, text in enumerate(inputs):
        text = text if isinstance(text, str) else str(text)
        excluded_slots, success, last_error = set(), False, "All keys exhausted or cooling down"
        for _ in range(min(len(API_KEYS), 5)):
            key, model = await acquire_slot(chain=[DEFAULT_EMBEDDING_MODEL], excluded_slots=excluded_slots)
            if not key:
                break

            req_id = get_next_request_id()
            m_key = mask_key(key)
            await broadcaster.broadcast("request_started", {"id": req_id, "key": m_key, "model": model})
            request_start_time = time.time()

            try:
                resp = await client.post(
                    EMBED_URL,
                    json={"model": f"models/{DEFAULT_EMBEDDING_MODEL}", "content": {"parts": [{"text": text}]}},
                    headers={"x-goog-api-key": key},
                )
                latency_ms = round((time.time() - request_start_time) * 1000)
                if resp.status_code == 200:
                    values = resp.json().get("embedding", {}).get("values", [])
                    data_items.append({"object": "embedding", "index": idx, "embedding": values})
                    total_tokens += max(1, len(text) // 4)
                    record_success(key, model)
                    await broadcaster.broadcast(
                        "request_finished",
                        {
                            "id": req_id,
                            "key": m_key,
                            "model": model,
                            "status": 200,
                            "latency_ms": latency_ms,
                            "slot": get_slot_payload(key, model),
                        },
                    )
                    success = True
                    break

                record_failure(key)
                apply_cooldown(key, model, resp.status_code, resp.text, dict(resp.headers))
                await broadcaster.broadcast(
                    "request_finished",
                    {
                        "id": req_id,
                        "key": m_key,
                        "model": model,
                        "status": resp.status_code,
                        "latency_ms": latency_ms,
                        "slot": get_slot_payload(key, model),
                    },
                )
                excluded_slots.add((key, model))
                last_error = err_msg(resp.text)
                if not is_retryable(resp.status_code, resp.text):
                    break
            except Exception as e:
                record_failure(key)
                excluded_slots.add((key, model))
                last_error = str(e) or type(e).__name__
                latency_ms = round((time.time() - request_start_time) * 1000)
                await broadcaster.broadcast(
                    "request_finished",
                    {
                        "id": req_id,
                        "key": m_key,
                        "model": model,
                        "status": 500,
                        "latency_ms": latency_ms,
                        "slot": get_slot_payload(key, model),
                    },
                )
        if not success:
            raise HTTPException(status_code=502, detail=f"Embedding generation failed: {last_error}")

    return {
        "object": "list",
        "data": data_items,
        "model": DEFAULT_EMBEDDING_MODEL,
        "usage": {"prompt_tokens": total_tokens, "total_tokens": total_tokens},
    }

@app.post("/test-all")
async def test_all(request: Request):
    client: httpx.AsyncClient = request.app.state.client
    payload = lambda model: json.dumps(
        {"model": model, "messages": [{"role": "user", "content": "hi"}], "max_tokens": 1}
    ).encode("utf-8")

    async def test_combo(key: str, model: str):
        req_id = get_next_request_id()
        m_key = mask_key(key)
        start = time.time()
        await broadcaster.broadcast("request_started", {"id": req_id, "key": m_key, "model": model})

        slot = get_slot_data(key, model)
        slot["timestamps"].append(start)
        try:
            resp = await client.post(
                CHAT_URL,
                content=payload(model),
                headers={"authorization": f"Bearer {key}", "content-type": "application/json"},
                timeout=20.0,
            )
            latency_ms = round((time.time() - start) * 1000)
            if resp.status_code == 200:
                record_success(key, model)
                slot["cooldown_until"] = 0.0
                model_cooldowns.pop(model, None)
                unsupported_slots[key].discard(model)
                key_cooldowns[key] = 0.0
                mark_state_dirty()
                await broadcaster.broadcast(
                    "request_finished",
                    {
                        "id": req_id,
                        "key": m_key,
                        "model": model,
                        "status": 200,
                        "latency_ms": latency_ms,
                        "slot": get_slot_payload(key, model),
                    },
                )
                return {"key": m_key, "model": model, "status": "ok", "latency_ms": latency_ms}

            record_failure(key)
            apply_cooldown(key, model, resp.status_code, resp.text, dict(resp.headers))
            await broadcaster.broadcast(
                "request_finished",
                {
                    "id": req_id,
                    "key": m_key,
                    "model": model,
                    "status": resp.status_code,
                    "latency_ms": latency_ms,
                    "slot": get_slot_payload(key, model),
                },
            )
            return {
                "key": m_key,
                "model": model,
                "status": "error",
                "code": resp.status_code,
                "detail": err_msg(resp.text),
                "latency_ms": latency_ms,
            }
        except Exception as e:
            record_failure(key)
            latency_ms = round((time.time() - start) * 1000)
            await broadcaster.broadcast(
                "request_finished",
                {
                    "id": req_id,
                    "key": m_key,
                    "model": model,
                    "status": 500,
                    "latency_ms": latency_ms,
                    "slot": get_slot_payload(key, model),
                },
            )
            return {
                "key": m_key,
                "model": model,
                "status": "error",
                "detail": str(e) or type(e).__name__,
                "latency_ms": latency_ms,
            }

    chain = PROFILES.get(active_profile, FLASH_FALLBACK_CHAIN)
    results = await asyncio.gather(*(test_combo(k, m) for k in API_KEYS for m in chain))
    ok_count = sum(r["status"] == "ok" for r in results)
    return {"ok": True, "tested": len(results), "passed": ok_count}

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(
        app,
        host=os.environ.get("HOST", "127.0.0.1"),
        port=int(os.environ.get("PORT", "9999")),
    )
