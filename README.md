# Wyvern

A lightweight Discord client in Rust + egui. Token and a local password, servers
and channels, history that pages in as you scroll up, images and GIFs, and a
debug log in `/tmp`. Everything runs on your own machine; there is no server of
its own.

Licensed under AGPL-3.0 (see `LICENSE`).

## Build and run

```bash
cargo build --release
cp -f target/release/wyvern ./wyvern   # a ready binary is committed in the repo
./wyvern
```

The token goes into the login field. If "Remember this account" is ticked, the
token is encrypted with a password in `~/.wyvern_accounts.json` (AES-256-GCM,
key derived via PBKDF2). The path can be overridden with `WYVERN_ACCOUNTS_PATH`
— test and sandbox runs then leave the real vault alone.

## Debug log

Each frame writes panel geometry to `/tmp/wyvern_layout.log`; on overflow the
file is rotated to `.1`. Gateway events and image downloads go there too, so the
log shows what the client sees. A message dump into `/tmp` happens only with
`WYVERN_DUMP=1`.

## Layout

| File | Contents |
|---|---|
| `src/main.rs` | window startup, assets, panics to the log |
| `src/app.rs` | application state, channels, messages, login |
| `src/gateway.rs` | WebSocket gateway and history/message parsing |
| `src/media.rs` | downloading, decoding and caching images and GIFs |
| `src/models.rs` | data structures, byte-budget cache |
| `src/ui/*` | screens: login, sidebar, chat, attachments, input, overlays, settings |
| `src/crypto.rs` | account vault encryption |
| `src/ss.rs` | native Shadowsocks (SIP002 parsing, AEAD ciphers, stream) |
| `src/vless.rs` | native VLESS (TLS + WebSocket/TCP, lazy response stripping) |
| `src/proxybridge.rs` | loopback SOCKS5 bridge that terminates `ss://` / `vless://` |
| `src/memcheck.rs` | memory and allocation measurement (test with `--ignored`) |

## Tests and measurements

```bash
cargo test --release                                        # 263 tests (260 + 3 ignored)
cargo test --release -- --ignored --nocapture memcheck       # memory measurement
WYVERN_SS_URL='ss://…' cargo test --release -- --ignored live_shadowsocks_proxy_smoke
WYVERN_VLESS_URL='vless://…' cargo test --release -- --ignored live_vless_proxy_smoke
```

---

# Changes

## 0.9.0 — 2026-10-19

- **Proxy form**: the Network tab gets a proper form — type (HTTP, SOCKS5,
  SOCKS5 remote-DNS, Shadowsocks), host, port, username and password — applied
  live as you type. SOCKS5 now negotiates RFC 1929 username/password auth.
- **Native Shadowsocks**: `ss://` proxies are terminated in-process. The client
  parses SIP002 and legacy links (or an imported `ss://` link pasted into the
  form), runs an `aes-128/256-gcm` / `chacha20-ietf-poly1305` AEAD client, and
  exposes a loopback SOCKS5 bridge so API, media and gateway traffic all flow
  through the server. No external core binary is required.
- **Native VLESS**: `vless://` links (TLS + WebSocket or TCP; Reality/XTLS are
  rejected with a clear message) are terminated the same way, including a pasted
  link imported straight into the form. Targets are resolved locally so servers
  with broken outbound DNS still work, and the response header is stripped lazily
  during pumping.

## 0.8.0 — 2026-10-18

- **Lightbox and quick switch**: clicking an image opens a full-size viewer
  (Esc/Close); it loads a separate copy up to 2048 px and drops it 30 s after
  closing (GIFs reuse the chat copy). Ctrl+K lists the current server and DMs;
  typing a query widens the search and tags channels with their server.
- **Settings and state**: a settings window (theme, UI and font scale, hide
  media, notifications, keep deleted, reset scale, clear drafts, reset window
  size). Theme, window size, last channel and per-channel drafts persist in
  `~/.wyvern_settings.json`; kept-deleted messages in `~/.wyvern_deleted.json`.
- **Typing indicator**: "Alice is typing…" / "Several people are typing…";
  it lapses after eight seconds (Discord sends no stop event); DM names are
  resolved from loaded messages; our own typing is ignored.
- **Message grouping**: consecutive messages from one author within seven
  minutes collapse into one block (name/avatar once, the rest indented). A
  reply, a different author, or a longer pause starts a new block.
- **Toasts and confirmations**: corner toasts for send/edit/delete errors,
  reconnect/disconnect, account switch and login rejection fade after four
  seconds. Deleting a message, removing an account and clearing drafts ask first
  via a modal (Esc or backdrop cancels).
- **Unread and mentions**: channels accumulate unread badges; a ping turns the
  row bold with a red `@n` count; servers aggregate over their channels. Opening
  a channel clears its badge and sends a best-effort read receipt. Badges are
  restored from Discord's `read_state` on login: mention counts exactly, other
  channels as a `•` dot when newer than the last read id.
- **Channel categories and announcements**: guild channels are grouped under
  collapsible category headers (uncategorized channels at the top, children by
  position); announcement channels show a 📢 marker. Voice channels are listed
  greyed out with 🔊 but stay inert, since the client doesn't connect to voice.
- **DM ordering and proxy**: the DM list is ordered by the newest message
  (most recent first) instead of alphabetically. A proxy URL (http, https or
  socks5) can be set in Settings; it applies to API/media requests immediately
  and to the gateway on its next connection.
- **Settings window and startup**: Settings is a full-client overlay with
  vertical tabs (Theme, Interface, Media, History, Network, Advanced). The app
  now opens on the DM/home view and selects the most recent conversation,
  instead of an arbitrary server; the remembered channel is only restored when
  it is a DM.

## 0.7.0 — 2026-10-18

- **Replies and message actions**: a context menu (Reply, Edit, Delete, Copy
  text); reply/edit bars in the composer; replies show a quote line parsed from
  both history and live messages; double-click replies, Esc cancels reply/edit.
- **Markdown**: bold, italic, underline, strikethrough, inline/fenced code,
  blockquotes, spoilers and clickable links (the height estimate accounts for
  the extra lines).
- **Composer and files**: multiline input (Enter sends, Shift+Enter newline),
  the field grows with its real line count up to `COMPOSER_MAX_H`; attachments
  via drag-and-drop, a file picker, pasted paths and Ctrl+V of an image (the key
  *release* is the gesture, since egui-winit swallows the press).
- **English UI**: all user-facing strings.

## 0.5.2 — 2026-10-02

Closed parts 4–5 of `BUGS.txt` (behaviour, reliability, per-frame cost) and what
surfaced on the live client after 0.5.1.

- Long GIFs play to the end (the frame count is read from the file; the limit is
  32 MB per GIF); a visible image is never evicted (no flicker); finished
  downloads no longer pile up in `pending_images`.
- The `n/2000` counter shows only when there is something to count; overlong
  text is not sent and the field is not cleared.
- Debug commands moved under `/debug ` ("/quit" and "/add" are plain text now).
- Lists: friends load after a 429; avatars/icons aren't re-requested; switching
  accounts forgets the previous one's images and failures.
- The gateway drains its queue in batches (200 events per frame); idle sleeps
  instead of running 20 fps; the client fingerprint matches
  `X-Super-Properties`.
- Fewer clones per frame: panels and the chat draw from their own list.

## 0.5.1 — 2026-09-28

Fixed the breakages 0.5.0 brought to the live client: history no longer
duplicates when scrolling up (pages are stored oldest→newest, `before` from the
first row); live messages no longer double (a repeat by `id` is skipped); the
chat can actually be scrolled (egui pins the bottom only when we move there);
the drawn frame no longer stays put while scrolling (the anchor is taken from
where the chat actually ended up); the chat scrolls to the bottom and keeps
itself there; the height estimate was off by +14% (constants measured on live
egui); images no longer change a message's height by ±180 px (space is reserved
from the size Discord sends).

## 0.5.0 — 2026-09-27

History pages in as you scroll up, and the message list is virtualized (only
what's visible is drawn); chat rendering got five times lighter.

- A channel opens with the last 50 messages, then 50 more per scroll up; once
  the oldest is reached history isn't requested; while the list fits the window
  it isn't requested at all.
- The view holds on a message at the top edge, not on a row number; manual
  scrolling releases the anchor. The per-channel cap was raised 300 → 500.
- Heights are measured on first draw and cached by id; the visible window is
  found by binary search over prefix sums; a 600-px margin avoids grey bands; a
  not-yet-downloaded image reserves its final size.
- Allocations: 112 534 → 32 827 over 10 frames (chat only 95 929 → 18 106), RSS
  84.9 → 80.4 MB.

## 0.4.0 — 2026-09-27

Fixes on top of a big rework: the client stopped eating memory and doing extra
work per frame, and loads images again.

- The 3-concurrent-download cap counted via `fetch_add` and leaked slots, so
  images stopped loading; a slot is now taken with `fetch_update` and released
  on any exit, including a thread panic.
- History loading no longer blocks the gateway queue; pagination takes `before`
  from the oldest message; a late response for a closed channel no longer clears
  "Loading messages…".
- An empty vault is not "wrong password"; the error shows the file path and its
  mtime; tests override the vault path in `App` (no writes to the real one).
- Unclaimed downloads on a channel switch are dropped; a broken or too-large
  image shows "failed to load" instead of an endless spinner.
- Memory: RSS 255 → 85 MB; the image cache is bounded by bytes (48 MB + 32
  items); image sizes come from the displayed size (768/448 px); embeds store
  only `{ image_url, description }`.
- Speed: history is parsed straight into messages (52 216 → 4 517 allocations);
  one parse function for live and history (compared by a test); name/text/time
  by reference; rendering 133 612 → 112 534.
- Development: the version is in the window title; `main.rs` was split into
  modules; tests 19 → 44 plus a measurement; 0 warnings; `/tmp/wyvern_layout.log`
  rotates.

## 0.3.0 — before 2026-09-01

First upload: the client as a single `main.rs` file, token login, servers,
channels, history, images.
