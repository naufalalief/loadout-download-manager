// Library crate target so integration tests under `tests/` can exercise
// internal modules (e.g. `download`) without going through the `main`
// binary. `main.rs` remains the Tauri app entry point and is unaffected.

pub mod download;
pub mod extract;
pub mod server;
