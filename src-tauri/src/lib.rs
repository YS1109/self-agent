mod bridge;

use bridge::{api_request, Bridge};
use tauri::{Emitter, Manager};

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Bridge::new())
        .invoke_handler(tauri::generate_handler![api_request])
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
        .expect("could not start the desktop application");
}
