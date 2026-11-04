use tauri::menu::MenuBuilder;
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Runtime};

pub const TRAY_MENU_OPEN: &str = "tray_open_control";
pub const TRAY_MENU_QUIT: &str = "tray_quit";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuAction {
    Open,
    Quit,
    Unknown,
}

pub fn map_menu_id_to_action(id: &str) -> TrayMenuAction {
    match id {
        TRAY_MENU_OPEN => TrayMenuAction::Open,
        TRAY_MENU_QUIT => TrayMenuAction::Quit,
        _ => TrayMenuAction::Unknown,
    }
}

pub fn show_main_window<R: Runtime>(app: &AppHandle<R>) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

pub fn setup_tray<R: Runtime>(app: &tauri::App<R>) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle();
    let menu = MenuBuilder::new(handle)
        .text(TRAY_MENU_OPEN, "Open Ryns Control")
        .text(TRAY_MENU_QUIT, "Quit")
        .build()?;

    let mut builder = TrayIconBuilder::with_id("control_tray")
        .menu(&menu)
        .show_menu_on_left_click(false);

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }

    #[cfg(target_os = "macos")]
    {
        builder = builder.icon_as_template(true);
    }

    builder
        .on_menu_event(
            |app, event| match map_menu_id_to_action(event.id().as_ref()) {
                TrayMenuAction::Open => {
                    show_main_window(app);
                }
                TrayMenuAction::Quit => {
                    app.exit(0);
                }
                TrayMenuAction::Unknown => {}
            },
        )
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            }
            | TrayIconEvent::DoubleClick {
                button: MouseButton::Left,
                ..
            } => {
                show_main_window(tray.app_handle());
            }
            _ => {}
        })
        .build(app)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tray_menu_id_mapping() {
        assert_eq!(
            map_menu_id_to_action(TRAY_MENU_OPEN),
            TrayMenuAction::Open,
            "TRAY_MENU_OPEN must map to TrayMenuAction::Open"
        );
        assert_eq!(
            map_menu_id_to_action(TRAY_MENU_QUIT),
            TrayMenuAction::Quit,
            "TRAY_MENU_QUIT must map to TrayMenuAction::Quit"
        );
        assert_eq!(
            map_menu_id_to_action("random_menu_id"),
            TrayMenuAction::Unknown,
            "Unrecognized menu id must map to TrayMenuAction::Unknown"
        );
        assert_eq!(
            map_menu_id_to_action(""),
            TrayMenuAction::Unknown,
            "Empty menu id must map to TrayMenuAction::Unknown"
        );
    }
}
