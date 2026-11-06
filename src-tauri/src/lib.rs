//! Sluice — Rust core.
//!
//! D-001: this process owns every database connection. The webview holds no
//! sockets and links no database crates; it receives plain data over IPC.

mod commands;
mod consent_dialog;
mod credentials;
mod diagnostics;
mod editor;
mod execution;
mod guardrails;
mod launcher;
mod state;
mod stream;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // T-023: shape-only logs and the crash hook, before anything can panic.
    diagnostics::init();

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
        .manage(state::Stores::default())
        .manage(launcher::Launcher::default())
        .manage(stream::Streams::default())
        .manage(std::sync::Arc::new(editor::Editors::default()))
        .setup(|app| {
            launcher::on_first_launch(app.handle().clone());
            // T-041: idle open transactions are rolled back by the core, even
            // when the webview is not looking.
            use tauri::Manager;
            let editors = app.state::<std::sync::Arc<editor::Editors>>().inner().clone();
            editor::spawn_reaper(app.handle().clone(), editors);
            Ok(())
        })
        // T-041 / D-028: closing the window rolls back every editor's open
        // transaction — rollback, never commit — before the process can go.
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                use tauri::Manager;
                let editors = window.state::<std::sync::Arc<editor::Editors>>().inner().clone();
                tauri::async_runtime::block_on(async move {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), editors.close_all()).await;
                });
            }
        })
        .invoke_handler(build_handler())
        .run(tauri::generate_context!())
        .expect("error while running Sluice");
}

#[cfg(debug_assertions)]
fn build_handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        commands::core_version,
        state::load_ui_state,
        state::save_ui_state,
        commands::store_credential,
        commands::connect,
        commands::take_launches,
        commands::connect_launch,
        commands::save_session_as_profile,
        commands::disconnect,
        commands::diagnostics_preview,
        commands::set_verbose_diagnostics,
        commands::verbose_diagnostics,
        commands::navigator_children,
        commands::refresh_navigator,
        commands::browse,
        commands::open_browse_stream,
        commands::grant_stream_credit,
        commands::cancel_stream,
        commands::run_script,
        commands::open_editor,
        commands::run_in_editor,
        commands::editor_transaction,
        commands::close_editor,
        commands::preview_edits,
        commands::apply_edits,
        commands::cancel_query,
        commands::fetch_value,
        commands::save_value_to_file,
        commands::debug_panicking_driver_call
    ]
}

#[cfg(not(debug_assertions))]
fn build_handler() -> impl Fn(tauri::ipc::Invoke) -> bool + Send + Sync + 'static {
    tauri::generate_handler![
        commands::core_version,
        state::load_ui_state,
        state::save_ui_state,
        commands::store_credential,
        commands::connect,
        commands::take_launches,
        commands::connect_launch,
        commands::save_session_as_profile,
        commands::disconnect,
        commands::diagnostics_preview,
        commands::set_verbose_diagnostics,
        commands::verbose_diagnostics,
        commands::navigator_children,
        commands::refresh_navigator,
        commands::browse,
        commands::open_browse_stream,
        commands::grant_stream_credit,
        commands::cancel_stream,
        commands::run_script,
        commands::open_editor,
        commands::run_in_editor,
        commands::editor_transaction,
        commands::close_editor,
        commands::preview_edits,
        commands::apply_edits,
        commands::cancel_query,
        commands::fetch_value,
        commands::save_value_to_file
    ]
}
