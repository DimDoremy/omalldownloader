# Replacing Ghost Downloader with OmaLLDownloader

What was on this machine, what was removed, what replaced it, and the one command left.

## What Ghost Downloader had installed

| Kind | Path |
|---|---|
| Package | `ghost-downloader-bin 4.4.3-1` (pacman), 224 files under `/usr/lib/ghost-downloader`, `/usr/bin/ghost-downloader` |
| Application bundle | `~/Applications/Ghost Downloader` (84 MB, installed by AppManager) |
| Desktop entries | `~/.local/share/applications/ghost-downloader.desktop`, `…/io.github.xiaoyouchr.GhostDownloader.desktop`, `/usr/share/applications/ghost-downloader.desktop` |
| Autostart | `~/.config/autostart/io.github.xiaoyouchr.GhostDownloader.desktop` |
| Data (settings, task records, logs, ed2k, BT) | `~/.local/share/GhostDownloader` (29 MB) |
| URL handlers | magnet, `application/x-bittorrent`, ed2k, ftp, ftps, DASH, HLS, `x-scheme-handler/ghostdownloader` |
| Browser extension | Edge, id `odaohmfjjbompdkmfbambadnagplcmce` v2.2.0 |
| Notification rule | `~/.config/omarchy/notification-click-ignore.list` had two lines for it |

## What was done

Everything user-level, after a backup:

```
~/Backups/ghost-downloader-20261026-174024.tar.gz     (98 MB, 282 entries)
```

* the AppManager bundle, the `~/.local/bin` shim, both user desktop entries, the autostart
  entry and `~/.local/share/GhostDownloader` were removed;
* the Edge extension folder and its `Preferences` entry were removed (a `.bak-ghost-removal`
  copy of `Preferences` is kept);
* the eight MIME associations were dropped, so a magnet link no longer points at a
  program that is not there;
* the two lines in the Omarchy notification list were removed — they existed because Ghost
  Downloader took over notification clicks.

## What replaced it

| Ghost Downloader | Here |
|---|---|
| `~/Applications/Ghost Downloader` | `~/.local/bin/omalldownloader` + `~/.local/share/applications/omalldownloader.desktop` (+ icons) |
| `ghd` command line | `omalld` (`probe`, `add`, `tasks`, `task`, `pause`, `resume`, `remove`, `wait`) |
| its browser extension | `~/.local/share/omalldownloader/extension` (build output, ready to load unpacked) |
| its bar presence (none) | `dimdoremy.omalldownloader` Omarchy bar widget |
| its task list | `~/.local/share/omalldownloader/omalldownloader.sqlite3` |

## The two steps that need a human

1. **Remove the package** (needs the sudo password):

   ```sh
   sudo pacman -Rns ghost-downloader-bin
   ```

   That takes `/usr/lib/ghost-downloader`, `/usr/bin/ghost-downloader` and
   `/usr/share/applications/ghost-downloader.desktop`. Until it runs, the files stay.

2. **Load the extension into Edge once**:

   * open `edge://extensions`, turn on *Developer mode*, *Load unpacked*,
     choose `~/.local/share/omalldownloader/extension`.

   That is the whole setup, the way Ghost Downloader's extension worked: the extension looks
   for `ws://127.0.0.1:14370`, the app trusts the first browser extension that connects and
   hands it a secret, and every later connection proves that secret. Nothing to type.

## What omalld took over

| Ghost Downloader | Here |
|---|---|
| HTTP/HTTPS | done, chunked and resumable |
| FTP | done, with `REST` resume; `ftp://` and `ftps://` registered |
| M3U8 (HLS) | done; `application/vnd.apple.mpegurl` registered |
| MPEG-DASH | partial — first `<BaseURL>` only; `application/dash+xml` registered |
| magnet / BT, eD2k | **not implemented**; those associations stay empty on purpose |
| YouTube/Bilibili and site extractors | not implemented |

`docs/PROTOCOLS.md` explains why BitTorrent and eD2k are a larger job than the others, and
what would have to change to add them.
