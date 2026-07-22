//! Framework-neutral HTTP boundary helpers for xapi.
//!
//! The core crate remains independent of HTTP types. This crate only maps
//! status/header policy to the core streaming reader and writer APIs.
//!
//! Enable the `tokio` feature for `decode_response_async`, which runs the
//! synchronous core parser behind `SyncIoBridge` without buffering the body.

use std::io::{Read, Write};

#[cfg(feature = "tokio")]
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use http::{header, HeaderMap, StatusCode};
use xapi::{ContentType, Error, Format, Limits, XapiRoot};

#[derive(Debug, thiserror::Error)]
pub enum HttpError {
    #[error("HTTP status is not successful: {0}")]
    Status(StatusCode),
    #[error("missing or invalid Content-Type header")]
    ContentType,
    #[error(transparent)]
    Xapi(#[from] Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    pub strict: bool,
    pub limits: Limits,
}

pub fn decode_response<T, R>(
    status: StatusCode,
    headers: &HeaderMap,
    body: R,
    options: Options,
) -> std::result::Result<T, HttpError>
where
    T: XapiRoot,
    R: Read,
{
    if !status.is_success() {
        return Err(HttpError::Status(status));
    }
    let format = request_format(headers)?;
    xapi::from_reader_with_options(body, format, options.limits, options.strict)
        .map_err(HttpError::from)
}

pub fn decode_response_with_cancel<T, R, F>(
    status: StatusCode,
    headers: &HeaderMap,
    body: R,
    options: Options,
    cancelled: F,
) -> std::result::Result<T, HttpError>
where
    T: XapiRoot,
    R: Read,
    F: FnMut() -> bool,
{
    decode_response(
        status,
        headers,
        CancelReader {
            inner: body,
            cancelled,
        },
        options,
    )
}

struct CancelReader<R, F> {
    inner: R,
    cancelled: F,
}

impl<R: Read, F: FnMut() -> bool> Read for CancelReader<R, F> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if (self.cancelled)() {
            // `Read::read_to_end` retries `Interrupted`, which would spin
            // forever when the decoder is cancelled at a read boundary.
            return Err(std::io::Error::other("xapi HTTP body read cancelled"));
        }
        self.inner.read(buffer)
    }
}

#[cfg(feature = "tokio")]
#[derive(Debug, thiserror::Error)]
pub enum AsyncHttpError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("xapi HTTP decode timed out")]
    Timeout,
    #[error("xapi HTTP decode task failed")]
    Task,
}

#[cfg(feature = "tokio")]
pub async fn decode_response_async<T, R>(
    status: StatusCode,
    headers: &HeaderMap,
    body: R,
    options: Options,
    timeout: Option<std::time::Duration>,
) -> std::result::Result<T, AsyncHttpError>
where
    T: XapiRoot + Send + 'static,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    decode_response_async_with_cancel(
        status,
        headers,
        body,
        options,
        Arc::new(AtomicBool::new(false)),
        timeout,
    )
    .await
}

#[cfg(feature = "tokio")]
pub async fn decode_response_async_with_cancel<T, R>(
    status: StatusCode,
    headers: &HeaderMap,
    body: R,
    options: Options,
    cancelled: Arc<AtomicBool>,
    timeout: Option<std::time::Duration>,
) -> std::result::Result<T, AsyncHttpError>
where
    T: XapiRoot + Send + 'static,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let headers = headers.clone();
    let task_cancelled = Arc::clone(&cancelled);
    let task = tokio::task::spawn_blocking(move || {
        let body = tokio_util::io::SyncIoBridge::new(body);
        decode_response_with_cancel(status, &headers, body, options, || {
            task_cancelled.load(Ordering::Acquire)
        })
    });
    let result = match timeout {
        Some(duration) => match tokio::time::timeout(duration, task).await {
            Ok(result) => result.map_err(|_| AsyncHttpError::Task)?,
            Err(_) => {
                cancelled.store(true, Ordering::Release);
                return Err(AsyncHttpError::Timeout);
            }
        },
        None => task.await.map_err(|_| AsyncHttpError::Task)?,
    };
    result.map_err(AsyncHttpError::from)
}

pub fn encode_response<T, W>(
    headers: &mut HeaderMap,
    body: W,
    value: &T,
    format: Format,
) -> std::result::Result<(), HttpError>
where
    T: XapiRoot,
    W: Write,
{
    headers.insert(
        header::CONTENT_TYPE,
        http::HeaderValue::from_static(match format {
            Format::NexacroJson10 => "application/json; charset=utf-8",
            Format::XPlatformXml4000 | Format::NexacroXml4000 => "application/xml; charset=utf-8",
        }),
    );
    xapi::to_writer(body, value, format).map_err(HttpError::from)
}

pub fn request_format(headers: &HeaderMap) -> std::result::Result<Format, HttpError> {
    let value = headers
        .get(header::CONTENT_TYPE)
        .ok_or(HttpError::ContentType)?
        .to_str()
        .map_err(|_| HttpError::ContentType)?;
    let content_type = ContentType::parse(value).map_err(|_| HttpError::ContentType)?;
    if content_type
        .charset
        .as_deref()
        .is_some_and(|charset| !charset.eq_ignore_ascii_case("utf-8"))
    {
        return Err(HttpError::ContentType);
    }
    Ok(content_type.format())
}

pub fn reject_body(status: StatusCode) -> std::result::Result<(), HttpError> {
    if status.is_success() {
        Ok(())
    } else {
        Err(HttpError::Status(status))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::{header, HeaderMap, HeaderValue, StatusCode};
    use std::io::Cursor;
    use xapi::{Format, XapiRoot, XapiRow};

    #[cfg(feature = "tokio")]
    struct AsyncBytes {
        bytes: Vec<u8>,
        position: usize,
    }

    #[cfg(feature = "tokio")]
    impl AsyncBytes {
        fn new(bytes: &[u8]) -> Self {
            Self {
                bytes: bytes.to_vec(),
                position: 0,
            }
        }
    }

    #[cfg(feature = "tokio")]
    impl tokio::io::AsyncRead for AsyncBytes {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _context: &mut std::task::Context<'_>,
            buffer: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            let remaining = &self.bytes[self.position..];
            if remaining.is_empty() {
                return std::task::Poll::Ready(Ok(()));
            }
            let amount = remaining.len().min(buffer.remaining());
            buffer.put_slice(&remaining[..amount]);
            self.position += amount;
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[derive(XapiRow)]
    struct Row {
        #[xapi(col = "id", wire = "INT")]
        id: i32,
    }
    #[derive(XapiRoot)]
    struct Response {
        #[xapi(dataset = "rows")]
        rows: Vec<Row>,
    }

    #[test]
    fn decodes_content_type_parameters_and_rejects_non_2xx() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/xml; charset=UTF-8"),
        );
        let body = Cursor::new(br#"<Root><Datasets><Dataset id="rows"><Rows><Row><Col id="id">3</Col></Row></Rows></Dataset></Datasets></Root>"#);
        let response: Response =
            decode_response(StatusCode::OK, &headers, body, Options::default()).unwrap();
        assert_eq!(response.rows[0].id, 3);
        assert!(matches!(
            decode_response::<Response, _>(
                StatusCode::BAD_REQUEST,
                &headers,
                Cursor::new([]),
                Options::default()
            ),
            Err(HttpError::Status(StatusCode::BAD_REQUEST))
        ));

        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=iso-8859-1"),
        );
        assert!(matches!(
            request_format(&headers),
            Err(HttpError::ContentType)
        ));
    }

    #[test]
    fn writes_content_type_and_streams_body() {
        let mut headers = HeaderMap::new();
        let mut body = Vec::new();
        let value = Response {
            rows: vec![Row { id: 9 }],
        };
        encode_response(&mut headers, &mut body, &value, Format::NexacroJson10).unwrap();
        assert_eq!(
            headers[header::CONTENT_TYPE],
            "application/json; charset=utf-8"
        );
        assert!(String::from_utf8(body).unwrap().contains("\"id\":9"));
    }

    #[test]
    fn cancellation_stops_body_reads() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let result = decode_response_with_cancel::<Response, _, _>(
            StatusCode::OK,
            &headers,
            Cursor::new(br#"{"Datasets":[]}"#),
            Options::default(),
            || true,
        );
        assert!(matches!(result, Err(HttpError::Xapi(_))));
    }

    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "current_thread")]
    async fn tokio_bridge_decodes_without_body_materialization() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let response: Response = decode_response_async(
            StatusCode::OK,
            &headers,
            AsyncBytes::new(br#"{"Datasets":[{"id":"rows","Rows":[{"id":3}]}]}"#),
            Options::default(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(response.rows[0].id, 3);
    }

    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "current_thread")]
    async fn tokio_bridge_propagates_cancellation_at_read_boundary() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let cancelled = Arc::new(AtomicBool::new(true));
        let result = decode_response_async_with_cancel::<Response, _>(
            StatusCode::OK,
            &headers,
            AsyncBytes::new(br#"{"Datasets":[]}"#),
            Options::default(),
            cancelled,
            None,
        )
        .await;
        assert!(matches!(
            result,
            Err(AsyncHttpError::Http(HttpError::Xapi(_)))
        ));
    }
}
