//! `net.fetch`: read one URL, under bounds the caller cannot raise past.
//!
//! # Why this is not `bash curl`
//!
//! A model that wants a documentation page can already ask for one, by
//! constructing a shell command. That works and is wrong in four ways:
//! it needs execute authority to do something read-only, it is unbounded in
//! size and in redirects, its failures arrive as a shell exit code rather than
//! as something to act on, and the request is invisible to policy — a rule
//! about which hosts a workspace may reach cannot be written against `sh`.
//!
//! A typed operation fixes all four: the requirement is `network.connect` over
//! the host, so an operator can allow one domain and deny the rest; the body,
//! the redirect chain, and the deadline are bounded here rather than hoped
//! for; and a failure is a structured result the model can read.
//!
//! # What comes back
//!
//! The body, as text. HTML is reduced to its readable content first, because
//! a page's markup is usually several times larger than what it says, and a
//! turn's context is the scarce thing. The full response is still stored as an
//! artifact, so nothing that was fetched is lost — only elided.

use arsy_kernel::{
    artifact::ArtifactStore,
    capability::{CapabilityAction, CapabilityGrant},
    domain::ResourceRef,
    operation::{
        ConcurrencyRule, Effect, Idempotency, InputSchema, JsonType, OperationContract,
        OperationError, OperationExecutor, OperationKind, OperationOutcome, OperationRequest,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{io::Read, sync::Arc, time::Duration};

/// Enough for a long documentation page; far short of a tarball someone linked
/// by mistake.
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_MAX_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 120_000;

/// A short chain is normal — http to https, a trailing slash, a CDN. A long
/// one is a loop or a tracker, and following it is not worth the turn.
const MAX_REDIRECTS: u32 = 5;

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FetchResult {
    pub url: String,
    pub status: u16,
    pub content_type: String,
    pub bytes: u64,
    /// Whether the body was cut at the byte limit.
    pub truncated: bool,
    /// Whether the text is the readable part of an HTML page rather than the
    /// response verbatim.
    pub extracted: bool,
    pub text: String,
}

pub struct FetchExecutor {
    contract: OperationContract,
    artifacts: Arc<dyn ArtifactStore>,
    retain_until_ms: u64,
}

impl FetchExecutor {
    pub fn new(artifacts: Arc<dyn ArtifactStore>, retain_until_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            contract: OperationContract {
                kind: OperationKind::new("net.fetch").expect("static operation kind is valid"),
                input_schema: InputSchema {
                    required: [("url".to_owned(), JsonType::String)].into_iter().collect(),
                    optional: [
                        ("max_bytes".to_owned(), JsonType::Number),
                        ("timeout_ms".to_owned(), JsonType::Number),
                    ]
                    .into_iter()
                    .collect(),
                    allow_extra: false,
                },
                actions: vec![CapabilityAction::NetworkConnect],
                // Two fetches of one URL are the same question asked twice.
                idempotency: Idempotency::Idempotent,
                // Reading a page changes nothing here or there, which is also
                // what lets it stay available while a plan is being made.
                reversible: true,
                concurrency: ConcurrencyRule::Parallel,
            },
            artifacts,
            retain_until_ms,
        })
    }
}

impl OperationExecutor for FetchExecutor {
    fn contract(&self) -> &OperationContract {
        &self.contract
    }

    fn execute(
        &self,
        request: &OperationRequest,
        _grants: &[CapabilityGrant],
    ) -> Result<OperationOutcome, OperationError> {
        let url = request
            .input
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let host = host_of(url).ok_or_else(|| {
            OperationError::Schema(format!(
                "`{url}` is not an http or https URL; only those two schemes are fetched"
            ))
        })?;
        let limit = request
            .input
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_MAX_BYTES)
            .clamp(1, MAX_MAX_BYTES);
        let timeout = Duration::from_millis(
            request
                .input
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, MAX_TIMEOUT_MS),
        );

        let result = fetch(url, limit, timeout)?;
        let value = super::agent::store(
            self.artifacts.as_ref(),
            &result,
            request.actor.clone(),
            self.retain_until_ms,
        )?;
        Ok(OperationOutcome {
            value: Some(value),
            observed_effects: vec![Effect {
                action: CapabilityAction::NetworkConnect,
                resource: ResourceRef::new("net", host)
                    .map_err(|error| OperationError::Execution(error.to_string()))?,
            }],
            evidence: Vec::new(),
            state: None,
        })
    }
}

/// Fetch `url`, following redirects only while they stay on the host the call
/// was authorized against.
///
/// # Why redirects are not followed transparently
///
/// The capability requirement for this call is `network.connect` over the host
/// in the URL, and the effect recorded afterwards names that same host. A
/// transport that followed a redirect by itself would break both: an operator
/// who allowed `docs.rs` and nothing else would have their allowlist bypassed
/// by a page that 302s to `169.254.169.254`, and the audit trail would still
/// say `docs.rs` — describing a request that was never made.
///
/// So the chain is walked here, one hop at a time, and a hop that leaves the
/// host is refused with the host it wanted. The model can then ask for that
/// URL directly, which is a new call, authorized against the new host, and
/// recorded as such. That is a worse user experience than following it
/// silently and the only version of it that policy can actually describe.
fn fetch(url: &str, limit: u64, timeout: Duration) -> Result<FetchResult, OperationError> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        // A 404 page often says what the right URL is, so the body is worth
        // returning: the status is reported instead of thrown away as an error.
        .http_status_as_error(false)
        // Every hop is this module's decision, not the transport's.
        .max_redirects(0)
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .user_agent(concat!("arsy/", env!("CARGO_PKG_VERSION")))
        .build()
        .into();

    let authorized = host_of(url)
        .ok_or_else(|| OperationError::Schema(format!("`{url}` is not an http or https URL")))?;
    let mut current = url.to_owned();
    for _ in 0..=MAX_REDIRECTS {
        let response = agent
            .get(&current)
            .call()
            .map_err(|error| OperationError::Execution(describe(&error)))?;
        let status = response.status().as_u16();

        if let Some(next) = redirect_target(&response, &current) {
            let host = host_of(&next).ok_or_else(|| {
                OperationError::Execution(format!(
                    "{current} redirected to `{next}`, which is not an http or https URL"
                ))
            })?;
            if host != authorized {
                return Err(OperationError::Execution(format!(
                    "{current} redirected to `{host}`, which this call was not authorized to \
                     reach — it asked for `{authorized}`. Fetch that URL directly if you want \
                     it, so the request is authorized and recorded against the host it goes to."
                )));
            }
            current = next;
            continue;
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        // One byte past the limit, so a body that exactly fills it is not
        // reported as truncated and one that overflows it is.
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(limit.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|error| OperationError::Execution(error.to_string()))?;
        let truncated = body.len() as u64 > limit;
        body.truncate(usize::try_from(limit).unwrap_or(usize::MAX));

        let raw = String::from_utf8_lossy(&body).into_owned();
        let html = content_type.contains("html") || looks_like_html(&raw);
        return Ok(FetchResult {
            url: current,
            status,
            content_type,
            bytes: body.len() as u64,
            truncated,
            extracted: html,
            text: if html { readable(&raw) } else { raw },
        });
    }
    Err(OperationError::Execution(format!(
        "the URL redirected more than {MAX_REDIRECTS} times"
    )))
}

/// Where a response says to go next, resolved against the URL it came from.
///
/// `None` for anything that is not a redirect, or a redirect with no usable
/// `Location`, which is then read as the response it is.
fn redirect_target<T>(response: &ureq::http::Response<T>, from: &str) -> Option<String> {
    if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return None;
    }
    let location = response
        .headers()
        .get("location")
        .and_then(|value| value.to_str().ok())?
        .trim();
    resolve(from, location)
}

/// Resolve a `Location` against the URL it was served from.
///
/// Absolute, protocol-relative, root-relative, and path-relative, which is
/// every form a real server sends. Anything else is `None` — and a `Location`
/// this cannot resolve stops the chain rather than being guessed at, because a
/// guess here is a request to somewhere nobody authorized.
fn resolve(from: &str, location: &str) -> Option<String> {
    if location.starts_with("http://") || location.starts_with("https://") {
        return Some(location.to_owned());
    }
    let scheme = if from.starts_with("https://") {
        "https"
    } else {
        "http"
    };
    if let Some(authority) = location.strip_prefix("//") {
        return Some(format!("{scheme}://{authority}"));
    }
    // A `Location` naming any other scheme — `javascript:`, `file:`, `data:` —
    // is not a relative path with a colon in it. Treating it as one would turn
    // `javascript:alert(1)` into a request for a page of that name; refusing
    // stops the chain, which is the safe reading of a target this cannot make.
    if location
        .split(['/', '?', '#'])
        .next()
        .is_some_and(|head| head.contains(':'))
    {
        return None;
    }
    let host = host_of(from)?;
    // The authority as written, port included: dropping a port would send the
    // next hop somewhere other than where the server pointed.
    let rest = from.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map_or(authority, |(_, it)| it);
    let _ = host;
    if location.starts_with('/') {
        return Some(format!("{scheme}://{authority}{location}"));
    }
    // Relative to the directory the current path is in.
    let path = rest.get(authority.len()..).unwrap_or("/");
    let path = path.split(['?', '#']).next().unwrap_or("/");
    let directory = path.rfind('/').map_or("/", |cut| &path[..=cut]);
    Some(format!("{scheme}://{authority}{directory}{location}"))
}

/// A transport failure as something to do next.
fn describe(error: &ureq::Error) -> String {
    match error {
        // The transport follows none, so this is unreachable in practice; it
        // is matched so a future configuration change cannot make the message
        // disappear silently.
        ureq::Error::TooManyRedirects => {
            format!("the URL redirected more than {MAX_REDIRECTS} times")
        }
        ureq::Error::Timeout(_) => "the request took longer than its deadline".to_owned(),
        other => other.to_string(),
    }
}

/// The host a URL names, or `None` when it is not one this can fetch.
///
/// Public because the capability requirement is built from the same answer:
/// the rule an operator writes is about a host, and the input carries a URL.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Userinfo is dropped before the host is matched against a rule, so
    // `https://evil.test@trusted.test/` cannot be read as the trusted host by
    // one reader and the hostile one by another.
    let host = authority.rsplit_once('@').map_or(authority, |(_, it)| it);
    let host = match host.strip_prefix('[') {
        Some(inner) => inner.split_once(']').map_or(inner, |(it, _)| it),
        None => host.split_once(':').map_or(host, |(it, _)| it),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn looks_like_html(body: &str) -> bool {
    let head = body
        .trim_start()
        .get(..64)
        .unwrap_or(body)
        .to_ascii_lowercase();
    head.starts_with("<!doctype html") || head.starts_with("<html")
}

/// The readable part of an HTML page.
///
/// ponytail: a tag stripper, not a parser. It drops `script` and `style`
/// bodies, removes tags, and collapses whitespace, which is what makes a
/// documentation page fit in a turn. It does not resolve entities beyond the
/// five that matter, follow frames, or run anything. If a page ever needs
/// real extraction — tables, code blocks, reading order — that is a parser,
/// and this is the place to put one.
fn readable(html: &str) -> String {
    let mut text = String::with_capacity(html.len() / 4);
    let bytes = html.as_bytes();
    let mut index = 0;
    let mut in_tag = false;
    while index < bytes.len() {
        if !in_tag {
            if let Some(skipped) =
                skip_block(html, index, "script").or_else(|| skip_block(html, index, "style"))
            {
                index = skipped;
                // A dropped block is a boundary: without this, the word before
                // it and the word after it would run together.
                push_space(&mut text);
                continue;
            }
        }
        match bytes[index] {
            b'<' => in_tag = true,
            b'>' if in_tag => {
                in_tag = false;
                push_space(&mut text);
            }
            _ if in_tag => {}
            byte if byte.is_ascii_whitespace() => push_space(&mut text),
            _ => {
                // Pushed by character rather than by byte so a multi-byte
                // sequence is not split into replacement characters.
                let rest = &html[index..];
                if let Some(character) = rest.chars().next() {
                    text.push(character);
                    index += character.len_utf8();
                    continue;
                }
            }
        }
        index += 1;
    }
    entities(text.trim())
}

/// Where the closing tag of `<name ...> ... </name>` ends, if one starts here.
fn skip_block(html: &str, index: usize, name: &str) -> Option<usize> {
    let rest = html.get(index..)?;
    let lowered = rest.get(..name.len() + 1)?.to_ascii_lowercase();
    if lowered != format!("<{name}") {
        return None;
    }
    let closing = format!("</{name}");
    let end = rest.to_ascii_lowercase().find(&closing)?;
    let after = rest.get(end..)?.find('>')?;
    Some(index + end + after + 1)
}

fn push_space(text: &mut String) {
    if !text.ends_with(' ') && !text.is_empty() {
        text.push(' ');
    }
}

/// The entities a stripped page actually contains. Anything else is left as
/// written rather than guessed at.
fn entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_host_is_read_from_the_url_and_userinfo_cannot_disguise_it() {
        assert_eq!(host_of("https://docs.rs/serde"), Some("docs.rs".to_owned()));
        assert_eq!(
            host_of("http://LOCALHOST:8080/x?y=1"),
            Some("localhost".to_owned()),
            "a host is matched case-insensitively, as DNS is"
        );
        assert_eq!(
            host_of("https://evil.test@trusted.test/page"),
            Some("trusted.test".to_owned()),
            "the host is what the request goes to, not what precedes the @"
        );
        assert_eq!(host_of("https://[::1]:443/"), Some("::1".to_owned()));
        assert_eq!(host_of("file:///etc/passwd"), None);
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn html_is_reduced_to_what_the_page_says() {
        let page = "<!doctype html><html><head><title>Doc</title>\
            <style>body { color: red }</style></head><body>\
            <h1>Install</h1><p>Run <code>cargo add serde</code> &amp; rebuild.</p>\
            <script>alert('no')</script><p>Done.</p></body></html>";
        assert!(looks_like_html(page));
        let text = readable(page);

        assert_eq!(text, "Doc Install Run cargo add serde & rebuild. Done.");
        assert!(!text.contains("color: red"), "style bodies are dropped");
        assert!(!text.contains("alert"), "script bodies are dropped");
        assert!(
            text.len() * 3 < page.len(),
            "extraction is supposed to be the cheap form: {} vs {}",
            text.len(),
            page.len()
        );
    }

    /// A redirect is resolved the way a browser would, so the host the next
    /// hop is checked against is the host the request would actually go to.
    #[test]
    fn a_location_header_resolves_to_the_url_the_next_hop_would_use() {
        let from = "https://docs.rs/serde/latest/serde/index.html?x=1";
        assert_eq!(
            resolve(from, "https://other.test/a").as_deref(),
            Some("https://other.test/a")
        );
        assert_eq!(
            resolve(from, "//other.test/a").as_deref(),
            Some("https://other.test/a"),
            "a protocol-relative target keeps the scheme it was served over"
        );
        assert_eq!(
            resolve(from, "/serde/1.0/").as_deref(),
            Some("https://docs.rs/serde/1.0/")
        );
        assert_eq!(
            resolve(from, "struct.Value.html").as_deref(),
            Some("https://docs.rs/serde/latest/serde/struct.Value.html"),
            "a relative target is resolved against the directory, not the file"
        );
        assert_eq!(
            resolve("http://host.test:8080/a/b", "/c").as_deref(),
            Some("http://host.test:8080/c"),
            "the port is part of where the next hop goes"
        );
        // The host of a resolved hop is what the allowlist is checked against,
        // and userinfo cannot disguise it.
        assert_eq!(
            host_of(&resolve(from, "https://evil.test@docs.rs/x").unwrap()).as_deref(),
            Some("docs.rs")
        );
        assert_eq!(resolve(from, "javascript:alert(1)"), None);
    }

    /// Markup that runs two words together must not silently join them.
    #[test]
    fn tag_boundaries_stay_word_boundaries() {
        assert_eq!(readable("<p>one</p><p>two</p>"), "one two");
        assert_eq!(readable("<b>bo</b>ld"), "bo ld");
        assert_eq!(readable("caf\u{e9} &lt;tag&gt;"), "caf\u{e9} <tag>");
    }
}
