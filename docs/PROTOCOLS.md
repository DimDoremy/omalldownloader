# Protocols

What this build downloads, how each one is tested, and what is still missing.

| Protocol | State | How it is reached | Tested against |
|---|---|---|---|
| HTTP/HTTPS | done — chunked, resumable, digest-checked | the engine's own path | local servers in `omad-engine/tests/` |
| HLS (`.m3u8`) | done — master and media playlists | `packs::run_hls` | a local playlist server, 3 segments → 6000 bytes |
| FTP / FTPS | done — `PASV`, `SIZE`, `REST` for resume | `packs::run_ftp` | a local FTP server, 300 KB, byte-identical |
| DASH (`.mpd`) | **partial** — follows the first `<BaseURL>` | `packs::run_dash` | unit test on the parser only |
| magnet / `.torrent` | done — [librqbit](https://crates.io/crates/librqbit) 9, in process | `packs::run_bt` | magnet accepted, pause during metadata fetch (`omad-engine/tests/bt.rs`) |
| eD2k | delegated to a local client (`amule`, or `$OMAD_ED2K_CLIENT`) | `packs::delegate_ed2k` | missing-client message; no end-to-end run |
| YouTube/Bilibili and other site extractors | **not implemented** | falls through to plain HTTP | — |

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

## Why the gap is drawn there

Site extractors (YouTube, Bilibili) are not a protocol but a moving target that `yt-dlp`
maintains; the right integration is a delegation like eD2k's, and it is next.

## What the code does today

```
packs::kind_of(url)   ftp/ftps → Ftp     .m3u8/.m3u → Hls
                      .mpd → Dash        everything else → Http
```

`Engine::run_task` asks `kind_of` before it probes. A pack reports progress through the same
`StoreSink` the HTTP path uses, so the window, the bar and `omad tasks` show it the same way;
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
