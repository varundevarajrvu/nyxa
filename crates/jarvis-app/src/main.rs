//! Jarvis tray app: runs the always-on engine in the background with a system
//! tray icon and a window that shows live status + the action history (read
//! from the audit log). No console window.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

use jarvis_core::engine::{Engine, EngineConfig};
use jarvis_core::kill;

struct AppState {
    status: Arc<Mutex<String>>,
    audit_path: PathBuf,
    halted: Arc<AtomicBool>,
}

/// One row for the history view, parsed back out of the audit log.
#[derive(Serialize, Default)]
struct HistoryEntry {
    ts: String,
    transcript: String,
    stage: String,
    model: Option<String>,
    action: Option<String>,
    tier: Option<String>,
    outcome: String,
}

#[tauri::command]
fn get_status(state: tauri::State<AppState>) -> serde_json::Value {
    let status = state.status.lock().map(|g| g.clone()).unwrap_or_default();
    serde_json::json!({
        "status": status,
        "paused": state.halted.load(Ordering::Relaxed),
    })
}

/// Read the tail of the audit log and return the most recent entries, newest
/// first. Cheap enough to poll — the log is small and local.
#[tauri::command]
fn get_history(state: tauri::State<AppState>) -> Vec<HistoryEntry> {
    let Ok(text) = std::fs::read_to_string(&state.audit_path) else {
        return Vec::new();
    };
    text.lines()
        .rev()
        .take(80)
        .filter_map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).ok()?;
            Some(HistoryEntry {
                ts: v["ts"].as_str().unwrap_or("").to_string(),
                transcript: v["transcript"].as_str().unwrap_or("").to_string(),
                stage: v["stage"].as_str().unwrap_or("").to_string(),
                model: v["model"].as_str().map(str::to_string),
                action: v["action"].as_str().map(str::to_string),
                tier: v["tier"].as_str().map(str::to_string),
                outcome: v["outcome"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect()
}

#[tauri::command]
fn toggle_pause(state: tauri::State<AppState>) -> bool {
    let now = !state.halted.load(Ordering::Relaxed);
    state.halted.store(now, Ordering::Relaxed);
    now
}

fn open_window(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
        .title("Nyxa")
        .inner_size(780.0, 640.0)
        .min_inner_size(440.0, 420.0)
        .resizable(true)
        // Auto-grant the webcam to the real camera device (no popup) so the
        // hand-gesture control can start immediately when the user enables it.
        // We keep Tauri's default WebView2 flags and append the media + autoplay
        // switches. `use-fake-ui-for-media-stream` accepts the permission using
        // the REAL device (it is `use-fake-device...` that would fake it).
        .additional_browser_args(
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
             --use-fake-ui-for-media-stream \
             --autoplay-policy=no-user-gesture-required",
        )
        .build();
}

fn main() {
    // Load the engine and grab the shared handles BEFORE moving it into the
    // background thread that runs the mic loop.
    let engine = match Engine::load(EngineConfig::default()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("jarvis: failed to start engine: {e:#}");
            std::process::exit(1);
        }
    };
    let status = engine.status_handle();
    let audit_path = engine.audit_path();
    let halted = kill::spawn_hotkey_watcher();

    // Run the always-on listener on its own thread.
    let mut engine_mut = engine;
    let halted_thread = halted.clone();
    std::thread::spawn(move || {
        if let Err(e) = engine_mut.run_mic(halted_thread) {
            eprintln!("jarvis: mic loop ended: {e:#}");
        }
    });

    tauri::Builder::default()
        // Single instance: if Jarvis is already running (e.g. in the tray) and
        // the user clicks the taskbar/pin again, don't spawn a duplicate —
        // just bring the existing window back.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            open_window(app);
        }))
        .manage(AppState { status, audit_path, halted })
        .invoke_handler(tauri::generate_handler![get_status, get_history, toggle_pause])
        .setup(|app| {
            let handle = app.handle().clone();

            let show = MenuItem::with_id(app, "show", "Open Nyxa", true, None::<&str>)?;
            let pause = MenuItem::with_id(app, "pause", "Pause / Resume", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Nyxa", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &pause, &quit])?;
            let icon = tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?;

            TrayIconBuilder::with_id("jarvis-tray")
                .icon(icon)
                .tooltip("Nyxa — listening")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => open_window(app),
                    "pause" => {
                        let state = app.state::<AppState>();
                        let now = !state.halted.load(Ordering::Relaxed);
                        state.halted.store(now, Ordering::Relaxed);
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // Show the window on launch so the app isn't invisible on first run.
            open_window(&handle);
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error building Nyxa")
        .run(|_app, event| {
            // Keep running in the tray when the window is closed.
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}
