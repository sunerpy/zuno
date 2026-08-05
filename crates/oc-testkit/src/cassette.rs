//! Replay of the oracle's recorded provider traffic.
//!
//! # The recorded corpus, as it actually exists
//!
//! The oracle records real provider traffic with `@opencode-ai/http-recorder` and
//! commits it under `packages/llm/test/fixtures/recordings/<route>/<name>.json`.
//! At the pinned commit that is 40 files across 11 route directories holding 52
//! HTTP interactions against 11 real endpoints — Anthropic Messages, OpenAI Chat
//! and Responses, Gemini `streamGenerateContent`, Bedrock Converse, Cloudflare
//! Workers AI and its AI Gateway, and four OpenAI-compatible vendors.
//!
//! **These bytes are the only trustworthy description of those wire protocols
//! this project has.** They were produced by real servers answering the real
//! client. A fixture written here instead would only prove that this project
//! agrees with itself — which is exactly how a reference Rust agent shipped an
//! MCP client that framed messages with `Content-Length` headers, a shape no real
//! MCP server speaks, and kept a fully green test suite while doing it, because
//! the Python fixtures it validated against had been written to parse the same
//! wrong framing.
//!
//! ## Format, version 1
//!
//! ```json
//! {
//!   "version": 1,
//!   "metadata": { "name": "anthropic-messages/streams-text",
//!                 "recordedAt": "2026-04-28T21:18:45.535Z",
//!                 "tags": ["prefix:anthropic-messages", "provider:anthropic"] },
//!   "interactions": [
//!     { "transport": "http",
//!       "request":  { "method": "POST", "url": "https://api.anthropic.com/v1/messages",
//!                     "headers": { "content-type": "application/json" },
//!                     "body": "{\"model\":\"…\",\"stream\":true}" },
//!       "response": { "status": 200,
//!                     "headers": { "content-type": "text/event-stream; charset=utf-8" },
//!                     "body": "event: message_start\ndata: {…}\n\n…" } }
//!   ]
//! }
//! ```
//!
//! Points that a Rust reader has to get right, each verified against the corpus:
//!
//! - **Bodies are strings, never nested JSON.** Both request and response bodies
//!   are the serialized payload, so a request body is a JSON *string* containing
//!   JSON.
//! - **A streaming response is one buffered string, not a chunk array.** The
//!   recorder drains the whole stream because `text/event-stream` matches its text
//!   content-type test. Event boundaries survive; network chunk boundaries and
//!   inter-chunk timing do not, and nothing in the file can recover them.
//! - **Binary bodies carry `bodyEncoding: "base64"`.** Only the four
//!   `bedrock-converse` interactions do, because
//!   `application/vnd.amazon.eventstream` is not a text type. Text bodies omit the
//!   field entirely rather than writing `"text"`.
//! - **Headers are a heavily filtered allow-list.** Requests retain only
//!   `content-type`, `accept`, `openai-beta` (plus `anthropic-version`, which the
//!   Anthropic recordings allow explicitly); responses retain only
//!   `content-type`. Authorization and api-key headers are dropped before the
//!   file is written, so a cassette can never be used to assert on credentials.
//! - **`metadata` is optional and comes in two shapes.** 31 files carry
//!   `{name, recordedAt, tags}`; 9 also carry `{provider, route, transport,
//!   model}`. Treat it as an open map.
//! - **Matching is a cursor, not a search.** Request *n* may only be served by
//!   interaction *n*. See [`CassettePlayer::next_http`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::error::{Result, TestkitError};

/// The only cassette format version this harness implements.
pub const CASSETTE_VERSION: u32 = 1;

/// Where the oracle keeps its recordings, relative to the source tree root.
pub const RECORDINGS_SUBPATH: &str = "packages/llm/test/fixtures/recordings";

/// Open metadata recorded alongside the interactions.
pub type CassetteMetadata = BTreeMap<String, serde_json::Value>;

/// The normalized request shape used for matching.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestSnapshot {
    /// HTTP method, upper case as recorded.
    pub method: String,
    /// Fully qualified URL after the recorder's redaction.
    pub url: String,
    /// The retained request headers, lower-cased by the recorder.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The request body, verbatim.
    pub body: String,
}

/// How a recorded response body is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyEncoding {
    /// UTF-8 text. The recorder omits the field rather than writing this, but the
    /// oracle's schema accepts it, so this harness accepts it too.
    Text,
    /// Base64 of the raw bytes, used for non-text content types.
    Base64,
}

/// A recorded response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseSnapshot {
    /// The status code.
    pub status: u16,
    /// The retained response headers, lower-cased by the recorder.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Text body, or base64 when [`Self::body_encoding`] says so.
    pub body: String,
    /// Present only for non-text bodies.
    #[serde(rename = "bodyEncoding", skip_serializing_if = "Option::is_none")]
    pub body_encoding: Option<BodyEncoding>,
}

impl ResponseSnapshot {
    /// The response body as raw bytes, decoding base64 when required.
    ///
    /// # Errors
    ///
    /// [`TestkitError::CassetteBodyEncoding`] when a body claims base64 but does
    /// not decode.
    pub fn decoded_body(&self, cassette: &str, index: usize) -> Result<Vec<u8>> {
        match self.body_encoding {
            Some(BodyEncoding::Base64) => base64::engine::general_purpose::STANDARD
                .decode(self.body.as_bytes())
                .map_err(|e| TestkitError::CassetteBodyEncoding {
                    cassette: cassette.to_owned(),
                    index,
                    detail: e.to_string(),
                }),
            Some(BodyEncoding::Text) | None => Ok(self.body.clone().into_bytes()),
        }
    }

    /// The declared content type, if one was retained.
    #[must_use]
    pub fn content_type(&self) -> Option<&str> {
        self.headers.get("content-type").map(String::as_str)
    }

    /// True when this response was recorded as a server-sent event stream.
    #[must_use]
    pub fn is_sse(&self) -> bool {
        self.content_type()
            .is_some_and(|ct| ct.starts_with("text/event-stream"))
    }

    /// The SSE frames in this body, in recorded order.
    ///
    /// Frames are split on the blank line that terminates an SSE event, and the
    /// `event:` and `data:` field values are returned verbatim. Nothing is parsed
    /// as JSON here: a consumer that needs the payload decodes it itself, so a
    /// malformed payload surfaces at the consumer rather than being swallowed.
    #[must_use]
    pub fn sse_frames(&self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        for block in self.body.split("\n\n") {
            let mut event: Option<String> = None;
            let mut data: Vec<String> = Vec::new();
            for line in block.lines() {
                if let Some(rest) = line.strip_prefix("event:") {
                    event = Some(rest.trim().to_owned());
                } else if let Some(rest) = line.strip_prefix("data:") {
                    data.push(rest.strip_prefix(' ').unwrap_or(rest).to_owned());
                }
            }
            if event.is_some() || !data.is_empty() {
                frames.push(SseFrame {
                    event,
                    data: data.join("\n"),
                });
            }
        }
        frames
    }
}

/// One server-sent event recovered from a recorded stream body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The `event:` field, when the provider sent one. Gemini and the
    /// OpenAI-compatible vendors send only `data:`.
    pub event: Option<String>,
    /// The joined `data:` field values.
    pub data: String,
}

/// A recorded HTTP request/response pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpInteraction {
    /// The request as recorded and redacted.
    pub request: RequestSnapshot,
    /// The response as recorded.
    pub response: ResponseSnapshot,
}

/// Direction of a recorded WebSocket frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameDirection {
    /// Sent by the client under test.
    Client,
    /// Sent by the server.
    Server,
}

/// One recorded WebSocket frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketEvent {
    /// Who sent it.
    pub direction: FrameDirection,
    /// `text` or `binary`, as the oracle's schema tags it.
    pub kind: String,
    /// The frame payload, base64 for binary frames.
    pub body: String,
    /// Present only for binary frames.
    #[serde(rename = "bodyEncoding", skip_serializing_if = "Option::is_none")]
    pub body_encoding: Option<BodyEncoding>,
}

/// A recorded WebSocket conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketInteraction {
    /// The handshake.
    pub open: WebSocketOpen,
    /// The frames, in recorded order. No timing is recorded.
    pub events: Vec<WebSocketEvent>,
}

/// The WebSocket handshake as recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebSocketOpen {
    /// The URL that was opened.
    pub url: String,
    /// The retained handshake headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// One recorded interaction, tagged by transport exactly as the oracle tags it.
///
/// The WebSocket arm is implemented because the format has it and
/// `packages/llm/test/recorded-websocket.ts` uses it, even though no cassette in
/// the recordings root at the pinned commit is a WebSocket one. Parsing a shape
/// the format allows is cheaper than discovering it does not parse later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "lowercase")]
pub enum Interaction {
    /// An HTTP request/response pair.
    Http(HttpInteraction),
    /// A WebSocket conversation.
    Websocket(WebSocketInteraction),
}

/// A parsed cassette file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cassette {
    /// The format version. Always 1 at the pinned commit.
    pub version: u32,
    /// Open metadata; absent in some older fixtures.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: CassetteMetadata,
    /// Every recorded interaction, in order.
    pub interactions: Vec<Interaction>,
}

impl Cassette {
    /// Parse a cassette from JSON text.
    ///
    /// # Errors
    ///
    /// [`TestkitError::CassetteDecode`] on malformed JSON, or
    /// [`TestkitError::CassetteVersion`] on an unimplemented format version.
    pub fn parse(path: &Path, text: &str) -> Result<Self> {
        let cassette: Self =
            serde_json::from_str(text).map_err(|source| TestkitError::CassetteDecode {
                path: path.to_path_buf(),
                source,
            })?;
        if cassette.version != CASSETTE_VERSION {
            return Err(TestkitError::CassetteVersion {
                path: path.to_path_buf(),
                found: cassette.version,
                expected: CASSETTE_VERSION,
            });
        }
        Ok(cassette)
    }

    /// The `metadata.name` the recorder wrote, when present.
    #[must_use]
    pub fn recorded_name(&self) -> Option<&str> {
        self.metadata.get("name")?.as_str()
    }

    /// The `metadata.tags` the recorder wrote.
    #[must_use]
    pub fn tags(&self) -> Vec<&str> {
        self.metadata
            .get("tags")
            .and_then(serde_json::Value::as_array)
            .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_default()
    }

    /// Every HTTP interaction, in recorded order.
    pub fn http_interactions(&self) -> impl Iterator<Item = &HttpInteraction> {
        self.interactions.iter().filter_map(|i| match i {
            Interaction::Http(h) => Some(h),
            Interaction::Websocket(_) => None,
        })
    }

    /// How many HTTP interactions this cassette holds.
    #[must_use]
    pub fn http_count(&self) -> usize {
        self.http_interactions().count()
    }
}

/// Sequential replay of one cassette's HTTP interactions.
///
/// The cursor is the point. The oracle's replay never searches the file for a
/// matching request, so neither does this: request *n* is served by interaction
/// *n* or the replay fails. That is what makes an extra, missing, or reordered
/// provider call a test failure instead of an invisible behavioural difference.
#[derive(Debug)]
pub struct CassettePlayer {
    name: String,
    path: PathBuf,
    cassette: Cassette,
    http: Vec<HttpInteraction>,
    cursor: usize,
}

impl CassettePlayer {
    /// Load `<root>/<name>.json`.
    ///
    /// # Errors
    ///
    /// [`TestkitError::InvalidCassetteName`] for a name that escapes `root`,
    /// [`TestkitError::Io`] when the file cannot be read, and the decode errors
    /// from [`Cassette::parse`].
    pub fn load(root: impl AsRef<Path>, name: &str) -> Result<Self> {
        let path = cassette_path(root.as_ref(), name)?;
        let text = std::fs::read_to_string(&path)
            .map_err(|e| TestkitError::io("read cassette", path.clone(), e))?;
        let cassette = Cassette::parse(&path, &text)?;
        Ok(Self {
            name: name.to_owned(),
            path,
            http: cassette.http_interactions().cloned().collect(),
            cassette,
            cursor: 0,
        })
    }

    /// Load a cassette from the oracle's recordings root.
    ///
    /// # Errors
    ///
    /// [`TestkitError::RecordingsRootNotFound`] when no oracle tree is available,
    /// plus the errors of [`Self::load`].
    pub fn from_oracle(name: &str) -> Result<Self> {
        Self::load(recordings_root()?, name)
    }

    /// The name this player was loaded under.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The file this player read.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The parsed cassette.
    #[must_use]
    pub fn cassette(&self) -> &Cassette {
        &self.cassette
    }

    /// How many HTTP interactions have not been consumed yet.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.http.len().saturating_sub(self.cursor)
    }

    /// The next interaction without consuming it.
    #[must_use]
    pub fn peek(&self) -> Option<&HttpInteraction> {
        self.http.get(self.cursor)
    }

    /// Serve the next interaction, requiring `incoming` to match it.
    ///
    /// # Errors
    ///
    /// [`TestkitError::CassetteExhausted`] when the code under test made more
    /// calls than were recorded, or [`TestkitError::CassetteMismatch`] when the
    /// call it made is not the call that was recorded. The cursor does not advance
    /// on a mismatch, mirroring the oracle.
    pub fn next_http(&mut self, incoming: &RequestSnapshot) -> Result<&HttpInteraction> {
        let index = self.cursor;
        let recorded = self
            .http
            .get(index)
            .ok_or_else(|| TestkitError::CassetteExhausted {
                cassette: self.name.clone(),
                requested: index + 1,
                recorded: self.http.len(),
            })?;
        if canonical_snapshot(&recorded.request) != canonical_snapshot(incoming) {
            return Err(TestkitError::CassetteMismatch {
                cassette: self.name.clone(),
                index: index + 1,
                recorded: canonical_snapshot(&recorded.request),
                incoming: canonical_snapshot(incoming),
            });
        }
        self.cursor += 1;
        Ok(recorded)
    }

    /// Serve the next interaction without matching the request.
    ///
    /// For consumers that are exercising response decoding rather than request
    /// construction. Prefer [`Self::next_http`] whenever the outbound request is
    /// part of what is under test.
    ///
    /// # Errors
    ///
    /// [`TestkitError::CassetteExhausted`] when nothing is left.
    pub fn next_unchecked(&mut self) -> Result<&HttpInteraction> {
        let index = self.cursor;
        let recorded = self
            .http
            .get(index)
            .ok_or_else(|| TestkitError::CassetteExhausted {
                cassette: self.name.clone(),
                requested: index + 1,
                recorded: self.http.len(),
            })?;
        self.cursor += 1;
        Ok(recorded)
    }

    /// Assert every recorded interaction was consumed.
    ///
    /// # Errors
    ///
    /// [`TestkitError::CassetteUnused`] when the code under test made fewer calls
    /// than the oracle did — the mirror of exhaustion, and just as much a
    /// behavioural difference.
    pub fn finish(&self) -> Result<()> {
        if self.remaining() == 0 {
            return Ok(());
        }
        Err(TestkitError::CassetteUnused {
            cassette: self.name.clone(),
            unused: self.remaining(),
            recorded: self.http.len(),
        })
    }
}

/// The oracle's recordings root.
///
/// # Errors
///
/// [`TestkitError::RecordingsRootNotFound`] when no oracle source tree is
/// reachable.
pub fn recordings_root() -> Result<PathBuf> {
    let tree = crate::oracle::locate_source_tree();
    match tree {
        Some(tree) => {
            let root = tree.join(RECORDINGS_SUBPATH);
            if root.is_dir() {
                Ok(root)
            } else {
                Err(TestkitError::RecordingsRootNotFound {
                    searched: vec![root],
                    remedy: format!(
                        "the located opencode tree has no {RECORDINGS_SUBPATH}; point \
                         OC_TESTKIT_ORACLE_SOURCE at a full checkout"
                    ),
                })
            }
        }
        None => Err(TestkitError::RecordingsRootNotFound {
            searched: vec![PathBuf::from(env!("CARGO_MANIFEST_DIR"))],
            remedy: "set OC_TESTKIT_ORACLE_SOURCE to a checkout of the opencode source tree"
                .to_owned(),
        }),
    }
}

/// Every cassette name under `root`, in sorted order.
///
/// Names are returned in the `<route>/<file>` form [`CassettePlayer::load`] takes.
///
/// # Errors
///
/// [`TestkitError::Io`] when `root` cannot be walked.
pub fn list_cassettes(root: impl AsRef<Path>) -> Result<Vec<String>> {
    let root = root.as_ref();
    let mut names = Vec::new();
    collect(root, root, &mut names)?;
    names.sort();
    Ok(names)
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| TestkitError::io("list recordings", dir.to_path_buf(), e))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| TestkitError::io("read recordings entry", dir.to_path_buf(), e))?;
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e == "json")
            && let Ok(rel) = path.strip_prefix(root)
        {
            let name = rel.with_extension("");
            out.push(name.to_string_lossy().replace('\\', "/"));
        }
    }
    Ok(())
}

/// Resolve `<root>/<name>.json`, rejecting any name that escapes `root`.
///
/// Mirrors the oracle's own guard in `packages/http-recorder/src/cassette.ts`.
fn cassette_path(root: &Path, name: &str) -> Result<PathBuf> {
    if name.is_empty() {
        return Err(TestkitError::InvalidCassetteName {
            name: name.to_owned(),
            reason: "empty",
        });
    }
    let candidate = Path::new(name);
    if candidate.is_absolute() || name.starts_with('/') || name.contains(':') {
        return Err(TestkitError::InvalidCassetteName {
            name: name.to_owned(),
            reason: "must be relative to the recordings root",
        });
    }
    if name.split(['/', '\\']).any(|seg| seg == "..") {
        return Err(TestkitError::InvalidCassetteName {
            name: name.to_owned(),
            reason: "must not contain a `..` segment",
        });
    }
    Ok(root.join(format!("{name}.json")))
}

/// The oracle's canonical request form, ported from
/// `packages/http-recorder/src/matching.ts`.
///
/// Object keys are sorted recursively; array order is preserved; a body that is
/// not JSON is compared as an exact string. There is no hashing anywhere in the
/// oracle's matcher, so there is none here.
#[must_use]
pub fn canonical_snapshot(snapshot: &RequestSnapshot) -> String {
    let body = match serde_json::from_str::<serde_json::Value>(&snapshot.body) {
        Ok(value) => canonicalize(&value),
        Err(_) => serde_json::Value::String(snapshot.body.clone()),
    };
    let mut headers = serde_json::Map::new();
    for (k, v) in &snapshot.headers {
        headers.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    let canonical = serde_json::json!({
        "method": snapshot.method,
        "url": snapshot.url,
        "headers": canonicalize(&serde_json::Value::Object(headers)),
        "body": body,
    });
    serde_json::to_string(&canonicalize(&canonical)).unwrap_or_default()
}

/// Recursively sort object keys, leaving arrays and scalars alone.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize).collect())
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::with_capacity(map.len());
            for key in keys {
                out.insert(key.clone(), canonicalize(&map[key]));
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(body: &str) -> RequestSnapshot {
        RequestSnapshot {
            method: "POST".to_owned(),
            url: "https://api.anthropic.com/v1/messages".to_owned(),
            headers: [("content-type".to_owned(), "application/json".to_owned())]
                .into_iter()
                .collect(),
            body: body.to_owned(),
        }
    }

    // ---------------------------------------------------------------------
    // Against the real corpus. These tests read the oracle's own recordings, so
    // they fail if this crate's understanding of the format drifts from the file
    // that a real provider produced.
    // ---------------------------------------------------------------------

    #[test]
    fn every_recorded_cassette_in_the_oracle_tree_parses() {
        let root = recordings_root().expect("the oracle recordings root");
        let names = list_cassettes(&root).expect("list recordings");
        assert!(
            names.len() >= 40,
            "expected at least the 40 recordings present at the pinned commit, found {}",
            names.len()
        );
        let mut http = 0usize;
        let mut base64_bodies = 0usize;
        let mut sse_bodies = 0usize;
        for name in &names {
            let player = CassettePlayer::load(&root, name)
                .unwrap_or_else(|e| panic!("cassette {name} failed to parse: {e}"));
            assert_eq!(player.cassette().version, CASSETTE_VERSION);
            assert!(
                !player.cassette().interactions.is_empty(),
                "cassette {name} records nothing"
            );
            for (i, interaction) in player.cassette().http_interactions().enumerate() {
                http += 1;
                assert_eq!(interaction.request.method, "POST", "in {name}");
                assert!(interaction.request.url.starts_with("https://"), "in {name}");
                let bytes = interaction
                    .response
                    .decoded_body(name, i + 1)
                    .unwrap_or_else(|e| panic!("cassette {name} body: {e}"));
                assert!(!bytes.is_empty(), "cassette {name} has an empty body");
                if interaction.response.body_encoding == Some(BodyEncoding::Base64) {
                    base64_bodies += 1;
                }
                if interaction.response.is_sse() {
                    sse_bodies += 1;
                }
            }
        }
        assert!(
            http >= 52,
            "expected at least 52 HTTP interactions, found {http}"
        );
        assert_eq!(
            base64_bodies, 4,
            "only the four bedrock-converse eventstream bodies are base64 at the pinned commit"
        );
        assert!(
            sse_bodies >= 45,
            "most recorded responses are SSE, found {sse_bodies}"
        );
    }

    #[test]
    fn a_real_anthropic_cassette_decodes_to_its_recorded_sse_frames() {
        let mut player = CassettePlayer::from_oracle("anthropic-messages/streams-text")
            .expect("the pinned corpus contains this recording");
        assert_eq!(player.remaining(), 1);
        let interaction = player.next_unchecked().expect("one interaction");
        assert_eq!(interaction.request.method, "POST");
        assert_eq!(
            interaction
                .request
                .headers
                .get("anthropic-version")
                .map(String::as_str),
            Some("2023-06-01")
        );
        assert!(interaction.response.is_sse());
        let frames = interaction.response.sse_frames();
        let events: Vec<&str> = frames.iter().filter_map(|f| f.event.as_deref()).collect();
        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start",
                "ping",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ],
            "the recorded Anthropic event order is the protocol's, not ours"
        );
        assert!(frames[3].data.contains("\"text\":\"Hello!\""));
        player.finish().expect("everything consumed");
    }

    #[test]
    fn a_real_bedrock_cassette_decodes_its_base64_eventstream() {
        let mut player = CassettePlayer::from_oracle("bedrock-converse/streams-text")
            .expect("the pinned corpus contains this recording");
        let interaction = player.next_unchecked().expect("one interaction");
        assert_eq!(
            interaction.response.content_type(),
            Some("application/vnd.amazon.eventstream")
        );
        assert_eq!(
            interaction.response.body_encoding,
            Some(BodyEncoding::Base64)
        );
        let bytes = interaction
            .response
            .decoded_body("bedrock-converse/streams-text", 1)
            .expect("base64 decodes");
        // The AWS eventstream frame prelude is a 4-byte total length; the recorded
        // body must therefore start with a length that matches what it carries.
        let declared = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        assert!(
            declared > 0 && declared <= bytes.len(),
            "declared {declared} of {}",
            bytes.len()
        );
        assert!(!interaction.response.is_sse());
    }

    #[test]
    fn a_multi_interaction_cassette_replays_in_order() {
        let mut player =
            CassettePlayer::from_oracle("anthropic-messages/claude-opus-4-7-drives-a-tool-loop")
                .expect("the pinned corpus contains this recording");
        assert_eq!(player.remaining(), 2, "the tool loop records two calls");
        let first = player.next_unchecked().expect("first").request.body.clone();
        let second = player
            .next_unchecked()
            .expect("second")
            .request
            .body
            .clone();
        assert_ne!(first, second, "the second call carries the tool result");
        assert!(second.len() > first.len(), "the conversation grows");
        player.finish().expect("both consumed");
    }

    // ---------------------------------------------------------------------
    // Cursor and matcher semantics.
    // ---------------------------------------------------------------------

    #[test]
    fn the_cursor_matches_a_request_and_advances() {
        let mut player =
            CassettePlayer::from_oracle("anthropic-messages/streams-text").expect("recording");
        let recorded = player.peek().expect("one interaction").request.clone();
        player
            .next_http(&recorded)
            .expect("the recorded request matches itself");
        assert_eq!(player.remaining(), 0);
    }

    #[test]
    fn a_differing_request_is_a_mismatch_and_the_cursor_holds() {
        let mut player =
            CassettePlayer::from_oracle("anthropic-messages/streams-text").expect("recording");
        let mut wrong = player.peek().expect("one interaction").request.clone();
        wrong.body = wrong.body.replace("\"temperature\":0", "\"temperature\":1");
        let err = player
            .next_http(&wrong)
            .expect_err("a changed body must not match");
        assert!(
            matches!(err, TestkitError::CassetteMismatch { index: 1, .. }),
            "{err:?}"
        );
        assert_eq!(
            player.remaining(),
            1,
            "a mismatch must not consume the interaction"
        );
    }

    #[test]
    fn running_past_the_end_is_an_error_not_a_wrap_around() {
        let mut player =
            CassettePlayer::from_oracle("anthropic-messages/streams-text").expect("recording");
        player.next_unchecked().expect("first");
        let err = player.next_unchecked().expect_err("nothing is left");
        assert!(
            matches!(
                err,
                TestkitError::CassetteExhausted {
                    requested: 2,
                    recorded: 1,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn finishing_early_is_reported() {
        let player =
            CassettePlayer::from_oracle("anthropic-messages/claude-opus-4-7-drives-a-tool-loop")
                .expect("recording");
        let err = player
            .finish()
            .expect_err("two interactions were recorded, none consumed");
        assert!(
            matches!(err, TestkitError::CassetteUnused { unused: 2, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn canonicalization_sorts_keys_but_not_arrays() {
        assert_eq!(
            canonical_snapshot(&snapshot(r#"{"b":1,"a":[3,1,2]}"#)),
            canonical_snapshot(&snapshot(r#"{"a":[3,1,2],"b":1}"#))
        );
        assert_ne!(
            canonical_snapshot(&snapshot(r#"{"a":[3,1,2]}"#)),
            canonical_snapshot(&snapshot(r#"{"a":[1,2,3]}"#)),
            "array order is semantic in every provider protocol"
        );
        assert_ne!(
            canonical_snapshot(&snapshot(r#"{"a":1}"#)),
            canonical_snapshot(&snapshot(r#"{"a":2}"#))
        );
    }

    #[test]
    fn a_non_json_body_is_compared_exactly() {
        assert_eq!(
            canonical_snapshot(&snapshot("not json")),
            canonical_snapshot(&snapshot("not json"))
        );
        assert_ne!(
            canonical_snapshot(&snapshot("not json")),
            canonical_snapshot(&snapshot("not jsen"))
        );
    }

    #[test]
    fn a_cassette_name_cannot_escape_the_recordings_root() {
        let root = Path::new("/recordings");
        for bad in ["", "/abs/name", "../outside", "a/../../outside"] {
            assert!(
                cassette_path(root, bad).is_err(),
                "{bad:?} should not be addressable"
            );
        }
        assert_eq!(
            cassette_path(root, "anthropic-messages/streams-text").expect("valid"),
            PathBuf::from("/recordings/anthropic-messages/streams-text.json")
        );
    }

    #[test]
    fn an_unimplemented_format_version_is_refused() {
        let err = Cassette::parse(Path::new("/x.json"), r#"{"version":2,"interactions":[]}"#)
            .expect_err("version 2 is not implemented");
        assert!(
            matches!(
                err,
                TestkitError::CassetteVersion {
                    found: 2,
                    expected: 1,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[test]
    fn a_websocket_interaction_parses_even_though_none_is_recorded_yet() {
        let cassette = Cassette::parse(
            Path::new("/ws.json"),
            r#"{"version":1,"interactions":[{"transport":"websocket",
                 "open":{"url":"wss://example.test/s","headers":{}},
                 "events":[{"direction":"client","kind":"text","body":"hi"},
                           {"direction":"server","kind":"binary","body":"AAEC","bodyEncoding":"base64"}]}]}"#,
        )
        .expect("the format allows websocket interactions");
        assert_eq!(cassette.http_count(), 0);
        let Interaction::Websocket(ws) = &cassette.interactions[0] else {
            panic!("expected a websocket interaction");
        };
        assert_eq!(ws.events.len(), 2);
        assert_eq!(ws.events[0].direction, FrameDirection::Client);
        assert_eq!(ws.events[1].body_encoding, Some(BodyEncoding::Base64));
    }

    #[test]
    fn sse_frames_handle_data_only_streams() {
        let response = ResponseSnapshot {
            status: 200,
            headers: [("content-type".to_owned(), "text/event-stream".to_owned())]
                .into_iter()
                .collect(),
            body: "data: {\"a\":1}\n\ndata: [DONE]\n\n".to_owned(),
            body_encoding: None,
        };
        let frames = response.sse_frames();
        assert_eq!(frames.len(), 2);
        assert!(frames[0].event.is_none());
        assert_eq!(frames[0].data, "{\"a\":1}");
        assert_eq!(frames[1].data, "[DONE]");
    }
}
