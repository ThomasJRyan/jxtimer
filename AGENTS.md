# Repository guidance

- Native `eframe`/`egui` timer application. The executable and its unit tests live in `src/main.rs`.
- Run locally with `cargo run`.
- Verify changes with `cargo fmt --all --check`, `cargo test --all -- --nocapture`, and `cargo build --release`.
- Timer state receives explicit elapsed durations so its behavior remains deterministic in unit tests; keep UI clock access at the application boundary.
- Global shortcuts use `global-hotkey`; retain the non-fatal fallback because it supports Windows, macOS, and Linux X11, but not native Wayland.
- For EGUI/EFrame changes, use the `egui-eframe-development` and `egui-ui-ux` skills, including their rendered-UI review workflow when available.
