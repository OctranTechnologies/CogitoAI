use std::io;

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
    let config: RuntimeLaunchConfig = serde_json::from_reader(stdin.lock())
        .map_err(|error| format!("could not read runtime startup configuration: {error}"))?;
    harness_core::init_logging("info")
        .map_err(|error| format!("could not initialize runtime logging: {error}"))?;
    serve_runtime_process(config)
}
