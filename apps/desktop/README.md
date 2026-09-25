# Desktop application

This directory is reserved for the Tauri 2 + React + TypeScript desktop
application.

It intentionally has no frontend or Tauri scaffold yet. The future client
will talk to the Rust runtime through `harness-rpc`; it will not directly run
privileged filesystem, shell, Git, network, or verification operations.

Setup and run commands will be added to the root README when the application
is runnable.
