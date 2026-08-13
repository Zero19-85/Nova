//! The newline-delimited JSON envelope, in both directions.
//!
//! One shape for every Nova/Echo conversation — the control channel on 48011
//! and the signaling relay alike. A request carries an `id` and a `command`
//! with its parameters flattened to the top level; the matching response
//! echoes the `id` and is either `ok` with a `result` or not-`ok` with an
//! `error`. Unsolicited server→client messages carry `event` instead of `id`,
//! so a peer can demultiplex without tracking state.
//!
//! Both directions live here because each participant needs both: Nova
//! *serves* this envelope to Echo on the control channel and *speaks* it to
//! the relay as a client. Defining request and response once, rather than
//! mirroring them per module, is what stops the two ends drifting apart.
//!
//! JSON rather than this project's usual hand-rolled binary framing is a
//! deliberate exception: these are control messages at human/UI cadence, never
//! on the frame path, and a hand-written parser on a network-facing port owned
//! by a LocalSystem service is exactly where a typo becomes a vulnerability.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Longest single line accepted from a peer. Control messages are tens to
/// hundreds of bytes; anything approaching this is a peer that has lost
/// framing or is probing, and an unbounded read is an out-of-memory primitive.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

/// A request, as **sent**. Parameters are flattened to the top level:
/// `{"id":1,"command":"set_display","res":"4K"}`.
#[derive(Debug, Clone, Serialize)]
pub struct OutboundRequest {
    pub id: u64,
    pub command: String,
    #[serde(flatten)]
    pub params: Map<String, Value>,
}

/// A request, as **received**. `id` is optional so a peer may fire and forget.
#[derive(Debug, Clone, Deserialize)]
pub struct InboundRequest {
    #[serde(default)]
    pub id: Option<u64>,
    pub command: String,
    #[serde(flatten)]
    pub params: Map<String, Value>,
}

/// A response, as **sent**.
#[derive(Debug, Clone, Serialize)]
pub struct OutboundResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
}

/// A response, as **received**.
#[derive(Debug, Clone, Deserialize)]
pub struct InboundResponse {
    #[serde(default)]
    pub id: Option<u64>,
    pub ok: bool,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<ErrorBody>,
}

/// `code` is the stable machine-readable discriminator — peers branch on it,
/// never on `message`, which is free to change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    #[serde(default)]
    pub code: String,
    #[serde(default)]
    pub message: String,
}

impl std::fmt::Display for ErrorBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl OutboundResponse {
    pub fn ok(id: Option<u64>, result: Value) -> Self {
        Self { id, ok: true, result: Some(result), error: None }
    }
    pub fn err(id: Option<u64>, code: &str, message: impl Into<String>) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(ErrorBody { code: code.to_string(), message: message.into() }),
        }
    }
}

/// Serialize one message and terminate it with the newline that delimits it.
pub fn encode_line<T: Serialize>(msg: &T) -> Result<Vec<u8>, String> {
    let mut body = serde_json::to_vec(msg).map_err(|e| format!("encode: {e}"))?;
    body.push(b'\n');
    Ok(body)
}

/// Parse the first non-empty line of a payload.
///
/// Tolerates trailing lines so a peer that batches several messages into one
/// body cannot desync the reader on the first one.
pub fn decode_line<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, String> {
    let text = std::str::from_utf8(body).map_err(|e| format!("not UTF-8: {e}"))?;
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| "empty payload".to_string())?;
    serde_json::from_str(line).map_err(|e| format!("malformed message: {e}"))
}

/// Read one `\n`-terminated line, refusing to buffer more than `max` bytes.
///
/// Hand-rolled over `fill_buf`/`consume` rather than `AsyncBufReadExt::
/// read_line` because that call is unbounded — a peer that never sends a
/// newline would grow the buffer until the process ran out of memory — and
/// `AsyncReadExt::take`, the obvious cap, returns a reader that no longer
/// implements `AsyncBufRead` and so cannot be combined with line reading at
/// all. Returns `Ok(None)` at a clean EOF.
pub async fn read_line_bounded(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    max: usize,
) -> std::io::Result<Option<String>> {
    use tokio::io::AsyncBufReadExt;

    let mut collected: Vec<u8> = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if collected.is_empty() {
                None
            } else {
                // Peer closed mid-line: surface the partial so it fails JSON
                // parsing with a clear error rather than vanishing.
                Some(String::from_utf8_lossy(&collected).into_owned())
            });
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |i| i);
        collected.extend_from_slice(&available[..take]);
        reader.consume(take + usize::from(newline.is_some()));
        if collected.len() > max {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "line exceeds the maximum length",
            ));
        }
        if newline.is_some() {
            return Ok(Some(String::from_utf8_lossy(&collected).into_owned()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn requests_flatten_params_and_terminate_with_a_newline() {
        let mut params = Map::new();
        params.insert("res".into(), json!("4K"));
        params.insert("hdr".into(), json!(true));
        let line = encode_line(&OutboundRequest {
            id: 3,
            command: "set_display".into(),
            params,
        })
        .unwrap();

        assert!(line.ends_with(b"\n"));
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["id"], 3);
        assert_eq!(v["command"], "set_display");
        assert_eq!(v["res"], "4K", "params sit at the top level, not nested");

        // …and the receiving side reads back exactly that.
        let back: InboundRequest = decode_line(&line).unwrap();
        assert_eq!(back.id, Some(3));
        assert_eq!(back.command, "set_display");
        assert_eq!(back.params["hdr"], json!(true));
    }

    #[test]
    fn responses_round_trip_in_both_shapes() {
        let ok = encode_line(&OutboundResponse::ok(Some(1), json!({"host_id":"h"}))).unwrap();
        let back: InboundResponse = decode_line(&ok).unwrap();
        assert!(back.ok);
        assert_eq!(back.result.unwrap()["host_id"], "h");

        let bad = encode_line(&OutboundResponse::err(Some(2), "denied", "nope")).unwrap();
        let back: InboundResponse = decode_line(&bad).unwrap();
        assert!(!back.ok);
        let e = back.error.unwrap();
        assert_eq!(e.code, "denied");
        assert_eq!(e.message, "nope");

        // Absent optional fields must not be serialized at all.
        assert!(!String::from_utf8(ok).unwrap().contains("error"));
    }

    #[test]
    fn decoding_rejects_junk_without_panicking() {
        assert!(decode_line::<InboundResponse>(b"").is_err());
        assert!(decode_line::<InboundResponse>(b"   \n  \n").is_err());
        assert!(decode_line::<InboundResponse>(b"not json").is_err());
        assert!(decode_line::<InboundResponse>(&[0xff, 0xfe]).is_err());
    }

    #[tokio::test]
    async fn bounded_reads_split_on_newlines_and_refuse_oversized_lines() {
        let data = b"{\"a\":1}\n{\"b\":2}\n".to_vec();
        let mut r = tokio::io::BufReader::new(std::io::Cursor::new(data));
        assert_eq!(read_line_bounded(&mut r, 1024).await.unwrap().unwrap(), "{\"a\":1}");
        assert_eq!(read_line_bounded(&mut r, 1024).await.unwrap().unwrap(), "{\"b\":2}");
        assert!(read_line_bounded(&mut r, 1024).await.unwrap().is_none(), "clean EOF");

        // A peer that never sends a newline must be cut off, not buffered.
        let flood = vec![b'x'; 5000];
        let mut r = tokio::io::BufReader::new(std::io::Cursor::new(flood));
        let e = read_line_bounded(&mut r, 1024).await.unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidData);

        // A final line with no trailing newline is still delivered.
        let mut r = tokio::io::BufReader::new(std::io::Cursor::new(b"tail".to_vec()));
        assert_eq!(read_line_bounded(&mut r, 1024).await.unwrap().unwrap(), "tail");
    }
}
