//! Sluice — Rust core.
//!
//! D-001: this process owns every database connection. The webview holds no
//! sockets and links no database crates; it receives plain data over IPC.

mod commands;
mod consent_dialog;
mod credentials;
mod guardrails;
mod launcher;
mod stream;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // D-003 forbids `panic = "abort"`, and Cargo.toml pins `panic = "unwind"` in
    // both profiles, because T-006's containment depends on panics unwinding.
    tauri::Builder::default()
        // T-026/D-012: registered first, as the plugin requires. A second
        // `sluice` invocation exits after handing its argv and working directory
        // to this instance, which opens a tab for it.
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            launcher::on_second_instance(app, argv, cwd);
        }))
        .plugin(tauri_plugin_dialog::init())
        .manage(commands::AppState::default())
        .manage(launcher::Launcher::default())
        .manage(stream::Streams::default())
        .setup(|app| {
            launcher::on_first_launch(app.handle().clone());
            Ok(())
        })
        .invoke_handler(build_handler())
        .run(tauri::generate_context!())
        .expect("error while running Sluice");
}

#[cfg(debug_assertions)]
fn build_handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        commands::core_version,
        commands::store_credential,
        commands::connect,
        commands::take_launches,
        commands::connect_launch,
        commands::save_session_as_profile,
        commands::disconnect,
        commands::navigator_children,
        commands::browse,
        commands::open_browse_stream,
        commands::grant_stream_credit,
        commands::cancel_stream,
        commands::run_script,
        commands::fetch_value,
        commands::save_value_to_file,
        commands::debug_panicking_driver_call
    ]
}

#[cfg(not(debug_assertions))]
fn build_handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        commands::core_version,
        commands::store_credential,
        commands::connect,
        commands::take_launches,
        commands::connect_launch,
        commands::save_session_as_profile,
        commands::disconnect,
        commands::navigator_children,
        commands::browse,
        commands::open_browse_stream,
        commands::grant_stream_credit,
        commands::cancel_stream,
        commands::run_script,
        commands::fetch_value,
        commands::save_value_to_file
    ]
}
