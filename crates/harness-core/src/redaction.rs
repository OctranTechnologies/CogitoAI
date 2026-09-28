use std::fmt::{self, Write as _};
use std::sync::{OnceLock, RwLock};

use tracing::field::{Field, Visit};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{FormatFields, Writer};
use zeroize::Zeroizing;

static REGISTERED_SECRETS: OnceLock<RwLock<Vec<Zeroizing<String>>>> = OnceLock::new();

fn registered_secrets() -> &'static RwLock<Vec<Zeroizing<String>>> {
    REGISTERED_SECRETS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Registers a credential for defense-in-depth redaction of logs and errors.
///
/// Short values are ignored to avoid hiding ordinary words. The in-memory copy
/// is zeroized when the process exits.
pub fn register_sensitive_value(secret: &str) {
    if secret.trim().len() < 8 {
        return;
    }
    let mut secrets = registered_secrets()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !secrets.iter().any(|value| value.as_str() == secret) {
        secrets.push(Zeroizing::new(secret.to_owned()));
    }
}

/// Replaces any registered credential occurrences in a string.
pub fn redact_sensitive(text: &str) -> String {
    let secrets = registered_secrets()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    secrets.iter().fold(text.to_owned(), |text, secret| {
        text.replace(secret.as_str(), "[redacted]")
    })
}

/// Field formatter used by the harness logging subscriber.
///
/// Exact registered values are redacted in every log field, and fields with
/// credential-related names are omitted even if a secret was not registered.
#[derive(Clone, Copy, Debug, Default)]
pub struct RedactingFields;

impl<'writer> FormatFields<'writer> for RedactingFields {
    fn format_fields<R: RecordFields>(
        &self,
        mut writer: Writer<'writer>,
        fields: R,
    ) -> fmt::Result {
        let mut visitor = RedactingVisitor::default();
        fields.record(&mut visitor);
        writer.write_str(&visitor.output)
    }
}

#[derive(Default)]
struct RedactingVisitor {
    output: String,
}

impl Visit for RedactingVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if !self.output.is_empty() {
            self.output.push(' ');
        }
        let value = if is_credential_field(field.name()) {
            "\"[redacted]\"".to_owned()
        } else {
            redact_sensitive(&format!("{value:?}"))
        };
        let _ = write!(self.output, "{}={value}", field.name());
    }
}

fn is_credential_field(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [
        "api_key",
        "apikey",
        "secret",
        "credential",
        "authorization",
        "access_token",
    ]
    .iter()
    .any(|sensitive| name.contains(sensitive))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuffer {
        type Writer = SharedBufferWriter;

        fn make_writer(&'a self) -> Self::Writer {
            SharedBufferWriter(Arc::clone(&self.0))
        }
    }

    struct SharedBufferWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedBufferWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn registered_values_are_redacted_from_text_and_structured_logs() {
        let secret = "sk-log-redaction-test-value";
        register_sensitive_value(secret);
        assert_eq!(
            redact_sensitive(&format!("provider echoed {secret}")),
            "provider echoed [redacted]"
        );

        let output = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(SharedBuffer(Arc::clone(&output)))
            .fmt_fields(RedactingFields)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(credential = secret, message = %format!("echo {secret}"));
        });
        let rendered = String::from_utf8(
            output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .unwrap();
        assert!(!rendered.contains(secret));
        assert!(rendered.contains("credential=\"[redacted]\""));
        assert!(rendered.contains("[redacted]"));
    }

    #[test]
    fn credential_named_fields_are_redacted_without_registration() {
        let output = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(SharedBuffer(Arc::clone(&output)))
            .fmt_fields(RedactingFields)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(api_key = "a-not-registered-key", "connected");
        });
        let rendered = String::from_utf8(
            output
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .unwrap();
        assert!(!rendered.contains("a-not-registered-key"));
        assert!(rendered.contains("api_key=\"[redacted]\""));
    }
}
