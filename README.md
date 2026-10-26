# OmaLLDownloader

A cross-platform download manager with a browser extension — the shape of Ghost Downloader,
built on ODK's design language. Linux first; the Windows and macOS targets are not
verified yet (see [`ARCHITECTURE.md`](ARCHITECTURE.md)).

![the task list](docs/assets/tasks.png)

The window is laid out the way Ghost Downloader is: a rail on the left (Tasks, Add a task,
Settings, the browser extension), a toolbar with All / Active / Done, Start all, Pause all
and a search field, and one card per download with its name, speed, time left, size, a
progress line and its own controls.

```
crates/omad-core     types and the three security checks (URL, file name, destination)
crates/omad-store    SQLite task records, resumable across restarts
crates/omad-engine   chunked HTTP(S) with range requests, resume and digest verification
crates/omad-rpc      the loopback bridge the Chrome extension talks to
crates/omad-app      the GPUI window (ODK components, ODK theme)
crates/omad-cli        `omad`, the command line
crates/omad-app-tests  the window's tests, apart so `test-support` never ships
extension/             the Chrome extension (Manifest V3, no bundler)
packaging/             the Omarchy desktop entry, icon and bar widget, plus the installer
tools/shot-omarchy.sh  a screenshot on a private headless output, without touching the screen
```

## Build and run

```sh
cargo run -p omad-app            # the window
cargo build -p omad-cli          # target/debug/omad
cargo test --workspace         # 84 tests, no network needed
```

The CLI:

```sh
omad probe https://example.com/big.iso        # size, name, ranges; downloads nothing
omad add https://example.com/big.iso          # download it now
omad add https://example.com/big.iso --name "My ISO"
omad tasks                                    # or --json
omad task tsk_3
omad resume tsk_3
omad wait tsk_3 --timeout 600
```

Loopback and private addresses are refused unless `--allow-private-network` is given.

## On Omarchy

```sh
packaging/install-omarchy.sh          # build release, install, enable the bar widget
packaging/install-omarchy.sh --debug  # same, with the faster debug build
packaging/install-omarchy.sh --remove # take it all back out
```

It installs three things under `$HOME`, no root:

* **the launcher entry** — `~/.local/share/applications/omalldownloader.desktop` and the
  icon at every size in `~/.local/share/icons/hicolor`, so the app appears in the Omarchy
  menu like any other program;
* **the executables** — `~/.local/bin/omalldownloader` and `~/.local/bin/omad`;
* **the bar widget** — `~/.config/omarchy/plugins/dimdoremy.omalldownloader/`, validated
  against the shell's schema and placed in the bar's right section. It shows a download
  glyph, how many tasks are running and their combined speed, and opens a popup listing
  each task with its own progress line. It reads `omad tasks --json`, so the bar, the window
  and the CLI never disagree.

A first-time plugin only appears after the shell has re-read its plugin list; if the icon
is not there yet, `omarchy plugin enable dimdoremy.omalldownloader right` places it. The
installer checks that the widget ended up in `shell.json` instead of assuming it.

Each monitor has its own bar, so each instance answers on its own IPC target:

```sh
omarchy-shell dimdoremy.omalldownloader.HDMI-A-1 toggle   # open the popup on that screen
```

![the bar popup](docs/assets/bar-popup.png)

The popup lists every task with its status, progress, size and live speed, offers Pause or
Resume and Remove per task, and shows a running total at the top. It reads the same
`omad tasks --json` the window and the CLI read.

## Platforms

Linux and macOS build and run; Windows builds in CI and has not been run. The details, the
patches ODK needed for the non-Linux builds, and the evidence from the Mac are in
[`docs/PLATFORMS.md`](docs/PLATFORMS.md). CI builds all three on every push and uploads the
binaries as artifacts.

![the extension page](docs/assets/extension-page.png)

The extension looks for `ws://127.0.0.1:14370` on its own. The first time it connects, the
app trusts it and hands it a secret; from then on every connection proves that secret. There
is nothing to type — the same "it just works" the Ghost Downloader extension had.

## Looking at the UI with a computer-use agent

`tools/cua/` runs CUA's computer server against this machine and a small look→act loop on
top of it. `docs/CUA.md` says exactly what works (capturing the window; keyboard navigation),
what does not (pointer clicks and typed text), and the three fixes that would finish it.
`tools/cua/drive.py --self-test` runs the loop and reports which half it reached.

## The extension

```sh
cd extension
npm test          # the pure parts: address rules, the HMAC proof, the protocol version
npm run build     # -> extension/dist
node --test tests/e2e/   # the extension in a real Chromium, driven over CDP
```

The end-to-end test launches Chromium with a throwaway profile, loads `dist`, pairs it
with a stand-in bridge that speaks the same protocol, then downloads 120 KB, 300 KB and
480 KB files from a local server. It asserts the tasks arrive with their names and referer,
and that the browser's own copies were cancelled.

Load `extension/dist` in Chrome (*chrome://extensions* → *Load unpacked*). Then, in the
desktop app, open **Browser extension**, press **Start pairing** and copy the address and
the six-digit code into the extension popup. The code is single use.

**Firefox is not built.** The extension is Manifest V3 without `webRequest`; a Gecko build
would need `sidebar_action`, a different download seam (`downloads.onCreated`, since
`onDeterminingFilename` is Chrome-only) and a `strict_min_version` of at least 128 for MAIN
world content scripts. The review documents why.

## What it will not do

* no path from the extension, the page or the command line: a task gets a destination name
  and a sanitised file name, and the app resolves the path inside one configured root;
* no long-lived token on the wire: the extension proves the pairing code once and then signs
  a fresh nonce per connection;
* no download from a loopback or private address unless the user says so for that request;
* no "complete" until the bytes are on disk and, when the server offers a sha-256 digest,
  match it.

## Licence

MIT. Ghost Downloader 3 is GPL v3; its behaviour was studied, its code was not copied.
