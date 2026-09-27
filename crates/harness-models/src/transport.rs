//! Private HTTP/SSE plumbing shared by model protocol adapters.
//!
//! Provider adapters own endpoint selection, headers, wire payloads, and event
//! interpretation. This module only handles generic HTTP JSON posts and SSE
//! framing, so another adapter can use it without sharing provider wire types.

use std::io::BufRead;

use serde_json::Value;

use super::ProviderError;

pub(crate) fn request_json(
    method: &str,
    endpoint: &str,
    headers: &[(&str, &str)],
    body: &Value,
    provider: &'static str,
) -> Result<ureq::Response, ProviderError> {
    let mut request = ureq::request(method, endpoint);
    for (name, value) in headers {
        request = request.set(name, value);
    }
    request
        .send_json(body)
        .map_err(|error| map_ureq_error(provider, error))
}

/// Delivers each SSE `data:` payload. Return `false` from the callback to stop
/// reading (for example, after the protocol's terminal marker).
pub(crate) fn for_each_sse_data<R, F>(
    reader: R,
    provider: &'static str,
    mut on_data: F,
) -> Result<(), ProviderError>
where
    R: BufRead,
    F: FnMut(&str) -> Result<bool, ProviderError>,
{
    for line in reader.lines() {
        let line = line.map_err(|_| ProviderError::Transport { provider })?;
        if !line.starts_with("data:") {
            continue;
        }
        if !on_data(line[5..].trim())? {
            break;
        }
    }
    Ok(())
}

fn map_ureq_error(provider: &'static str, error: ureq::Error) -> ProviderError {
    match error {
        ureq::Error::Status(status, _) => ProviderError::Request {
            provider,
            status: Some(status),
        },
        ureq::Error::Transport(_) => ProviderError::Transport { provider },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sse_data_lines_and_stops_on_terminal_event() {
        let input = b"event: update\ndata: one\n\ndata: two\ndata: [DONE]\ndata: ignored\n";
        let mut values = Vec::new();
        for_each_sse_data(&input[..], "test", |value| {
            values.push(value.to_owned());
            Ok(value != "[DONE]")
        })
        .unwrap();
        assert_eq!(values, ["one", "two", "[DONE]"]);
    }
}
