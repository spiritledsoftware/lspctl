use std::{io, num::NonZeroUsize, str};

use serde_json::Value;
#[cfg(test)]
use serde_json::{Map, Number};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

/// Maximum inbound LSP header size, including the final blank line.
pub(crate) const JSON_RPC_HEADER_LIMIT_BYTES: usize = 8 * 1024;
/// Default maximum JSON-RPC body size in bytes.
#[cfg(test)]
const DEFAULT_JSON_RPC_BODY_LIMIT_BYTES: usize = 64 * 1024 * 1024;

/// Reads bounded Language Server Protocol JSON-RPC frames from one byte stream.
pub(crate) struct JsonRpcFrameReader<R> {
    input: BufReader<R>,
    max_body_bytes: NonZeroUsize,
}

/// One decoded JSON-RPC message with its exact wire bytes.
pub(crate) struct JsonRpcFrame {
    pub(crate) message: Value,
    pub(crate) header: Vec<u8>,
    pub(crate) body: Vec<u8>,
}

impl<R: AsyncRead + Unpin> JsonRpcFrameReader<R> {
    /// Creates a frame reader with the default 64 MiB body limit.
    #[cfg(test)]
    pub(crate) fn new(input: R) -> Self {
        Self::with_body_limit(
            input,
            NonZeroUsize::new(DEFAULT_JSON_RPC_BODY_LIMIT_BYTES).unwrap(),
        )
    }

    /// Creates a frame reader with a validated positive body-byte limit.
    pub(crate) fn with_body_limit(input: R, max_body_bytes: NonZeroUsize) -> Self {
        Self {
            input: BufReader::new(input),
            max_body_bytes,
        }
    }

    /// Returns `None` only when the stream ends between complete frames.
    #[cfg(test)]
    pub(crate) async fn read_json_rpc_frame(
        &mut self,
    ) -> Result<Option<Value>, JsonRpcTransportError> {
        Ok(self
            .read_json_rpc_frame_with_bytes()
            .await?
            .map(|frame| frame.message))
    }

    /// Reads one message while retaining the exact header and body for explicit tracing.
    pub(crate) async fn read_json_rpc_frame_with_bytes(
        &mut self,
    ) -> Result<Option<JsonRpcFrame>, JsonRpcTransportError> {
        let Some(header) = self.read_bounded_header().await? else {
            return Ok(None);
        };
        let content_length = parse_content_length(&header)?;
        if content_length > self.max_body_bytes.get() {
            return Err(JsonRpcTransportError::InboundBodyTooLarge {
                declared_size: content_length,
                limit: self.max_body_bytes.get(),
            });
        }

        let mut body = vec![0; content_length];
        self.input
            .read_exact(&mut body)
            .await
            .map_err(JsonRpcTransportError::ReadBody)?;
        let body_text = str::from_utf8(&body).map_err(JsonRpcTransportError::NonUtf8Body)?;
        let message: Value =
            serde_json::from_str(body_text).map_err(JsonRpcTransportError::InvalidJsonBody)?;

        match message {
            Value::Object(_) => Ok(Some(JsonRpcFrame {
                message,
                header,
                body,
            })),
            Value::Array(_) => Err(JsonRpcTransportError::BatchUnsupported),
            _ => Err(JsonRpcTransportError::MessageNotObject),
        }
    }

    async fn read_bounded_header(&mut self) -> Result<Option<Vec<u8>>, JsonRpcTransportError> {
        let mut header = Vec::with_capacity(128);

        loop {
            let (consumed, complete) = {
                let available = self
                    .input
                    .fill_buf()
                    .await
                    .map_err(JsonRpcTransportError::ReadHeader)?;
                if available.is_empty() {
                    if header.is_empty() {
                        return Ok(None);
                    }
                    return Err(JsonRpcTransportError::TruncatedHeader);
                }

                let remaining = JSON_RPC_HEADER_LIMIT_BYTES - header.len();
                let mut consumed = 0;
                let mut complete = false;
                for &byte in available.iter().take(remaining) {
                    header.push(byte);
                    consumed += 1;
                    if header.ends_with(b"\r\n\r\n") {
                        complete = true;
                        break;
                    }
                }
                (consumed, complete)
            };

            self.input.consume(consumed);
            if complete {
                return Ok(Some(header));
            }
            if header.len() == JSON_RPC_HEADER_LIMIT_BYTES {
                return Err(JsonRpcTransportError::HeaderTooLarge {
                    limit: JSON_RPC_HEADER_LIMIT_BYTES,
                });
            }
        }
    }
}

/// Writes complete bounded JSON-RPC frames without partial oversize output.
pub(crate) struct JsonRpcFrameWriter<W> {
    output: W,
    max_body_bytes: NonZeroUsize,
    reusable: bool,
}

impl<W: AsyncWrite + Unpin> JsonRpcFrameWriter<W> {
    /// Creates a frame writer with the default 64 MiB body limit.
    #[cfg(test)]
    pub(crate) fn new(output: W) -> Self {
        Self::with_body_limit(
            output,
            NonZeroUsize::new(DEFAULT_JSON_RPC_BODY_LIMIT_BYTES).unwrap(),
        )
    }

    /// Creates a frame writer with a validated positive body-byte limit.
    pub(crate) fn with_body_limit(output: W, max_body_bytes: NonZeroUsize) -> Self {
        Self {
            output,
            max_body_bytes,
            reusable: true,
        }
    }

    /// Serializes, bounds, writes, and flushes one JSON-RPC object.
    #[cfg(test)]
    pub(crate) async fn write_json_rpc_frame(
        &mut self,
        message: &Value,
    ) -> Result<(), JsonRpcTransportError> {
        self.write_json_rpc_frame_with_bytes(
            message,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .map(|_| ())
    }

    pub(crate) fn is_reusable(&self) -> bool {
        self.reusable
    }

    /// Writes one message and returns its exact header and body for explicit tracing.
    pub(crate) async fn write_json_rpc_frame_with_bytes(
        &mut self,
        message: &Value,
        deadline: tokio::time::Instant,
    ) -> Result<(Vec<u8>, Vec<u8>), JsonRpcTransportError> {
        if !self.reusable {
            return Err(JsonRpcTransportError::WriteFrame(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "A previous JSON-RPC frame write did not complete",
            )));
        }
        if !message.is_object() {
            return Err(JsonRpcTransportError::MessageNotObject);
        }

        let body = serde_json::to_vec(message).map_err(JsonRpcTransportError::SerializeBody)?;
        if body.len() > self.max_body_bytes.get() {
            return Err(JsonRpcTransportError::OutboundBodyTooLarge {
                size: body.len(),
                limit: self.max_body_bytes.get(),
            });
        }

        let header = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        // Cancellation drops this future without clearing the bit: never append to a partial frame.
        self.reusable = false;
        let timeout_error =
            || io::Error::new(io::ErrorKind::TimedOut, "JSON-RPC frame write timed out");
        tokio::time::timeout_at(deadline, async {
            // Ready I/O must not bypass a budget already spent serializing/synchronizing.
            if tokio::time::Instant::now() >= deadline {
                return Err(timeout_error());
            }
            self.output.write_all(&header).await?;
            self.output.write_all(&body).await?;
            self.output.flush().await?;
            if tokio::time::Instant::now() >= deadline {
                return Err(timeout_error());
            }
            Ok(())
        })
        .await
        .map_err(|_| JsonRpcTransportError::WriteFrame(timeout_error()))?
        .map_err(JsonRpcTransportError::WriteFrame)?;
        self.reusable = true;
        Ok((header, body))
    }
}

/// Distinguishes an omitted request parameter member from any supplied JSON value.
#[cfg(test)]
pub(crate) enum RawRequestParameters {
    Omitted,
    Present(Value),
}

/// Builds one raw JSON-RPC request without collapsing explicit `params: null`.
#[cfg(test)]
pub(crate) fn build_json_rpc_request(
    id: JsonRpcRequestId,
    method: String,
    parameters: RawRequestParameters,
) -> Value {
    let mut request = Map::new();
    request.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
    request.insert("id".to_owned(), id.into_value());
    request.insert("method".to_owned(), Value::String(method));
    if let RawRequestParameters::Present(parameters) = parameters {
        request.insert("params".to_owned(), parameters);
    }
    Value::Object(request)
}

/// A JSON-RPC request identifier supported by the LSP transport.
#[cfg(test)]
pub(crate) enum JsonRpcRequestId {
    Integer(i64),
    String(String),
}

#[cfg(test)]
impl JsonRpcRequestId {
    fn into_value(self) -> Value {
        match self {
            Self::Integer(id) => Value::Number(Number::from(id)),
            Self::String(id) => Value::String(id),
        }
    }
}

/// Reports framing, encoding, size, and stream failures at the JSON-RPC boundary.
#[derive(Debug, Error)]
pub(crate) enum JsonRpcTransportError {
    #[error("JSON-RPC header read failed")]
    ReadHeader(#[source] io::Error),
    #[error("JSON-RPC header ended before its blank line")]
    TruncatedHeader,
    #[error("JSON-RPC header exceeded the {limit}-byte limit")]
    HeaderTooLarge { limit: usize },
    #[error("JSON-RPC header contains non-ASCII bytes")]
    NonAsciiHeader,
    #[error("JSON-RPC header line is malformed")]
    MalformedHeaderLine,
    #[error("JSON-RPC Content-Length header is missing")]
    MissingContentLength,
    #[error("JSON-RPC Content-Length header is duplicated")]
    DuplicateContentLength,
    #[error("JSON-RPC Content-Length value is invalid")]
    InvalidContentLength,
    #[error("JSON-RPC Content-Type value is unsupported")]
    UnsupportedContentType,
    #[error("JSON-RPC Content-Type header is duplicated")]
    DuplicateContentType,
    #[error("JSON-RPC inbound body declared {declared_size} bytes, over the {limit}-byte limit")]
    InboundBodyTooLarge { declared_size: usize, limit: usize },
    #[error("JSON-RPC body read failed")]
    ReadBody(#[source] io::Error),
    #[error("JSON-RPC body is not UTF-8")]
    NonUtf8Body(#[source] str::Utf8Error),
    #[error("JSON-RPC body is not valid JSON")]
    InvalidJsonBody(#[source] serde_json::Error),
    #[error("JSON-RPC batches are unsupported")]
    BatchUnsupported,
    #[error("JSON-RPC message must be an object")]
    MessageNotObject,
    #[error("JSON-RPC body serialization failed")]
    SerializeBody(#[source] serde_json::Error),
    #[error("JSON-RPC outbound body is {size} bytes, over the {limit}-byte limit")]
    OutboundBodyTooLarge { size: usize, limit: usize },
    #[error("JSON-RPC frame write failed")]
    WriteFrame(#[source] io::Error),
}

fn parse_content_length(header: &[u8]) -> Result<usize, JsonRpcTransportError> {
    if !header.is_ascii() {
        return Err(JsonRpcTransportError::NonAsciiHeader);
    }

    let header = str::from_utf8(&header[..header.len() - 4]).unwrap();
    let mut content_length = None;
    let mut content_type_seen = false;

    for line in header.split("\r\n") {
        let (name, value) = line
            .split_once(':')
            .ok_or(JsonRpcTransportError::MalformedHeaderLine)?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() {
            return Err(JsonRpcTransportError::MalformedHeaderLine);
        }

        if name.eq_ignore_ascii_case("Content-Length") {
            if content_length.is_some() {
                return Err(JsonRpcTransportError::DuplicateContentLength);
            }
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(JsonRpcTransportError::InvalidContentLength);
            }
            content_length = Some(
                value
                    .parse()
                    .map_err(|_| JsonRpcTransportError::InvalidContentLength)?,
            );
        } else if name.eq_ignore_ascii_case("Content-Type") {
            if content_type_seen {
                return Err(JsonRpcTransportError::DuplicateContentType);
            }
            content_type_seen = true;
            validate_content_type(value)?;
        }
    }

    content_length.ok_or(JsonRpcTransportError::MissingContentLength)
}

fn validate_content_type(content_type: &str) -> Result<(), JsonRpcTransportError> {
    let mut parts = content_type.split(';');
    if !parts.next().is_some_and(|media_type| {
        media_type
            .trim()
            .eq_ignore_ascii_case("application/vscode-jsonrpc")
    }) {
        return Err(JsonRpcTransportError::UnsupportedContentType);
    }

    let mut charset_seen = false;
    for parameter in parts {
        let (name, value) = parameter
            .split_once('=')
            .ok_or(JsonRpcTransportError::UnsupportedContentType)?;
        if name.trim().eq_ignore_ascii_case("charset") {
            if charset_seen {
                return Err(JsonRpcTransportError::UnsupportedContentType);
            }
            charset_seen = true;
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .unwrap_or(value);
            if !value.eq_ignore_ascii_case("utf-8") && !value.eq_ignore_ascii_case("utf8") {
                return Err(JsonRpcTransportError::UnsupportedContentType);
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;

    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    use super::*;

    #[tokio::test]
    async fn reads_partial_and_consecutive_frames() {
        let first = b"Content-Length: 38\r\nX-Test: yes\r\n\r\n{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null}";
        let second = b"Content-Length: 46\r\nContent-Type: application/vscode-jsonrpc; charset=utf8\r\n\r\n{\"jsonrpc\":\"2.0\",\"method\":\"window/logMessage\"}";
        let (mut sender, receiver) = duplex(3);
        let sending = tokio::spawn(async move {
            sender.write_all(first).await.unwrap();
            sender.write_all(second).await.unwrap();
        });
        let mut reader = JsonRpcFrameReader::new(receiver);

        assert_eq!(
            reader.read_json_rpc_frame().await.unwrap(),
            Some(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": null
            }))
        );
        assert_eq!(
            reader.read_json_rpc_frame().await.unwrap(),
            Some(json!({
                "jsonrpc": "2.0",
                "method": "window/logMessage"
            }))
        );
        assert_eq!(reader.read_json_rpc_frame().await.unwrap(), None);
        sending.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_header_and_body_bounds_before_reading_a_body() {
        let oversized_header = vec![b'x'; JSON_RPC_HEADER_LIMIT_BYTES];
        let mut reader = JsonRpcFrameReader::with_body_limit(
            oversized_header.as_slice(),
            NonZeroUsize::new(64).unwrap(),
        );
        assert!(matches!(
            reader.read_json_rpc_frame().await,
            Err(JsonRpcTransportError::HeaderTooLarge { .. })
        ));

        let mut reader = JsonRpcFrameReader::with_body_limit(
            b"Content-Length: 65\r\n\r\n".as_slice(),
            NonZeroUsize::new(64).unwrap(),
        );
        assert!(matches!(
            reader.read_json_rpc_frame().await,
            Err(JsonRpcTransportError::InboundBodyTooLarge {
                declared_size: 65,
                limit: 64
            })
        ));
    }

    #[tokio::test]
    async fn rejects_duplicate_lengths_and_json_batches() {
        let mut duplicate = JsonRpcFrameReader::with_body_limit(
            b"Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}".as_slice(),
            NonZeroUsize::new(64).unwrap(),
        );
        assert!(matches!(
            duplicate.read_json_rpc_frame().await,
            Err(JsonRpcTransportError::DuplicateContentLength)
        ));

        let mut batch = JsonRpcFrameReader::with_body_limit(
            b"Content-Length: 2\r\n\r\n[]".as_slice(),
            NonZeroUsize::new(64).unwrap(),
        );
        assert!(matches!(
            batch.read_json_rpc_frame().await,
            Err(JsonRpcTransportError::BatchUnsupported)
        ));
    }

    #[tokio::test]
    async fn rejects_non_utf8_malformed_and_non_object_bodies() {
        let mut non_utf8 = JsonRpcFrameReader::with_body_limit(
            b"Content-Length: 1\r\n\r\n\xff".as_slice(),
            NonZeroUsize::new(64).unwrap(),
        );
        assert!(matches!(
            non_utf8.read_json_rpc_frame().await,
            Err(JsonRpcTransportError::NonUtf8Body(_))
        ));

        let mut malformed = JsonRpcFrameReader::with_body_limit(
            b"Content-Length: 1\r\n\r\n{".as_slice(),
            NonZeroUsize::new(64).unwrap(),
        );
        assert!(matches!(
            malformed.read_json_rpc_frame().await,
            Err(JsonRpcTransportError::InvalidJsonBody(_))
        ));

        let mut scalar = JsonRpcFrameReader::with_body_limit(
            b"Content-Length: 4\r\n\r\nnull".as_slice(),
            NonZeroUsize::new(64).unwrap(),
        );
        assert!(matches!(
            scalar.read_json_rpc_frame().await,
            Err(JsonRpcTransportError::MessageNotObject)
        ));
    }

    #[tokio::test]
    async fn raw_request_keeps_explicit_null_on_the_wire() {
        let explicit_null = build_json_rpc_request(
            JsonRpcRequestId::Integer(1),
            "experimental/example".to_owned(),
            RawRequestParameters::Present(Value::Null),
        );
        let omitted = build_json_rpc_request(
            JsonRpcRequestId::String("next".to_owned()),
            "experimental/example".to_owned(),
            RawRequestParameters::Omitted,
        );
        assert_eq!(explicit_null.get("params"), Some(&Value::Null));
        assert!(!omitted.as_object().unwrap().contains_key("params"));

        let (sender, mut receiver) = duplex(256);
        let mut writer = JsonRpcFrameWriter::new(sender);
        writer.write_json_rpc_frame(&explicit_null).await.unwrap();
        drop(writer);

        let mut wire = Vec::new();
        receiver.read_to_end(&mut wire).await.unwrap();
        let body = wire.split(|byte| *byte == b'\n').next_back().unwrap();
        assert!(str::from_utf8(body).unwrap().contains("\"params\":null"));
    }

    #[tokio::test(start_paused = true)]
    async fn frame_write_timeout_poisons_partial_frame() {
        use tokio::time::{Duration, Instant, timeout};
        // Capacity three stalls within the header; capacity 64 stalls within the body.
        for capacity in [3, 64] {
            let (sender, mut receiver) = duplex(capacity);
            let mut writer = JsonRpcFrameWriter::new(sender);
            let message = json!({"payload": "x".repeat(256)});
            let result = timeout(
                Duration::from_millis(200),
                writer.write_json_rpc_frame_with_bytes(
                    &message,
                    Instant::now() + Duration::from_millis(20),
                ),
            )
            .await
            .expect("frame write ignored its deadline");
            assert!(
                matches!(result, Err(JsonRpcTransportError::WriteFrame(error)) if error.kind() == io::ErrorKind::TimedOut)
            );
            let mut prefix = vec![0; capacity];
            timeout(Duration::from_millis(100), receiver.read_exact(&mut prefix))
                .await
                .expect("write produced no partial frame")
                .unwrap();
            assert!(
                timeout(
                    Duration::from_millis(100),
                    writer.write_json_rpc_frame(&json!({}))
                )
                .await
                .expect("poisoned writer blocked")
                .is_err()
            );
            drop(writer);
            let mut remainder = Vec::new();
            receiver.read_to_end(&mut remainder).await.unwrap();
            assert!(remainder.is_empty(), "a new frame followed a partial frame");
        }
        // Dropping an in-flight future must poison the writer even without its own timeout.
        let (sender, mut receiver) = duplex(3);
        let mut writer = JsonRpcFrameWriter::new(sender);
        assert!(
            timeout(
                Duration::from_millis(20),
                writer.write_json_rpc_frame(&json!({}))
            )
            .await
            .is_err()
        );
        let mut prefix = [0; 3];
        timeout(Duration::from_millis(100), receiver.read_exact(&mut prefix))
            .await
            .expect("cancelled write produced no partial frame")
            .unwrap();
        assert!(
            timeout(
                Duration::from_millis(100),
                writer.write_json_rpc_frame(&json!({}))
            )
            .await
            .expect("cancelled writer blocked")
            .is_err()
        );
        drop(writer);
        let mut remainder = Vec::new();
        receiver.read_to_end(&mut remainder).await.unwrap();
        assert!(remainder.is_empty());
    }

    #[tokio::test]
    async fn frame_write_io_error_poisons_partial_frame() {
        use std::{
            pin::Pin,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            task::{Context, Poll},
        };
        struct FailingWriter {
            bytes: Arc<AtomicUsize>,
            flush_error: bool,
            failed: bool,
        }
        impl AsyncWrite for FailingWriter {
            fn poll_write(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
                bytes: &[u8],
            ) -> Poll<io::Result<usize>> {
                if !self.flush_error && !self.failed && self.bytes.load(Ordering::SeqCst) > 0 {
                    self.failed = true;
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "injected write failure",
                    )));
                }
                let count = if self.flush_error || self.failed {
                    bytes.len()
                } else {
                    3.min(bytes.len())
                };
                self.bytes.fetch_add(count, Ordering::SeqCst);
                Poll::Ready(Ok(count))
            }
            fn poll_flush(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
                if self.flush_error && !self.failed {
                    self.failed = true;
                    Poll::Ready(Err(io::Error::other("injected flush failure")))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }
        for flush_error in [false, true] {
            let bytes = Arc::new(AtomicUsize::new(0));
            let mut writer = JsonRpcFrameWriter::new(FailingWriter {
                bytes: Arc::clone(&bytes),
                flush_error,
                failed: false,
            });
            assert!(matches!(
                writer.write_json_rpc_frame(&json!({})).await,
                Err(JsonRpcTransportError::WriteFrame(_))
            ));
            let before = bytes.load(Ordering::SeqCst);
            assert!(before > 0);
            assert!(
                writer.write_json_rpc_frame(&json!({})).await.is_err(),
                "failed writer accepted another frame"
            );
            assert_eq!(bytes.load(Ordering::SeqCst), before);
        }
    }

    #[tokio::test]
    async fn frame_write_validation_error_preserves_stream() {
        let (sender, mut receiver) = duplex(64);
        let mut writer = JsonRpcFrameWriter::with_body_limit(sender, NonZeroUsize::new(2).unwrap());
        assert!(matches!(
            writer.write_json_rpc_frame(&json!({"too": "large"})).await,
            Err(JsonRpcTransportError::OutboundBodyTooLarge { .. })
        ));
        assert!(matches!(
            writer.write_json_rpc_frame(&Value::Null).await,
            Err(JsonRpcTransportError::MessageNotObject)
        ));
        writer.write_json_rpc_frame(&json!({})).await.unwrap();
        drop(writer);
        let mut wire = Vec::new();
        receiver.read_to_end(&mut wire).await.unwrap();
        assert_eq!(wire, b"Content-Length: 2\r\n\r\n{}");
    }

    #[tokio::test]
    async fn outbound_limit_writes_nothing() {
        let (sender, mut receiver) = duplex(64);
        let mut writer = JsonRpcFrameWriter::with_body_limit(sender, NonZeroUsize::new(2).unwrap());
        assert!(matches!(
            writer.write_json_rpc_frame(&json!({})).await,
            Ok(())
        ));
        assert!(matches!(
            writer.write_json_rpc_frame(&json!({"too": "large"})).await,
            Err(JsonRpcTransportError::OutboundBodyTooLarge { .. })
        ));
        drop(writer);

        let mut wire = Vec::new();
        receiver.read_to_end(&mut wire).await.unwrap();
        assert_eq!(wire, b"Content-Length: 2\r\n\r\n{}");
    }
}
