//! Private HTTP/SSE plumbing shared by model protocol adapters.
//!
//! Provider adapters own endpoint selection, headers, wire payloads, and event
//! interpretation. This module only handles generic HTTP requests, bounded
//! retries before a response starts, and SSE framing.

use std::error::Error as _;
use std::io::{BufRead, ErrorKind};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::ProviderError;

const MAX_ATTEMPTS: usize = 3;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(80);

pub(crate) fn request_json(
    agent: &ureq::Agent,
    method: &str,
    endpoint: &str,
    headers: &[(&str, &str)],
    body: &Value,
    provider: &'static str,
) -> Result<ureq::Response, ProviderError> {
    request_with_retry(provider, || {
        let mut request = agent.request(method, endpoint);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        request.send_json(body).map_err(Box::new)
    })
}

pub(crate) fn request_get(
    agent: &ureq::Agent,
    endpoint: &str,
    headers: &[(&str, &str)],
    provider: &'static str,
) -> Result<ureq::Response, ProviderError> {
    request_with_retry(provider, || {
        let mut request = agent.get(endpoint);
        for (name, value) in headers {
            request = request.set(name, value);
        }
        request.call().map_err(Box::new)
    })
}

fn request_with_retry<F>(
    provider: &'static str,
    mut send: F,
) -> Result<ureq::Response, ProviderError>
where
    F: FnMut() -> Result<ureq::Response, Box<ureq::Error>>,
{
    for attempt in 0..MAX_ATTEMPTS {
        match send() {
            Ok(response) => return Ok(response),
            Err(error) if attempt + 1 < MAX_ATTEMPTS && is_transient(&error) => {
                thread::sleep(INITIAL_RETRY_DELAY * (1u32 << attempt));
            }
            Err(error) => return Err(map_ureq_error(provider, *error)),
        }
    }
    unreachable!("the bounded retry loop always returns")
}

fn is_transient(error: &ureq::Error) -> bool {
    match error {
        ureq::Error::Status(status, _) => *status == 408 || *status == 429 || *status >= 500,
        ureq::Error::Transport(error) => {
            matches!(
                error.kind(),
                ureq::ErrorKind::Dns
                    | ureq::ErrorKind::ConnectionFailed
                    | ureq::ErrorKind::ProxyConnect
            ) || is_io_timeout(error)
        }
    }
}

fn map_ureq_error(provider: &'static str, error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Status(status, _) => ProviderError::Request {
            provider,
            status: Some(status),
        },
        ureq::Error::Transport(error) if is_io_timeout(&error) => {
            ProviderError::Timeout { provider }
        }
        ureq::Error::Transport(_) => ProviderError::Transport { provider },
    }
}

fn is_io_timeout(error: &ureq::Transport) -> bool {
    error
        .source()
        .and_then(|source| source.downcast_ref::<std::io::Error>())
        .is_some_and(|source| source.kind() == std::io::ErrorKind::TimedOut)
}

/// Delivers each SSE `data:` payload. Return `false` from the callback to stop
/// reading (for example, after the protocol's terminal event).
#[cfg(test)]
pub(crate) fn for_each_sse_data<R, F>(
    reader: R,
    provider: &'static str,
    on_data: F,
) -> Result<(), ProviderError>
where
    R: BufRead,
    F: FnMut(&str) -> Result<bool, ProviderError>,
{
    for_each_sse_data_cancellable(reader, provider, None, &|| false, on_data)
}

/// The streaming agent uses a short read timeout so it can observe cancellation
/// even when the remote server is idle between events. Timed out partial lines
/// remain buffered and the loop resumes reading until an event or cancellation.
pub(crate) fn for_each_sse_data_cancellable<R, F>(
    mut reader: R,
    provider: &'static str,
    timeout: Option<Duration>,
    is_cancelled: &dyn Fn() -> bool,
    mut on_data: F,
) -> Result<(), ProviderError>
where
    R: BufRead,
    F: FnMut(&str) -> Result<bool, ProviderError>,
{
    let mut line = String::new();
    let mut data = String::new();
    let started_at = Instant::now();
    loop {
        if is_cancelled() {
            return Err(ProviderError::Cancelled);
        }
        if timeout.is_some_and(|limit| started_at.elapsed() >= limit) {
            return Err(ProviderError::Timeout { provider });
        }
        match reader.read_line(&mut line) {
            Ok(0) => {
                if let Some(payload) = line.trim_end_matches(['\r', '\n']).strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(payload.trim_start());
                }
                if !data.is_empty() && !dispatch_data(&data, &mut on_data)? {
                    return Ok(());
                }
                return Ok(());
            }
            Ok(_) => {
                let current_line = std::mem::take(&mut line);
                let line = current_line.trim_end_matches(['\r', '\n']);
                if line.is_empty() {
                    if !data.is_empty() && !dispatch_data(&data, &mut on_data)? {
                        return Ok(());
                    }
                    data.clear();
                } else if let Some(payload) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(payload.trim_start());
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                continue;
            }
            Err(_) => return Err(ProviderError::Transport { provider }),
        }
    }
}

fn dispatch_data<F>(data: &str, on_data: &mut F) -> Result<bool, ProviderError>
where
    F: FnMut(&str) -> Result<bool, ProviderError>,
{
    on_data(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn parses_sse_data_lines_and_stops_on_terminal_event() {
        let input = b"event: update\ndata: one\n\ndata: two\n\ndata: [DONE]\n\ndata: ignored\n\n";
        let mut values = Vec::new();
        for_each_sse_data(&input[..], "test", |value| {
            values.push(value.to_owned());
            Ok(value != "[DONE]")
        })
        .unwrap();
        assert_eq!(values, ["one", "two", "[DONE]"]);
    }

    #[test]
    fn dispatches_multiline_sse_data_as_one_payload() {
        let input = Cursor::new(b"data: first\ndata: second\n\n".to_vec());
        let mut values = Vec::new();
        for_each_sse_data(input, "test", |value| {
            values.push(value.to_owned());
            Ok(true)
        })
        .unwrap();
        assert_eq!(values, ["first\nsecond"]);
    }

    #[test]
    fn cancellable_sse_reader_reports_cancel_and_total_timeout() {
        let cancelled =
            for_each_sse_data_cancellable(&b""[..], "test", None, &|| true, |_| Ok(true));
        assert!(matches!(cancelled, Err(ProviderError::Cancelled)));

        let timed_out = for_each_sse_data_cancellable(
            &b""[..],
            "test",
            Some(Duration::ZERO),
            &|| false,
            |_| Ok(true),
        );
        assert!(matches!(
            timed_out,
            Err(ProviderError::Timeout { provider: "test" })
        ));
    }
}
