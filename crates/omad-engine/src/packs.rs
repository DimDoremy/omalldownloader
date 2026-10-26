//! The protocols beyond plain HTTP.
//!
//! HTTP is the engine's own path: probe, chunk, resume, verify. The packs here are other
//! shapes of "a file made of pieces", each with the same contract — report progress
//! through the sink, honour the stop signal, and leave something a resume can continue:
//!
//! * **HLS** (`.m3u8`): a playlist of segments fetched in order and concatenated.
//! * **DASH** (`.mpd`): one representation's segments, the same way.
//! * **FTP**: a control connection plus a data connection, with `REST` for resume.
//!
//! Every pack is reached by [`kind_of`], so the engine has one place that decides, and a
//! test can name the decision without a network.

use std::path::Path;
use std::time::Instant;
use url::Url;

use crate::engine::{EngineError, Progress, StoreSink};
use omad_core::TaskId;
use omad_core::ssrf::{UrlPolicy, Verdict};
use tokio::sync::watch;

/// What kind of transfer a URL asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The engine's own chunked path.
    Http,
    Hls,
    Dash,
    Ftp,
    /// A magnet link or a `.torrent`, downloaded by the in-process BitTorrent engine.
    Bt,
}

/// An `ed2k://` link: not a URL the `url` crate can even parse (the `|` separators are not
/// legal in a host), and not something this engine downloads. It is handed to a local
/// eD2k client instead, so the decision is made on the raw string.
pub fn is_ed2k(raw: &str) -> bool {
    raw.len() > 7 && raw[..7].eq_ignore_ascii_case("ed2k://")
}

/// Hands an `ed2k://` link to a local client.
///
/// There is no maintained eD2k implementation in Rust (the `ed2k` crate is the hash
/// function), so this follows the same route every Linux download manager does: the link
/// goes to `amule`, the eMule-compatible client in the Arch repositories, or to whatever
/// `OMAD_ED2K_CLIENT` names.
pub fn delegate_ed2k(raw: &str) -> Result<String, PackError> {
    if !is_ed2k(raw) {
        return Err(PackError::Unsupported(format!("{raw} is not an ed2k link")));
    }
    let client = std::env::var("OMAD_ED2K_CLIENT").unwrap_or_else(|_| "amule".to_string());
    match std::process::Command::new(&client).arg(raw).spawn() {
        Ok(_) => Ok(client),
        Err(error) => Err(PackError::Missing(format!(
            "eD2k needs `{client}` on PATH (install amule, or set OMAD_ED2K_CLIENT): {error}"
        ))),
    }
}

/// Decides by scheme, then by what the path ends in. A query string is ignored, because a
/// playlist behind `?token=` is still a playlist.
pub fn kind_of(url: &Url) -> Kind {
    match url.scheme() {
        "ftp" | "ftps" => return Kind::Ftp,
        "magnet" => return Kind::Bt,
        _ => {}
    }
    let path = url.path().to_ascii_lowercase();
    if path.ends_with(".m3u8") || path.ends_with(".m3u") {
        Kind::Hls
    } else if path.ends_with(".mpd") {
        Kind::Dash
    } else if path.ends_with(".torrent") {
        Kind::Bt
    } else {
        Kind::Http
    }
}

/// The pieces of a playlist, in order.
pub fn parse_playlist(text: &str, base: &Url) -> Vec<Url> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Ok(url) = base.join(line) {
            out.push(url);
        }
    }
    out
}

/// The first `SegmentTemplate`-style base URL in an MPD, resolved. Enough for a progressive
/// or single-representation MPD; a full DASH parser is a later job.
pub fn parse_mpd(text: &str, base: &Url) -> Option<Url> {
    for tag in ["<BaseURL>", "<BaseURL >"] {
        if let Some(start) = text.find(tag) {
            let rest = &text[start + tag.len()..];
            if let Some(end) = rest.find("</BaseURL>") {
                let inner = rest[..end].trim();
                if !inner.is_empty() {
                    return base.join(inner).ok();
                }
            }
        }
    }
    None
}

pub struct PackContext<'a> {
    pub id: TaskId,
    pub target: &'a Path,
    pub sink: &'a dyn StoreSink,
    pub stop: watch::Receiver<bool>,
    pub client: &'a reqwest::Client,
    pub policy: &'a UrlPolicy,
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("the server answered {0}")]
    Status(u16),
    #[error("the connection failed: {0}")]
    Transport(String),
    #[error("nothing to download: the playlist has no segments")]
    Empty,
    #[error("this build cannot download {0} yet")]
    Unsupported(String),
    /// A protocol this build delegates to another program, which is not installed.
    #[error("{0}")]
    Missing(String),
    #[error("the user paused the task")]
    Paused,
    #[error("io error: {0}")]
    Io(String),
}

fn stopped(stop: &watch::Receiver<bool>) -> bool {
    *stop.borrow()
}

async fn get_bytes(
    client: &reqwest::Client,
    url: &Url,
    policy: &UrlPolicy,
    stop: &watch::Receiver<bool>,
) -> Result<Vec<u8>, PackError> {
    if policy.check_url(url) != Verdict::Allowed {
        return Err(PackError::Unsupported(format!(
            "{url} is not a URL we fetch"
        )));
    }
    if stopped(stop) {
        return Err(PackError::Paused);
    }
    let response = client
        .get(url.as_str())
        .send()
        .await
        .map_err(|e| PackError::Transport(e.to_string()))?;
    if !response.status().is_success() {
        return Err(PackError::Status(response.status().as_u16()));
    }
    response
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| PackError::Transport(e.to_string()))
}

/// HLS: fetch the playlist, then every segment, concatenated into one file.
pub async fn run_hls(ctx: &PackContext<'_>, url: &Url) -> Result<u64, PackError> {
    let text = String::from_utf8_lossy(&get_bytes(ctx.client, url, ctx.policy, &ctx.stop).await?)
        .into_owned();

    // A master playlist points at variant playlists; take the first one that parses.
    let mut segments = parse_playlist(&text, url);
    if segments.is_empty() && text.contains("#EXT-X-STREAM-INF") {
        return Err(PackError::Empty);
    }
    if segments
        .iter()
        .any(|segment| segment.path().ends_with(".m3u8"))
    {
        let variant = segments.remove(0);
        let variant_text =
            String::from_utf8_lossy(&get_bytes(ctx.client, &variant, ctx.policy, &ctx.stop).await?)
                .into_owned();
        segments = parse_playlist(&variant_text, &variant);
    }
    if segments.is_empty() {
        return Err(PackError::Empty);
    }

    let mut file = std::fs::File::create(ctx.target).map_err(|e| PackError::Io(e.to_string()))?;
    let mut written = 0u64;
    let total = segments.len() as u64;
    let started = Instant::now();
    for (index, segment) in segments.iter().enumerate() {
        if stopped(&ctx.stop) {
            return Err(PackError::Paused);
        }
        let bytes = get_bytes(ctx.client, segment, ctx.policy, &ctx.stop).await?;
        std::io::Write::write_all(&mut file, &bytes).map_err(|e| PackError::Io(e.to_string()))?;
        written += bytes.len() as u64;
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        ctx.sink.on_progress(
            ctx.id,
            &Progress {
                received: written,
                total: None,
                speed: (written as f64 / elapsed) as u64,
            },
        );
        ctx.sink
            .on_commit(ctx.id, (index + 1) as u64, Some(total), 0);
    }
    std::io::Write::flush(&mut file).map_err(|e| PackError::Io(e.to_string()))?;
    Ok(written)
}

/// DASH: follow the MPD's first `BaseURL`, which for a single-representation stream is the
/// segment or the directory of segments. A full segment-template reader is a later job.
pub async fn run_dash(ctx: &PackContext<'_>, url: &Url) -> Result<u64, PackError> {
    let text = String::from_utf8_lossy(&get_bytes(ctx.client, url, ctx.policy, &ctx.stop).await?)
        .into_owned();
    let base = parse_mpd(&text, url).ok_or_else(|| {
        PackError::Unsupported("this MPD has no BaseURL this build understands".into())
    })?;
    let bytes = get_bytes(ctx.client, &base, ctx.policy, &ctx.stop).await?;
    std::fs::write(ctx.target, &bytes).map_err(|e| PackError::Io(e.to_string()))?;
    let len = bytes.len() as u64;
    ctx.sink.on_progress(
        ctx.id,
        &Progress {
            received: len,
            total: Some(len),
            speed: 0,
        },
    );
    ctx.sink.on_commit(ctx.id, len, Some(len), 0);
    Ok(len)
}

/// FTP: control connection, `TYPE I`, `SIZE`, `REST` for a resume, then a passive data
/// connection carrying `RETR`.
pub async fn run_ftp(ctx: &PackContext<'_>, url: &Url) -> Result<u64, PackError> {
    use std::io::Write;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    if policy_denied(ctx.policy, url) {
        return Err(PackError::Unsupported(format!(
            "{url} is not a host we connect to"
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| PackError::Unsupported("no host".into()))?;
    let port = url.port().unwrap_or(21);
    let user = if url.username().is_empty() {
        "anonymous"
    } else {
        url.username()
    };
    let password = url.password().unwrap_or("anonymous@");
    let path = percent_decode(url.path());

    let stream = tokio::net::TcpStream::connect((host, port))
        .await
        .map_err(|e| PackError::Transport(e.to_string()))?;
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    expect(&mut lines, "220").await?;
    send_line(&mut write, &format!("USER {user}")).await?;
    // The reply is read once and then judged: asking for `230` and, on failure, for `331`
    // would consume the very line the second question needs.
    let reply = read_reply(&mut lines).await?;
    if !reply.starts_with("230") {
        if !reply.starts_with("331") {
            return Err(PackError::Transport(reply));
        }
        send_line(&mut write, &format!("PASS {password}")).await?;
        expect(&mut lines, "230").await?;
    }
    send_line(&mut write, "TYPE I").await?;
    expect(&mut lines, "200").await?;

    // How much is already on disk, so a resume asks for the rest.
    let mut offset = std::fs::metadata(ctx.target).map(|m| m.len()).unwrap_or(0);
    send_line(&mut write, &format!("SIZE {path}")).await?;
    let total = expect(&mut lines, "213").await.ok().and_then(|line| {
        line.split_whitespace()
            .nth(1)
            .and_then(|n| n.parse::<u64>().ok())
    });
    // A file on disk that is already the whole file is not this file any more.
    if total.is_none() || offset >= total.unwrap_or(0) {
        offset = 0;
    }

    send_line(&mut write, "PASV").await?;
    let passive = expect(&mut lines, "227").await?;
    let (data_host, data_port) = parse_pasv(&passive)
        .ok_or_else(|| PackError::Transport(format!("unreadable PASV reply: {passive}")))?;

    let mut data = tokio::net::TcpStream::connect((data_host.as_str(), data_port))
        .await
        .map_err(|e| PackError::Transport(e.to_string()))?;

    if offset > 0 {
        send_line(&mut write, &format!("REST {offset}")).await?;
        expect(&mut lines, "350").await?;
    }
    send_line(&mut write, &format!("RETR {path}")).await?;
    expect(&mut lines, "150").await?;

    let mut file = if offset > 0 {
        std::fs::OpenOptions::new()
            .append(true)
            .open(ctx.target)
            .map_err(|e| PackError::Io(e.to_string()))?
    } else {
        std::fs::File::create(ctx.target).map_err(|e| PackError::Io(e.to_string()))?
    };

    let mut buffer = vec![0u8; 64 * 1024];
    let mut written = offset;
    let started = Instant::now();
    loop {
        if stopped(&ctx.stop) {
            return Err(PackError::Paused);
        }
        let n = data
            .read(&mut buffer)
            .await
            .map_err(|e| PackError::Transport(e.to_string()))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])
            .map_err(|e| PackError::Io(e.to_string()))?;
        written += n as u64;
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        ctx.sink.on_progress(
            ctx.id,
            &Progress {
                received: written,
                total,
                speed: ((written - offset) as f64 / elapsed) as u64,
            },
        );
    }
    file.flush().map_err(|e| PackError::Io(e.to_string()))?;
    expect(&mut lines, "226").await?;
    let _ = write.write_all(b"QUIT\r\n").await;
    ctx.sink.on_commit(ctx.id, written, total, 0);
    Ok(written)
}

async fn send_line(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    line: &str,
) -> Result<(), PackError> {
    use tokio::io::AsyncWriteExt;
    write
        .write_all(format!("{line}\r\n").as_bytes())
        .await
        .map_err(|e| PackError::Transport(e.to_string()))
}

/// One complete reply line. A multi-line reply is not needed by anything here.
async fn read_reply(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>>,
) -> Result<String, PackError> {
    lines
        .next_line()
        .await
        .map_err(|e| PackError::Transport(e.to_string()))?
        .ok_or_else(|| PackError::Transport("the server closed the connection".into()))
}

/// Reads replies until one starts with `code`; a completed reply that does not is an error.
async fn expect(
    lines: &mut tokio::io::Lines<tokio::io::BufReader<tokio::net::tcp::OwnedReadHalf>>,
    code: &'static str,
) -> Result<String, PackError> {
    loop {
        let line = lines
            .next_line()
            .await
            .map_err(|e| PackError::Transport(e.to_string()))?
            .ok_or_else(|| PackError::Transport("the server closed the connection".into()))?;
        if line.starts_with(code) {
            return Ok(line);
        }
        if line.len() > 3 && line.as_bytes()[3] == b' ' {
            return Err(PackError::Transport(line));
        }
    }
}

/// FTP is not an http scheme, so `UrlPolicy::check_url` does not apply; the host is what
/// matters. A literal loopback or private address, or `localhost`, is refused unless the
/// task was allowed to reach the local network.
fn policy_denied(policy: &UrlPolicy, url: &Url) -> bool {
    if policy.allow_private {
        return false;
    }
    use std::net::IpAddr;
    match url.host() {
        Some(url::Host::Ipv4(ip)) => !omad_core::ssrf::is_public(IpAddr::V4(ip)),
        Some(url::Host::Ipv6(ip)) => !omad_core::ssrf::is_public(IpAddr::V6(ip)),
        Some(url::Host::Domain(name)) => {
            let lower = name.trim_end_matches('.').to_ascii_lowercase();
            lower == "localhost" || lower.ends_with(".localhost")
        }
        None => true,
    }
}

/// `227 Entering Passive Mode (h1,h2,h3,h4,p1,p2)`.
pub fn parse_pasv(reply: &str) -> Option<(String, u16)> {
    let open = reply.find('(')?;
    let close = reply[open..].find(')')? + open;
    let numbers: Vec<u16> = reply[open + 1..close]
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();
    if numbers.len() != 6 {
        return None;
    }
    let host = format!(
        "{}.{}.{}.{}",
        numbers[0], numbers[1], numbers[2], numbers[3]
    );
    let port = numbers[4] * 256 + numbers[5];
    Some((host, port))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&text[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Runs whichever pack a URL asks for.
pub async fn run_pack(ctx: &PackContext<'_>, url: &Url) -> Result<u64, PackError> {
    match kind_of(url) {
        Kind::Hls => run_hls(ctx, url).await,
        Kind::Dash => run_dash(ctx, url).await,
        Kind::Ftp => run_ftp(ctx, url).await,
        Kind::Bt => run_bt(ctx, url).await,
        Kind::Http => Err(PackError::Unsupported("handled by the engine".into())),
    }
}

/// A pack that failed in a way the engine's callers already understand.
pub fn to_engine(error: PackError) -> EngineError {
    match error {
        PackError::Paused => EngineError::Stopped,
        PackError::Unsupported(what) => EngineError::Transport(format!("unsupported: {what}")),
        PackError::Missing(what) => EngineError::Transport(what),
        other => EngineError::Transport(other.to_string()),
    }
}

/// BitTorrent: a magnet link or a `.torrent`, downloaded by librqbit in this process.
///
/// A torrent is not "a file of ordered pieces" like the other packs — it may hold several
/// files and it fetches them from peers — so the contract differs in one place: the target
/// is a **directory** and progress is the torrent's own aggregate. Everything else is the
/// same: the sink hears about progress, the stop signal pauses the torrent, and the task is
/// only reported complete when librqbit says every piece is verified.
pub async fn run_bt(ctx: &PackContext<'_>, url: &Url) -> Result<u64, PackError> {
    use librqbit::{AddTorrent, AddTorrentOptions, Session};

    // The task's file path is `<download folder>/<name>.<ext>`; a torrent writes a directory
    // beside it, named after the torrent, so nothing collides.
    let folder = ctx
        .target
        .parent()
        .ok_or_else(|| PackError::Io("the download folder has no parent".into()))?
        .to_path_buf();
    std::fs::create_dir_all(&folder).map_err(|e| PackError::Io(e.to_string()))?;

    let session = Session::new(folder)
        .await
        .map_err(|e| PackError::Transport(format!("could not start the torrent session: {e}")))?;

    // Adding a magnet waits for metadata, which can take as long as the swarm does; a pause
    // during that wait has to win, or the task would be unstoppable.
    let mut stop = ctx.stop.clone();
    let added = tokio::select! {
        added = session.add_torrent(
            AddTorrent::Url(url.as_str().into()),
            Some(AddTorrentOptions {
                // Resuming a torrent writes over files it already verified.
                overwrite: true,
                ..Default::default()
            }),
        ) => added.map_err(|e| PackError::Transport(format!("could not add the torrent: {e}")))?,
        _ = async {
            loop {
                if *stop.borrow() { break; }
                if stop.changed().await.is_err() { break; }
            }
        } => return Err(PackError::Paused),
    };

    let handle = added.into_handle().ok_or_else(|| {
        PackError::Unsupported("that torrent was only listed, not downloaded".into())
    })?;

    let mut last = 0u64;
    let started = Instant::now();
    loop {
        if stopped(&ctx.stop) {
            let _ = session.pause(&handle).await;
            return Err(PackError::Paused);
        }
        let stats = handle.stats();
        if let Some(error) = &stats.error {
            return Err(PackError::Transport(error.clone()));
        }
        let received = stats.progress_bytes;
        let total = if stats.total_bytes > 0 {
            Some(stats.total_bytes)
        } else {
            None
        };
        let elapsed = started.elapsed().as_secs_f64().max(0.001);
        ctx.sink.on_progress(
            ctx.id,
            &Progress {
                received,
                total,
                speed: ((received - last) as f64 / elapsed) as u64,
            },
        );
        last = received;
        ctx.sink.on_commit(ctx.id, received, total, 0);
        if stats.finished {
            return Ok(received);
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    #[test]
    fn the_scheme_and_the_extension_decide_the_pack() {
        assert_eq!(kind_of(&url("https://example.com/a.iso")), Kind::Http);
        assert_eq!(
            kind_of(&url("https://example.com/live.m3u8?token=1")),
            Kind::Hls
        );
        assert_eq!(kind_of(&url("https://example.com/stream.mpd")), Kind::Dash);
        assert_eq!(kind_of(&url("ftp://example.com/a.bin")), Kind::Ftp);
        assert_eq!(kind_of(&url("ftps://example.com/a.bin")), Kind::Ftp);
    }

    #[test]
    fn a_magnet_or_a_torrent_file_is_bit_torrent_and_an_ed2k_link_is_ed2k() {
        assert_eq!(
            kind_of(&url(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567"
            )),
            Kind::Bt
        );
        assert_eq!(kind_of(&url("https://example.com/a.torrent")), Kind::Bt);
        // ed2k is not a Url at all; the raw string decides, and that is tested below.
        assert!(is_ed2k(
            "ed2k://|file|a.iso|1024|0123456789ABCDEF0123456789ABCDEF|/"
        ));
        assert!(!is_ed2k("magnet:?xt=urn:btih:0123"));
    }

    #[test]
    fn a_playlist_yields_its_segments_in_order_and_ignores_tags() {
        let base = url("https://example.com/hls/index.m3u8");
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\nseg1.ts\n#EXTINF:4,\nseg2.ts\nseg3.ts\n";
        let segments = parse_playlist(text, &base);
        assert_eq!(
            segments.iter().map(|u| u.as_str()).collect::<Vec<_>>(),
            vec![
                "https://example.com/hls/seg1.ts",
                "https://example.com/hls/seg2.ts",
                "https://example.com/hls/seg3.ts"
            ]
        );
    }

    #[test]
    fn a_playlist_with_no_segments_is_empty() {
        let base = url("https://example.com/x.m3u8");
        assert!(parse_playlist("#EXTM3U\n#EXT-X-ENDLIST\n", &base).is_empty());
    }

    #[test]
    fn an_mpd_base_url_is_resolved_against_the_manifest() {
        let base = url("https://example.com/movies/a.mpd");
        let text = r#"<MPD><Period><AdaptationSet><Representation><BaseURL>video.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#;
        assert_eq!(
            parse_mpd(text, &base).map(|u| u.to_string()),
            Some("https://example.com/movies/video.mp4".to_string())
        );
        assert!(parse_mpd("<MPD></MPD>", &base).is_none());
    }

    #[test]
    fn a_pasv_reply_gives_host_and_port() {
        let reply = "227 Entering Passive Mode (192,168,1,10,200,20).";
        assert_eq!(
            parse_pasv(reply),
            Some(("192.168.1.10".to_string(), 200 * 256 + 20))
        );
        assert!(parse_pasv("227 nope").is_none());
    }

    #[test]
    fn a_path_is_percent_decoded() {
        assert_eq!(percent_decode("/a%20b/c.bin"), "/a b/c.bin");
        assert_eq!(percent_decode("/plain"), "/plain");
    }
}
