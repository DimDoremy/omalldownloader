# Platforms

The app is a Rust workspace whose only platform-sensitive layer is ODK/GPUI. This page
records what builds where, what was actually run, and what had to change to get there.

## CI

`.github/workflows/build.yml` builds and tests three targets on every push:

| Job | Runner | Does |
|---|---|---|
| `linux-x86_64` | `ubuntu-24.04` | installs Wayland/X11/Vulkan headers, builds, runs the engine and core tests |
| `macos-arm64` | `macos-14` | downloads the Metal toolchain, builds, tests |
| `windows-x86_64` | `windows-latest` | builds, tests |

Each job uploads its binaries as an artifact (`omalldownloader-<platform>`), and one step
fails the build if `gpui/test-support` reaches `omad-app` — cargo unifies features across a
build, so a workspace member that depends on it would silently turn the release binary into
a test binary. That happened once; the guard is why it cannot happen again.

## What had to change for the non-Linux builds

ODK is vendored at commit `8e8f2d2` (`vendor/odk/README-omad.md` lists every patch):

| Patch | Platform | Why |
|---|---|---|
| `vendor/xattr`: one `ENOATTR` per crate | macOS | ODK's Linux patch redefined the constant where libc already has it |
| `odk-ui`: `paste_source()` — primary selection on Linux, clipboard elsewhere | macOS, Windows | GPUI only defines `read_from_primary` for X11/Wayland |
| `odk-ui`: `publish_selection()` gated to X11/Wayland | macOS, Windows | same, for `write_to_primary` |
| `odk-theme`: no-op compositor module on non-Unix | Windows | Hyprland IPC is a Unix socket |
| `odk-ui`: file identity from creation time on Windows | Windows | `MetadataExt::ino()` is Unix-only |

None of these change behaviour on Linux.

## macOS: what was actually run

On `Doremys-MacBook-Pro-3.local` (macOS 27.2, arm64), over SSH:

```
$ cargo build --release --workspace
    Finished `release` profile [optimized] target(s) in 2m 03s

$ omalld probe http://127.0.0.1:18999/src.bin
url:        http://127.0.0.1:18999/src.bin
status:     200
size:       390.6 KB
ranges:     no
name:       src.bin

$ omalld --allow-private-network add http://127.0.0.1:18999/src.bin
completed
saved to /private/tmp/omad-mac/dl/src.bin

$ shasum -a 256 dl/src.bin ; cat src.sha
e72b6d65caafde21fe8a6360694293fbaaf2ba7ff1d99ad9da532a7b5bc1522a
e72b6d65caafde21fe8a6360694293fbaaf2ba7ff1d99ad9da532a7b5bc1522a
```

The window and the service layer:

```
$ pgrep -f target/release/omalldownloader
31819                                   # alive after 31 minutes, 89 MB RSS
$ osascript -e 'tell application "System Events" to tell process "omalldownloader" to get {name, frontmost}'
omalldownloader, true                   # a real foreground app with a window
$ lsof -a -p 31819 -iTCP -sTCP:LISTEN
omalldown 31819 doremy 12u IPv4 TCP 127.0.0.1:57499 (LISTEN)   # the extension bridge
$ ls ~/.local/share/omalldownloader/
omalldownloader.sqlite3  ...-shm        # the store opened in WAL, as on Linux
```

The bridge enforces the same boundary as on Linux:

```
page origin      -> HTTP/1.1 403 Forbidden
rebinding host   -> HTTP/1.1 403 Forbidden
extension origin -> HTTP/1.1 101 Switching Protocols
```

## What is not verified

* **The macOS window has not been photographed.** `screencapture` over SSH fails with
  "could not create image from display" — the SSH session has no Screen Recording
  permission. Grant it in *System Settings → Privacy & Security → Screen Recording*, or look
  at the Mac's screen: the window is up.
* **Nothing in the window was clicked on macOS.** The evidence above is process, window,
  store and bridge, not interaction.
* **Windows is build-and-test only.** No part of it has been run.
* **The theme comes from Omarchy**, so on macOS and Windows the app draws ODK's emergency
  palette. A `ThemeProvider` that reads the system accent colour and light/dark mode is
  designed but not built (`ARCHITECTURE.md`, "Following the desktop's theme").
