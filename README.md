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
key derived via PBKDF2). The path can be overridden with
`WYVERN_ACCOUNTS_PATH` — test and sandbox runs then leave the real vault alone.

## Debug log

Each frame writes panel geometry to `/tmp/wyvern_layout.log`; on overflow the
file is rotated to `.1` and starts over. Gateway events and image downloads are
written there too, so the log shows what the client sees. A message dump into
`/tmp` happens only with `WYVERN_DUMP=1`.

## Layout

| File | Contents |
|---|---|
| `src/main.rs` | window startup, assets, panics to the log |
| `src/app.rs` | application state, channels, messages, login |
| `src/gateway.rs` | WebSocket gateway and history/message parsing |
| `src/media.rs` | downloading, decoding and caching images and GIFs |
| `src/models.rs` | data structures, byte-budget cache |
| `src/ui/*` | screens: login, sidebar, chat, attachments, input |
| `src/crypto.rs` | account vault encryption |
| `src/memcheck.rs` | memory and allocation measurement (test with `--ignored`) |

The full list of ideas with numbers is in [TODO.md](TODO.md); the release plan
0.6.0 → 1.0.0 is in [ROADMAP.md](ROADMAP.md). A source snapshot of this version
is in [`releases/0.5.2/`](releases/0.5.2/).

## Tests and measurements

```bash
cargo test --release                                        # 221 tests (220 + measurement)
cargo test --release -- --ignored --nocapture memcheck       # memory measurement
```

---

# Changes

## 0.8.0 — 2026-10-18

### Lightbox and quick switch

- Clicking an image opens a full-size viewer; Esc or Close dismisses it. The
  viewer loads a separate high-resolution copy (up to 2048 px) and drops it 30
  seconds after closing, so the chat cache stays small; GIFs reuse the chat
  copy.
- Quick switch (Ctrl+K) lists only the current server and DMs; typing a query
  widens the search across all servers and tags each channel with its server.

### Settings and state

- A settings window: theme (Dark / Cyberpunk / Light), UI and font scale,
  "hide media until clicked", "notify about new messages", "keep deleted
  messages", plus "reset scale", "clear drafts" and "reset window size".
- Theme, window size, the last channel and per-channel drafts persist across
  restarts in `~/.wyvern_settings.json` (override `WYVERN_SETTINGS_PATH`).
- Clicking a hidden (blurred) image reveals it.
- Kept-deleted messages persist across restarts in `~/.wyvern_deleted.json`
  (override `WYVERN_DELETED_PATH`); a history re-fetch no longer loses them.

## 0.7.0 — 2026-10-18

### Replies and message actions

- A context menu on each message: Reply, Edit, Delete, Copy text. Own messages
  can be edited and deleted; a pending local echo hides its actions until the
  server confirms.
- The composer shows a reply bar (author + preview, Cancel) and an
  "Editing message" bar.
- Replies show a quote line (author + one-line preview); `referenced_message` is
  parsed from both history and live messages, and the optimistic echo shows it
  immediately.
- Double-clicking a message replies to it and focuses the composer; Esc cancels a
  pending reply, else a pending edit.

### Markdown

- Messages render Discord's common markdown, inline and across lines: bold,
  italic, underline, strikethrough, inline `code`, fenced code blocks,
  `> ` blockquotes, spoilers (greyed out) and links — both `[label](url)` and
  bare URLs. It is drawn by the client, and the height estimate accounts for the
  extra lines so the layout does not jump.

### Composer and files

- Multiline input: Enter sends, Shift+Enter inserts a newline.
- The field grows with its real line count and is capped (`COMPOSER_MAX_H`),
  scrolling beyond that. It used to be clipped to a single row because it sat in
  a `ui.horizontal`; the layout is now `ui.horizontal_top`.
- Files can be attached by drag-and-drop, a file-picker button, pasted paths
  (including `file://`) and Ctrl+V of an image from the clipboard (`arboard` —
  the key *release* is the gesture, since egui-winit swallows the press).

### English UI

- All user-facing strings are English now.

## 0.5.2 — 2026-10-02

Closed parts 4 and 5 of `BUGS.txt` (B-1…B-25 and T-1…T-23) — behaviour,
reliability and per-frame cost — plus what surfaced on the live client after
0.5.1.

### GIFs and images

- **A long GIF plays to the end.** The hard 16-frame cap is gone: the frame count
  is read from the file structure, and the limit is memory (32 MB per GIF). Long
  ones are downscaled but played through, not cut off in the middle.
- **A visible image or GIF no longer flickers.** The cache remembers what was
  drawn this frame and the previous one and does not evict it; before, a
  neighbouring image that finished loading could push out the visible one, and
  it would be downloaded again.
- **Finished downloads no longer pile up.** The result was collected only while
  drawing its own image, so one scrolled off screen left decoded pixels in
  `pending_images`. On a long chat with GIFs this grew to hundreds of megabytes
  of RSS. Now everything ready goes into the cache every frame, where memory is
  bounded by the budget and LRU.

### Input and chat

- **The `n/2000` counter** is shown only when there is something to count;
  "0/2000" no longer hangs over an empty field. Overlong text is not sent
  (Discord doesn't reject it with code 400 after "sending"), and the field is
  not cleared.
- **Debug commands** moved under `/debug `: plain text "/quit" no longer closes
  the client, and "/add" doesn't create a ghost channel.
- A closed channel's message no longer accumulates or jolts the chat; channel
  edits, deletions and renames apply immediately; duplicate history survives a
  reconnect.

### Reliability and speed

- **Lists**: friends load after a 429, avatars and icons aren't re-requested
  every frame, and switching accounts forgets the previous one's images and
  failures.
- **The gateway** drains its queue in batches (200 events per frame) — a burst
  after a reconnect no longer stalls the frame; when idle the window sleeps
  instead of running 20 frames per second.
- **The client fingerprint** in IDENTIFY matches `X-Super-Properties`: Discord
  no longer sees two different systems for one client.
- Fewer clones per frame: panels and the chat draw from their own list, not a
  copy.

## 0.5.1 — 2026-09-28

Fixed the breakages that 0.5.0 brought to the live client.

- **The chat duplicated when scrolling up**: under every new history request the
  same messages appeared, and scrolling up cycled through the same pages.
  Discord returns a page from newest to oldest, but the app stored it as
  received — so the newest row ended up first, and `before` for the next page
  was taken from it. Discord answered with the same page, it was prepended, and
  so on up to the 500-message cap. Now the page is reversed into storage order:
  the list always goes oldest to newest, `before` is taken from its first row,
  and the cap drops the oldest.
- **A message could double**: the gateway sends a live copy of everything after
  the subscription, and the same row had already landed in a history page. The
  duplicate was drawn twice. Now a repeat by `id` is skipped.
- **The chat could not be scrolled at all.** While the user is at the bottom we
  asked egui to keep the list pinned to the bottom — and immediately overwrote
  the wheel result back to "bottom", i.e. erased the scroll every frame. The
  offset could never move. Now egui holds the bottom only when we are moving
  there ourselves, and "the user scrolled" is detected by the list ending up
  somewhere other than where we asked and not at the real bottom. There is a
  test with a real wheel: it drives the list to the start of history and to the
  last message.
- **When scrolling up the drawn frame stayed put**: the wheel turned but the
  screen showed the same thing. The frame is drawn at the offset we asked for,
  while egui applies the wheel after drawing — so "asked" was always a frame
  behind. The view anchor was taken from "asked" and pinned the next frame in
  the old place. With a fast wheel this wasn't visible (the anchor was reset in
  time, the list jittered every other frame); with a slow one — a trackpad, the
  tail of smoothing — the anchor survived and the list stood still. Now the
  anchor is taken from where the chat actually ended up.
- The chat didn't scroll all the way down and crept away from the last message.
  The list length changes on the fly — heights are refined, images arrive — but
  the bottom was recalculated only on the frame when something arrived. Now,
  while the user is at the bottom, the bottom holds itself; once gone up, the
  view no longer jitters.
- A frame that rewinds to the bottom drew the top of the channel: the list
  opened at the old offset, not the one the message window had been computed
  for. When loading a page this showed as a flash.
- The message height estimate was off by +14% on every message, and the list
  length drifted as you scrolled — that is why the bottom kept sliding. The
  constants come from a measurement on live egui, and a test keeps them from
  diverging again.
- An image no longer changes a message's height by ±180 px when it loads:
  Discord sends the size along with the link, and the space is reserved from it.

## 0.5.0 — 2026-09-27

History in chats pages in as you scroll up, like Discord, and the message list is
drawn virtualized: only what's visible makes it into a frame. Chat rendering got
five times lighter.

### Fixed

- **The chat didn't show new messages.** The scroll anchor was taken before the
  list was rewound to the bottom and pinned the view to the top of the channel.
  Because of this the chat opened at the start of history, a new message arrived
  "below" what was drawn, and history-up loaded repeated pages until it hit the
  cap.

### Paged history

- A channel opens straight away with the last 50 messages, then pages in 50 more
  as you scroll up. Before, the whole history was taken at once, and a long
  channel hung entirely in memory.
- Once the oldest message is reached, the client remembers there's nothing more
  and stops hitting the API for history. While the list fits entirely in the
  window, history isn't requested at all: there's nothing to scroll.
- The view holds on a specific message at the top edge of the window, not on a
  row number. When a page loads above, the message you were looking at stays at
  the same place on screen; manual scrolling releases the anchor.
- The per-channel cap was raised from 300 to 500 messages: now that history
  accumulates upward, that is the only memory limit.

### List virtualization

- Each message's height is measured on first draw and lives in a cache keyed by
  id; not-yet-drawn ones are estimated by text length. The window of visible
  ones is found by binary search over prefix sums rather than scanning the
  channel.
- Skipped messages aren't drawn at all, but space is kept for them: otherwise
  the list would collapse to the visible part and the scrollbar would lie about
  the history length.
- A 600-pixel margin is drawn around the window so no grey bands are left
  between wheel frames.
- Until an image has downloaded, a placeholder of exactly the size it will be
  drawn at (180×360) is reserved for it. Without it the message height would
  jump by hundreds of pixels as soon as the image arrived.

### Speed — 5× fewer allocations per frame

Measurement on 300 messages, 32 images, a GIF and 189 channels
(`cargo test --release -- --ignored --nocapture memcheck`):

| What | 0.4.0 | 0.5.0 |
|---|---|---|
| 10 render frames (chat + lists) | 112 534 | **32 827** |
| — of which chat only | 95 929 | **18 106** |
| — of which lists only | 14 080 | 14 080 |
| Process RSS | 84.9 MB | **80.4 MB** |
| Total allocations over the test | 294 701 | 137 170 |

### Tests

- 44 → 57 tests. Geometry checks on live egui appeared: the scrollbar doesn't
  lie about the history length, no gaps remain while scrolling, loading above
  doesn't shift the view, a new message pulls down only from the bottom
  position, and history is requested exactly once. It was these that found the
  anchor bug that kept the chat from showing new messages.

## 0.4.0 — 2026-09-27

Small fixes on top of a big rework: the client stopped eating memory, stopped
doing extra work every frame, and loads images again.

### Fixed

- **Images and GIFs didn't load at all.** The 3-concurrent-download cap counted
  slots via `fetch_add` and incremented the counter even on refusal. Within a
  couple of frames the counter was permanently over the limit, and it could only
  come back down on a successful download — which never started. The slot is now
  taken with `fetch_update` and released on any exit, including a thread panic.
  Verified end-to-end: 6 images with a limit of 3 all load; on the old code,
  exactly 3 and then silence.
- **Channel history wasn't shown:** the gateway loaded it in the shared queue and
  blocked other commands. Loading was moved out of the queue, pagination takes
  `before` from the oldest message (there were duplicates and four times as many
  requests), and a late response for an already closed channel no longer clears
  the "Loading messages…" label.
- **"Wrong password" on a correct password:** an empty vault is no longer
  treated as a wrong password, extra spaces are forgiven, and the error shows the
  file path and its modification time.
- **Tests wrote to the real `~/.wyvern_accounts.json`** — the vault path is
  overridden in `App`, not via an environment variable, so tests can run in
  parallel.
- **Leak of unclaimed downloads:** on a channel switch, the previous channel's
  downloads stayed in `pending_images` forever along with decoded pixels
  (megabytes per image).
- A broken or too-large image no longer spins a forever spinner but shows
  "failed to load".

### Memory — 3× less

Measurement on 300 messages, 32 images, a GIF and 189 channels
(`cargo test --release -- --ignored --nocapture memcheck`):

| What | Before | After |
|---|---|---|
| Process RSS | 255.2 MB | **84.9 MB** |
| Image cache | 216.0 MB | 47.3 MB |
| History parse peak | 5.64 MB | 0.54 MB |
| Messages in memory | 2.62 MB | 0.44 MB |
| Total allocations over the test | 384 539 | 294 701 |

- The image cache is bounded by **bytes** (48 MB + 32 items), not by item count:
  it never takes more than 48 MB whatever the images.
- Image sizes are computed from the displayed size: in chat a maximum of
  360×360, hence 768 pixels (with headroom for HiDPI) instead of 1536; a GIF 448
  instead of 640. The same 48 MB now holds 28 images instead of 16.
- Embeds no longer store a JSON chunk (`Vec<Value>`), only
  `{ image_url, description }`.
- GIFs play to the end: before, `collect_frames()` decoded the whole GIF (500
  frames of 1000×1000 is 2 GB), then there was a 16-frame cap and a long GIF was
  cut off in the middle. Now the frame count is read from the file structure and
  the limit is memory (32 MB per GIF): long ones are downscaled but played to the
  end.
- An image or GIF currently on screen is not evicted from the cache while it's
  visible. Before, a neighbouring image that had just loaded could push out the
  visible one, and next frame it was downloaded again — it flickered and the chat
  jittered.
- **Finished downloads no longer pile up.** The result was collected only while
  drawing its own image, and one scrolled off screen left decoded pixels in
  `pending_images` forever: on a long chat with GIFs this grew to 269 MB of RSS.
  Now everything ready goes into the cache every frame, where memory is bounded
  by the budget and LRU.
- **The "n/2000" counter is shown only when there is something to count.** On an
  empty field "0/2000" looked like a frozen indicator; space is reserved for it
  so the field doesn't jump in width.
- A cap on source size: decoding goes by source pixels, and Discord accepts up
  to 10000×10000. A too-large image is rejected before allocating memory.
- Lists in the sidebar are no longer cloned every frame.

### Speed

- History is parsed straight into messages (serde structs instead of a
  `serde_json::Value` tree for the whole page): 52 216 → 4 517 allocations. If
  the format is unexpected, the page is parsed by a tolerant fallback — a
  channel's history won't disappear because of one odd row.
- A live message and a history page are parsed by one function, and a test
  compares their result: they can no longer diverge.
- A message's name, text and time are taken by reference rather than by copy; a
  separate pass over all messages just for avatars is gone; the "is this an
  image?" check no longer does `to_lowercase()` and `to_string()` on every URL
  every frame.
- Rendering: 133 612 → 112 534 allocations over 10 frames.

### Development

- The version is visible in the window title: the binary at the repo root isn't
  always rebuilt, and the title makes it clear which one is running.
- A debug line removed from the GUI (`CHAT-AREA w=646 h=1001 …`); it stays in the
  log.
- `main.rs` split into modules (was a single 2591-line file).
- Tests: 19 → 44, plus a memory measurement; 0 compiler warnings.
- The `/tmp/wyvern_layout.log` no longer grows forever (2 MB rotation); the
  message dump only with `WYVERN_DUMP=1`.

## 0.3.0 — before 2026-09-01

First upload: the client as a single `main.rs` file, token login, servers,
channels, history, images.
