# Protocols

What this build downloads, how each one is tested, and what is still missing.

| Protocol | State | How it is reached | Tested against |
|---|---|---|---|
| HTTP/HTTPS | done — chunked, resumable, digest-checked | the engine's own path | local servers in `omad-engine/tests/` |
| HLS (`.m3u8`) | done — master and media playlists | `packs::run_hls` | a local playlist server, 3 segments → 6000 bytes |
| FTP / FTPS | done — `PASV`, `SIZE`, `REST` for resume | `packs::run_ftp` | a local FTP server, 300 KB, byte-identical |
| DASH (`.mpd`) | done — best representation, `SegmentTemplate`, `SegmentTimeline`, open-ended until 404 | `packs::run_dash` + `dash.rs` | local MPD server, init + 3 segments, 5200 bytes byte-identical |
| magnet / `.torrent` | done — [librqbit](https://crates.io/crates/librqbit) 9, in process | `packs::run_bt` | **a real transfer**: one session seeds, another downloads 256 KB by magnet, byte-identical (`tests/bt_transfer.rs`) |
| eD2k | delegated to a local client (`amule`, or `$OMAD_ED2K_CLIENT`) | `packs::delegate_ed2k` | missing-client message only |
| YouTube/Bilibili and other site extractors | delegated to `yt-dlp` (or `$OMAD_YTDLP`) | `packs::run_site` | fake extractor writing the output file (`tests/site.rs`) |

## BitTorrent

There is no point writing a BitTorrent client here: [`librqbit`](https://github.com/ikatson/rqbit)
is a maintained, pure-Rust one with DHT, trackers, uTP and piece verification, and it is what
`rqbit` itself is built on. `packs::run_bt` starts a session with the task's download folder,
adds the magnet or torrent, and reports the torrent's own aggregate progress through the
same `StoreSink` the HTTP path uses. A pause during metadata fetch cancels the wait; later
progress is polled every 500 ms.

One thing differs from every other pack: a torrent may contain several files, so the target
is a folder and the task shows the torrent's total, not one file's size.

Not yet here: a seeder fixture in the tests. The current test proves the wiring and the
pause; a full peer transfer needs a second session seeding a torrent file, with either a
local tracker or an explicit peer address.

## eD2k

There is no maintained eD2k implementation in Rust — the `ed2k` crate is only the hash
function — and writing the protocol (server list, sources, parts, queueing) is far larger
than the value. So an `ed2k://` link is **handed to a real client**: `amule` (in the Arch
repositories) or whatever `OMAD_ED2K_CLIENT` names. The link is passed as an argument, the
client downloads into its own folder, and this app says so instead of pretending.

Without that client the task fails with:

```
eD2k needs `amule` on PATH (install amule, or set OMAD_ED2K_CLIENT): …
```

`ed2k` is not registered as a MIME handler for that reason: the handler would exist whether
or not the client does.

## Site extractors

YouTube, Bilibili, Vimeo, Twitch and the rest are not protocols but a moving target that
`yt-dlp` keeps up with, so the page is handed to it: `run_site` runs
`yt-dlp --no-playlist -f best -o <task file> <url>`, reports progress from the growing file,
kills the child on pause, and turns a non-zero exit into the last line of its stderr.

The list of hosts is in `packs::SITE_HOSTS`; a lookalike like `youtube.com.evil.example` does
not match. A normal `https://…/file.iso` still goes to the engine's own HTTP path.

## What is still missing

* **eD2k** is the only protocol here that is delegation without an end-to-end test: this
  machine has no `amule`, and installing it needs a password.
* Nothing else: HTTP, HLS, DASH, FTP, BitTorrent and site extraction all have a test that
  moves bytes.

## What the code does today

```
packs::kind_of(url)   ftp/ftps → Ftp     .m3u8/.m3u → Hls
                      .mpd → Dash        everything else → Http
```

`Engine::run_task` asks `kind_of` before it probes. A pack reports progress through the same
`StoreSink` the HTTP path uses, so the window, the bar and `omalld tasks` show it the same way;
the stop signal is honoured, so pause works for packs too. A magnet or eD2k URL is refused by
the CLI and by the app with a sentence, not with a silent failure.

## The MIME associations

`packaging/omad.desktop` claims only what the build can do:

```
x-scheme-handler/omalldownloader, x-scheme-handler/ftp, x-scheme-handler/ftps,
x-scheme-handler/magnet, application/x-bittorrent,
application/vnd.apple.mpegurl, application/dash+xml
```

`x-scheme-handler/ed2k` and the site handlers stay absent: the first depends on a program
that may not be installed, and the second does not exist yet.

## When the browser refuses the WebSocket

Some browsers refuse `ws://` on loopback from an extension while allowing `http://` to the
same port (this machine's Edge does exactly that). The bridge therefore answers two more
requests on its port:

* `GET /ping` — `{"app":"omalldownloader","version":"1.0.0","protocol":1}`, no origin or
  secret needed, so the extension can tell "the app is closed" from "this browser will not
  open a socket";
* `POST /rpc` — the same commands as the WebSocket (`list`, `add`, `pause`, `resume`,
  `remove`), with the same Origin check and the same secret in `x-omad-secret`, and the same
  trust-on-first-use that answers with the secret it issued.

The extension switches to `/rpc` by itself: after 1.5 s without a socket `open` (a blocked
socket often never fires `error`), when the popup sees `/ping` answer, or when a download
starts while the bridge looks down. Once switched, it polls the task list every 1.5 s.

Verified on this machine: pointed at a port that accepts TCP and never answers, the bridge
reaches `mode:"http", ready:true` in about four seconds, and a download from a page is then
taken by the app instead of the browser
(`extension/tests/e2e/http-fallback.test.mjs` covers the transport and the task hand-off).
