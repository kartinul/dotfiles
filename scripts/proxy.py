import time
import os
import json
import httpx
import logging
from dotenv import load_dotenv
from fastapi import FastAPI, Request, HTTPException
from fastapi.responses import StreamingResponse, Response

# --- 1. SETUP LOGGER ---
logging.basicConfig(
    level=logging.INFO,
    format="%(asctime)s | %(levelname)-8s | %(message)s",
    datefmt="%Y-%m-%d %H:%M:%S"
)
logger = logging.getLogger("GeminiProxy")

app = FastAPI()
load_dotenv()

# --- 2. GLOBAL EXCEPTION HANDLER ---
@app.exception_handler(Exception)
async def global_exception_handler(request: Request, exc: Exception):
    logger.error(f"CRITICAL ERROR on {request.method} {request.url.path}: {str(exc)}", exc_info=True)
    return Response(
        content=json.dumps({"error": str(exc)}), 
        status_code=500, 
        media_type="application/json"
    )

raw_keys = os.environ.get("GEMINI_API_KEYS", "")
API_KEYS = [k.strip() for k in raw_keys.split(",") if k.strip()]

if not API_KEYS:
    logger.critical("No keys found! Set GEMINI_API_KEYS in .env")
    raise ValueError("No keys found! Set GEMINI_API_KEYS=\"key1,key2,key3\" in .env")

logger.info(f"Proxy started. Loaded {len(API_KEYS)} API keys.")

MODELS = [
    "gemini-3.8-flash",
    "gemini-3.7-flash",
    "gemini-3.6-flash",
    "gemini-3.5-flash",
    "gemini-3-flash",
    "gemini-2.5-flash"
]

usage_state = {
    key: {
        model: {
            "timestamps": [], 
            "day_count": 0, 
            "last_reset": time.time(),
            "cooldown_until": 0.0
        }
        for model in MODELS
    }
    for key in API_KEYS
}

RPM_LIMIT = 5
RPD_LIMIT = 20

# Unsupported fields to strip before forwarding to Google's OpenAI layer
UNSUPPORTED_OPENAI_FIELDS = {"thinking_config", "name"}

def get_available_slot():
    now = time.time()
    
    for model in MODELS:
        for key in API_KEYS:
            data = usage_state[key][model]
            
            if now - data["last_reset"] > 86400:
                data["day_count"] = 0
                data["last_reset"] = now
                data["cooldown_until"] = 0.0
                
            # Skip slots currently in penalty/cool-down
            if now < data["cooldown_until"]:
                continue
                
            data["timestamps"] = [t for t in data["timestamps"] if now - t < 60]
            
            if len(data["timestamps"]) < RPM_LIMIT and data["day_count"] < RPD_LIMIT:
                return key, model
                
    return None, None

@app.api_route("/{path:path}", methods=["GET", "POST", "PUT", "DELETE", "PATCH"])
async def proxy_to_gemini(request: Request, path: str):
    body = await request.body()
    try:
        body_json = json.loads(body.decode("utf-8")) if body else {}
    except Exception:
        body_json = {}

    is_openai_format = "messages" in body_json or "chat/completions" in path
    is_native_format = ":generateContent" in path or ":streamGenerateContent" in path

    # Firewall: Drop everything that isn't explicitly a chat completion request
    if not is_openai_format and not is_native_format:
        logger.warning(f"Ignored unsupported endpoint probe: /{path}")
        raise HTTPException(status_code=404, detail="Not a supported completion endpoint")

    key, model = get_available_slot()
    
    if not key:
        logger.error("Rate Limit Hit: All Gemini keys and models are currently maxed out or cooling down.")
        raise HTTPException(
            status_code=429, 
            detail="All Gemini keys and models are currently maxed out."
        )

    # Lock in usage
    usage_state[key][model]["timestamps"].append(time.time())
    usage_state[key][model]["day_count"] += 1
    
    masked_key = f"...{key[-4:]}"
    rpm_count = len(usage_state[key][model]["timestamps"])
    rpd_count = usage_state[key][model]["day_count"]
    
    logger.info(f"Routing -> Model: {model} | Key: {masked_key} | RPM: {rpm_count}/{RPM_LIMIT} | RPD: {rpd_count}/{RPD_LIMIT}")

    headers = dict(request.headers)
    for h in ["host", "authorization", "x-goog-api-key", "content-length"]:
        headers.pop(h, None)
    
    is_stream = False

    if is_openai_format:
        logger.info("Detected OpenAI format payload. Sanitizing and Translating...")
        
        for field in UNSUPPORTED_OPENAI_FIELDS:
            body_json.pop(field, None)
            
        body_json["model"] = model
        body = json.dumps(body_json).encode("utf-8")
        
        target_url = "https://generativelanguage.googleapis.com/v1beta/openai/chat/completions"
        headers["authorization"] = f"Bearer {key}"
        is_stream = body_json.get("stream", False)
    else:
        logger.info("Detected Native Gemini format payload.")
        action = path.split(":")[-1] if ":" in path else "generateContent"
        target_url = f"https://generativelanguage.googleapis.com/v1beta/models/{model}:{action}?key={key}"
        is_stream = "stream" in action.lower()

    if is_stream:
        async def stream_forward():
            try:
                async with httpx.AsyncClient(timeout=120.0) as client:
                    req = client.build_request(request.method, target_url, content=body, headers=headers)
                    resp = await client.send(req, stream=True)
                    logger.info(f"Upstream Response: {resp.status_code} (Streaming)")
                    
                    if resp.status_code >= 400:
                        await resp.aread()
                        error_text = resp.text
                        logger.error(f"Google API Error [{resp.status_code}]: {error_text}")
                        
                        if resp.status_code == 429:
                            if "PerDay" in error_text:
                                usage_state[key][model]["cooldown_until"] = time.time() + 86400.0
                                logger.warning(f"Daily quota exhausted for {model} on key {masked_key}. Blacklisted for 24h.")
                            else:
                                usage_state[key][model]["cooldown_until"] = time.time() + 60.0
                                logger.warning(f"RPM rate limit hit for {model} on key {masked_key}. Cooling down for 60s.")
                        elif resp.status_code == 503:
                            usage_state[key][model]["cooldown_until"] = time.time() + 15.0
                            logger.warning(f"Server overloaded for {model} on key {masked_key}. Cooling down for 15s.")
                            
                        yield resp.content
                    else:
                        async for chunk in resp.aiter_bytes():
                            yield chunk
            except Exception as e:
                logger.error(f"Stream failed: {str(e)}")
                raise
        return StreamingResponse(stream_forward())
    else:
        try:
            async with httpx.AsyncClient(timeout=120.0) as client:
                resp = await client.request(request.method, target_url, content=body, headers=headers)
                logger.info(f"Upstream Response: {resp.status_code}")
                
                if resp.status_code >= 400:
                    error_text = resp.text
                    logger.error(f"Google API Error [{resp.status_code}]: {error_text}")
                    if resp.status_code == 429:
                        if "PerDay" in error_text:
                            usage_state[key][model]["cooldown_until"] = time.time() + 86400.0
                        else:
                            usage_state[key][model]["cooldown_until"] = time.time() + 60.0
                    elif resp.status_code == 503:
                        usage_state[key][model]["cooldown_until"] = time.time() + 15.0
                        
                return Response(content=resp.content, status_code=resp.status_code)
        except Exception as e:
            logger.error(f"Request failed: {str(e)}")
            raise HTTPException(status_code=502, detail="Bad Gateway")

if __name__ == "__main__":
    import uvicorn
    uvicorn.run(app, host="127.0.0.1", port=9999)