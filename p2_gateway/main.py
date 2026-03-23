"""
Nostr P2P Gateway - WebSocket and HTTP server for broker communication.
"""
import asyncio
import base64
import hashlib
import json
import logging
import os
import secrets
import ssl
import struct
import time
import random
from datetime import datetime
import aiohttp
from aiohttp import web

import secp256k1
from cryptography.hazmat.primitives.ciphers import Cipher, algorithms, modes
from cryptography.hazmat.primitives.hashes import SHA256
from cryptography.hazmat.primitives.hmac import HMAC as _HMAC
from cryptography.hazmat.primitives.kdf.hkdf import HKDFExpand
from cryptography.hazmat.backends import default_backend

# ── bech32 ────────────────────────────────────────────────────────────────────
_CS = "qpzry9x8gf2tvdw0s3jn54khce6mua7l"
_CM = {c: i for i, c in enumerate(_CS)}
_GN = [0x3b6a57b2, 0x26508e6d, 0x1ea119fa, 0x3d4233dd, 0x2a1462b3]

def _pm(vals):
    c = 1
    for v in vals:
        b = c >> 25; c = (c & 0x1ffffff) << 5 ^ v
        for i in range(5): c ^= _GN[i] if (b >> i) & 1 else 0
    return c

def _hrp(h): return [ord(x) >> 5 for x in h] + [0] + [ord(x) & 31 for x in h]

def _bits(data, f, t, pad=True):
    a = bits = 0; r = []; mx = (1 << t) - 1
    for v in data:
        a = (a << f) | v; bits += f
        while bits >= t: bits -= t; r.append((a >> bits) & mx)
    if pad and bits: r.append((a << (t - bits)) & mx)
    return r

def _b32enc(hrp, data):
    d = _bits(data, 8, 5)
    p = _pm(_hrp(hrp) + d + [0] * 6) ^ 1
    return hrp + "1" + "".join(_CS[x] for x in d + [(p >> 5*(5-i)) & 31 for i in range(6)])

def _b32dec(s):
    s = s.lower(); p = s.rfind("1")
    if p < 1 or p + 7 > len(s): raise ValueError("bad bech32")
    hrp = s[:p]
    try: d = [_CM[c] for c in s[p+1:]]
    except KeyError: raise ValueError("bad char")
    if _pm(_hrp(hrp) + d) != 1: raise ValueError("bad checksum")
    return hrp, bytes(_bits(d[:-6], 5, 8, pad=False))

def to_npub(h): return _b32enc("npub", bytes.fromhex(h))
def to_nsec(h): return _b32enc("nsec", bytes.fromhex(h))

def npub2hex(s):
    hrp, b = _b32dec(s)
    if hrp != "npub": raise ValueError("not an npub")
    return b.hex()

def nsec2hex(s):
    hrp, b = _b32dec(s)
    if hrp != "nsec": raise ValueError("not an nsec")
    return b.hex()

# ── crypto ────────────────────────────────────────────────────────────────────
def gen_keys():
    b = secrets.token_bytes(32)
    return b.hex(), secp256k1.PrivateKey(b).pubkey.serialize()[1:].hex()

def derive_pub(priv):
    return secp256k1.PrivateKey(bytes.fromhex(priv)).pubkey.serialize()[1:].hex()

def schnorr(eid, priv):
    return secp256k1.PrivateKey(bytes.fromhex(priv)).schnorr_sign(
        bytes.fromhex(eid), None, raw=True).hex()

def _ecdh(priv, pub):
    pk = secp256k1.PublicKey(bytes.fromhex("02" + pub), raw=True)
    return pk.tweak_mul(secp256k1.PrivateKey(bytes.fromhex(priv)).private_key
                        ).serialize(compressed=True)[1:]

# ── NIP-44 v2（现代加密）────────────────────────────────────────
def _nip44_conv_key(priv, pub):
    """HKDF-Extract(salt="nip44-v2", IKM=ecdh_x)"""
    h = _HMAC(b"nip44-v2", SHA256(), backend=default_backend())
    h.update(_ecdh(priv, pub))
    return h.finalize()

def _nip44_pad_len(l):
    if l <= 32: return 32
    np = 1 << (l - 1).bit_length()
    chunk = max(np // 8, 32)
    return chunk * ((l - 1) // chunk + 1)

def nip44_enc(text, priv, pub):
    ck = _nip44_conv_key(priv, pub)
    nonce = secrets.token_bytes(32)
    keys = HKDFExpand(SHA256(), 76, nonce, default_backend()).derive(ck)
    ck2, cn, hk = keys[:32], keys[32:44], keys[44:]
    plain = text.encode()
    pl = _nip44_pad_len(len(plain))
    padded = struct.pack('>H', len(plain)) + plain + b'\x00' * (pl - len(plain))
    enc = Cipher(algorithms.ChaCha20(ck2, b'\x00\x00\x00\x00' + cn),
                 None, default_backend()).encryptor()
    ct = enc.update(padded) + enc.finalize()
    hm = _HMAC(hk, SHA256(), backend=default_backend())
    hm.update(nonce + ct)
    return base64.b64encode(b'\x02' + nonce + ct + hm.finalize()).decode()

def nip44_dec(payload, priv, pub):
    try:
        raw = base64.b64decode(payload)
        if raw[0] != 2: return "[unsupported nip44 version]"
        nonce, ct, mac = raw[1:33], raw[33:-32], raw[-32:]
        ck = _nip44_conv_key(priv, pub)
        keys = HKDFExpand(SHA256(), 76, nonce, default_backend()).derive(ck)
        ck2, cn, hk = keys[:32], keys[32:44], keys[44:]
        hm = _HMAC(hk, SHA256(), backend=default_backend())
        hm.update(nonce + ct)
        if not secrets.compare_digest(mac, hm.finalize()): return "[nip44 bad mac]"
        dec = Cipher(algorithms.ChaCha20(ck2, b'\x00\x00\x00\x00' + cn),
                     None, default_backend()).decryptor()
        padded = dec.update(ct) + dec.finalize()
        l = struct.unpack('>H', padded[:2])[0]
        return padded[2:2+l].decode()
    except Exception as e: return f"[nip44 error: {e}]"

# ── NIP-59 Gift Wrap + NIP-17───────────────────────────────
def _rand_ts(): return int(time.time()) - random.randint(0, 172800)

def _ev_id(ev):
    s = json.dumps([0, ev["pubkey"], ev["created_at"], ev["kind"],
                    ev["tags"], ev["content"]], separators=(",",":"), ensure_ascii=False)
    return hashlib.sha256(s.encode()).hexdigest()

def nip17_wrap(text, sender_priv, sender_pub, recipient_pub):
    """
    NIP-17 Gift Wrap DM:
      Rumor (kind:14, unsigned) → Seal (kind:13, NIP-44) → Gift Wrap (kind:1059, 临时密钥)
    """
    # 1. Rumor（kind:14，不签名）
    rumor = {"pubkey": sender_pub, "created_at": int(time.time()),
             "kind": 14, "tags": [["p", recipient_pub]], "content": text}
    rumor["id"] = _ev_id(rumor)

    # 2. Seal（kind:13，发送方签名，NIP-44 加密 rumor）
    seal = {"pubkey": sender_pub, "created_at": _rand_ts(),
            "kind": 13, "tags": [], "content": nip44_enc(json.dumps(rumor), sender_priv, recipient_pub)}
    seal["id"] = _ev_id(seal); seal["sig"] = schnorr(seal["id"], sender_priv)

    # 3. Gift Wrap（kind:1059，临时密钥签名，NIP-44 加密 seal）
    eph_priv, eph_pub = gen_keys()
    wrap = {"pubkey": eph_pub, "created_at": _rand_ts(),
            "kind": 1059, "tags": [["p", recipient_pub]],
            "content": nip44_enc(json.dumps(seal), eph_priv, recipient_pub)}
    wrap["id"] = _ev_id(wrap); wrap["sig"] = schnorr(wrap["id"], eph_priv)
    return wrap

def nip17_unwrap(wrap_ev, my_priv):
    """解包 NIP-17 Gift Wrap，返回 (rumor_dict, sender_pub_hex)"""
    seal_json = nip44_dec(wrap_ev["content"], my_priv, wrap_ev["pubkey"])
    seal = json.loads(seal_json)
    if seal.get("kind") != 13: raise ValueError(f"expected kind:13, got {seal.get('kind')}")
    rumor_json = nip44_dec(seal["content"], my_priv, seal["pubkey"])
    rumor = json.loads(rumor_json)
    if rumor.get("kind") != 14: raise ValueError(f"expected kind:14, got {rumor.get('kind')}")
    return rumor, seal["pubkey"]

# ── 通用事件构建 ──────────────────────────────────────────────────────────────
def mkevent(kind, content, priv, pub, tags=None):
    ev = {"pubkey": pub, "created_at": int(time.time()),
          "kind": kind, "tags": tags or [], "content": content}
    ev["id"] = _ev_id(ev); ev["sig"] = schnorr(ev["id"], priv)
    return ev

# ── relay pool ────────────────────────────────────────────────────────────────
RELAYS = [
    "wss://relay.damus.io",
    "wss://relay.0xchat.com",
    "wss://nostr.oxtr.dev",
    "wss://nostr-pub.wellorder.net",
    "wss://relay.primal.net",
]
HISTORY_SECS = 7 * 24 * 3600   # 拉取最近 7 天的历史消息
PROXY = (os.environ.get("HTTPS_PROXY") or os.environ.get("https_proxy") or
         os.environ.get("ALL_PROXY")   or os.environ.get("all_proxy"))
_SSL = ssl.create_default_context()
_SSL.check_hostname = False; _SSL.verify_mode = ssl.CERT_NONE

_COL  = {"OK":"\033[32m","WARN":"\033[33m","ERROR":"\033[31m",
         "INFO":"\033[36m","RECV":"\033[32m","SEND":"\033[34m","AUTH":"\033[35m"}
_ICON = {"OK":"●","WARN":"▲","ERROR":"✗","INFO":"·","RECV":"▼","SEND":"►","AUTH":"◆"}
_RST  = "\033[0m"

def _seen_add(seen, eid):
    now = time.time(); seen[eid] = now
    if len(seen) > 5000:
        cutoff = now - 7200
        for k in [k for k, v in seen.items() if v < cutoff]: del seen[k]


class RelayPool:
    def __init__(self, on_event, pub, priv, on_log):
        self._ev = on_event; self._pub = pub; self._priv = priv; self._log = on_log
        self._conns = {}; self._seen = {}; self._tasks = []; self._sess = None
        self._t0 = int(time.time()); self._pubshow = False
        self._psubs = {}; self._authed = set(); self._lock = asyncio.Lock()

    connected = property(lambda self: len(self._conns))

    def _dbg(self, url, lvl, msg): self._log(url, lvl, msg)

    async def start(self):
        self._sess = aiohttp.ClientSession(
            timeout=aiohttp.ClientTimeout(total=60, connect=15, sock_read=60),
            connector=aiohttp.TCPConnector(ssl=_SSL) if PROXY else None)
        for u in RELAYS:
            self._tasks.append(asyncio.create_task(self._connect(u)))

    async def close(self):
        for t in self._tasks: t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        for ws in list(self._conns.values()):
            try: await ws.close()
            except Exception: pass
        self._conns.clear()
        if self._sess and not self._sess.closed:
            await self._sess.close(); await asyncio.sleep(0.25)

    async def publish(self, ev):
        msg = json.dumps(["EVENT", ev]); ok = 0
        for ws in list(self._conns.values()):
            try: await ws.send_str(msg); ok += 1
            except Exception: pass
        return ok

    async def set_pubshow(self, on):
        if on == self._pubshow: return
        async with self._lock:
            self._pubshow = on
            for url, ws in list(self._conns.items()):
                try:
                    if on:
                        sid = secrets.token_hex(8); self._psubs[url] = sid
                        await ws.send_str(json.dumps(
                            ["REQ", sid, {"kinds":[1],"since":self._t0,"limit":0}]))
                    elif url in self._psubs:
                        await ws.send_str(json.dumps(["CLOSE", self._psubs.pop(url)]))
                except Exception: pass

    async def _connect(self, url):
        bo = 2; kw = {"headers": {"User-Agent": "NostrCLI/1.0"}}
        if PROXY: kw["proxy"] = PROXY
        while True:
            self._dbg(url, "INFO", f"connecting… (retry={bo}s)")
            try:
                t0 = time.time()
                async with self._sess.ws_connect(url, ssl=_SSL, **kw) as ws:
                    self._conns[url] = ws; bo = 2
                    self._dbg(url, "OK", f"connected ({(time.time()-t0)*1000:.0f}ms)")
                    await self._subscribe(ws, url)
                    ping = asyncio.create_task(self._ping(ws, url))
                    try:
                        async for msg in ws:
                            if msg.type == aiohttp.WSMsgType.TEXT:
                                await self._handle(msg.data, url, ws)
                            elif msg.type in (aiohttp.WSMsgType.ERROR,
                                              aiohttp.WSMsgType.CLOSED): break
                    finally:
                        ping.cancel()
                        try: await ping
                        except asyncio.CancelledError: pass
            except asyncio.CancelledError: return
            except Exception as e: self._dbg(url, "ERROR", str(e))
            finally:
                self._conns.pop(url, None); self._psubs.pop(url, None)
                self._authed.discard(url); self._dbg(url, "WARN", "disconnected")
            await asyncio.sleep(bo); bo = min(bo * 2, 60)

    async def _subscribe(self, ws, url):
        async with self._lock:
            sid = secrets.token_hex(8)
            since_hist = int(time.time()) - HISTORY_SECS
            # ① 历史消息：kind:1059，最近 7 天，最多 0 条
            await ws.send_str(json.dumps(
                ["REQ", sid, {"kinds":[1059],"#p":[self._pub],
                              "since": since_hist, "limit": 0}]))
            self._dbg(url, "SEND", f"REQ kind=4,1059 history sub={sid[:8]}")
            # ② 实时消息：kind:1059，从现在起
            sid2 = secrets.token_hex(8)
            await ws.send_str(json.dumps(
                ["REQ", sid2, {"kinds":[1059],"#p":[self._pub],
                               "since": self._t0, "limit": 0}]))
            self._dbg(url, "SEND", f"REQ kind=4,1059 live  sub={sid2[:8]}")
            if self._pubshow:
                pid = secrets.token_hex(8); self._psubs[url] = pid
                await ws.send_str(json.dumps(
                    ["REQ", pid, {"kinds":[1],"since":self._t0,"limit":0}]))
                self._dbg(url, "SEND", f"REQ kind=1 sub={pid[:8]}")

    async def _ping(self, ws, url):
        while True:
            await asyncio.sleep(30)
            try:
                await asyncio.wait_for(ws.ping(), timeout=10)
                self._dbg(url, "INFO", "ping ok")
            except asyncio.TimeoutError:
                self._dbg(url, "WARN", "ping timeout — dropping"); await ws.close(); break
            except Exception: break

    async def _auth(self, ws, url, challenge):
        self._dbg(url, "AUTH", f"challenge={challenge[:32]}…")
        ev = mkevent(22242, "", self._priv, self._pub,
                     [["relay", url], ["challenge", challenge]])
        await ws.send_str(json.dumps(["AUTH", ev]))
        self._authed.add(url)
        self._dbg(url, "AUTH", f"responded id={ev['id'][:12]}…")

    async def _handle(self, raw, url, ws):
        try: msg = json.loads(raw)
        except Exception: return
        if not isinstance(msg, list) or not msg: return
        t = msg[0]
        if t == "EVENT" and len(msg) >= 3:
            ev = msg[2]; eid = ev.get("id", "")
            self._dbg(url, "RECV",
                f"kind={ev.get('kind')} id={eid[:12]}… pub={ev.get('pubkey','')[:10]}…")
            if eid and eid not in self._seen:
                _seen_add(self._seen, eid); await self._ev(ev)
        elif t == "AUTH"   and len(msg) >= 2: await self._auth(ws, url, str(msg[1]))
        elif t == "NOTICE" and len(msg) >= 2: self._dbg(url, "WARN", f"NOTICE: {str(msg[1])[:100]}")
        elif t == "OK"     and len(msg) >= 3:
            ok = msg[2]; reason = (str(msg[3]) if len(msg) > 3 else "")[:60]
            self._dbg(url, "OK" if ok else "WARN", f"OK id={str(msg[1])[:12]}… accepted={ok} {reason}")
        elif t == "EOSE"   and len(msg) >= 2: self._dbg(url, "INFO",  f"EOSE sub={str(msg[1])[:12]}…")
        elif t == "CLOSED" and len(msg) >= 2:
            self._dbg(url, "WARN",
                f"CLOSED {str(msg[1])[:12]}… {(str(msg[2]) if len(msg)>2 else '')[:60]}")


# Gateway state
ws_broker = None
gateway_keys = {}
relay_pool = None

# ── key management ─────────────────────────────────────────────────────────────
KEY_FILE = "all_key.json"

def save_keys(data: dict):
    """保存密钥到all_key.json"""
    with open(KEY_FILE, "w") as f:
        json.dump(data, f, indent=2)

def load_keys() -> dict:
    """从all_key.json加载密钥"""
    if os.path.exists(KEY_FILE):
        try:
            with open(KEY_FILE) as f:
                return json.load(f)
        except (json.JSONDecodeError, IOError) as e:
            print(f"[gateway] Warning: Failed to load keys: {e}")
    return {"version": 1, "keys": []}

def get_or_create_key(cwd: str) -> dict:
    """根据cwd获取或创建密钥"""
    keys_data = load_keys()

    # 查找已存在的cwd
    for key in keys_data.get("keys", []):
        if key.get("cwd") == cwd:
            return {"npub": key["npub"], "nsec": key["nsec"]}

    # 生成新密钥
    priv, pub = gen_keys()
    npub = to_npub(pub)
    nsec = to_nsec(priv)

    keys_data.setdefault("keys", []).append({
        "npub": npub,
        "nsec": nsec,
        "cwd": cwd,
        "created_at": datetime.now().isoformat()
    })
    save_keys(keys_data)

    return {"npub": npub, "nsec": nsec}

def get_nsec_by_npub(npub: str) -> str | None:
    """根据npub查找nsec"""
    keys_data = load_keys()
    for key in keys_data.get("keys", []):
        if key["npub"] == npub:
            return key["nsec"]
    return None

# Configuration
WS_PORT = 7899
ALL_KEYS_FILE = "all_key.json"

logging.basicConfig(level=logging.INFO)
logger = logging.getLogger(__name__)


async def handle_broker_ws(request):
    """
    Handle WebSocket connections from Broker.
    Receives messages and processes them.
    """
    global ws_broker

    ws = web.WebSocketResponse()
    await ws.prepare(request)

    ws_broker = ws
    logger.info("Broker connected")

    try:
        async for msg in ws:
            if msg.type == web.WSMsgType.TEXT:
                await process_broker_message(msg.data)
            elif msg.type == web.WSMsgType.ERROR:
                logger.error(f"WebSocket error: {ws.exception()}")
    except Exception as e:
        logger.error(f"Broker connection error: {e}")
    finally:
        ws_broker = None
        logger.info("Broker disconnected")

    return ws


async def process_broker_message(data):
    """
    Process incoming messages from Broker.
    Delegates to specific handlers based on message type.
    """
    try:
        message = json.loads(data)
        msg_type = message.get("type")

        if msg_type == "request_key":
            await handle_request_key(message)
        elif msg_type == "register":
            await handle_register(message)
        elif msg_type == "send_dm":
            await handle_send_dm(message)
        elif msg_type == "relay_event":
            await relay_event(message)
        else:
            logger.warning(f"Unknown message type: {msg_type}")
    except json.JSONDecodeError:
        logger.error(f"Invalid JSON: {data}")
    except Exception as e:
        logger.error(f"Error processing message: {e}")


async def handle_request_key(message):
    """处理密钥请求"""
    cwd = message.get("cwd")
    if not cwd:
        logger.warning("request_key message missing cwd")
        return

    print(f"[gateway] Key request for cwd: {cwd}")
    key = get_or_create_key(cwd)
    await ws_to_broker({
        "type": "key_assigned",
        "cwd": cwd,
        "npub": key["npub"],
        "nsec": key["nsec"]
    })


async def handle_register(message):
    """Subscribe to npub's DM events."""
    npub = message.get("npub")
    if not npub:
        logger.warning("register message missing npub")
        return
    print(f"[gateway] Subscribing to npub: {npub[:20]}...")

    if relay_pool:
        pub_hex = npub2hex(npub)
        # Subscribe to this npub's DMs
        for ws in list(relay_pool._conns.values()):
            await ws.send_str(json.dumps([
                "REQ", secrets.token_hex(8),
                {"kinds": [1059], "#p": [pub_hex], "limit": 0}
            ]))


async def relay_event(event: dict):
    """Process events from relays"""
    kind = event.get("kind")

    # Only handle NIP-17 Gift Wrap
    if kind == 1059:
        try:
            # Try to decrypt with all managed nsec
            keys_data = load_keys()
            for key in keys_data.get("keys", []):
                my_nsec = key["nsec"]
                my_priv = nsec2hex(my_nsec)

                try:
                    rumor, real_sender = nip17_unwrap(event, my_priv)
                    content = rumor.get("content", "")

                    print(f"[gateway] DM from {real_sender[:20]}... to {key['npub'][:20]}...")

                    # Forward to Broker
                    await ws_to_broker({
                        "type": "dm_received",
                        "from_npub": to_npub(real_sender),
                        "to_npub": key["npub"],
                        "content": content
                    })
                    return  # Only forward to one matching recipient
                except Exception:
                    continue  # Try next nsec

        except Exception as e:
            print(f"[gateway] Error processing gift wrap: {e}")


async def handle_send_dm(data: dict):
    """处理发送DM请求"""
    to_npub = data["to_npub"]
    content = data["content"]
    from_npub = data["from_npub"]

    print(f"[gateway] Send DM to {to_npub[:20]}... from {from_npub[:20]}...")

    # 获取发送方的nsec
    from_nsec = get_nsec_by_npub(from_npub)
    if not from_nsec:
        print(f"[gateway] Error: No nsec found for npub {from_npub[:20]}...")
        await ws_to_broker({"type": "dm_sent", "ok": False})
        return

    from_priv = nsec2hex(from_nsec)
    from_pub = derive_pub(from_priv)
    to_pub = npub2hex(to_npub)

    try:
        # NIP-17加密
        wrapped = nip17_wrap(content, from_priv, from_pub, to_pub)

        # 发布到relay
        if relay_pool:
            ok = await relay_pool.publish(wrapped)
            print(f"[gateway] Published to {ok} relays")
            await ws_to_broker({"type": "dm_sent", "ok": ok > 0})
        else:
            print(f"[gateway] Error: Relay pool not initialized")
            await ws_to_broker({"type": "dm_sent", "ok": False})
    except Exception as e:
        print(f"[gateway] Error sending DM: {e}")
        await ws_to_broker({"type": "dm_sent", "ok": False})


async def ws_to_broker(message):
    """
    Send a message to the Broker via WebSocket.
    """
    global ws_broker
    if ws_broker is not None and not ws_broker.closed:
        await ws_broker.send_json(message)
    else:
        logger.warning("Cannot send to broker: not connected")


async def health_handler(request):
    """
    HTTP health check endpoint.
    Returns 200 OK if the gateway is running.
    """
    return web.Response(text="OK")


async def start_relay_pool():
    """Start the RelayPool for connecting to Nostr relays."""
    global relay_pool
    relay_pool = RelayPool(
        on_event=relay_event,
        pub="",  # Gateway doesn't need its own pub
        priv="",
        on_log=lambda url, lvl, msg: print(f"[relay:{url}] {lvl}: {msg}")
    )
    await relay_pool.start()
    print(f"[gateway] Relay pool started, {relay_pool.connected} connected")


async def main():
    """
    Start the Gateway server with WebSocket and HTTP endpoints.
    """
    app = web.Application()

    # WebSocket endpoint for Broker
    app.router.add_route("GET", "/ws", handle_broker_ws)

    # HTTP health check endpoint
    app.router.add_get("/health", health_handler)

    runner = web.AppRunner(app)
    await runner.setup()

    site = web.TCPSite(runner, "0.0.0.0", WS_PORT)
    await site.start()

    logger.info(f"Gateway started on port {WS_PORT}")

    # Start the relay pool
    await start_relay_pool()

    # Keep the server running
    try:
        await asyncio.Event().wait()
    except KeyboardInterrupt:
        logger.info("Gateway shutting down")
        if relay_pool:
            await relay_pool.close()


if __name__ == "__main__":
    asyncio.run(main())
