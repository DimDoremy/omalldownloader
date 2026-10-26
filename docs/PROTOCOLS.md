# Protocols

What this build downloads, how each one is tested, and what is still missing.

| Protocol | State | How it is reached | Tested against |
|---|---|---|---|
| HTTP/HTTPS | done — chunked, resumable, digest-checked | the engine's own path | local servers in `omad-engine/tests/` |
| HLS (`.m3u8`) | done — master and media playlists | `packs::run_hls` | a local playlist server, 3 segments → 6000 bytes |
| FTP / FTPS | done — `PASV`, `SIZE`, `REST` for resume | `packs::run_ftp` | a local FTP server, 300 KB, byte-identical |
| DASH (`.mpd`) | **partial** — follows the first `<BaseURL>` | `packs::run_dash` | unit test on the parser only |
| magnet / BitTorrent | **not implemented** | refused with a reason | — |
| eD2k | **not implemented** | refused with a reason | — |
| YouTube/Bilibili and other site extractors | **not implemented** | falls through to plain HTTP | — |

## Why the gap is drawn there

The engine's contract is "a file made of ordered pieces that can be resumed". HLS and FTP fit
it directly, so they are here and tested end to end.

BitTorrent does not: it needs a peer protocol, DHT, a listening port, piece verification and
its own scheduling. The honest options are `librqbit` (a pure-Rust client) or an external
engine, and either way the task model changes from "one file" to "a torrent with files" —
the UI, the store and the resume logic all grow. Wiring a half version in would produce a
download manager that claims torrents and fails on them, which is exactly the failure this
project criticised in the MIME associations.

eD2k is a different protocol again (server list, sources, parts, hashing) with no maintained
Rust stack; it is the lowest-value of the set.

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
application/vnd.apple.mpegurl, application/dash+xml
```

`magnet`, `application/x-bittorrent`, `x-scheme-handler/ed2k` and the site handlers are
absent on purpose. When BitTorrent lands, they get added in the same commit as the pack.
