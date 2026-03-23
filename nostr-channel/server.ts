#!/usr/bin/env bun
/**
 * Nostr Channel MCP server
 *
 * Spawned by Claude Code as a stdio MCP server (one per instance).
 * Connects to the shared broker daemon for peer discovery and messaging.
 * Declares claude/channel capability to push inbound messages immediately.
 *
 * Usage:
 *   claude --dangerously-load-development-channels server:nostr
 *
 * With .mcp.json:
 *   { "nostr": { "command": "bun", "args": ["./server.ts"] } }
 */

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  ListToolsRequestSchema,
  CallToolRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import type {
  Peer,
  RegisterResponse,
  PollMessagesResponse,
  Message,
} from "./shared/types.ts";

// --- Configuration ---

const BROKER_PORT = parseInt(process.env.NOSTR_BROKER_PORT ?? "7900", 10);
const BROKER_URL = `http://127.0.0.1:${BROKER_PORT}`;
const POLL_INTERVAL_MS = 1000;
const HEARTBEAT_INTERVAL_MS = 15_000;
const BROKER_SCRIPT = new URL("./broker.ts", import.meta.url).pathname;

// --- Broker communication ---

async function brokerFetch<T>(path: string, body: unknown): Promise<T> {
  const res = await fetch(`${BROKER_URL}${path}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    const err = await res.text();
    throw new Error(`Broker error (${path}): ${res.status} ${err}`);
  }
  return res.json() as Promise<T>;
}

async function isBrokerAlive(): Promise<boolean> {
  try {
    const res = await fetch(`${BROKER_URL}/health`, { signal: AbortSignal.timeout(2000) });
    return res.ok;
  } catch {
    return false;
  }
}

async function ensureBroker(): Promise<void> {
  if (await isBrokerAlive()) {
    log("Broker already running");
    return;
  }

  log("Starting broker daemon...");
  const proc = Bun.spawn(["bun", BROKER_SCRIPT], {
    stdio: ["ignore", "ignore", "inherit"],
  });

  proc.unref();

  // Wait for it to come up
  for (let i = 0; i < 30; i++) {
    await new Promise((r) => setTimeout(r, 200));
    if (await isBrokerAlive()) {
      log("Broker started");
      return;
    }
  }
  throw new Error("Failed to start broker daemon after 6 seconds");
}

// --- Utility ---

function log(msg: string) {
  console.error(`[nostr-channel] ${msg}`);
}

// --- State ---

let myId: string | null = null;
let myNpub: string = "";
let myCwd = process.cwd();

// --- MCP Server ---

const mcp = new Server(
  { name: "nostr-channel", version: "1.0.0" },
  {
    capabilities: {
      experimental: { "claude/channel": {} },
      tools: {},
    },
    instructions: `You are connected to the Nostr peer-to-peer network. Other Claude Code instances can send you messages via their npub identifiers.

IMPORTANT: When you receive a <channel source="nostr" ...> message, RESPOND IMMEDIATELY. Do not wait until your current task is finished. Pause what you are doing and reply using send_nostr_message.

Read the from_npub attribute to understand who sent the message. Reply by calling send_nostr_message with their npub.

Available tools:
- list_peers: Discover other Claude Code instances on this machine
- send_nostr_message: Send a message to another instance by npub
- check_messages: Manually check for new messages

Your identity (npub) is shown at startup. Share it with peers so they can message you.`,
  }
);

// --- Tool definitions ---

const TOOLS = [
  {
    name: "list_peers",
    description:
      "List other Claude Code instances running on this machine. Returns their ID, working directory, and npub.",
    inputSchema: {
      type: "object" as const,
      properties: {
        scope: {
          type: "string" as const,
          enum: ["machine", "directory", "repo"],
          description:
            'Scope of peer discovery. "machine" = all instances on this computer.',
        },
      },
      required: ["scope"],
    },
  },
  {
    name: "send_nostr_message",
    description:
      "Send a message to another Claude Code instance by their npub. The message will be delivered via Nostr.",
    inputSchema: {
      type: "object" as const,
      properties: {
        to_npub: {
          type: "string" as const,
          description: "The npub of the recipient",
        },
        content: {
          type: "string" as const,
          description: "The message to send",
        },
      },
      required: ["to_npub", "content"],
    },
  },
  {
    name: "check_messages",
    description:
      "Manually check for new messages from other Claude Code instances.",
    inputSchema: {
      type: "object" as const,
      properties: {},
    },
  },
];

// --- Tool handlers ---

mcp.setRequestHandler(ListToolsRequestSchema, async () => ({
  tools: TOOLS,
}));

mcp.setRequestHandler(CallToolRequestSchema, async (req) => {
  const { name, arguments: args } = req.params;

  switch (name) {
    case "list_peers": {
      try {
        const peers = await brokerFetch<Peer[]>("/list-peers", {
          scope: (args as { scope: string }).scope || "machine",
          cwd: myCwd,
          exclude_id: myId,
        });

        if (peers.length === 0) {
          return {
            content: [
              {
                type: "text" as const,
                text: "No other Claude Code instances found.",
              },
            ],
          };
        }

        const lines = peers.map((p) => {
          const parts = [
            `ID: ${p.id}`,
            `npub: ${p.npub}`,
            `CWD: ${p.cwd}`,
            `Last seen: ${p.last_seen}`,
          ];
          return parts.join("\n  ");
        });

        return {
          content: [
            {
              type: "text" as const,
              text: `Found ${peers.length} peer(s):\n\n${lines.join("\n\n")}`,
            },
          ],
        };
      } catch (e) {
        return {
          content: [
            {
              type: "text" as const,
              text: `Error listing peers: ${e instanceof Error ? e.message : String(e)}`,
            },
          ],
          isError: true,
        };
      }
    }

    case "send_nostr_message": {
      const { to_npub, content } = args as { to_npub: string; content: string };
      if (!myId) {
        return {
          content: [{ type: "text" as const, text: "Not registered with broker yet" }],
          isError: true,
        };
      }
      try {
        const result = await brokerFetch<{ ok: boolean; error?: string }>("/send-message", {
          from_id: myId,
          from_npub: myNpub,
          to_npub: to_npub,
          text: content,
          from_cwd: myCwd,
        });
        if (!result.ok) {
          return {
            content: [{ type: "text" as const, text: `Failed to send: ${result.error}` }],
            isError: true,
          };
        }
        return {
          content: [{ type: "text" as const, text: `Message sent to ${to_npub.slice(0, 20)}...` }],
        };
      } catch (e) {
        return {
          content: [
            {
              type: "text" as const,
              text: `Error sending message: ${e instanceof Error ? e.message : String(e)}`,
            },
          ],
          isError: true,
        };
      }
    }

    case "check_messages": {
      log(`[check_messages] myId=${myId}`);
      if (!myId) {
        return {
          content: [{ type: "text" as const, text: "Not registered with broker yet" }],
          isError: true,
        };
      }
      try {
        const result = await brokerFetch<PollMessagesResponse>("/poll-messages", { id: myId });
        if (result.messages.length === 0) {
          return {
            content: [{ type: "text" as const, text: "No new messages." }],
          };
        }
        const lines = result.messages.map(
          (m) => `From ${m.from_npub.slice(0, 20)}... (${m.sent_at}):\n${m.text}`
        );
        return {
          content: [
            {
              type: "text" as const,
              text: `${result.messages.length} new message(s):\n\n${lines.join("\n\n---\n\n")}`,
            },
          ],
        };
      } catch (e) {
        return {
          content: [
            {
              type: "text" as const,
              text: `Error checking messages: ${e instanceof Error ? e.message : String(e)}`,
            },
          ],
          isError: true,
        };
      }
    }

    default:
      throw new Error(`Unknown tool: ${name}`);
  }
});

// --- Polling loop for inbound messages ---

async function pollAndPushMessages() {
  if (!myId) return;

  try {
    const result = await brokerFetch<PollMessagesResponse>("/poll-messages", { id: myId });
    log(`[poll] myId=${myId}, got ${result.messages.length} messages`);

    for (const msg of result.messages) {
      log(`[poll] Pushing: from=${msg.from_npub.slice(0, 20)}, text="${msg.text.slice(0, 30)}"`);
      // Push as channel notification — this is what makes it immediate
      await mcp.notification({
        method: "notifications/claude/channel",
        params: {
          content: msg.text,
          meta: {
            from_npub: msg.from_npub,
            from_id: msg.from_id,
            sent_at: msg.sent_at,
          },
        },
      });

      log(`Pushed message from ${msg.from_npub.slice(0, 20)}...: ${msg.text.slice(0, 50)}...`);
    }
  } catch (e) {
    // Broker might be down temporarily, don't crash
    log(`Poll error: ${e instanceof Error ? e.message : String(e)}`);
  }
}

// --- Poll for key assignment from broker ---

const KEY_FILE = () => `${myCwd}/key.json`;

async function pollForKey(): Promise<void> {
  if (!myId || myNpub) return;

  try {
    // Call /get-key to check for pending key assignment
    const res = await brokerFetch<{ npub?: string; nsec?: string }>("/get-key", { id: myId });
    if (res && res.npub && res.nsec) {
      // Save key to file
      const keyData = { npub: res.npub, nsec: res.nsec };
      await Bun.write(KEY_FILE(), JSON.stringify(keyData, null, 2));

      myNpub = res.npub;
      log(`Key assigned: ${myNpub.slice(0, 20)}...`);

      // Re-register with npub
      await brokerFetch("/register", {
        pid: process.pid,
        cwd: myCwd,
        npub: myNpub,
      });
    }
  } catch {
    // Ignore errors
  }
}

// --- Startup ---

// Parse command line arguments
const args = process.argv.slice(2);
let customCwd: string | null = null;
for (let i = 0; i < args.length; i++) {
  if (args[i] === "--cwd" && i + 1 < args.length) {
    customCwd = args[i + 1];
    break;
  }
}

async function main() {
  // 1. Ensure broker is running
  await ensureBroker();

  // 2. Gather context
  myCwd = customCwd ?? process.cwd();
  log(`Working directory: ${myCwd}`);

  // 3. Try to load key from key.json in cwd directory
  let npub = "";
  let nsec = "";

  try {
    const keyFile = Bun.file(KEY_FILE());
    if (await keyFile.exists()) {
      const keyData = JSON.parse(await keyFile.text());
      npub = keyData.npub || "";
      nsec = keyData.nsec || "";
      log(`Loaded key from ${KEY_FILE()}: ${npub.slice(0, 20)}...`);
    }
  } catch {
    // No existing key found
  }

  // 4. Register with broker (with or without npub)
  let reg: RegisterResponse;
  if (npub) {
    // Has key, register directly
    reg = await brokerFetch<RegisterResponse>("/register", {
      pid: process.pid,
      cwd: myCwd,
      npub: npub,
    });
    myId = reg.id;
    myNpub = npub;
  } else {
    // No key, register first and wait for key assignment
    reg = await brokerFetch<RegisterResponse>("/register", {
      pid: process.pid,
      cwd: myCwd,
    });
    myId = reg.id;
    log(`Registered, waiting for key assignment... id=${myId}`);

    // Poll for key assignment via /get-key endpoint
    for (let i = 0; i < 60; i++) {
      await new Promise((r) => setTimeout(r, 500));
      const keyRes = await brokerFetch<{ npub?: string; nsec?: string }>("/get-key", { id: myId });
      if (keyRes && keyRes.npub && keyRes.nsec) {
        // Save key to file
        await Bun.write(KEY_FILE(), JSON.stringify({ npub: keyRes.npub, nsec: keyRes.nsec }, null, 2));
        myNpub = keyRes.npub;
        log(`Key assigned: ${myNpub.slice(0, 20)}...`);
        break;
      }
    }

    if (!myNpub) {
      throw new Error("Failed to get key from Gateway");
    }
  }

  log(`My npub: ${myNpub.slice(0, 20)}...`);
  log(`CWD: ${myCwd}`);

  // 5. Connect MCP over stdio
  await mcp.connect(new StdioServerTransport());
  log("MCP connected");

  // 6. Start polling for inbound messages and key assignments
  const pollTimer = setInterval(async () => {
    await pollAndPushMessages();
    await pollForKey();
  }, POLL_INTERVAL_MS);

  // 7. Start heartbeat
  const heartbeatTimer = setInterval(async () => {
    if (myId) {
      try {
        await brokerFetch("/heartbeat", { id: myId });
      } catch {
        // Non-critical
      }
    }
  }, HEARTBEAT_INTERVAL_MS);

  // 8. Clean up on exit
  const cleanup = async () => {
    clearInterval(pollTimer);
    clearInterval(heartbeatTimer);
    if (myId) {
      try {
        await brokerFetch("/unregister", { id: myId });
        log("Unregistered from broker");
      } catch {
        // Best effort
      }
    }
    process.exit(0);
  };

  process.on("SIGINT", cleanup);
  process.on("SIGTERM", cleanup);
}

main().catch((e) => {
  log(`Fatal: ${e instanceof Error ? e.message : String(e)}`);
  process.exit(1);
});
