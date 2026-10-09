//! SoundCraft on Android.
//!
//! Runs the same [`soundcraft_ui_egui::SoundApp`] as the desktop app inside a `GameActivity`
//! (android-activity's `game-activity` backend, which eframe needs for the soft keyboard and
//! accesskit). Built with `cargo ndk` into `android/app/src/main/jniLibs`, then packaged by the
//! Gradle project in `android/`.
//!
//! Differences from the desktop app:
//! - no audio playback or recording yet: the app runs without a player (the transport is
//!   simulated, as in tests and offscreen renders), so no audio device is opened;
//! - no TCP control server;
//! - Browse… in an Open / Import dialog asks `MainActivity.pickOpen()` (Storage Access Framework);
//!   the file is copied into the app's private `Imports/` folder on a Java thread and the command
//!   runs with that path on the next frame (`nativeDeliverFile`);
//! - Browse… in Save As / Save Copy picks `Sessions/<name>` in the private files folder; every
//!   saved session, bounce or export is then copied to `Downloads/SoundCraft/<name>` through
//!   `MainActivity.copyToDownloads` (MediaStore, no dialog);
//! - `$HOME` (where path prompts start), preferences, autosaves and presets live in the app's
//!   private files directory.

#![cfg(target_os = "android")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use android_activity::AndroidApp;
use jni::objects::{JObject, JString};
use jni::{Env, EnvUnowned, JavaVM, jni_sig, jni_str};
use serde_json::json;
use soundcraft_engine::Engine;
use soundcraft_ui_egui::dialogs::Dialog;
use soundcraft_ui_egui::{Services, SoundApp, UiState};

const LOG_TAG: &str = "soundcraft";
/// Preferences are also written this often while the app runs: Android may end the process
/// without calling `on_exit`.
const PREFS_INTERVAL: Duration = Duration::from_secs(30);

/// Files the Kotlin side delivers (command id, path in `Imports/`); the app runs the command on
/// its next frame.
static PICKED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());
/// The egui context, to wake the app when a file arrives from a Java thread.
static CTX: OnceLock<egui::Context> = OnceLock::new();
/// The process's Java VM (set once) and the current activity (a reference android-activity
/// owns, stored as an address; 0 = none).
static VM: OnceLock<JavaVM> = OnceLock::new();
static ACTIVITY: Mutex<usize> = Mutex::new(0);

/// The activity's entry point, called by android-activity's GameActivity glue on its own thread.
/// It returns when the activity is destroyed.
#[unsafe(no_mangle)]
fn android_main(app: AndroidApp) {
    static LOGGER: OnceLock<()> = OnceLock::new();
    LOGGER.get_or_init(|| {
        android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag(LOG_TAG));
    });
    // SAFETY: `vm_as_ptr` is the process's JavaVM, valid for the life of the process.
    let vm = unsafe { JavaVM::from_raw(app.vm_as_ptr().cast()) };
    let _ = VM.set(vm);
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = app.activity_as_ptr() as usize;

    let data_dir = app.internal_data_path().unwrap_or_else(|| PathBuf::from("/data/local/tmp"));
    for sub in ["Imports", "Sessions", "Autosave", "Presets"] {
        if let Err(e) = std::fs::create_dir_all(data_dir.join(sub)) {
            log::warn!("couldn't create {sub}: {e}");
        }
    }
    // The UI's path prompts start at `$HOME/`, which Android doesn't set.
    // SAFETY: set once at startup, before this shell starts any thread that reads the environment.
    unsafe { std::env::set_var("HOME", &data_dir) };
    log::info!("SoundCraft {} starting; data in {}", env!("CARGO_PKG_VERSION"), data_dir.display());
    let options = eframe::NativeOptions {
        android_app: Some(app),
        // eframe saves egui window sizes here on exit.
        persistence_path: Some(data_dir.join("ui.ron")),
        ..Default::default()
    };
    let result = eframe::run_native(
        "SoundCraft",
        options,
        Box::new(move |cc| {
            let _ = CTX.set(cc.egui_ctx.clone());
            if let Some(rs) = &cc.wgpu_render_state {
                let info = rs.adapter.get_info();
                log::info!("wgpu backend {:?}, adapter {}", info.backend, info.name);
            }
            let prefs = data_dir.join("ui.json");
            let saved = load_prefs(&prefs);
            // The first launch shows the demo session (there is nothing to open yet); later
            // launches start empty, like the desktop app.
            let engine = if saved.is_none() { soundcraft_engine::demo::demo_engine() } else { Engine::default() };
            let mut app = SoundApp::new(engine, None, services(&data_dir));
            app.autosave_dir = Some(data_dir.join("Autosave"));
            app.preset_dir = Some(data_dir.join("Presets"));
            if let Some(ui) = saved {
                app.ui = ui;
            }
            Ok(Box::new(Shell { app, prefs, journal_seen: 0, last_prefs: String::new(), prefs_saved_at: Instant::now() }))
        }),
    );
    *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    if let Err(e) = result {
        log::error!("SoundCraft stopped: {e}");
        // winit allows one event loop per process: when Android recreates the activity in the
        // same process, end the process so the next launch starts clean instead of a blank window.
        std::process::exit(0);
    }
}

/// The eframe app: [`SoundApp`] plus what the desktop shell does around it (preferences, quit)
/// and the Android file flows.
struct Shell {
    app: SoundApp,
    prefs: PathBuf,
    /// Journal entries already checked for files to copy to Downloads.
    journal_seen: usize,
    last_prefs: String,
    prefs_saved_at: Instant,
}

impl eframe::App for Shell {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.logic(ctx);
        let picked = std::mem::take(&mut *PICKED.lock().unwrap_or_else(PoisonError::into_inner));
        for (cmd, path) in picked {
            open_picked(&mut self.app, &cmd, &path);
        }
        self.mirror_outputs();
        if self.app.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if self.prefs_saved_at.elapsed() >= PREFS_INTERVAL {
            self.prefs_saved_at = Instant::now();
            self.save_prefs();
        }
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        self.app.raw_input_hook(raw);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.ui(ui);
    }

    fn on_exit(&mut self) {
        self.save_prefs();
    }
}

impl Shell {
    /// Every session save, bounce or export the engine journaled since the last frame: copy the
    /// file to `Downloads/SoundCraft` so it is reachable outside the app's private storage.
    fn mirror_outputs(&mut self) {
        let journal = &self.app.engine.journal;
        let start = self.journal_seen.min(journal.len());
        let mut files: Vec<String> = Vec::new();
        for (id, params) in &journal[start..] {
            if !(id.starts_with("session.save") || id.starts_with("file.bounce") || id.starts_with("file.export") || id == "file.print_score") {
                continue;
            }
            let path = params.get("path").and_then(|p| p.as_str()).map(str::to_string).or_else(|| self.app.engine.path.clone());
            if let Some(p) = path
                && !p.contains("/Autosave/")
                && Path::new(&p).is_file()
            {
                files.push(p);
            }
        }
        self.journal_seen = journal.len();
        for p in files {
            let name = file_name(&p);
            match copy_to_downloads(&p, &name) {
                Ok(()) => self.app.ui.status = format!("Copied to Downloads/SoundCraft/{name}"),
                Err(e) => {
                    log::warn!("couldn't copy {p} to Downloads: {e}");
                    self.app.ui.status = format!("Couldn't copy {name} to Downloads: {e}");
                }
            }
        }
    }

    /// The same `ui.json` as the desktop app, in the private files directory.
    fn save_prefs(&mut self) {
        let mut ui = self.app.ui.clone();
        ui.plugin_windows.clear();
        ui.status.clear();
        let Ok(text) = serde_json::to_string_pretty(&ui) else { return };
        if text == self.last_prefs {
            return;
        }
        match write_atomic(&self.prefs, text.as_bytes()) {
            Ok(()) => self.last_prefs = text,
            Err(e) => log::warn!("couldn't save preferences: {e}"),
        }
    }
}

fn load_prefs(path: &Path) -> Option<UiState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A picked file arrived: finish the dialog that asked for it (with that command's own parameter
/// key), or run the command directly when the dialog is already gone.
fn open_picked(app: &mut SoundApp, cmd: &str, path: &str) {
    let dialog_key = match &app.dialogs.open {
        Some(Dialog::PathPrompt { cmd: c, key, .. }) if c == cmd => Some(key.clone()),
        _ => None,
    };
    let key = match dialog_key {
        Some(k) => {
            app.dialogs.open = None;
            k
        }
        None => "path".to_string(),
    };
    let mut params = json!({});
    params[key.as_str()] = json!(path);
    if cmd == "file.import_audio" {
        params["at"] = json!(app.engine.session().edit.selection.start);
    }
    if let Err(e) = app.run(cmd, params) {
        log::warn!("{cmd} {path}: {e}");
    }
}

/// The host services: Browse… buttons in the path dialogs.
fn services(data_dir: &Path) -> Services {
    let sessions = data_dir.join("Sessions");
    Services {
        // The picker is asynchronous: the file arrives later through `nativeDeliverFile`.
        pick_open: Some(Box::new(|cmd: &str, _exts: &[&str]| {
            if let Err(e) = pick_open(cmd) {
                log::error!("couldn't open the file picker: {e}");
            }
            None
        })),
        // No save dialog: the suggested name goes to the private Sessions folder, and from there
        // to Downloads/SoundCraft once saved.
        pick_save: Some(Box::new(move |_cmd: &str, default: &str| {
            let name = file_name(default);
            let name = if name.is_empty() { "Untitled.scraft".to_string() } else { name };
            Some(sessions.join(name).to_string_lossy().into_owned())
        })),
    }
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string())
}

/// Write `bytes` to `path` through a temporary file, so a crash mid-write keeps the old file.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

// ---- Calls into MainActivity (Kotlin) ----------------------------------------------------------

/// Run `f` with a JNI environment on this thread and the current activity.
fn with_activity<T>(f: impl FnOnce(&mut Env<'_>, &JObject<'_>) -> jni::errors::Result<T>) -> Result<T, String> {
    let vm = VM.get().ok_or("the Java VM is not available")?;
    let raw = *ACTIVITY.lock().unwrap_or_else(PoisonError::into_inner);
    if raw == 0 {
        return Err("the activity is not running".to_string());
    }
    let raw = raw as jni::sys::jobject;
    vm.attach_current_thread(|env| -> jni::errors::Result<T> {
        // SAFETY: the reference comes from android-activity's `activity_as_ptr`, which keeps it
        // valid while the activity runs (ACTIVITY is cleared when `run_native` returns). `Cast`
        // neither owns nor deletes it.
        let activity = unsafe { env.as_cast_raw::<JObject>(&raw)? };
        f(env, &activity)
    })
    .map_err(|e| e.to_string())
}

/// `MainActivity.pickOpen(cmd)`: show the system file picker; the file comes back with `cmd`
/// through `nativeDeliverFile`.
fn pick_open(cmd: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jcmd = JString::from_str(env, cmd)?;
        env.call_method(activity, jni_str!("pickOpen"), jni_sig!("(Ljava/lang/String;)V"), &[(&jcmd).into()])?;
        Ok(())
    })
}

/// `MainActivity.copyToDownloads(path, name)`: `null` on success, else the error message.
fn copy_to_downloads(path: &str, name: &str) -> Result<(), String> {
    with_activity(|env, activity| {
        let jpath = JString::from_str(env, path)?;
        let jname = JString::from_str(env, name)?;
        let ret = env
            .call_method(
                activity,
                jni_str!("copyToDownloads"),
                jni_sig!("(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;"),
                &[(&jpath).into(), (&jname).into()],
            )?
            .l()?;
        if ret.is_null() {
            return Ok(Ok(()));
        }
        let message = env.cast_local::<JString>(ret)?;
        Ok(Err(message.to_string()))
    })?
}

// ---- Calls from MainActivity (Kotlin) ----------------------------------------------------------

/// `MainActivity.nativeDeliverFile(cmd, path)`: a picked file, copied into `Imports/`, from a
/// Java thread.
#[unsafe(no_mangle)]
pub extern "system" fn Java_com_iameberhard_soundcraft_MainActivity_nativeDeliverFile<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _this: JObject<'caller>,
    cmd: JString<'caller>,
    path: JString<'caller>,
) {
    let outcome = unowned_env.with_env(|_env| -> jni::errors::Result<()> {
        let cmd = cmd.to_string();
        let path = path.to_string();
        log::info!("picked {path} for {cmd}");
        PICKED.lock().unwrap_or_else(PoisonError::into_inner).push((cmd, path));
        if let Some(ctx) = CTX.get() {
            ctx.request_repaint();
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::LogErrorAndDefault>()
}
