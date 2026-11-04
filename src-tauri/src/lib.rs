pub mod client;
pub mod commands;
pub mod config;
pub mod models;
pub mod server_process;
pub mod sse;
pub mod state;
pub mod tray;

use tauri::Manager;

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            // Autostart plugin initialization on desktop platforms
            #[cfg(desktop)]
            {
                let _ = app.handle().plugin(tauri_plugin_autostart::init(
                    tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                    Some(vec!["--minimized"]),
                ));
            }

            let config_dir = app.path().app_config_dir().ok();
            let initial_settings = config::load_settings(config_dir.as_deref());
            let app_state = state::AppState::new(config_dir, initial_settings);
            app.manage(app_state);

            // Spawn server child process (or connect if already running)
            let server_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = server_handle.state::<state::AppState>();
                let settings = state.settings.read().await.clone();
                let _ = state.server_process.start(&settings).await;
            });

            // Start background SSE event listener loop
            let sse_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let state = sse_handle.state::<state::AppState>();
                state.start_sse_loop(sse_handle.clone()).await;
            });

            // Initialize system tray
            if let Err(err) = tray::setup_tray(app) {
                eprintln!("Failed to setup system tray: {err}");
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            // Settings
            commands::get_settings,
            commands::save_settings,
            commands::test_connection,
            // Server process lifecycle
            commands::start_server,
            commands::stop_server,
            commands::restart_server,
            commands::get_server_status,
            // Server config & tools
            commands::fetch_server_config,
            commands::fetch_tools_list,
            commands::update_tool_policy,
            // Autostart
            commands::get_autostart,
            commands::set_autostart,
            // Safety console
            commands::list_pending,
            commands::decide,
            commands::get_audit_history,
            commands::clear_audit_history,
            commands::reconnect_sse,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                let state = app_handle.state::<state::AppState>();
                state.server_process.stop_child();
            }
        });
}
