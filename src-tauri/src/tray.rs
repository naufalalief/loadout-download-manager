//! System tray icon with an "Open" / "Quit" menu.
//!
//! Tauri 2 builds and attaches a tray icon against a live `AppHandle` (unlike Tauri 1's
//! `SystemTray`, which could be constructed standalone and attached later), so `build_tray`
//! takes the handle and registers the tray directly instead of returning a detached value.

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    AppHandle, Manager,
};

/// Builds the tray icon and wires up its "Open" / "Quit" menu handlers.
///
/// - `"open"` shows and focuses the main window.
/// - `"quit"` exits the process immediately.
pub fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    let open_item = MenuItem::with_id(app, "open", "Open", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open_item, &quit_item])?;

    TrayIconBuilder::new()
        .icon(app.default_window_icon().cloned().expect("default window icon missing"))
        .menu(&menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "open" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}
