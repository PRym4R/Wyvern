# Changelog

All notable changes to Wyvern, newest first. The version itself lives in
`Cargo.toml`.

## 0.10.6 - 2026-11-11

- **A stuck vault lock can no longer lock you out**: `accounts.lock` guards
  against two clients, it isn't the vault itself. When the file cannot be
  created — most often because a running client's lock was deleted, leaving the
  name "delete pending" on Windows until that client exits — the old code failed
  login with "Access is denied" and no way forward. It now clears the stale
  name, retries once, and otherwise logs in without the lock and says so.

## 0.10.5 - 2026-11-11

- **Private state files**: `settings.json` (drafts, proxy password) and
  `deleted.json` (other people's messages) are now written `0600`, and the
  state directory is `0700`, instead of trusting the process umask. The vault
  already did this; all three now share one atomic-write helper, which also
  removes three copies of the temp-file dance.
- **A corrupted vault is refused, not fatal**: a hand-edited `accounts.json`
  with a nonce that wasn't 12 bytes used to panic at unlock, because the nonce
  length comes from the file. It now counts as a wrong password. An unknown
  vault `version` is refused too, so a newer file is never half-read and then
  rewritten in the old layout.
- **Media is not a network probe**: avatar and attachment URLs arrive in
  Discord's JSON, so a crafted message could point them at `http://127.0.0.1:…`
  and use the client to poke the local network. Only `https` on a real hostname
  is fetched now. Avatars also get their own decode limits (8 MB, 2048 px)
  instead of the full attachment budget, which allowed a 40-megapixel "avatar".
- **Links are http(s) only**: `[text](smb://server/share)` on Windows sent the
  system handler an NTLM-authenticating URL; unknown schemes now render as
  plain text.
- **Rate-limit buckets per major parameter**: the `X-RateLimit-Bucket` hash
  excludes `channel_id`/`guild_id`, so keying a wait by the hash alone made a
  429 in one channel stall another. The bucket key now includes the route's
  major parameter, which is what every client library does.
- **Smaller builds**: `image` is compiled with only the codecs the client can
  display (png, jpeg, gif, webp) instead of the whole default set.

## 0.10.4 - 2026-11-10

- **Rate-limit buckets**: `X-RateLimit-Bucket` is now read and remembered, so
  routes Discord groups into one bucket share the wait. A 429 on a send no
  longer lets an edit walk into the same exhausted bucket, and a wait recorded
  before the bucket was known is moved into it instead of being forgotten.
- **Notification sound and do-not-disturb**: two new switches under *Media and
  notifications*. The sound is the platform's own short cue (PowerShell
  `SystemSounds` on Windows, `afplay` on macOS, `canberra-gtk-play` on Linux);
  DND silences notifications and sound while badges and unread counts keep
  working.

## 0.10.3 - 2026-11-10

- **Moderator reaction sweeps**: `MESSAGE_REACTION_REMOVE_EMOJI` drops the whole
  pill for one emoji and `MESSAGE_REACTION_REMOVE_ALL` clears every pill on the
  message. Before, a cleared reaction kept its old counter until the channel was
  reloaded, and clicking the stale pill then disagreed with the server.

## 0.10.2 - 2026-11-10

- **Quick reactions on hover**: pointing at a message brings up a small strip of
  eight emoji above its top-right corner; one click adds (or removes) that
  reaction, so reacting no longer needs the context menu. The strip is painted
  as an overlay, not part of the layout, so message heights and the virtualized
  list stay exactly as they were. It floats just outside the row, and the
  remembered rect keeps it from flickering while the cursor crosses the gap.

## 0.10.1 - 2026-11-10

- **Live reactions**: `MESSAGE_REACTION_ADD` / `MESSAGE_REACTION_REMOVE` are
  applied as they arrive, so somebody else's reaction shows up (and disappears)
  without reloading the channel. Our own events don't count twice: the pill is
  already flipped optimistically, and an event that only repeats it is ignored.
  A pill with nobody left is removed, and a stale cached row height is dropped so
  the message re-measures.

## 0.10.0 - 2026-11-06

- **Reactions**: messages show their reactions as pills under the text, with a
  count and a highlight on the ones you added. Clicking a pill adds or removes
  your reaction at once and sends `PUT`/`DELETE
  /channels/{id}/messages/{id}/reactions/{emoji}/@me` behind it; a refusal
  (no permission, message deleted, no connection) rolls the pill back and says
  why. Unicode and custom (`name:id`) emoji both work, and history and live
  payloads are parsed by the same rules.
- **Notifications everywhere**: desktop notifications no longer depend on
  `notify-send`. Windows gets a PowerShell balloon tip and macOS an `osascript`
  notification; Linux behaves as before. Still off while the window is focused.
- **Login**: clicking a saved account signs in immediately when the vault is
  already open (the password was just entered), instead of asking for the same
  password and a second "Log in" press. With a locked vault the click only
  selects the account, and the strip says which of the two applies.

## 0.9.4 — 2026-11-06

- **Rate limits**: requests now wait for Discord's own `X-RateLimit-*` headers
  and `retry-after` per route instead of sleeping a fixed two seconds, and
  retries no longer hammer a bucket that is already exhausted. The browser-like
  headers are built in one place.
- **Send queue**: sends go through one worker, so a retried message can't land
  after a newer one. A message retries up to three times on `429`, `5xx` or a
  dropped connection (`retry-after` when Discord sends it, otherwise 0.5 → 4 s
  of backoff); only a real refusal returns the text to the composer. Attachments
  are read once and re-uploaded from memory on a retry.

## 0.9.3 — 2026-10-20

- **Animations**: toasts slide up and fade in (and still fade out); the
  selected server grows a rounded accent bar instead of snapping; server and
  channel highlights cross-fade; the settings overlay fades and slides into
  place; freshly downloaded images fade in over the reserved placeholder
  instead of popping. Settings → Interface has an `Animate …` checkbox that
  turns all of it off (including egui's own widget fades).
- **Images and files**: right-click an image (in chat or the lightbox) to copy
  the original-resolution pixels to the clipboard; the composer's 📎 opens the
  native file chooser (zenity/kdialog/yad on Linux, the PowerShell dialog on
  Windows) and queues the selection for upload. DMs no longer repeat the
  person's name above the composer — it's already in the left list.

## 0.9.2 — 2026-10-20

- **Cross-platform paths**: settings, the vault and deleted history now live in
  the OS data directory, and logs in the cache directory, resolved through the
  `dirs` crate instead of hidden `~/.wyvern_*` files. The old files are moved
  once on startup. On Linux the paths are `~/.local/share/wyvern` and
  `~/.cache/wyvern`; Windows release builds no longer open a console window.
- **Live messages in large guilds**: opening a channel now sends a gateway
  guild subscription (`op 14`) for that channel. Without it Discord withholds
  `MESSAGE_CREATE` in big servers and new messages only appeared after
  reopening the channel (REST history still worked).
- **Themes**: expanded from 3 to 13 — added Midnight, Nord, Dracula, Tokyo
  Night, AMOLED, Catppuccin, Gruvbox, plus gradient palettes Aurora, Sunset and
  Ocean. Dark/Cyberpunk/Light were reworked for contrast, and Settings shows a
  live preview for each theme.

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
  `settings.json`; kept-deleted messages in `deleted.json`. Both live in the
  platform data directory (`~/.local/share/wyvern` on Linux, `%APPDATA%\wyvern`
  on Windows, `~/Library/Application Support/wyvern` on macOS); pre-0.9.2
  `~/.wyvern_*` files are migrated once on startup.
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
