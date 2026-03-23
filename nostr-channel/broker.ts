#!/usr/bin/env bun
/**
 * Nostr Channel broker daemon
 *
 * Pure in-memory store — no persistence.
 * All peers/messages lost on restart.
 * Bridges MCP servers with the Python Gateway via WebSocket.
 */

import type {
  RegisterRequest,
  RegisterResponse,
  HeartbeatRequest,
  ListPeersRequest,
  SendMessageRequest,
  PollMessagesRequest,
  PollMessagesResponse,
  Peer,
  Message,
} from "./shared/types.ts";

const PORT = parseInt(process.env.NOSTR_BROKER_PORT ?? "7900", 10);
const GATEWAY_WS_URL = process.env.GATEWAY_WS_URL ?? "ws://127.0.0.1:7899/ws";
const DEBUG = process.argv.includes("--debug") || process.env.DEBUG === "1";

function dlog(...args: unknown[]) {
  if (DEBUG) console.error("[broker:debug]", ...args);
}

// --- In-memory store ---

interface Store {
  peers: Record<string, Peer>;
  messages: Message[];
  nextMsgId: number;
  pendingKeys: Record<string, { npub: string; nsec: string }>;
}

const store: Store = { peers: {}, messages: [], nextMsgId: 1, pendingKeys: {} };

// --- Gateway WebSocket connection ---

let gatewayWs: WebSocket | null = null;
let gatewayConnected = false;

function connectGateway(): void {
  if (gatewayWs) {
    gatewayWs.close();
  }

  console.error(`[broker] Connecting to Gateway at ${GATEWAY_WS_URL}...`);
  gatewayWs = new WebSocket(GATEWAY_WS_URL);

  gatewayWs.onopen = () => {
    gatewayConnected = true;
    console.error("[broker] Connected to Gateway");
  };

  gatewayWs.onmessage = (event) => {
    try {
      const data = JSON.parse(event.data as string);
      dlog("Gateway WS msg type:", data.type);

      if (data.type === "dm_received") {
        const fromNpub = data.from_npub || "";
        const toNpub = data.to_npub || "";
        const content = data.content || "";
        const now = new Date().toISOString();

        dlog(`dm_received: from=${fromNpub.slice(0, 20)}..., to=${toNpub.slice(0, 20)}..., content="${content.slice(0, 20)}"`);

        // Route DM only to the specific recipient peer
        let deliveredCount = 0;
        for (const [peerId, peer] of Object.entries(store.peers)) {
          if (peer.npub === toNpub) {
            store.messages.push({
              id: store.nextMsgId++,
              from_id: "gateway",
              from_npub: fromNpub,
              to_id: peerId,
              text: content,
              sent_at: now,
              delivered: 0,
            });
            deliveredCount++;
            console.error(`[broker] Routed DM from ${fromNpub.slice(0, 20)}... to peer ${peerId}`);
          }
        }
        if (deliveredCount === 0) {
          console.error(`[broker] No peer registered for npub ${toNpub.slice(0, 20)}..., DM dropped`);
        }
      }

      // key_assigned: 收到Gateway分配的密钥
      if (data.type === "key_assigned") {
        const { cwd, npub, nsec } = data;
        dlog(`key_assigned: cwd=${cwd}, npub=${npub.slice(0, 20)}...`);

        // 存储密钥到pendingKeys（以cwd为键）
        store.pendingKeys[cwd] = { npub, nsec };

        // 找到cwd对应的peer
        for (const [peerId, peer] of Object.entries(store.peers)) {
          if (peer.cwd === cwd) {
            // 更新peer的npub
            peer.npub = npub;

            // 注册npub到Gateway
            sendToGateway({
              type: "register",
              npub: npub,
              cwd: cwd,
            }).catch(() => {});

            console.error(`[broker] Key assigned for peer ${peerId}: ${npub.slice(0, 20)}...`);
            break;
          }
        }
      }
    } catch (e) {
      console.error("[broker] Failed to parse gateway message:", e);
    }
  };

  gatewayWs.onclose = () => {
    gatewayConnected = false;
    console.error("[broker] Disconnected from Gateway, reconnecting in 2s...");
    setTimeout(connectGateway, 2000);
  };

  gatewayWs.onerror = (e) => {
    console.error("[broker] Gateway WebSocket error:", e);
  };
}

function sendToGateway(message: object): Promise<void> {
  if (!gatewayWs || !gatewayConnected) {
    return Promise.reject(new Error("Gateway not connected"));
  }
  gatewayWs.send(JSON.stringify(message));
  return Promise.resolve();
}

// Start gateway connection
connectGateway();

// --- Generate peer ID ---

function generateId(): string {
  const chars = "abcdefghijklmnopqrstuvwxyz0123456789";
  let id = "";
  for (let i = 0; i < 8; i++) {
    id += chars[Math.floor(Math.random() * chars.length)];
  }
  return id;
}

// --- Request handlers ---

function handleRegister(body: RegisterRequest): RegisterResponse {
  const now = new Date().toISOString();

  // 使用cwd和pid查找已有peer
  let existingId: string | null = null;
  for (const [id, peer] of Object.entries(store.peers)) {
    if (peer.cwd === body.cwd && peer.pid === body.pid) {
      existingId = id;
      break;
    }
  }

  if (existingId) {
    // 更新现有peer
    if (body.npub) {
      store.peers[existingId].npub = body.npub;
    }
    store.peers[existingId].last_seen = now;
    return { id: existingId, npub: store.peers[existingId].npub };
  }

  // 创建新peer
  const id = generateId();
  store.peers[id] = {
    id,
    npub: body.npub || "",
    pid: body.pid,
    cwd: body.cwd,
    registered_at: now,
    last_seen: now,
  };

  // 如果没有npub，向Gateway请求密钥
  if (!body.npub) {
    sendToGateway({
      type: "request_key",
      cwd: body.cwd,
    }).catch(() => {});
  } else {
    // 注册npub到Gateway
    sendToGateway({
      type: "register",
      npub: body.npub,
      cwd: body.cwd,
    }).catch(() => {});
  }

  return { id };
}

function handleHeartbeat(body: HeartbeatRequest): void {
  const peer = store.peers[body.id];
  if (peer) {
    peer.last_seen = new Date().toISOString();
  }
}

function handleListPeers(body: ListPeersRequest): Peer[] {
  let peers = Object.values(store.peers);

  if (body.exclude_id) {
    peers = peers.filter((p) => p.id !== body.exclude_id);
  }

  // Verify process still alive
  peers = peers.filter((p) => {
    try {
      process.kill(p.pid, 0);
      return true;
    } catch {
      delete store.peers[p.id];
      return false;
    }
  });

  return peers;
}

async function handleSendMessage(body: SendMessageRequest): Promise<{ ok: boolean; error?: string }> {
  dlog(`handleSendMessage: to=${body.to_npub.slice(0, 20)}, text="${body.text.slice(0, 20)}"`);

  // Forward to gateway for relay publish
  try {
    await sendToGateway({
      type: "send_dm",
      to_npub: body.to_npub,
      content: body.text,
      from_npub: body.from_npub,
    });
    dlog("send_dm forwarded to gateway");
    return { ok: true };
  } catch (e) {
    return { ok: false, error: e instanceof Error ? e.message : String(e) };
  }
}

function handlePollMessages(body: PollMessagesRequest): PollMessagesResponse {
  dlog(`handlePollMessages: peer_id=${body.id}, ${store.messages.length} total msgs`);

  const undelivered = store.messages.filter(
    (m) => m.to_id === body.id && m.delivered === 0
  );

  dlog(`Undelivered for ${body.id}: ${undelivered.length}`);

  for (const msg of undelivered) {
    msg.delivered = 1;
  }

  return { messages: undelivered };
}

function handleUnregister(body: { id: string }): void {
  if (store.peers[body.id]) {
    delete store.peers[body.id];
  }
}

function handleGetKey(body: { id: string }): { npub?: string; nsec?: string } | null {
  const peer = store.peers[body.id];
  if (!peer) return null;

  const key = store.pendingKeys[peer.cwd];
  if (key) {
    // 清除pending key
    delete store.pendingKeys[peer.cwd];
    return key;
  }
  return null;
}

// --- HTTP Server ---

Bun.serve({
  port: PORT,
  hostname: "127.0.0.1",
  async fetch(req) {
    const url = new URL(req.url);
    const path = url.pathname;

    if (req.method !== "POST") {
      if (path === "/health") {
        return Response.json({
          status: "ok",
          peers: Object.keys(store.peers).length,
          gateway: gatewayConnected ? "connected" : "disconnected",
        });
      }
      return new Response("Nostr Channel broker (in-memory)", { status: 200 });
    }

    try {
      const body = await req.json();
      dlog(`HTTP ${req.method} ${path}`);

      switch (path) {
        case "/register":
          return Response.json(handleRegister(body as RegisterRequest));
        case "/heartbeat":
          handleHeartbeat(body as HeartbeatRequest);
          return Response.json({ ok: true });
        case "/list-peers":
          return Response.json(handleListPeers(body as ListPeersRequest));
        case "/send-message":
          return Response.json(await handleSendMessage(body as SendMessageRequest));
        case "/poll-messages":
          return Response.json(handlePollMessages(body as PollMessagesRequest));
        case "/unregister":
          handleUnregister(body as { id: string });
          return Response.json({ ok: true });
        case "/get-key":
          return Response.json(handleGetKey(body as { id: string }));
        default:
          return Response.json({ error: "not found" }, { status: 404 });
      }
    } catch (e) {
      return Response.json({ error: e instanceof Error ? e.message : String(e) }, { status: 500 });
    }
  },
});

console.error(`[broker] listening on 127.0.0.1:${PORT} (in-memory, no persistence)`);
