# Wyvern

A lightweight Discord client in Rust + egui. Token and a local password, servers
and channels, history that pages in as you scroll up, images and GIFs, and a
debug log in the platform cache directory. Everything runs on your own machine;
there is no server of its own.

Licensed under AGPL-3.0 (see `LICENSE`).

## Download

Ready builds live in [Releases](https://github.com/PRym4R/Wyvern/releases): the
Windows `wyvern.exe` is a portable single file, no installer, and the Linux
binary comes from source below or from the same release page. Neither binary is
committed — release assets carry them.

## Build and run

```bash
cargo build --release
cp -f target/release/wyvern ./wyvern   # only to run it from the repo root
./wyvern
```

The token goes into the login field. If "Remember this account" is ticked, the
token is encrypted with a password in `accounts.json` (AES-256-GCM, key derived
via PBKDF2). It lives in the platform data directory — `~/.local/share/wyvern`
on Linux, `%APPDATA%\wyvern` on Windows, `~/Library/Application Support/wyvern`
on macOS — and the path can be overridden with `WYVERN_ACCOUNTS_PATH`, so test
and sandbox runs leave the real vault alone.

## Debug log

Each frame writes panel geometry to `layout.log` in the platform cache directory
(`~/.cache/wyvern` on Linux, `%LOCALAPPDATA%\wyvern` on Windows,
`~/Library/Caches/wyvern` on macOS); on overflow the file is rotated to `.1`.
Gateway events and image downloads go there too, so the log shows what the
client sees. `WYVERN_LOG_DIR` moves the directory, and a message dump
(`messages_dump.txt`) happens only with `WYVERN_DUMP=1`.

## Layout

| File | Contents |
|---|---|
| `src/main.rs` | window startup, assets, panics to the log |
| `src/app.rs` | application state, channels, messages, login |
| `src/gateway.rs` | WebSocket gateway, connect/reconnect loop |
| `src/gateway/rest.rs` | REST calls: messages, history pages, DMs, friends, reactions |
| `src/gateway/parse.rs` | lenient JSON → models, shared by live and history |
| `src/messages.rs` | gateway → app event enum and read state |
| `src/media.rs` | downloading, decoding and caching images and GIFs |
| `src/models.rs` | data structures, byte-budget cache |
| `src/net.rs` | process-wide proxy used by REST, media and the gateway |
| `src/paths.rs` | cross-platform data/cache paths and legacy migration |
| `src/settings.rs` | settings model, `settings.json` load/save |
| `src/deleted.rs` | deleted-message store (`deleted.json`) |
| `src/util.rs` | base64 and client-fingerprint helpers |
| `src/file_picker.rs` | native "choose files" dialog for uploads |
| `src/ui/*` | screens: login, sidebar, chat, attachments, input, overlays, settings, markdown |
| `src/crypto.rs` | account vault encryption |
| `src/ss.rs` | native Shadowsocks (SIP002 parsing, AEAD ciphers, stream) |
| `src/vless.rs` | native VLESS (TLS + WebSocket/TCP, lazy response stripping) |
| `src/proxybridge.rs` | loopback SOCKS5 bridge that terminates `ss://` / `vless://` |
| `src/memcheck.rs` | memory and allocation measurement (test with `--ignored`) |

## Tests and measurements

```bash
cargo test --release                                        # 321 tests (318 + 3 ignored)
cargo test --release -- --ignored --nocapture memcheck       # memory measurement
WYVERN_SS_URL='ss://…' cargo test --release -- --ignored live_shadowsocks_proxy_smoke
WYVERN_VLESS_URL='vless://…' cargo test --release -- --ignored live_vless_proxy_smoke
```

## Changes

Full release history: [`CHANGELOG.md`](CHANGELOG.md).
