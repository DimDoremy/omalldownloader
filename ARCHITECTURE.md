# OmaLLDownloader — architecture, and why it is split this way

OmaLLDownloader is a download manager in the shape of Ghost Downloader: a desktop app across
Linux, macOS and Windows, a command line, and a Chrome extension that hands downloads over
to it. It is built the way ODK builds things — a Rust workspace on GPUI, ODK's design
language, declarative rules kept in one place — with the app bundle format (OMA) left out
until it stops being Linux-only.

This file is the decision record. It says what is built, what is not, and which choices
are load-bearing.

## The shape

```
        omad-app (GPUI window)      omad-cli (omad)      omad-rpc (extension bridge)
                   \                  |                    /
                    \                 |                   /
                     --------- omad-core (types + security) ---------
                             /                          \
                      omad-engine (download)        omad-store (sqlite)
```

`omad-core` holds the types every layer shares and the three checks that guard the
application's only real inputs: URLs, file names and destinations. `omad-engine` downloads.
`omad-store` remembers. `omad-rpc` is the socket the browser talks to. Nothing below `omad-app`
knows what a window is — and `crates/omad-engine/tests/architecture.rs` fails the build with
`cargo tree` if a GUI crate ever reaches the engine.

### Why the split is not optional

ODK's UI stack (GPUI 0.2.2 through `odk-ui`) is developed and tested on Linux Wayland only.
The review that preceded this code ranked "UI selection" as the single largest risk to a
cross-platform product, with one mitigation that removes it: **the engine and the protocol
must not depend on the UI.** If GPUI turns out not to work on Windows or macOS, only
`omad-app` is replaced. The engine, the store, the bridge, the CLI and every test above them
keep working. That is also why the extension can be developed and tested without the
desktop's window existing.

## The four decisions that matter

### 1. A caller chooses a destination and a name, never a path

The reference implementation (Ghost Downloader 3) accepts an absolute `path` from the
extension and passes it to `Path(...)`. That makes the extension bridge a way to write any
file the user can write, and `cancel` a way to delete one. OmaLLDownloader removes the
possibility rather than validating it:

* the wire format has no path field (`omad_core::AddTask` has `dest` and `filename`);
* the download root is canonicalised **once**, when the app starts (`DownloadRoot::new`);
* every target is resolved inside that root and re-checked against symlinks
  (`DownloadRoot::resolve`), including a symlink placed in the download folder;
* a name never contains a separator, a control character or a reserved device name, and is
  never `.`, `..` or empty (`filename::sanitize`);
* removing a task **keeps its file**: deletion is not something a remote caller gets to
  request.

### 2. The extension bridge trusts an origin, not a token

The reference implementation hands a long-lived token to whoever connects first and never
looks at `Origin`. Both are fixed here (`omad-rpc/src/server.rs`):

* the bridge binds a **random loopback port**, chosen by the operating system, and reports it
  to the user once;
* the `Host` header must be exactly the loopback address bound, which is what a DNS
  rebinding page cannot produce;
* the `Origin` header must be a `chrome-extension://` or `moz-extension://` origin
  (checked before the upgrade completes — a web page is refused at the handshake);
* authentication is `HMAC-SHA256(secret, nonce)`, with a fresh single-use nonce per
  connection and a 60 s expiry; the secret never travels again after pairing;
* pairing is **trust on first use**: the first browser extension to connect is trusted and
  handed a secret, because the origin check already guarantees it is an extension and not a
  page; every later connection proves the secret. This replaced a six-digit out-of-band code
  at the user's request — Ghost Downloader's extension had no code at all, and the code made
  a fresh install need two manual steps;
* comparisons are constant time, and a failure tells the peer a status code and nothing else.

The trade this makes, stated plainly: the first `chrome-extension://` origin to connect is
trusted. A page cannot present one, and the `Host` check still refuses a rebinding name, but
any *extension* that speaks the protocol before yours does would be trusted. Pinning the
extension id after the first pair is the obvious next step; the user asked for the
zero-step experience Ghost Downloader's extension had, and this is the smallest design that
gives it while keeping the page and rebinding walls.

Each of these is a test in `omad-rpc/tests/security.rs`, and the whole path — pair, authenticate,
add, download, verify bytes — is one test in `omad-rpc/tests/end_to_end.rs`.

### 3. Progress is durable, and "complete" means verified

The engine follows three rules, all of them tested:

* a chunk's bytes are written **before** the record learns about them, and the record only
  ever receives the sum of the chunk counters — so a crash loses progress rather than
  inventing it;
* a positioned write that returns short is an error, never a partial success
  (`write_at`, tested on both platforms' implementations);
* a task is marked complete only after the file is read back: its size must match, and when
  the server published a sha-256 `Digest`, the digest must match too.

`crates/omad-engine/tests/crash_consistency.rs` kills a real child process with `SIGKILL`
mid-download and asserts that every byte left on disk matches the source, which is the
property a resume depends on.

### 4. No code path from the reference implementation was copied

Ghost Downloader 3 is **GPL v3**, and its extension vendors a third-party module. OmaLLDownloader
is MIT. The behaviour and the product design were studied; the implementations — the engine,
the bridge, the extension — are new. Nothing about the protocol here is wire-compatible with
Ghost Downloader on purpose.

## What is built, and what is not

**Working, with tests:**

| Piece | State |
|---|---|
| HTTP/HTTPS with range requests, chunking, resume, digest verification | done |
| Non-ranged servers (single stream) and empty files | done |
| Windows/macOS writers (`write_all_at` / `seek_write` loop) | written, untested off Linux |
| SQLite task store, WAL, resumable tasks on restart | done |
| Extension bridge: origin/host checks, HMAC pairing, task list and control | done |
| CLI: `probe`, `add`, `tasks`, `task`, `resume`, `pause`, `remove`, `wait` | done |
| Desktop view: task list, add field, pause/resume, extension tab, pairing code panel | done |
| Chrome MV3 extension: download interception, context menu, popup, settings, badge | done |
| Security regression suite (paths, SSRF, origin, rebinding, pairing, aria2-style token) | done |

**Not done, and deliberately not claimed:**

* ~~The window has not been photographed.~~ **Photographed, on Hyprland.** The first attempts
  used Xvfb and Xwayland, where GPUI 0.2.2 cannot map a window at all. The answer was the
  compositor's own headless output: `hyprctl output create headless` gives a private monitor,
  the app opens on it at a fixed size, `grim` captures it, and the output is removed. The
  user's own screens never appear in the image and their focus is put back.
  `tools/shot-omarchy.sh` does this, and `docs/assets/` holds what it produced. The view
  also still has twelve tests on GPUI's headless test platform, which assert on real element
  bounds and real clicks.
* **Windows and macOS are untested.** The crates compile by construction for those targets
  (no Linux-only API outside `#[cfg]`), but nothing was built or run there.
* **The bar widget's popup was not opened in a screenshot.** The bar pill itself is
  confirmed in `docs/assets/` style captures during the session; the popup's layout was
  reviewed in code only.
* **Pause is not cooperative yet.** The record flips to `paused`; the worker keeps going
  until it finishes. A per-task stop channel is the next change — the engine already accepts
  one (`run_task`'s `stop` receiver).
* **A download queue is missing.** Tasks run as commands arrive; there is no concurrency cap
  or scheduler.
* **BT, M3U8/DASH, FTP and site extractors** are not implemented. The engine's `ProtocolPack`
  seam exists conceptually but only HTTP is built.
* **The extension has been loaded in a real Chromium, over CDP.** `extension/tests/e2e`
  launches Chromium with a throwaway profile, loads `dist`, pairs it against a stand-in
  bridge that speaks the same protocol, downloads 120/300/480 KB files from a local server
  and asserts both that the tasks arrive and that the browser's copies were cancelled. It
  has not been run against a *signed* extension from the Web Store, nor in Edge itself.
* **No AUMID/file associations/single-instance tray.** ODK v1 leaves these to the
  application; they are not written.
* **No accessibility tree.** GPUI 0.2.2 has none, so a screen reader cannot read the window.
  Keyboard, focus, contrast and the rules about colour are respected; the limitation is real
  and is stated here rather than glossed over.

## Theming

The colours come from ODK's four token layers. `omad-app` asks for a border colour through the
semantic layer (`BorderToken::Subtle`), never a literal; spacing is `Space`; buttons are
`ButtonVariant`s. On Linux with Omarchy installed, `odk_theme::ThemeManager` follows the
desktop theme. Without it, ODK's emergency palette is used — which is what happened in the
run recorded below, and why the app logged `no theme loaded, drawing the emergency palette`.

Making Windows and macOS supply a palette is the `ThemeProvider` seam described in the
review: `LinuxOmarchy`, `MacOSSystem`, `WindowsSystem`, `StaticFallback`, each producing a
valid `Palette`, while every foundation number (0/2 px corners, the 4 px grid, the three
densities, the motion durations) stays identical on all three. It is not built.

## Following the desktop's theme

ODK reads a theme from `$XDG_STATE_HOME/omarchy/current/theme` and refuses a `colors.toml`
without the keys it knows. This machine's Omarchy theme carries only
`accent/selection/background/foreground` and the ANSI table, so ODK would have drawn every
window on its emergency palette. `crates/omad-app/src/theme.rs` is the adapter: it copies
the theme into `…/omalldownloader/theme/omarchy/current/theme/`, adds the `mode` Omarchy
leaves implicit (inferred from the background's luminance) and fills the missing surfaces
and colours from the palette it does have, then points `XDG_STATE_HOME` at the copy before
ODK starts. A watcher re-copies when the theme changes, which ODK's own watcher sees, so a
theme switch in Omarchy switches the app within about a second. This is a workaround for an
upstream difference, not a change to Omarchy: worth reporting to ODK.

The task list lives in `$XDG_DATA_HOME/omalldownloader/`, because `XDG_STATE_HOME` now
belongs to the theme adapter.

## Running it

```sh
cargo run -p omad-app        # the desktop window
cargo build -p omad-cli      # then: target/debug/omad --help
cargo test --workspace       # 84 tests
cd extension && npm test && npm run build
cd extension && node --test tests/e2e/    # a real Chromium, over CDP
tools/shot-omarchy.sh /tmp/shot.png --seed   # a screenshot on a private headless output
```

The CLI refuses loopback and private addresses unless `--allow-private-network` is given:
a download manager is otherwise a ready-made way to make a request inside a network on
someone else's behalf (`omad-core/src/ssrf.rs`).

## A recorded run

```
$ omalld probe http://127.0.0.1:18901/source.bin
omalld: ... is not a URL the downloader will fetch: PrivateAddress(127.0.0.1)

$ omalld --allow-private-network --dir /tmp/e2e/downloads add http://127.0.0.1:18901/source.bin
added tsk_1
completed
saved to /tmp/e2e/downloads/report.bin      # the name the server sent

$ sha256sum source.bin downloads/report.bin
63ec3a47…ce173  source.bin
63ec3a47…ce173  downloads/report.bin        # identical
```
