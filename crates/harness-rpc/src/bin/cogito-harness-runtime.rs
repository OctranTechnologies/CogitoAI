use std::io::{self, BufRead, Read};

use harness_rpc::{serve_runtime_process, RuntimeLaunchConfig};

fn main() {
    if let Err(error) = run() {
        let safe_error = harness_core::redact_sensitive(&error);
        eprintln!("Harness runtime failed: {safe_error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let stdin = io::stdin();
    const MAX_STARTUP_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
    let mut input = String::new();
    let bytes_read = io::BufReader::new(stdin.lock())
        .take(MAX_STARTUP_CONFIG_BYTES)
        .read_line(&mut input)
        .map_err(|error| format!("could not read runtime startup configuration: {error}"))?;
    if bytes_read == 0 || !input.ends_with('\n') || bytes_read as u64 >= MAX_STARTUP_CONFIG_BYTES {
        return Err(
            "runtime startup configuration was missing or exceeded its size limit".to_owned(),
        );
    }
    let config: RuntimeLaunchConfig = serde_json::from_str(&input)
        .map_err(|error| format!("could not parse runtime startup configuration: {error}"))?;
    harness_core::init_logging("info")
        .map_err(|error| format!("could not initialize runtime logging: {error}"))?;
    serve_runtime_process(config)
}
