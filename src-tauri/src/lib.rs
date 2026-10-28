mod bridge;
mod event_stream;
mod menu;

use bridge::{ack_event_stream, api_request, start_event_stream, stop_event_stream, Bridge};
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .menu(menu::chinese_menu)
        .manage(Bridge::new())
        .invoke_handler(tauri::generate_handler![
            api_request,
            start_event_stream,
            stop_event_stream,
            ack_event_stream
        ])
        .on_window_event(|window, event| {
            if matches!(event, tauri::WindowEvent::Destroyed) {
                window.state::<Bridge>().stop_window(window.label());
            }
        })
        .setup(|app| {
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let bridge = handle.state::<Bridge>();
                let status = match bridge.ensure_daemon(&handle).await {
                    Ok(_) => serde_json::json!({ "status": "connected" }),
                    Err(message) => serde_json::json!({ "status": "error", "message": message }),
                };
                let _ = handle.emit("daemon-status", status);
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("无法启动桌面应用");
}
