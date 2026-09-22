"""
Local OpenAI-compatible router for free-tier Gemini flash keys.
Rotates across GEMINI_API_KEYS + a flash-model fallback chain to maximize
throughput within Google's free limits. RPM and RPD are tracked per key AND per
model (that's how Google counts them). OpenAI-shaped endpoints only.
Needs Python 3.10+.  pip install fastapi uvicorn "httpx[http2]" python-dotenv tzdata
"""

import os
import re
import json
import time
import asyncio
import logging
from contextlib import asynccontextmanager
from datetime import datetime, timezone, timedelta
from dotenv import load_dotenv
from fastapi import FastAPI, Request, HTTPException
from fastapi.responses import StreamingResponse, Response, JSONResponse, HTMLResponse
from fastapi.middleware.cors import CORSMiddleware
import httpx

logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s | %(levelname)-8s | %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S",
)
logger = logging.getLogger("GeminiRouter")
logging.getLogger("httpx").setLevel(
    logging.WARNING
)  # httpx logs full request URLs at INFO

load_dotenv()

try:
    import h2  # noqa: F401

    HTTP2 = True
except ImportError:
    HTTP2 = False

# --- CONFIG ---
API_KEYS = list(
    dict.fromkeys(
        k.strip() for k in os.environ.get("GEMINI_API_KEYS", "").split(",") if k.strip()
    )
)
if not API_KEYS:
    raise ValueError('No keys found! Set GEMINI_API_KEYS="key1,key2,key3" in .env')

RPM_LIMIT = int(os.environ.get("RPM_LIMIT", "5"))  # per key, per chat model
RPD_LIMIT = int(os.environ.get("RPD_LIMIT", "20"))  # per key, per chat model
EMBED_RPM_LIMIT = int(
    os.environ.get("EMBED_RPM_LIMIT", "100")
)  # embeddings have their own, much bigger quota
EMBED_RPD_LIMIT = int(os.environ.get("EMBED_RPD_LIMIT", "1000"))
STATE_FLUSH_INTERVAL = float(os.environ.get("STATE_FLUSH_INTERVAL", "5"))

# Free flash-tier only -- no pro models.
FLASH_FALLBACK_CHAIN = [
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3-flash",
    "gemini-2.5-flash",
    "gemini-3.5-flash-lite",
]


DEFAULT_EMBEDDING_MODEL = "gemini-embedding-001"
ALL_DISCOVERED_MODELS = FLASH_FALLBACK_CHAIN + [DEFAULT_EMBEDDING_MODEL]

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
RETRY_STATUSES = {429, 500, 502, 503, 404, 401, 403}

BASE_DIR = os.path.dirname(os.path.abspath(__file__))
STATE_FILE = os.environ.get(
    "ROUTER_STATE_FILE", os.path.join(BASE_DIR, ".router_state.json")
)
DASHBOARD_FILE = os.environ.get(
    "DASHBOARD_FILE", os.path.join(BASE_DIR, "dashboard.html")
)
START_TIME = time.time()

# --- PACIFIC TIME (Google's quota reset clock) ---
try:
    from zoneinfo import ZoneInfo

    PT = ZoneInfo("America/Los_Angeles")
except Exception:
    PT = timezone(timedelta(hours=-8))
    logger.warning(
        "tzdata not installed: using fixed UTC-8 for the quota reset (1h off during DST). Fix: pip install tzdata"
    )


def get_pacific_date_str() -> str:
    return datetime.now(PT).strftime("%Y-%m-%d")


def seconds_until_pacific_midnight() -> float:
    now = datetime.now(PT)
    midnight = datetime.combine(
        now.date() + timedelta(days=1), datetime.min.time(), tzinfo=PT
    )
    return max(60.0, midnight.timestamp() - now.timestamp())


# --- PERSISTENT STATE ---
# The process can be killed/restarted at any time. Persisted per key: request
# stats, blacklist expiry, unsupported models, and per-model daily count +
# cooldown expiry. Writes never block the event loop: mutations flip a dirty
# flag and a background task flushes to disk (in a thread) on an interval.
slot_lock = asyncio.Lock()
_state_dirty = False
_flush_task: asyncio.Task | None = None


def mask_key(k: str) -> str:
    return f"...{k[-4:]}" if len(k) >= 4 else "..."


def mark_state_dirty():
    global _state_dirty
    _state_dirty = True


def load_persistent_state() -> dict:
    fresh = {"pacific_date": get_pacific_date_str(), "keys": {}}
    try:
        with open(STATE_FILE, "r", encoding="utf-8") as f:
            data = json.load(f)
        if (
            isinstance(data, dict)
            and isinstance(data.get("keys"), dict)
            and isinstance(data.get("pacific_date"), str)
        ):
            return data
        logger.warning(
            f"State file {STATE_FILE} has an unexpected shape. Starting fresh."
        )
    except FileNotFoundError:
        pass
    except Exception as e:
        logger.warning(f"Could not load state from {STATE_FILE}: {e}. Starting fresh.")
    return fresh


def write_state_to_disk(data: dict):
    tmp_file = f"{STATE_FILE}.tmp.{os.getpid()}"
    with open(tmp_file, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp_file, STATE_FILE)


# --- RUNTIME STATE INIT (restored from disk) ---
saved_state = load_persistent_state()
current_day = saved_state["pacific_date"]

key_stats: dict[str, dict] = {}
unsupported_slots: dict[str, set[str]] = {}
key_cooldowns: dict[str, float] = {}
usage_state: dict[str, dict[str, dict]] = (
    {}
)  # key -> model -> {timestamps, cooldown_until, day_count}

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
    return usage_state[key].setdefault(
        model, {"timestamps": [], "cooldown_until": 0.0, "day_count": 0}
    )


# --- SSE BROADCASTER ---
class SSEBroadcaster:
    def __init__(self):
        self.subscribers = set()

    async def subscribe(self) -> asyncio.Queue:
        queue = asyncio.Queue(maxsize=100)
        self.subscribers.add(queue)
        return queue

    def unsubscribe(self, queue: asyncio.Queue):
        self.subscribers.discard(queue)

    async def broadcast(self, event_name: str, data: dict):
        payload = {
            "event": event_name,
            "data": json.dumps(data)
        }
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


_next_request_id = 0


def get_next_request_id() -> int:
    global _next_request_id
    _next_request_id += 1
    return _next_request_id


def get_slot_payload(key: str, model: str) -> dict:
    now = time.time()
    d = get_slot_data(key, model)
    return {
        "used": d["day_count"],
        "rpm": len([t for t in d["timestamps"] if now - t < 60]),
        "cooldown_ends_at": d["cooldown_until"] if d["cooldown_until"] > now else 0.0,
        "unsupported": model in unsupported_slots[key]
    }


async def broadcast_reset_and_snapshot():
    await broadcaster.broadcast("reset", {})
    await broadcaster.broadcast("snapshot", get_stats_payload())


def roll_day():
    """Zero every daily counter when Pacific midnight passes (also clears stale counts loaded from disk)."""
    global current_day
    today = get_pacific_date_str()
    if today != current_day:
        logger.info(
            f"Pacific day rollover ({current_day} -> {today}). Resetting daily counts."
        )
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
                "cooldown_until": (
                    d["cooldown_until"] if d["cooldown_until"] > now else 0.0
                ),
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
    return {"pacific_date": current_day, "keys": keys}


def get_stats_payload() -> dict:
    roll_day()
    now = time.time()
    reset_at = now + seconds_until_pacific_midnight()
    keys = []
    for k in API_KEYS:
        slots = {}
        for m in FLASH_FALLBACK_CHAIN:
            d = get_slot_data(k, m)
            slots[m] = {
                "used": d["day_count"],
                "rpm": len([t for t in d["timestamps"] if now - t < 60]),
                "cooldown": max(0, round(d["cooldown_until"] - now)),
                "cooldown_ends_at": d["cooldown_until"] if d["cooldown_until"] > now else 0.0,
                "unsupported": m in unsupported_slots[k],
            }
        keys.append(
            {
                "key": mask_key(k),
                "blacklisted": key_cooldowns[k] > now,
                **key_stats[k],
                "slots": slots,
            }
        )
    return {
        "pacific_date": current_day,
        "seconds_to_midnight_reset": round(seconds_until_pacific_midnight()),
        "reset_at": reset_at,
        "rpm_limit": RPM_LIMIT,
        "rpd_limit": RPD_LIMIT,
        "models": FLASH_FALLBACK_CHAIN,
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
                _state_dirty = True  # retry next tick
                logger.error(f"Failed to persist router state to {STATE_FILE}: {e}")


async def periodic_reset_check_loop():
    while True:
        await asyncio.sleep(60.0)
        roll_day()


# --- APP LIFECYCLE ---
@asynccontextmanager
async def lifespan(app: FastAPI):
    global _flush_task
    app.state.client = httpx.AsyncClient(
        http2=HTTP2,
        limits=httpx.Limits(
            max_connections=120, max_keepalive_connections=30, keepalive_expiry=60.0
        ),
        timeout=httpx.Timeout(connect=10.0, read=180.0, write=20.0, pool=10.0),
    )
    _flush_task = asyncio.create_task(flush_state_loop())
    reset_check_task = asyncio.create_task(periodic_reset_check_loop())
    logger.info(
        f"Router up. {len(API_KEYS)} keys, RPM={RPM_LIMIT} RPD={RPD_LIMIT} (per key, per model), http2={HTTP2}."
    )
    yield
    _flush_task.cancel()
    reset_check_task.cancel()
    try:
        await asyncio.gather(_flush_task, reset_check_task, return_exceptions=True)
    except asyncio.CancelledError:
        pass
    try:
        write_state_to_disk(
            build_state_snapshot()
        )  # final blocking flush is fine, no traffic during shutdown
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
    logger.error(
        f"CRITICAL ERROR on {request.method} {request.url.path}: {exc}", exc_info=True
    )
    return JSONResponse(
        status_code=500,
        content={
            "error": {
                "message": f"Internal router error: {exc}",
                "type": "internal_error",
                "code": 500,
            }
        },
    )


# --- SLOT ACQUISITION (flash-chain fallback, per key+model RPM/RPD gating) ---
def limits_for(model: str) -> tuple[int, int]:
    return (
        (EMBED_RPM_LIMIT, EMBED_RPD_LIMIT)
        if model == DEFAULT_EMBEDDING_MODEL
        else (RPM_LIMIT, RPD_LIMIT)
    )


async def acquire_slot(
    preferred_model: str | None = None, excluded_slots: set | None = None
) -> tuple[str | None, str | None]:
    excluded_slots = excluded_slots or set()
    async with slot_lock:
        roll_day()
        now = time.time()

        if preferred_model == DEFAULT_EMBEDDING_MODEL:
            models = [preferred_model]
        elif preferred_model in FLASH_FALLBACK_CHAIN:
            models = [preferred_model] + [
                m for m in FLASH_FALLBACK_CHAIN if m != preferred_model
            ]
        else:
            models = FLASH_FALLBACK_CHAIN

        for model in models:
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
        "failed_requests": key_stats[key]["failed_requests"]
    })
    broadcast_bg("slot_changed", {
        "key": m_key,
        "model": model,
        "slot": get_slot_payload(key, model)
    })


def record_failure(key: str):
    key_stats[key]["failed_requests"] += 1
    mark_state_dirty()
    
    m_key = mask_key(key)
    broadcast_bg("key_changed", {
        "key": m_key,
        "blacklisted": key_cooldowns[key] > time.time(),
        "successful_requests": key_stats[key]["successful_requests"],
        "failed_requests": key_stats[key]["failed_requests"]
    })


def is_retryable(status: int, text: str) -> bool:
    """Errors worth failing over on. Google reports a bad key as HTTP 400 API_KEY_INVALID, so 400 needs a peek at the body."""
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

    if status_code == 429:
        if "PerDay" in error_text or (
            "RESOURCE_EXHAUSTED" in error_text and "day" in error_text.lower()
        ):
            cooldown = max(delay, seconds_until_pacific_midnight())
            data["day_count"] = max(data["day_count"], RPD_LIMIT)
            logger.warning(
                f"Daily quota exhausted for {model} on {m_key}. Cooldown ~{int(cooldown)}s."
            )
        else:
            cooldown = max(delay, 60.0)
            logger.warning(
                f"RPM limit hit for {model} on {m_key}. Cooldown {cooldown:.1f}s."
            )
        data["cooldown_until"] = now + cooldown
        mark_state_dirty()
    elif status_code == 503:
        data["cooldown_until"] = now + max(delay, 60.0)
        logger.warning(f"503 overload for {model} on {m_key}. Cooldown 60s.")
        mark_state_dirty()
    elif status_code == 404:
        if (
            "no longer available" in error_text.lower()
            or "not found" in error_text.lower()
        ):
            logger.warning(
                f"Model {model} unsupported on {m_key}. Disabling for this key (a passing Test all re-enables it)."
            )
            unsupported_slots[key].add(model)
            mark_state_dirty()
    elif status_code in (400, 401, 403) and any(
        e in error_text
        for e in [
            "API_KEY_INVALID",
            "PERMISSION_DENIED",
            "CONSUMER_SUSPENDED",
            "UNAUTHENTICATED",
        ]
    ):
        key_cooldowns[key] = now + 86400.0 * 30
        logger.critical(
            f"Key {m_key} returned {status_code}. Blacklisted for 30 days (a passing Test all clears it)."
        )
        mark_state_dirty()

    broadcast_bg("slot_changed", {
        "key": m_key,
        "model": model,
        "slot": get_slot_payload(key, model)
    })
    broadcast_bg("key_changed", {
        "key": m_key,
        "blacklisted": key_cooldowns[key] > time.time(),
        "successful_requests": key_stats[key]["successful_requests"],
        "failed_requests": key_stats[key]["failed_requests"]
    })


# --- HEALTH / STATS / DASHBOARD ---
@app.get("/health")
async def health_check():
    return {
        "status": "healthy",
        "keys_loaded": len(API_KEYS),
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
        # On connect, immediately send a "snapshot" event
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
        headers={
            "Cache-Control": "no-cache",
            "Connection": "keep-alive",
            "X-Accel-Buffering": "no",
        }
    )


@app.get("/", response_class=HTMLResponse)
async def dashboard():
    try:
        with open(DASHBOARD_FILE, "r", encoding="utf-8") as f:
            return HTMLResponse(content=f.read())
    except FileNotFoundError:
        return HTMLResponse(
            content=f"<p>dashboard.html not found at {DASHBOARD_FILE}. Put it next to router.py.</p>",
            status_code=404,
        )


# --- MODEL DISCOVERY ---
@app.get("/v1/models")
@app.get("/models")
async def list_models():
    models = list(ALL_DISCOVERED_MODELS) + ["auto-router", "auto", "default"]
    return {
        "object": "list",
        "data": [
            {"id": m, "object": "model", "created": 1700000000, "owned_by": "google"}
            for m in models
        ],
    }


@app.get("/v1/models/{model_id:path}")
@app.get("/models/{model_id:path}")
async def get_model(model_id: str):
    return {
        "id": model_id.replace("models/", ""),
        "object": "model",
        "created": 1700000000,
        "owned_by": "google",
    }


# --- EMBEDDINGS (OpenAI-shaped request/response; calls Gemini's embedContent) ---
@app.post("/v1/embeddings")
@app.post("/embeddings")
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

    client: httpx.AsyncClient = app.state.client
    data_items, total_tokens = [], 0

    for idx, text in enumerate(inputs):
        text = text if isinstance(text, str) else str(text)
        excluded_slots, success, last_error = (
            set(),
            False,
            "All keys exhausted or cooling down",
        )
        for _ in range(min(len(API_KEYS), 5)):
            key, model = await acquire_slot(
                preferred_model=DEFAULT_EMBEDDING_MODEL, excluded_slots=excluded_slots
            )
            if not key:
                break
            
            req_id = get_next_request_id()
            m_key = mask_key(key)
            await broadcaster.broadcast("request_started", {"id": req_id, "key": m_key, "model": model})
            request_start_time = time.time()

            try:
                resp = await client.post(
                    EMBED_URL,
                    json={
                        "model": f"models/{DEFAULT_EMBEDDING_MODEL}",
                        "content": {"parts": [{"text": text}]},
                    },
                    headers={
                        "x-goog-api-key": key
                    },  # header, not ?key=, so the key never lands in a URL/log
                )
                latency_ms = round((time.time() - request_start_time) * 1000)
                if resp.status_code == 200:
                    values = resp.json().get("embedding", {}).get("values", [])
                    data_items.append(
                        {"object": "embedding", "index": idx, "embedding": values}
                    )
                    total_tokens += max(1, len(text) // 4)
                    record_success(key, model)
                    
                    await broadcaster.broadcast("request_finished", {
                        "id": req_id,
                        "key": m_key,
                        "model": model,
                        "status": 200,
                        "latency_ms": latency_ms,
                        "slot": get_slot_payload(key, model)
                    })
                    success = True
                    break
                
                record_failure(key)
                apply_cooldown(
                    key, model, resp.status_code, resp.text, dict(resp.headers)
                )
                
                await broadcaster.broadcast("request_finished", {
                    "id": req_id,
                    "key": m_key,
                    "model": model,
                    "status": resp.status_code,
                    "latency_ms": latency_ms,
                    "slot": get_slot_payload(key, model)
                })
                excluded_slots.add((key, model))
                last_error = err_msg(resp.text)
                if not is_retryable(resp.status_code, resp.text):
                    break
            except Exception as e:
                record_failure(key)
                excluded_slots.add((key, model))
                last_error = str(e) or type(e).__name__
                latency_ms = round((time.time() - request_start_time) * 1000)
                
                await broadcaster.broadcast("request_finished", {
                    "id": req_id,
                    "key": m_key,
                    "model": model,
                    "status": 500,
                    "latency_ms": latency_ms,
                    "slot": get_slot_payload(key, model)
                })
        if not success:
            raise HTTPException(
                status_code=502, detail=f"Embedding generation failed: {last_error}"
            )

    return {
        "object": "list",
        "data": data_items,
        "model": DEFAULT_EMBEDDING_MODEL,
        "usage": {"prompt_tokens": total_tokens, "total_tokens": total_tokens},
    }


# --- CHAT COMPLETIONS (the core router: try a slot, fail over to the next key/model) ---
@app.post("/v1/chat/completions")
@app.post("/chat/completions")
async def chat_completions(request: Request):
    try:
        body_json = await request.json()
    except Exception:
        raise HTTPException(status_code=400, detail="Invalid JSON body")
    if not isinstance(body_json, dict):
        raise HTTPException(status_code=400, detail="Body must be a JSON object")

    # Any model name outside the flash chain (gpt-4o, a pro model, ...) just means "route for me", starting at the top.
    requested = str(body_json.get("model", "")).replace("models/", "")
    preferred_model = requested if requested in FLASH_FALLBACK_CHAIN else None
    is_stream = bool(body_json.get("stream", False))
    max_retries = min(len(API_KEYS) * len(FLASH_FALLBACK_CHAIN), 30)
    excluded_slots: set = set()
    last_status, last_content = 429, json.dumps(RATE_LIMITED).encode()
    client: httpx.AsyncClient = app.state.client

    for attempt in range(max_retries):
        key, model = await acquire_slot(
            preferred_model=preferred_model, excluded_slots=excluded_slots
        )
        if not key:
            return JSONResponse(status_code=429, content=RATE_LIMITED)

        m_key = mask_key(key)
        slot = get_slot_data(key, model)
        logger.info(
            f"Attempt {attempt + 1}/{max_retries} -> {model} | {m_key} | RPM {len(slot['timestamps'])}/{RPM_LIMIT} | RPD {slot['day_count']}/{RPD_LIMIT}"
        )

        req_id = get_next_request_id()
        await broadcaster.broadcast("request_started", {"id": req_id, "key": m_key, "model": model})
        request_start_time = time.time()

        payload = dict(body_json)
        if "thinking_config" in payload:
            tc = payload.pop("thinking_config", {}) or {}
            budget = tc.get("budget_tokens") or tc.get("thinking_budget")
            if budget == 0:
                payload["reasoning_effort"] = "low"
            elif budget and budget > 4000:
                payload["reasoning_effort"] = "high"
            elif budget:
                payload["reasoning_effort"] = "medium"
        payload["model"] = model
        headers = {"authorization": f"Bearer {key}", "content-type": "application/json"}

        try:
            req = client.build_request(
                "POST",
                CHAT_URL,
                content=json.dumps(payload).encode("utf-8"),
                headers=headers,
            )
            resp = await client.send(req, stream=True)

            if resp.status_code >= 400 or not is_stream:
                try:
                    data = await resp.aread()
                finally:
                    await resp.aclose()
                latency_ms = round((time.time() - request_start_time) * 1000)
                if resp.status_code < 400:
                    record_success(key, model)
                    
                    await broadcaster.broadcast("request_finished", {
                        "id": req_id,
                        "key": m_key,
                        "model": model,
                        "status": resp.status_code,
                        "latency_ms": latency_ms,
                        "slot": get_slot_payload(key, model)
                    })
                    return Response(
                        content=data,
                        status_code=resp.status_code,
                        media_type=resp.headers.get("content-type", "application/json"),
                    )
                err_text = data.decode("utf-8", errors="replace")
                record_failure(key)
                
                await broadcaster.broadcast("request_finished", {
                    "id": req_id,
                    "key": m_key,
                    "model": model,
                    "status": resp.status_code,
                    "latency_ms": latency_ms,
                    "slot": get_slot_payload(key, model)
                })
                if not is_retryable(resp.status_code, err_text):
                    return Response(
                        content=data,
                        status_code=resp.status_code,
                        media_type="application/json",
                    )  # client's fault (bad request): don't rotate
                logger.warning(
                    f"Upstream [{resp.status_code}] {m_key}/{model}: {err_msg(err_text)[:140]}"
                )
                apply_cooldown(
                    key, model, resp.status_code, err_text, dict(resp.headers)
                )
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
                    await broadcaster.broadcast("request_finished", {
                        "id": r_id,
                        "key": m_key,
                        "model": model,
                        "status": 200,
                        "latency_ms": latency_ms,
                        "slot": get_slot_payload(key, model)
                    })

            return StreamingResponse(
                stream_forward(),
                status_code=resp.status_code,
                media_type="text/event-stream",
                headers={
                    "Cache-Control": "no-cache",
                    "Connection": "keep-alive",
                    "X-Accel-Buffering": "no",
                },
            )
        except Exception as e:
            logger.error(f"Request error {m_key}/{model}: {e or type(e).__name__}")
            record_failure(key)
            latency_ms = round((time.time() - request_start_time) * 1000)
            
            await broadcaster.broadcast("request_finished", {
                "id": req_id,
                "key": m_key,
                "model": model,
                "status": 500,
                "latency_ms": latency_ms,
                "slot": get_slot_payload(key, model)
            })
            excluded_slots.add((key, model))

    return Response(
        content=last_content, status_code=last_status, media_type="application/json"
    )


# --- TEST ALL KEYS x MODELS (dashboard button: fires every combo live, in parallel) ---
@app.post("/test-all")
async def test_all():
    client: httpx.AsyncClient = app.state.client
    payload = lambda model: json.dumps(
        {
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1,
        }
    ).encode("utf-8")

    async def test_combo(key: str, model: str):
        req_id = get_next_request_id()
        m_key = mask_key(key)
        start = time.time()

        # Broadcast request_started!
        await broadcaster.broadcast("request_started", {"id": req_id, "key": m_key, "model": model})

        slot = get_slot_data(key, model)
        slot["timestamps"].append(
            start
        )  # bypasses the gate, but keeps RPM accounting honest
        try:
            resp = await client.post(
                CHAT_URL,
                content=payload(model),
                headers={
                    "authorization": f"Bearer {key}",
                    "content-type": "application/json",
                },
                timeout=20.0,
            )
            latency_ms = round((time.time() - start) * 1000)
            if resp.status_code == 200:
                record_success(key, model)
                slot["cooldown_until"] = (
                    0.0  # it demonstrably works right now, so clear any stale flags
                )
                unsupported_slots[key].discard(model)
                key_cooldowns[key] = 0.0
                mark_state_dirty()

                # Broadcast request_finished!
                await broadcaster.broadcast("request_finished", {
                    "id": req_id,
                    "key": m_key,
                    "model": model,
                    "status": 200,
                    "latency_ms": latency_ms,
                    "slot": get_slot_payload(key, model)
                })
                return {
                    "key": m_key,
                    "model": model,
                    "status": "ok",
                    "latency_ms": latency_ms,
                }
            
            record_failure(key)
            apply_cooldown(key, model, resp.status_code, resp.text, dict(resp.headers))

            # Broadcast request_finished!
            await broadcaster.broadcast("request_finished", {
                "id": req_id,
                "key": m_key,
                "model": model,
                "status": resp.status_code,
                "latency_ms": latency_ms,
                "slot": get_slot_payload(key, model)
            })
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

            # Broadcast request_finished!
            await broadcaster.broadcast("request_finished", {
                "id": req_id,
                "key": m_key,
                "model": model,
                "status": 500,
                "latency_ms": latency_ms,
                "slot": get_slot_payload(key, model)
            })
            return {
                "key": m_key,
                "model": model,
                "status": "error",
                "detail": str(e) or type(e).__name__,
                "latency_ms": latency_ms,
            }

    results = await asyncio.gather(
        *(test_combo(k, m) for k in API_KEYS for m in FLASH_FALLBACK_CHAIN)
    )
    ok_count = sum(r["status"] == "ok" for r in results)
    return {
        "ok": True,
        "tested": len(results),
        "passed": ok_count
    }


if __name__ == "__main__":
    import uvicorn

    uvicorn.run(
        app,
        host=os.environ.get("HOST", "127.0.0.1"),
        port=int(os.environ.get("PORT", "9999")),
    )
