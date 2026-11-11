# Spark 0.1

Rust service + Rust/WASM UI (Leptos 0.8 CSR, web-sys media), thin Electron shell for the same Chromium WebRTC on Linux and Windows. No cloud/STUN/TURN, no accounts. Every client has its own loopback service; one starts a LAN host. Rooms/chat/history/files run through the host; encrypted WebRTC media is peer-to-peer. A trusted LAN is assumed for host HTTP/WebSocket chat and file transport; room code controls joining, it is not end-to-end encryption.

## Ownership

- `spark-protocol`: shared JSON contract, owned by backend agent.
- `spark-service`: Tokio/Axum native executable, SQLite, LAN discovery, network proxy, streaming files, owned by backend agent.
- `spark-ui`: Leptos Rust/WASM UI, owned by UI agent except `media.rs` owned by media agent.
- Electron shell, builds, packaging and cross-client end-to-end verification owned by root.

## Frozen JSON contract

All enums use `#[serde(tag="kind", rename_all="snake_case")]`. IDs are strings; timestamps Unix milliseconds i64.

`Profile { id, name }`; `Channel { id, name, kind }` with `kind` string `text` or `voice`; `Peer { id, name, voice_channel: Option<String>, muted: bool, camera: bool, screen: bool }`.

`FileInfo { id, name, size: u64, sha256, mime }`.
`ChatMessage { id, channel_id, sender_id, sender_name, text, created_at: i64, file: Option<FileInfo> }`.
`Discovery { id, name, address, peers: usize }`.
`ConnectionInfo { server_name, address, hosting: bool }`.
`Bootstrap { version, token, profile }`.

Client commands:
- `chat { channel_id, text }`
- `create_channel { name, channel_kind }`
- `join_voice { channel_id }`, `leave_voice`
- `media_state { muted, camera, screen }`
- `signal { to, payload: JSON }`
- `history { channel_id, before: Option<i64> }`
- `invite { to, channel_id, video: bool }` (invite someone to an existing voice room; recipient chooses accept/reject locally).

Server events:
- `welcome { self_id, server_name, channels: Vec<Channel>, peers: Vec<Peer>, messages: Vec<ChatMessage> }`
- `peer_joined { peer }`, `peer_left { peer_id }`, `presence { peer }`
- `channel_created { channel }`, `chat { message }`
- `signal { from, payload: JSON }`
- `history { channel_id, messages }`
- `invite { from: Peer, channel_id, video }`
- `error { code, message }`
- `disconnected { reason }` (gateway generated).

## Local service HTTP

Default service: `spark-service --data-dir PATH --ui-dir assets --gateway-port 0`; optionally `--headless`. stdout one JSON ready line `{ "kind":"ready", "url":"http://127.0.0.1:PORT", "token":"LOCAL_NONCE" }`, diagnostic logs stderr only. Ctrl+C clean shutdown. Linux/Windows platform-neutral Rust.

GET `/api/bootstrap` -> Bootstrap (same-origin only). All other local API requests require `x-spark-token`; events/file-link may use `?token=` because browser WebSocket cannot set headers. Local bind strictly127.0.0.1, validate Host and Origin; no permissive CORS. Assets/index served at `/` and static child paths. API max JSON size64KiB, clear typed JSON errors `{ error: string }`.

- PUT `/api/profile` `{name}` -> Profile.
- GET `/api/discovery` -> `Vec<Discovery>` (UDP23994 LAN discovery; include manual IP join regardless multicast firewall).
- POST `/api/host` `{name, code, port?:u16}` -> ConnectionInfo, bind LAN TCP39393 by default, persist host SQLite/channels/files under data dir. Reject empty/short code (<6 chars). Defaults text `general`/`Общий`, voice `lounge`/`Голосовая`.
- POST `/api/join` `{address, code}` -> ConnectionInfo, validate host credentials and save active remote config. Addresses `IP:port` or hostname:port, plain host only (no arbitrary paths/credentials).
- POST `/api/disconnect` -> `{ok:true}`; shut down hosted space if applicable.
- GET `/api/events?token=...` upgrades socket and makes the authenticated remote host connection using Profile and active host config; bidirectional ClientCommand/ServerEvent proxy. Remote host Welcome is always fresh when reconnecting.
- POST `/api/upload?channel_id=...&name=percentencoded` raw File body, content-type from browser -> ChatMessage. Stream local->host->temporary file, bound size2GiB, verify SHA256, atomic finalization, publish completed file chat event once. Never buffer full file in Rust or WASM. No auto download/open remote files.
- GET `/api/files/:id?token=...` streams proxied download, Content-Disposition attachment.
- POST `/api/save` `{file_id,path}` -> `{path,sha256,bytes}` for native shell Save dialog: streamed download, expected SHA256/size verification before atomic completion, refuse overwrite without native dialog approval. Intended shell-only action with local nonce.

LAN API routes are backend implementation details. Shared room code authorizes connection/uploads/downloads; validate active sender identities, channel access and signaling same voice room; prevent arbitrary path names/oversized input. Persistent host history last200 messages on Welcome and paged History. No script/HTML rendering from chat.

## Media Rust module contract

`MediaManager` uses Rc internals (Clone), shared Rust-only public functions, no JS app implementation. UI agent and media agent finalize exact callbacks together. Requirements: no STUN/TURN (`iceServers:[]`), one connection per voice-room peer, polite perfect negotiation or deterministic initiator with queued ICE, audio/camera/screen tracks separately negotiated, mic mute, deafen, camera toggle, screen stop on browser-ended event, end-call stops all tracks/closes peers. Screen and camera can coexist. Remote stream keyed by peer and stream kind. Bounded errors exposed in UI; no silent dead buttons. Incoming invites ask user before starting mic/camera.

Electron preload `window.sparkDesktop`:
- `sources()` -> Promise array `{id,name,thumbnail}` for a source picker.
- `selectSource(id)` -> Promise<void>, then Rust media invokes getDisplayMedia.
- `saveFile(fileId,fileName)` -> Promise `{path,sha256,bytes}` using main-process dialog + local Rust `/api/save`.
- `version()` -> version string (optional).
For ordinary browser test fallback, screen capture uses browser picker and files download via local API anchor.

## UI and QA

Russian UI, polished dark Discord-like three-column desktop layout, amber/orange Spark accent, responsive at1000x700. Host/join onboarding with display name, room name/code, discovered hosts and manual IP. Channels/text, message composer Enter/Shift+Enter, member list, file picker and progress/errors. Voice panel with mute/deafen/camera/screen/end controls, local/remote video tiles and actual connection state. Source selection, invites, clear disconnect and rejoin. Settings name + local address details; no fake people/chats or placeholder buttons in production.

E2E uses two independent Rust services/data dirs and real browser/Electron contexts over host network sockets: chat both directions/persistence, file bytes+SHA256, voice RTP, camera video frames, screen capture through Xvfb actual display (synthetic test desktop), mute/stop/rejoin/cleanup. Fake input devices only explicit unpackaged test flags; never capture user's live desktop during automated testing. Native unit/integration tests cover bad codes, stale peer cleanup, validation/path traversal, atomic file commit and transfer hash. Linux builds locally, Windows CI builds/tests/installers. Release unsigned installers/assets in a PRIVATE GitHub repo; no public publishing of user projects.
