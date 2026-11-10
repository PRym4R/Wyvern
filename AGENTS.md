# AGENTS.md — Wyvern

Lightweight Discord client in Rust + egui 0.31.1 (user tokens).

## Two checkouts — read this first
- **Linux (primary)**: `/home/hackerman/wyvern` — the git repo, the real
  build/test host, and where the user runs the client.
- **Windows (this copy)**: `C:\Users\hackerman\Documents\Wyvern-main` — no
  `.git`; the git clone that is pushed from lives next to it in
  `...\wyvern-git`. There is no MSVC linker, so `cargo` alone fails: use the
  gnu toolchain below. Treat `Wyvern-main` as a mirror: inspect and test here,
  but the Linux release build happens on Linux.
- Both roots carry a committed **Linux ELF** `./wyvern` (~29 MB). Never read
  it and never copy a Windows build over it. The **Windows** build is not
  committed: it is attached to the GitHub release for its tag
  (`wyvern.exe`), and `wyvern.exe` stays gitignored.

## Commands
- Build (Linux, the release target):
  `cargo build --release && cp -f target/release/wyvern ./wyvern`.
  The root `wyvern` binary is tracked; a stale copy means the user runs old code.
  Never run that `cp` on Windows — it would replace the Linux ELF with an `.exe`.
- Tests: `cargo test` — 317 tests (314 + 3 ignored); keep that count in sync
  with README.
- 0 warnings required.
- Windows has no MSVC linker (`link.exe`), so `cargo` alone fails. Use the gnu
  toolchain that is installed there (one `#[cfg(unix)]` test is skipped, so
  Windows reports 316 in total):

  ```
  $env:Path = 'C:\Users\hackerman\AppData\Local\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\mingw64\bin;' + $env:Path
  cargo +stable-x86_64-pc-windows-gnu test
  ```

## Git and releases
- `git` 2.56 is installed on Windows and pushes go out from
  `C:\Users\hackerman\Documents\wyvern-git` (identity `prym4r`, credentials via
  Git Credential Manager). The Linux checkout commits as
  `git -c user.name='hackerman' -c user.email='hackerman@local' commit ...`.
- Only `main`; no side branches. Never push without being asked.
- Style: `feat(scope): ...`, `fix(scope): ...`, `docs(...)`.
- Releases: `gh` (GitHub CLI) is installed at
  `C:\Program Files\GitHub CLI\gh.exe`. The stored credential is a PAT without
  `read:org`, so `gh auth login` refuses it — export it instead:
  `$env:GH_TOKEN = (... | git credential fill ... | password line)` and run
  `gh release create <tag> ./wyvern.exe --title <ver> --notes-file <file>`.

## Conventions
- Code comments: English, brief ("why" first). User-facing strings: English.
- Test messages (`assert!` / `eprintln!`) and debug logs (`push_debug`): Russian.
- Docs are Russian; TODO uses `[x]` done, `[ ]` left, `[!]` priority, `(S/M/L)` effort.

## Backups (Linux only, after the user says "работает")

```
mkdir -p /home/hackerman/backups
tar czf /home/hackerman/backups/wyvern-src-<YYYYMMDD>-<shortsha>.tar.gz \
  --exclude=./target --exclude=./wyvern --exclude=./.git -C /home/hackerman/wyvern .
```

## Map
- `src/app.rs` app state; `src/gateway.rs` + `src/gateway/{rest,parse}.rs`
  Discord; `src/gateway/ratelimit.rs` per-route limits; `src/models.rs` structs;
  `src/ui/*` screens; `src/paths.rs`
  data/cache paths; `src/net.rs` process-wide proxy; `src/settings.rs` +
  `src/deleted.rs` state files; `src/messages.rs` gateway→app events;
  `src/notify.rs` desktop notifications per OS;
  `src/crypto.rs` vault; `src/ss.rs` + `src/vless.rs` + `src/proxybridge.rs`
  proxies; `src/util.rs` helpers; `src/file_picker.rs` upload dialog;
  `src/memcheck.rs` memory test (`--ignored`).
- `README.md` is short on purpose: release history lives in `CHANGELOG.md`.
- State files (all overridable) live in the platform data dir
  (`dirs::data_dir()/wyvern`, e.g. `~/.local/share/wyvern`): `settings.json`,
  `deleted.json`, `accounts.json` (vault), `accounts.lock`; logs go to
  `dirs::cache_dir()/wyvern`. Legacy `~/.wyvern_*` files migrate on startup.
- Any change to message height must update `estimate_msg_height` and the geometry
  tests (`src/ui/chat.rs`, `mod geometry_tests`).
