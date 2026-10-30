mod bridge;
mod compatibility;
mod event_stream;
mod menu;
mod startup;

use bridge::{
    ack_event_stream, api_request, daemon_startup_status, start_event_stream, stop_event_stream,
    Bridge,
};
use tauri::Manager;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .menu(menu::chinese_menu)
        .manage(Bridge::new())
        .invoke_handler(tauri::generate_handler![
            api_request,
            daemon_startup_status,
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
                let _ = bridge.ensure_daemon(&handle).await;
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("无法启动桌面应用");
}
