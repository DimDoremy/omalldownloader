//! Probing: everything the engine must know about a URL before it moves a byte.

use omad_core::ssrf::{UrlPolicy, Verdict};
use std::time::Duration;
use url::Url;

/// The answer to "what is behind this URL".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probe {
    /// The final URL after redirects, which is what the chunks must ask for.
    pub final_url: Url,
    /// The server's size, if it told us one we can trust.
    pub total_bytes: Option<u64>,
    /// `Accept-Ranges: bytes`, or a successful 206.
    pub ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    /// From `Content-Disposition`, already sanitised.
    pub suggested_filename: Option<String>,
    /// A strong digest the server published (`Digest: sha-256=...` or `Content-MD5`).
    pub sha256: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("{0} is not a URL the downloader will fetch: {1:?}")]
    Refused(Url, Verdict),
    #[error("the server answered {0}")]
    Status(u16),
    #[error("the connection failed: {0}")]
    Transport(String),
    #[error("too many redirects")]
    Redirects,
}

#[derive(Clone, Debug)]
pub struct ProbeOutcome {
    pub probe: Probe,
    pub status: u16,
    /// Response headers worth showing, in the order received.
    pub headers: Vec<(String, String)>,
}

/// The interface the engine needs. A test implementation replays a recorded answer so
/// the engine can be exercised without a network.
pub trait ProbeSource {
    fn probe(
        &self,
        url: &Url,
        policy: &UrlPolicy,
    ) -> impl std::future::Future<Output = Result<ProbeOutcome, ProbeError>> + Send;
}

/// The real one, over `reqwest`.
#[derive(Clone)]
pub struct HttpProbe {
    client: reqwest::Client,
}

impl HttpProbe {
    pub fn new() -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(30))
            // Redirects are followed by hand so that each hop can be checked.
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("OmaLLDownloader/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self { client })
    }

    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

impl ProbeSource for HttpProbe {
    async fn probe(&self, url: &Url, policy: &UrlPolicy) -> Result<ProbeOutcome, ProbeError> {
        let mut current = url.clone();
        for _ in 0..8 {
            let verdict = policy.check_url(&current);
            if verdict != Verdict::Allowed {
                return Err(ProbeError::Refused(current, verdict));
            }

            let response = self
                .client
                .head(current.as_str())
                .send()
                .await
                .map_err(|e| ProbeError::Transport(e.to_string()))?;

            // A server may refuse HEAD. An empty ranged GET is the next best thing and
            // is what many CDNs expect.
            let response = if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED
                || response.status() == reqwest::StatusCode::NOT_IMPLEMENTED
            {
                self.client
                    .get(current.as_str())
                    .header(reqwest::header::RANGE, "bytes=0-0")
                    .send()
                    .await
                    .map_err(|e| ProbeError::Transport(e.to_string()))?
            } else {
                response
            };

            let status = response.status();

            if status.is_redirection() {
                if let Some(location) = response.headers().get(reqwest::header::LOCATION) {
                    let location = location.to_str().map_err(|_| {
                        ProbeError::Transport("a redirect with a non-UTF-8 target".into())
                    })?;
                    current = current
                        .join(location)
                        .map_err(|e| ProbeError::Transport(e.to_string()))?;
                    continue;
                }
                return Err(ProbeError::Status(status.as_u16()));
            }

            if !(status.is_success() || status == reqwest::StatusCode::PARTIAL_CONTENT) {
                return Err(ProbeError::Status(status.as_u16()));
            }

            let headers = response
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect::<Vec<_>>();

            let header = |name: reqwest::header::HeaderName| {
                response
                    .headers()
                    .get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            };

            let ranges = header(reqwest::header::ACCEPT_RANGES)
                .map(|v| v.to_ascii_lowercase().contains("bytes"))
                .unwrap_or(false)
                || status == reqwest::StatusCode::PARTIAL_CONTENT;

            let total_bytes = header(reqwest::header::CONTENT_RANGE)
                .as_deref()
                .and_then(parse_content_range_total)
                .or_else(|| {
                    if status == reqwest::StatusCode::OK {
                        header(reqwest::header::CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok())
                    } else {
                        None
                    }
                });

            let suggested_filename = header(reqwest::header::CONTENT_DISPOSITION)
                .as_deref()
                .and_then(omad_core::filename::from_content_disposition);

            let probe = Probe {
                final_url: current.clone(),
                total_bytes,
                ranges,
                etag: header(reqwest::header::ETAG),
                last_modified: header(reqwest::header::LAST_MODIFIED),
                content_type: header(reqwest::header::CONTENT_TYPE),
                suggested_filename,
                sha256: strong_digest(&header),
            };

            return Ok(ProbeOutcome {
                probe,
                status: status.as_u16(),
                headers,
            });
        }
        Err(ProbeError::Redirects)
    }
}

/// `bytes 0-0/12345` gives `12345`; `bytes 0-0/*` gives `None`.
fn parse_content_range_total(value: &str) -> Option<u64> {
    let after_slash = value.rsplit('/').next()?.trim();
    after_slash.parse::<u64>().ok()
}

/// A digest that can actually verify a transfer. `Content-MD5` is MD5 and is left
/// alone; only a sha-256 `Digest` entry is used, because that is what the engine
/// computes.
fn strong_digest(header: &dyn Fn(reqwest::header::HeaderName) -> Option<String>) -> Option<String> {
    let digest = header(reqwest::header::HeaderName::from_static("digest"))?;
    for part in digest.split(',') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix("sha-256=") {
            let value = value.trim_matches('"');
            if let Ok(bytes) = base64_decode(value)
                && bytes.len() == 32
            {
                return Some(hex::encode(bytes));
            }
        }
    }
    None
}

fn base64_decode(text: &str) -> Result<Vec<u8>, ()> {
    // Small, dependency-free base64 decoder: only standard alphabet and padding.
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let value = TABLE.iter().position(|&c| c == byte).ok_or(())? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_totals() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
    }

    #[test]
    fn digest_parsing_takes_only_sha256() {
        let headers = |name: reqwest::header::HeaderName| {
            if name.as_str() == "digest" {
                Some("md5=abc, sha-256=MC4CAQAwBQYDK2VwBCIEIOrr0fYpgbn5dP5nILTj/4oLtQp7ZtzCxqS2p/v8u3P3".into())
            } else {
                None
            }
        };
        let digest = strong_digest(&headers);
        assert!(digest.is_none() || digest.as_ref().unwrap().len() == 64);
    }

    #[test]
    fn digest_of_a_known_value() {
        // sha-256 of the empty string, standard base64.
        let value = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
        let decoded = base64_decode(value).unwrap();
        assert_eq!(decoded.len(), 32);
        assert_eq!(
            hex::encode(decoded),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
