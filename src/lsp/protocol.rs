// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Mark Wells <contact@markwells.dev>

use anyhow::{Context, Result};
use bytes::{Buf, BytesMut};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, warn};

const fn default_null() -> serde_json::Value {
    serde_json::Value::Null
}

/// An LSP request message.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RequestMessage {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The request ID.
    pub id: RequestId,
    /// The method name.
    pub method: String,
    /// The request parameters.
    #[serde(default = "default_null")]
    pub params: serde_json::Value,
}

/// An LSP response message.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseMessage {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The request ID, if any.
    pub id: Option<RequestId>,
    /// The result of the request, if successful.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// The error, if the request failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// An LSP notification message.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct NotificationMessage {
    /// The JSON-RPC version.
    pub jsonrpc: String,
    /// The method name.
    pub method: String,
    /// The notification parameters.
    #[serde(default = "default_null")]
    pub params: serde_json::Value,
}

/// An LSP request or response ID.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Hash)]
#[serde(untagged)]
pub enum RequestId {
    /// A numeric ID.
    Number(i64),
    /// A string ID.
    String(String),
}

/// An LSP response error.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ResponseError {
    /// The error code.
    pub code: i64,
    /// The error message.
    pub message: String,
    /// Additional error data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Error returned by [`LspServer::on_request`](super::server::LspServer::on_request())
/// for server requests the client cannot handle.
///
/// Connection translates this into a JSON-RPC error response.
/// `LspServer` never constructs the response envelope.
#[derive(Debug)]
pub struct RpcError {
    /// JSON-RPC error code (e.g., -32601 for `MethodNotFound`).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        Self::Number(n)
    }
}

/// A framing-valid message extracted from the wire buffer.
///
/// `wire_len` is the total number of buffer bytes the frame consumed
/// (header block through body end), computed from the frame's own
/// `Content-Length` arithmetic — the independent consumption claim the
/// [`FramePump`] byte-conservation check verifies against.
#[derive(Debug)]
pub struct Frame {
    /// The message body (header block stripped).
    pub body: String,
    /// Total wire bytes consumed: header block length + `Content-Length`.
    pub wire_len: usize,
}

/// Helper to parse the Content-Length header and body from a buffer.
///
/// # Errors
///
/// Returns an error if:
/// - Headers are not valid UTF-8.
/// - Content-Length is invalid or missing.
/// - The body is not valid UTF-8.
pub fn try_parse_message(buffer: &mut BytesMut) -> Result<Option<String>> {
    Ok(try_parse_frame(buffer)?.map(|frame| frame.body))
}

/// Like [`try_parse_message`], but also reports the frame's wire length —
/// the byte count the parse consumed, per its own header arithmetic.
///
/// # Errors
///
/// Same contract as [`try_parse_message`]; on error the buffer is left
/// unchanged for [`resync_to_next_message`].
pub fn try_parse_frame(buffer: &mut BytesMut) -> Result<Option<Frame>> {
    let mut headers_end = None;
    let mut content_length = None;

    // Scan for \r\n\r\n
    for i in 0..buffer.len().saturating_sub(3) {
        if &buffer[i..i + 4] == b"\r\n\r\n" {
            headers_end = Some(i + 4);

            // Parse headers
            let headers_str =
                std::str::from_utf8(&buffer[0..i]).context("Failed to parse headers as UTF-8")?;

            for line in headers_str.lines() {
                if line.to_ascii_lowercase().starts_with("content-length:") {
                    let parts: Vec<&str> = line.split(':').collect();
                    if parts.len() == 2 {
                        content_length = Some(parts[1].trim().parse::<usize>()?);
                    }
                }
            }
            break;
        }
    }

    if let Some(header_len) = headers_end {
        // A complete header block terminated by CRLFCRLF, but no parseable
        // `Content-Length` in it, is a *framing error*, not an under-read (bug
        // 95). Reporting `Ok(None)` here would tell the reader "need more data";
        // when no more data follows, every subsequent frame is stranded in the
        // buffer forever. Return an error so the reader resyncs past this bogus
        // block. The buffer is left unchanged for resync.
        let Some(content_len) = content_length else {
            anyhow::bail!("header block has no Content-Length");
        };

        let total_len = header_len + content_len;

        if buffer.len() >= total_len {
            // Validate body UTF-8 before consuming the buffer so that
            // on any error the buffer remains unchanged for resync.
            let message = std::str::from_utf8(&buffer[header_len..total_len])
                .context("Failed to parse message body as UTF-8")?
                .to_string();
            buffer.advance(total_len);
            return Ok(Some(Frame {
                body: message,
                wire_len: total_len,
            }));
        }
    }

    Ok(None)
}

/// Discard bytes up to the next `Content-Length:` header in the buffer.
///
/// Called after [`try_parse_message`] returns an error to skip past corrupt
/// data and resynchronize with the next LSP message boundary. Scans forward
/// from byte 1 (byte 0 belongs to the current broken message) for a
/// case-insensitive `Content-Length:` prefix. If found, the buffer is
/// advanced to that position. If no full header is found, everything is
/// discarded EXCEPT a trailing case-insensitive prefix of the needle: a
/// read boundary can land mid-`Content-Length:`, and that tail may be the
/// next real frame's header still arriving — clearing it destroys the frame
/// (CI-found proptest counterexample: `\r\n\r\n` garbage completes a bogus
/// header block while the next header's first byte sits at the buffer
/// tail). The kept tail is capped at `buffer.len() - 1`, so resync always
/// discards at least one byte and always makes progress.
///
/// Returns the number of bytes discarded — the resync path's own
/// consumption claim, verified by the [`FramePump`] conservation check.
pub fn resync_to_next_message(buffer: &mut BytesMut) -> usize {
    let needle = b"content-length:";
    for i in 1..=buffer.len().saturating_sub(needle.len()) {
        if buffer[i..i + needle.len()]
            .iter()
            .zip(needle.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            buffer.advance(i);
            return i;
        }
    }
    // No full header found — keep the longest tail that could still grow
    // into one, discard the rest.
    let max_tail = needle.len().min(buffer.len().saturating_sub(1));
    let keep = (1..=max_tail)
        .rev()
        .find(|&len| {
            buffer[buffer.len() - len..]
                .iter()
                .zip(needle.iter())
                .all(|(a, b)| a.to_ascii_lowercase() == *b)
        })
        .unwrap_or(0);
    let discarded = buffer.len() - keep;
    buffer.advance(discarded);
    discarded
}

/// The reader's framing pump: buffer, parse/resync drain loop, and a
/// byte-conservation audit.
///
/// Extracted from `reader_loop` so the *actual* production pump — not a
/// test replica — sits under the fragmentation property tests (bug 95).
///
/// Every byte handed to [`ingest`](Self::ingest) must end up in exactly one
/// of three places: consumed by a parsed frame (per the frame's own
/// `Content-Length` arithmetic), consumed by resync (per the resync scan's
/// own count), or still buffered awaiting more data. The pump cross-checks
/// that identity — `total_bytes_read == total_bytes_accounted + buffered` —
/// after every drain step. A divergence means the reader lost or duplicated
/// bytes; it emits one `error!` (the interrupt is earned: a reader that
/// drops bytes silently corrupts every downstream pipeline) with both
/// counters, then resyncs and re-trues the accounting rather than continue
/// silently.
pub struct FramePump {
    server_name: String,
    scope_root: Option<String>,
    buffer: BytesMut,
    /// Total bytes ever appended by `ingest`.
    total_read: u64,
    /// Total bytes accounted as consumed (parsed frames + resync discards).
    total_accounted: u64,
    /// Conservation violations detected (each emitted one `error!`).
    divergences: u64,
}

impl FramePump {
    /// Creates a pump for one server connection. `server_name` and
    /// `scope_root` tag the pump's tracing events.
    #[must_use]
    pub fn new(server_name: String, scope_root: Option<String>) -> Self {
        Self {
            server_name,
            scope_root,
            buffer: BytesMut::with_capacity(8192),
            total_read: 0,
            total_accounted: 0,
            divergences: 0,
        }
    }

    /// Appends one read chunk and drains every complete message.
    ///
    /// Returns the framing-valid message bodies in wire order. Framing
    /// errors are logged and resynced past, exactly as the reader loop
    /// always has; the byte-conservation identity is checked after every
    /// drain step.
    pub fn ingest(&mut self, chunk: &[u8]) -> Vec<String> {
        self.total_read += chunk.len() as u64;
        self.buffer.extend_from_slice(chunk);

        let mut out = Vec::new();
        loop {
            match try_parse_frame(&mut self.buffer) {
                Ok(None) => break, // Need more data
                Ok(Some(frame)) => {
                    self.total_accounted += frame.wire_len as u64;
                    out.push(frame.body);
                }
                Err(e) => {
                    let dump_len = self.buffer.len().min(128);
                    warn!(
                        server = self.server_name.as_str(),
                        source = crate::source::Source::LspProtocol.as_str(),
                        scope_root = self.scope_root.as_deref(),
                        "malformed LSP message from {}, resynchronizing: {e}",
                        self.server_name,
                    );
                    debug!(
                        server = self.server_name.as_str(),
                        buffer_len = self.buffer.len(),
                        "buffer head (hex): {:02x?}",
                        &self.buffer[..dump_len]
                    );
                    self.total_accounted += resync_to_next_message(&mut self.buffer) as u64;
                }
            }
            self.check_conservation();
        }
        self.check_conservation();
        out
    }

    /// Verifies the byte-conservation identity and recovers on divergence.
    ///
    /// `total_bytes_read` must equal `total_bytes_accounted` plus the bytes
    /// still buffered. On divergence: one `error!` with both counters, the
    /// delta, and the buffer length; then resync past whatever the buffer
    /// currently holds and re-true the accounting so a single fault reports
    /// once instead of on every subsequent iteration.
    fn check_conservation(&mut self) {
        let accounted = self.total_accounted + self.buffer.len() as u64;
        if accounted == self.total_read {
            return;
        }
        self.divergences += 1;
        let delta = i128::from(self.total_read) - i128::from(accounted);
        error!(
            server = self.server_name.as_str(),
            source = crate::source::Source::LspDispatch.as_str(),
            scope_root = self.scope_root.as_deref(),
            total_bytes_read = self.total_read,
            total_bytes_accounted = accounted,
            delta,
            buffer_len = self.buffer.len(),
            "LSP reader byte-conservation violated for {}: bytes were lost or \
             double-consumed on the stdout pipe; resynchronizing",
            self.server_name,
        );
        resync_to_next_message(&mut self.buffer);
        self.total_accounted = self.total_read.saturating_sub(self.buffer.len() as u64);
    }
}

#[cfg(test)]
impl FramePump {
    /// Number of conservation violations detected so far.
    pub(crate) const fn divergences(&self) -> u64 {
        self.divergences
    }

    /// Bytes currently buffered awaiting more data.
    pub(crate) fn buffered_len(&self) -> usize {
        self.buffer.len()
    }

    /// Test-only fault injection: silently discard one buffered byte
    /// without accounting for it — simulates a byte-conservation bug so
    /// tests can prove the divergence check fires.
    pub(crate) fn lose_one_buffered_byte_for_test(&mut self) {
        if !self.buffer.is_empty() {
            self.buffer.advance(1);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for readable assertions"
)]
mod tests {
    use super::*;
    use proptest::prelude::{prop, prop_assert, prop_assert_eq, proptest};

    /// Frame a single body into an LSP wire message (`Content-Length` header +
    /// CRLFCRLF + body), byte-for-byte as [`super::super::Connection::send_message`]
    /// does.
    fn frame(body: &str) -> Vec<u8> {
        let mut v = Vec::with_capacity(body.len() + 24);
        v.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
        v.extend_from_slice(body.as_bytes());
        v
    }

    /// Drive the REAL framing pump `reader_loop` runs — [`FramePump`], not a
    /// replica (bug 95): feed `stream` one read-chunk at a time (chunk
    /// boundaries at `cut_points`) through [`FramePump::ingest`], exactly as
    /// the reader does per `read()`.
    ///
    /// Returns the ordered list of message bodies the framing layer emitted —
    /// i.e. what the reader would hand to the JSON parse. This is the reader's
    /// buffer reassembly under an adversarial partition of the byte stream,
    /// with the byte-conservation audit armed (any divergence fails the test).
    fn drive_reader(stream: &[u8], cut_points: &[usize]) -> Vec<String> {
        let mut pump = FramePump::new("test-server".to_string(), None);
        let mut out = Vec::new();
        let mut pos = 0usize;

        // Chunk boundaries: the sorted cut points, then the end of stream.
        let mut cuts: Vec<usize> = cut_points
            .iter()
            .map(|&c| c.min(stream.len()))
            .filter(|&c| c > 0)
            .collect();
        cuts.sort_unstable();
        cuts.dedup();
        cuts.push(stream.len());

        for cut in cuts {
            if cut < pos {
                continue;
            }
            out.extend(pump.ingest(&stream[pos..cut]));
            pos = cut;
        }
        assert_eq!(
            pump.divergences(),
            0,
            "the pump's byte-conservation audit diverged while reassembling a stream"
        );
        out
    }

    /// Like [`drive_reader`], but applies the reader's post-framing step: each
    /// emitted body is parsed as JSON and, on failure, DROPPED (the reader's
    /// `Ok(Some)` branch does `continue`). Returns only the bodies the reader
    /// actually dispatches to handlers — the ground truth for "did a pipeline
    /// receive this message".
    fn drive_reader_dispatched(stream: &[u8], cut_points: &[usize]) -> Vec<String> {
        drive_reader(stream, cut_points)
            .into_iter()
            .filter(|body| serde_json::from_str::<serde_json::Value>(body).is_ok())
            .collect()
    }

    #[test]
    fn test_parse_complete_message() -> Result<()> {
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = BytesMut::from(raw.as_str());

        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, Some(body.to_string()));
        assert!(buffer.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_incomplete_header() -> Result<()> {
        let mut buffer = BytesMut::from("Content-Length: 10\r\n");
        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, None);
        Ok(())
    }

    #[test]
    fn test_parse_incomplete_body() -> Result<()> {
        let mut buffer = BytesMut::from("Content-Length: 100\r\n\r\n{\"partial\":");
        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, None);
        Ok(())
    }

    #[test]
    fn test_parse_multiple_messages() -> Result<()> {
        let body1 = r#"{"jsonrpc":"2.0","id":1}"#;
        let body2 = r#"{"jsonrpc":"2.0","id":2}"#;
        let raw = format!(
            "Content-Length: {}\r\n\r\n{}Content-Length: {}\r\n\r\n{}",
            body1.len(),
            body1,
            body2.len(),
            body2
        );
        let mut buffer = BytesMut::from(raw.as_str());

        let result1 = try_parse_message(&mut buffer)?;
        assert_eq!(result1, Some(body1.to_string()));

        let result2 = try_parse_message(&mut buffer)?;
        assert_eq!(result2, Some(body2.to_string()));

        assert!(buffer.is_empty());
        Ok(())
    }

    #[test]
    fn test_parse_case_insensitive_header() -> Result<()> {
        let body = r#"{"test":true}"#;
        let raw = format!("content-length: {}\r\n\r\n{}", body.len(), body);
        let mut buffer = BytesMut::from(raw.as_str());

        let result = try_parse_message(&mut buffer)?;
        assert_eq!(result, Some(body.to_string()));
        Ok(())
    }

    #[test]
    fn test_request_id_number() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":42,"method":"test"}"#;
        let msg: RequestMessage = serde_json::from_str(json)?;
        assert_eq!(msg.id, RequestId::Number(42));
        Ok(())
    }

    #[test]
    fn test_request_id_string() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":"abc-123","method":"test"}"#;
        let msg: RequestMessage = serde_json::from_str(json)?;
        assert_eq!(msg.id, RequestId::String("abc-123".to_string()));
        Ok(())
    }

    #[test]
    fn test_response_with_result() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#;
        let msg: ResponseMessage = serde_json::from_str(json)?;
        assert!(msg.result.is_some());
        assert!(msg.error.is_none());
        Ok(())
    }

    #[test]
    fn test_response_with_error() -> Result<()> {
        let json =
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;
        let msg: ResponseMessage = serde_json::from_str(json)?;
        assert!(msg.result.is_none());
        assert!(msg.error.is_some());
        assert_eq!(msg.error.context("missing error")?.code, -32600);
        Ok(())
    }

    #[test]
    fn test_response_null_result() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","id":1,"result":null}"#;
        let msg: ResponseMessage = serde_json::from_str(json)?;
        // null deserializes to None for Option<Value>
        assert!(msg.result.is_none());
        Ok(())
    }

    #[test]
    fn test_notification_no_id() -> Result<()> {
        let json = r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#;
        let msg: NotificationMessage = serde_json::from_str(json)?;
        assert_eq!(msg.method, "initialized");
        Ok(())
    }

    // --- Error recovery tests ---

    #[test]
    fn test_resync_after_corrupt_content_length() {
        // Corrupt Content-Length value followed by a valid message.
        let valid_body = r#"{"jsonrpc":"2.0","id":1}"#;
        let raw = format!(
            "Content-Length: abc\r\n\r\ngarbage\
             Content-Length: {}\r\n\r\n{}",
            valid_body.len(),
            valid_body
        );
        let mut buffer = BytesMut::from(raw.as_str());

        // First parse fails on "abc"
        assert!(try_parse_message(&mut buffer).is_err());

        // Resync finds the next Content-Length: header
        resync_to_next_message(&mut buffer);

        // Second parse succeeds
        let result = try_parse_message(&mut buffer)
            .expect("should parse")
            .expect("should have message");
        assert_eq!(result, valid_body);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_resync_after_non_utf8_header() {
        let valid_body = r#"{"jsonrpc":"2.0","id":2}"#;
        // Non-UTF-8 bytes (0xFF 0xFE) in the header region, then \r\n\r\n,
        // then a valid message.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"Content-Length: 5\r\nX-Bad: ");
        raw.extend_from_slice(&[0xFF, 0xFE]);
        raw.extend_from_slice(b"\r\n\r\nhello");
        raw.extend_from_slice(
            format!("Content-Length: {}\r\n\r\n{}", valid_body.len(), valid_body).as_bytes(),
        );
        let mut buffer = BytesMut::from(&raw[..]);

        // First parse fails on non-UTF-8 header
        assert!(try_parse_message(&mut buffer).is_err());

        // Resync
        resync_to_next_message(&mut buffer);

        // The valid second message is recovered
        let result = try_parse_message(&mut buffer)
            .expect("should parse")
            .expect("should have message");
        assert_eq!(result, valid_body);
    }

    #[test]
    fn test_resync_after_non_utf8_body() {
        let valid_body = r#"{"jsonrpc":"2.0","id":3}"#;
        // Valid header but body contains non-UTF-8 bytes
        let bad_body: &[u8] = &[0x80, 0x81, 0x82, 0x83, 0x84];
        let mut raw = Vec::new();
        raw.extend_from_slice(format!("Content-Length: {}\r\n\r\n", bad_body.len()).as_bytes());
        raw.extend_from_slice(bad_body);
        raw.extend_from_slice(
            format!("Content-Length: {}\r\n\r\n{}", valid_body.len(), valid_body).as_bytes(),
        );
        let mut buffer = BytesMut::from(&raw[..]);

        // First parse fails on non-UTF-8 body
        assert!(try_parse_message(&mut buffer).is_err());

        // Buffer is unchanged (body UTF-8 checked before advance)
        resync_to_next_message(&mut buffer);

        // Second message recovered
        let result = try_parse_message(&mut buffer)
            .expect("should parse")
            .expect("should have message");
        assert_eq!(result, valid_body);
    }

    #[test]
    fn test_resync_garbage_prefix_then_valid_message() {
        let valid_body = r#"{"jsonrpc":"2.0","id":4}"#;
        let mut raw = Vec::new();
        raw.extend_from_slice(b"GARBAGE BYTES HERE ");
        raw.extend_from_slice(
            format!("Content-Length: {}\r\n\r\n{}", valid_body.len(), valid_body).as_bytes(),
        );
        let mut buffer = BytesMut::from(&raw[..]);

        // No \r\n\r\n before the valid header, so try_parse returns Ok(None)
        // (it can't find headers_end). Simulate receiving more data that
        // doesn't help — the caller should resync if stuck.
        // In practice, resync is called after Err, but we can test the
        // resync function directly on a garbage-prefixed buffer.
        resync_to_next_message(&mut buffer);

        let result = try_parse_message(&mut buffer)
            .expect("should parse")
            .expect("should have message");
        assert_eq!(result, valid_body);
    }

    #[test]
    fn test_resync_no_subsequent_header_clears_buffer() {
        let mut buffer = BytesMut::from("garbage with no valid header at all");
        resync_to_next_message(&mut buffer);
        assert!(buffer.is_empty());
    }

    #[test]
    fn test_err_does_not_consume_buffer() {
        // Verify that try_parse_message on error leaves the buffer unchanged.
        let mut raw = Vec::new();
        raw.extend_from_slice(b"Content-Length: 3\r\n\r\n");
        raw.extend_from_slice(&[0xFF, 0xFE, 0xFD]); // non-UTF-8 body
        let original = raw.clone();
        let mut buffer = BytesMut::from(&raw[..]);

        assert!(try_parse_message(&mut buffer).is_err());
        assert_eq!(
            &buffer[..],
            &original[..],
            "buffer must be unchanged after error"
        );
    }

    // --- Fragmentation / split-invariance property tests (bug 95) ---
    //
    // The captured incident: under 3x parallel load the reader received exactly
    // one framing-valid message whose body failed serde with "expected value at
    // line 1 column 1" (first byte not JSON). The sender writes header+body+flush
    // under a Mutex, so the suspect is the reader's buffer reassembly when
    // `read()` boundaries fall at unlucky places. These tests assert
    // *split-invariance*: for a valid stream of framed messages, ANY partition of
    // the byte stream into read-chunks must yield exactly the same messages, each
    // still valid JSON.

    /// Bodies chosen to be adversarial to the framing parser: they embed the
    /// raw `\r\n\r\n` frame delimiter and the literal `Content-Length:` header
    /// text as *payload*. Content-Length framing is byte-length delimited, so
    /// the reader must byte-preserve these regardless of whether they parse as
    /// JSON — the property here is exact reassembly, not JSON validity. (A real
    /// LSP body escapes control chars, so raw delimiters never appear on the
    /// wire; these push the framing arithmetic harder than reality does.)
    fn adversarial_bodies() -> Vec<String> {
        vec![
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_string(),
            // Raw CRLFCRLF + a fake header, as a string value.
            "{\"log\":\"line\r\n\r\nContent-Length: 5\r\n\r\nfake\"}".to_string(),
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#.to_string(),
            // Body that *begins* with header-looking text.
            "{\"t\":\"Content-Length: 42\"}".to_string(),
            // Multiple embedded delimiters.
            "{\"a\":\"\r\n\r\n\",\"b\":\"\r\n\r\nContent-Length: 9\r\n\r\n\"}".to_string(),
            r#"{"jsonrpc":"2.0","id":7,"result":{"x":[1,2,3,4,5]}}"#.to_string(),
        ]
    }

    /// Valid-JSON bodies that carry the frame delimiter and header token as
    /// *escaped* payload (`\r\n\r\n` as the two-char escape, `Content-Length:`
    /// as literal text) — exactly how a real LSP server encodes such content.
    /// Used where the property also requires the reassembled body to round-trip
    /// through `serde_json`.
    fn adversarial_json_bodies() -> Vec<String> {
        vec![
            r#"{"jsonrpc":"2.0","id":1,"result":null}"#.to_string(),
            r#"{"log":"line\r\n\r\nContent-Length: 5\r\n\r\nfake"}"#.to_string(),
            r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#.to_string(),
            r#"{"t":"Content-Length: 42"}"#.to_string(),
            r#"{"a":"\r\n\r\n","b":"\r\n\r\nContent-Length: 9\r\n\r\n"}"#.to_string(),
            r#"{"jsonrpc":"2.0","id":7,"result":{"x":[1,2,3,4,5]}}"#.to_string(),
        ]
    }

    /// Concatenate framed bodies into one wire stream.
    fn framed_stream(bodies: &[String]) -> Vec<u8> {
        let mut stream = Vec::new();
        for b in bodies {
            stream.extend_from_slice(&frame(b));
        }
        stream
    }

    #[test]
    fn fragment_exhaustive_single_split_preserves_all_messages() {
        // Every possible single read-boundary must reassemble to the identical
        // message sequence (byte-exact), even for bodies carrying raw frame
        // delimiters as payload.
        let bodies = adversarial_bodies();
        let stream = framed_stream(&bodies);

        // Sanity: the whole stream in one chunk yields the expected bodies.
        assert_eq!(drive_reader(&stream, &[]), bodies);

        for split in 1..stream.len() {
            let got = drive_reader(&stream, &[split]);
            assert_eq!(
                got, bodies,
                "single split at byte {split} mis-assembled the frame stream"
            );
        }
    }

    #[test]
    fn fragment_single_split_valid_json_bodies_still_parse() {
        // The incident's exact shape: a framing-valid message whose body failed
        // serde. For realistic (valid-JSON) bodies, every single read-boundary
        // must yield bodies that ALL still round-trip through serde_json —
        // reassembly must never hand a mis-sliced body to the JSON parse.
        let bodies = adversarial_json_bodies();
        let stream = framed_stream(&bodies);

        for split in 1..stream.len() {
            let got = drive_reader(&stream, &[split]);
            assert_eq!(
                got, bodies,
                "single split at byte {split} mis-assembled the frame stream"
            );
            for (i, body) in got.iter().enumerate() {
                assert!(
                    serde_json::from_str::<serde_json::Value>(body).is_ok(),
                    "split at {split}: message[{i}] is framing-valid but not JSON: {body:?}"
                );
            }
        }
    }

    #[test]
    fn fragment_exhaustive_double_split_preserves_all_messages() {
        // Two independent read boundaries — the case the incident implicates
        // (naked header, then half a body + next header, etc.). A short stream
        // keeps this O(n^2) sweep fast while still exercising every boundary
        // pair, including boundaries that fall inside an embedded delimiter.
        let bodies = vec![
            r#"{"a":1}"#.to_string(),
            "{\"x\":\"\r\n\r\nContent-Length: 3\r\n\r\nabc\"}".to_string(),
            r#"{"b":2}"#.to_string(),
        ];
        let stream = framed_stream(&bodies);

        for s1 in 1..stream.len() {
            for s2 in s1..stream.len() {
                let got = drive_reader(&stream, &[s1, s2]);
                assert_eq!(
                    got, bodies,
                    "double split at ({s1},{s2}) mis-assembled the frame stream"
                );
            }
        }
    }

    #[test]
    fn resync_garbage_between_frames_loses_only_the_garbage() {
        // The ticket-93 path: raw garbage bytes injected between two valid
        // frames. After resync, EVERY subsequent valid frame must still parse —
        // no frame loss beyond the garbage itself. Sweep the split point across
        // the whole stream so the garbage and the resync land at every boundary.
        let before = r#"{"before":true}"#.to_string();
        let after = r#"{"after":true}"#.to_string();
        let garbage = b"\x00\x01\x02 not a header \xff junk ";

        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(&before));
        stream.extend_from_slice(garbage);
        stream.extend_from_slice(&frame(&after));

        for split in 1..stream.len() {
            let got = drive_reader(&stream, &[split]);
            // The garbage carries no `Content-Length:`, so it never fabricates a
            // frame; both real frames must survive, in order.
            assert_eq!(
                got,
                vec![before.clone(), after.clone()],
                "garbage-between-frames at split {split} lost or corrupted a frame"
            );
        }
    }

    // --- Deterministic regressions minimized from the property tests (bug 95) ---

    #[test]
    fn headerblock_without_content_length_is_an_error_not_underrun() {
        // Root cause, minimized from `resync_never_eats_the_trailing_valid_frame`
        // (shrunk input: a single stray `c` byte before a valid header).
        //
        // The buffer holds a COMPLETE `\r\n\r\n` header block, but the block has
        // no parseable `Content-Length` (the stray byte glued onto the header
        // line so `content-length:` no longer starts the line). The parser must
        // report this as a framing error (→ resync), NOT `Ok(None)`. `Ok(None)`
        // means "need more data" and, when no more data follows, strands every
        // subsequent frame in the buffer forever.
        let mut buffer = BytesMut::from(&b"cContent-Length: 11\r\n\r\n{\"after\":2}"[..]);
        let result = try_parse_message(&mut buffer);
        assert!(
            result.is_err(),
            "a complete header block with no Content-Length must be an error \
             (resync), got {result:?}"
        );
        // And the buffer is left intact for resync to work on.
        assert_eq!(
            &buffer[..],
            &b"cContent-Length: 11\r\n\r\n{\"after\":2}"[..]
        );
    }

    #[test]
    fn resync_preserves_partial_header_prefix_at_buffer_tail() {
        // CI-minimized from `resync_true_garbage_between_frames_keeps_both_
        // frames` (run 29065557035; seed pinned in proptest-regressions):
        // garbage `\r\n\r\n` + colons completes a bogus EMPTY header block, so
        // the parser bails and resyncs while the next frame's header is only
        // partially arrived. The old resync cleared the whole buffer on
        // no-needle-found — destroying the partial `Content-Length:` prefix,
        // and with it the next healthy frame (the orphaned header remainder
        // then failed and cleared the body too). Resync must keep a trailing
        // needle prefix; sweep EVERY split so the hazard is pinned at every
        // read boundary, not just the CI-found one.
        let before = r#"{"before":1}"#.to_string();
        let after = r#"{"after":2}"#.to_string();
        let mut stream = Vec::new();
        stream.extend_from_slice(&frame(&before));
        stream.extend_from_slice(b"\r\n\r\n:::::::::");
        stream.extend_from_slice(&frame(&after));

        for split in 1..stream.len() {
            let got = drive_reader(&stream, &[split]);
            assert_eq!(
                got,
                vec![before.clone(), after.clone()],
                "split {split} lost a frame to the resync clear"
            );
        }
    }

    #[test]
    fn resync_keeps_a_trailing_content_length_prefix() {
        // The tail-keep semantics directly: no full needle in the buffer, but
        // the tail is a case-insensitive prefix of `content-length:` — resync
        // discards everything before it and reports exactly that count.
        let mut buffer = BytesMut::from(&b"\r\n\r\n::junk::Content-Le"[..]);
        let discarded = resync_to_next_message(&mut buffer);
        assert_eq!(&buffer[..], b"Content-Le");
        assert_eq!(discarded, b"\r\n\r\n::junk::".len());

        // And progress is guaranteed even when the WHOLE buffer (from byte 1)
        // is a needle prefix: the keep is capped at len - 1.
        let mut buffer = BytesMut::from(&b"content-le"[..]);
        let discarded = resync_to_next_message(&mut buffer);
        assert!(discarded >= 1, "resync must always discard at least a byte");
    }

    #[test]
    fn stray_byte_before_header_does_not_strand_the_frame() {
        // Same fault, end to end through the reader loop: a stray byte lands
        // (via a read boundary or prior garbage) immediately before a valid
        // frame's header. The frame must still be recovered, not stranded.
        let mut stream = Vec::new();
        stream.extend_from_slice(b"c");
        stream.extend_from_slice(&frame(r#"{"after":2}"#));

        let got = drive_reader(&stream, &[]);
        assert_eq!(
            got,
            vec![r#"{"after":2}"#.to_string()],
            "stray byte before header stranded the frame"
        );
    }

    #[test]
    fn forged_content_length_in_garbage_is_loud_dropped_not_dispatched() {
        // The captured incident's exact shape, and the LIMIT of Content-Length
        // resync. When real garbage carries its *own* `Content-Length:` header
        // whose length carves across the following real frame's header,
        // `try_parse_message` produces a framing-valid slice with a wrong first
        // byte — the incident's non-JSON body. resync landing on that forged
        // header is inherent to length-delimited framing and cannot be undone by
        // arithmetic: once a forged length damages the next real header, that
        // frame is unrecoverable.
        //
        // The ruled posture (bug 95) is therefore the safety net, not perfect
        // recovery: the corrupt slice must NEVER be dispatched to a pipeline
        // (the reader's JSON-parse-or-drop step, now a loud `warn!`). This test
        // pins that guarantee — the framing layer may emit a bogus body, but the
        // reader drops it rather than routing garbage to a handler.
        let real = r#"{"real":true}"#.to_string();
        let mut stream = Vec::new();
        // Leading true garbage (no header) forces the error → resync path.
        stream.extend_from_slice(b"\xff\xff garbage ");
        // Forged header claiming a 5-byte body, then a stray byte, then the real
        // frame. resync jumps to this forged header and slices 5 bytes, carving
        // into the real frame's header.
        stream.extend_from_slice(b"Content-Length: 5\r\n\r\nXYZ");
        stream.extend_from_slice(&frame(&real));

        // The framing layer emits the forged (non-JSON) slice...
        let framed = drive_reader(&stream, &[]);
        assert!(
            framed
                .iter()
                .any(|m| serde_json::from_str::<serde_json::Value>(m).is_err()),
            "expected the forged header to carve a non-JSON slice; got {framed:?}"
        );
        // ...but the reader NEVER dispatches a non-JSON body — the loud-drop
        // protects the pipeline. (The real frame is collateral to the forged
        // length here; the guarantee is that no corrupt body is routed.)
        let dispatched = drive_reader_dispatched(&stream, &[]);
        for body in &dispatched {
            assert!(
                serde_json::from_str::<serde_json::Value>(body).is_ok(),
                "the reader dispatched a non-JSON body (the incident): {body:?}"
            );
        }
    }

    // --- The captured incident, reconstructed byte-for-byte (bug 95 settle) ---

    #[test]
    fn incident_95_drain_sentinel_injected_between_header_and_body() {
        // Byte-exact reconstruction of the captured incident
        // (bugs/evidence/95/victim-tmpTQGNWt, 2026-07-09 20:10:24.698Z).
        //
        // The daemon held a clone of the server's stdout pipe write end and
        // `Connection::drain()` injected a sentinel response frame into it
        // after every settle. mockls writes each frame through
        // `std::io::Stdout` — a `LineWriter` — so the header (ending in
        // `\r\n\r\n`) flushes as one pipe write and the body lands in a
        // SECOND write at `flush()`. Under 3x full-suite load, the
        // post-didSave settle's sentinel landed exactly in that gap:
        //
        //   [begin header: "Content-Length: 135\r\n\r\n"]   <- mockls syscall 1
        //   [sentinel frame: 60 bytes, id 8]                <- daemon drain()
        //   [begin body: 135 bytes]                         <- mockls syscall 2
        //
        // The reader then parsed a framing-VALID message: Content-Length 135
        // sliced the sentinel frame (60 bytes) plus the first 75 bytes of the
        // begin body. Its first byte is `C` — serde_json fails with exactly
        // the incident's `expected value at line 1 column 1`. The sentinel
        // response was destroyed (drain()'s oneshot never resolved — the
        // diagnostics pipeline parked forever, so round 2 sent no codeAction)
        // and the `$/progress` begin was destroyed (no progress evidence).
        // The leftover begin-body tail then glued onto the next frame's
        // header; pre-c1dcd11 that returned Ok(None) forever — the 57 s of
        // total wire silence. Post-c1dcd11 the reader resyncs and recovers
        // every subsequent frame, as this test also pins.
        //
        // The fix removes the second writer entirely: drain() is now a
        // reader-side barrier over a control channel and NOTHING but the
        // server process writes to the reader's pipe.
        let begin_body = r#"{"jsonrpc":"2.0","method":"$/progress","params":{"token":"mockls-flycheck","value":{"kind":"begin","percentage":0,"title":"Flycheck"}}}"#;
        assert_eq!(begin_body.len(), 135, "the incident's begin-frame length");
        // The drain sentinel the daemon wrote at 24.698 (id 8: the eighth
        // Connection request/drain id of the victim connection's lifetime).
        let sentinel_body = r#"{"jsonrpc":"2.0","id":8,"result":null}"#;
        let sentinel_frame = frame(sentinel_body);
        assert_eq!(
            sentinel_frame.len(),
            60,
            "the incident's sentinel frame length"
        );
        // The two frames mockls wrote after the begin (never parsed in the
        // incident; the wire shard ends at the create id:2 reply).
        let publish_body = r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"diagnostics":[{"message":"mockls: mock diagnostic (2 lines)","range":{"end":{"character":1,"line":0},"start":{"character":0,"line":0}},"severity":2,"source":"mockls"}],"uri":"file:///home/mark/.claude/tmp/.tmppeL1L2/test.yX4Za","version":1}}"#;
        let end_body = r#"{"jsonrpc":"2.0","method":"$/progress","params":{"token":"mockls-flycheck","value":{"kind":"end","message":"Flycheck complete"}}}"#;

        let mut stream = Vec::new();
        stream
            .extend_from_slice(format!("Content-Length: {}\r\n\r\n", begin_body.len()).as_bytes());
        stream.extend_from_slice(&sentinel_frame);
        stream.extend_from_slice(begin_body.as_bytes());
        stream.extend_from_slice(&frame(publish_body));
        stream.extend_from_slice(&frame(end_body));

        // The wire interleaving fixed the damage; the reader's read
        // fragmentation must not change it. Sweep every single read boundary
        // (plus the unfragmented stream) and assert the identical outcome.
        let mut splits: Vec<Vec<usize>> = vec![vec![]];
        splits.extend((1..stream.len()).map(|s| vec![s]));
        for cuts in splits {
            let emitted = drive_reader(&stream, &cuts);
            assert_eq!(
                emitted.len(),
                3,
                "split {cuts:?}: expected [composite garbage, publish, end]"
            );

            // Frame 1: the incident's framing-valid garbage body.
            let garbage = &emitted[0];
            assert_eq!(garbage.len(), 135, "split {cuts:?}: Content-Length slice");
            assert!(
                garbage.starts_with("Content-Length: 38\r\n\r\n"),
                "split {cuts:?}: the slice must begin with the sentinel's header"
            );
            let err = serde_json::from_str::<serde_json::Value>(garbage)
                .expect_err("the composite slice is not JSON");
            assert!(
                err.to_string()
                    .starts_with("expected value at line 1 column 1"),
                "split {cuts:?}: expected the incident's exact serde error, got: {err}"
            );

            // The sentinel response and the begin notification are destroyed
            // (the incident's real loss); everything after is recovered.
            assert_eq!(emitted[1], publish_body, "split {cuts:?}");
            assert_eq!(emitted[2], end_body, "split {cuts:?}");
        }
    }

    // --- Byte-conservation audit (bug 95 settle, task 4) ---

    #[test]
    fn conservation_divergence_fires_once_and_recovers() {
        use crate::logging::test_support::{query_all_messages, setup_logging};

        let (_logging, recorder, _guard) = setup_logging();

        let first = frame(r#"{"first":1}"#);
        let second_body = r#"{"second":2}"#.to_string();
        let second = frame(&second_body);

        let mut pump = FramePump::new("test-server".to_string(), None);

        // Feed half of frame 1 so bytes sit buffered, then simulate a
        // conservation bug: one buffered byte vanishes unaccounted.
        let half = first.len() / 2;
        assert!(pump.ingest(&first[..half]).is_empty());
        assert!(pump.buffered_len() > 0);
        pump.lose_one_buffered_byte_for_test();

        // The next ingest must detect the divergence (read != accounted +
        // buffered), emit ONE error!, resync, and re-true the accounting.
        let _ = pump.ingest(&first[half..]);
        assert_eq!(pump.divergences(), 1, "divergence must be detected");

        // Recovery: a subsequent valid frame parses normally with no
        // further divergence reports.
        let got = pump.ingest(&second);
        assert_eq!(got, vec![second_body]);
        assert_eq!(
            pump.divergences(),
            1,
            "a single fault must report once, not on every iteration"
        );

        let errors: Vec<_> = query_all_messages(&recorder)
            .into_iter()
            .filter(|m| m.level == "error")
            .collect();
        assert_eq!(errors.len(), 1, "exactly one error! for one divergence");
        assert_eq!(errors[0].server, "test-server");
        assert!(
            errors[0].payload.contains("total_bytes_read"),
            "the error must carry the counters, got: {}",
            errors[0].payload
        );
    }

    proptest! {
        // 2048 cases per property — a serious soak over the fragmentation and
        // resync space, well past proptest's default 256.
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(2048))]

        /// Split-invariance under an arbitrary partition of the byte stream:
        /// for N valid framed messages (varied sizes, bodies that embed the
        /// frame delimiter and `Content-Length:` as payload text), ANY set of
        /// read-chunk boundaries must yield exactly those N messages, each still
        /// parsing as JSON. This is the reader's reassembly path (bug 95) driven
        /// against adversarial fragmentation.
        #[test]
        fn fragment_split_invariance(
            // A handful of bodies (each a valid JSON object with a random
            // payload string that may contain the delimiter / header text) and a
            // random set of cut points.
            payloads in prop::collection::vec(
                prop::string::string_regex(
                    // printable ASCII plus the raw delimiter and header token,
                    // interleaved — the adversarial alphabet.
                    "([ -~]|\r\n\r\n|Content-Length: [0-9]+){0,40}"
                ).expect("valid regex"),
                1..6,
            ),
            raw_cuts in prop::collection::vec(0usize..512, 0..12),
        ) {
            // Wrap each random payload as a JSON string value so bodies are valid
            // JSON regardless of the payload's contents.
            let bodies: Vec<String> = payloads
                .iter()
                .map(|p| serde_json::Value::String(p.clone()).to_string())
                .collect();
            let stream = framed_stream(&bodies);

            let got = drive_reader(&stream, &raw_cuts);
            prop_assert_eq!(
                &got, &bodies,
                "partition {:?} mis-assembled the frame stream", raw_cuts
            );
            for body in &got {
                prop_assert!(
                    serde_json::from_str::<serde_json::Value>(body).is_ok(),
                    "framing-valid message is not JSON: {:?}", body
                );
            }
        }

        /// Garbage-between-frames + resync (the ticket-93 path), quantified.
        /// A run of arbitrary bytes that carries NO `content-length:` token —
        /// so it cannot forge a header — is injected between two valid frames,
        /// at a random split. This is the honest, provable invariant: resync
        /// consumes exactly the garbage, and BOTH real frames survive, in order.
        ///
        /// (Garbage that *does* forge a `Content-Length:` header can legitimately
        /// swallow the next frame — that is inherent to Content-Length resync and
        /// is why the body-parse-failure branch loud-drops. That case is
        /// characterized deterministically in
        /// `forged_content_length_in_garbage_does_not_carve_a_wrong_body`, not
        /// asserted as an invariant here.)
        #[test]
        fn resync_true_garbage_between_frames_keeps_both_frames(
            // Palette of adversarial bytes MINUS the letters that spell the
            // header token, so no `content-length:` needle can form: CRLF,
            // colons, digits, braces, spaces, and raw high bytes.
            garbage in prop::collection::vec(
                prop::sample::select(vec![
                    b':', b'-', b' ', b'\r', b'\n', b'0', b'9', b'{', b'}',
                    b'#', b'*', b'@', b'~', 0x00, 0x7f, 0x80, 0xfe, 0xff,
                ]),
                0..80,
            ),
            split_frac in 0usize..100,
        ) {
            let before = r#"{"before":1}"#.to_string();
            let after = r#"{"after":2}"#.to_string();

            let mut stream = Vec::new();
            stream.extend_from_slice(&frame(&before));
            stream.extend_from_slice(&garbage);
            stream.extend_from_slice(&frame(&after));

            let split = 1 + (split_frac * stream.len()) / 100;
            let dispatched = drive_reader_dispatched(
                &stream,
                &[split.min(stream.len().saturating_sub(1)).max(1)],
            );

            prop_assert!(
                serde_json::from_str::<serde_json::Value>(&after).is_ok(),
                "trailing frame is not JSON (test invariant)"
            );
            // Both real frames survive resync of true (non-forging) garbage.
            prop_assert_eq!(
                dispatched,
                vec![before, after],
                "true garbage between frames lost a frame"
            );
        }
    }
}
