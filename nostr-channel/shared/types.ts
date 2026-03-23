// Shared types for Nostr Channel broker API

export interface Peer {
  id: string;
  npub: string;
  pid: number;
  cwd: string;
  registered_at: string;
  last_seen: string;
}

export interface Message {
  id: number;
  from_id: string;
  from_npub: string;
  to_id: string;
  text: string;
  sent_at: string;
  delivered: number;
}

// Broker API types
export interface RegisterRequest {
  pid: number;
  cwd: string;
  npub?: string;
}

export interface RegisterResponse {
  id: string;
}

export interface HeartbeatRequest {
  id: string;
}

export interface ListPeersRequest {
  scope: "machine" | "directory" | "repo";
  cwd: string;
  exclude_id?: string;
}

export interface SendMessageRequest {
  from_id: string;
  from_npub: string;
  to_npub: string;
  text: string;
  from_cwd: string;  // sender's workspace path for reading key.json
}

export interface PollMessagesRequest {
  id: string;
}

export interface PollMessagesResponse {
  messages: Message[];
}

// Gateway ↔ Broker WebSocket消息类型
export interface GatewayMessage {
  type: "request_key" | "key_assigned" | "register" | "send_dm" | "dm_received" | "dm_sent";
}

export interface GatewayRequestKey extends GatewayMessage {
  type: "request_key";
  cwd: string;
}

export interface GatewayKeyAssigned extends GatewayMessage {
  type: "key_assigned";
  cwd: string;
  npub: string;
  nsec: string;
}

export interface GatewayRegister extends GatewayMessage {
  type: "register";
  npub: string;
  cwd: string;
}

export interface GatewaySendDm extends GatewayMessage {
  type: "send_dm";
  to_npub: string;
  content: string;
  from_npub: string;
}

export interface GatewayDmReceived extends GatewayMessage {
  type: "dm_received";
  from_npub: string;
  to_npub: string;
  content: string;
}

export interface GatewayDmSent extends GatewayMessage {
  type: "dm_sent";
  ok: boolean;
}
