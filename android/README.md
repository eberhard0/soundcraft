# SoundCraft for Android

The Gradle project that packages `apps/soundcraft-android` (the Rust app as a `GameActivity`
shell) into an APK/AAB. CI (`.github/workflows/android.yml`) builds it on every push to the
`android` branch and attaches signed builds to a GitHub Release on `android-v*` tags.

## How it fits together

- `apps/soundcraft-android/src/lib.rs`: `android_main`, the host services (Browse… in the path
  dialogs), preferences, the copy of saved files to Downloads, and the JNI bridge to `MainActivity`.
- `app/src/main/java/.../MainActivity.kt`: the Storage Access Framework picker (the picked file
  is copied into the app's private `Imports/` folder), copying saved sessions, bounces and
  exports into `Downloads/SoundCraft/`, and the full-screen window.
- `cargo ndk` drops `libsoundcraft_android.so` into `app/src/main/jniLibs/arm64-v8a/` (ignored by
  git); Gradle packages it.

## Building locally

Needs the Android SDK (platform 35, build-tools 35), NDK r27, a stable Rust toolchain with the
`aarch64-linux-android` target, and `cargo-ndk`:

```sh
rustup target add aarch64-linux-android
cargo install cargo-ndk
export ANDROID_NDK_HOME=$ANDROID_SDK_ROOT/ndk/<version>
cargo ndk -t arm64-v8a --platform 30 -o android/app/src/main/jniLibs build --release -p soundcraft-android
cd android && ./gradlew assembleDebug
```

## Files

- Open / Import: the Browse… button opens the system picker (all files); the file is copied to
  the app's private storage and the command runs with that copy.
- Save As / Save Copy: Browse… picks `Sessions/<name>.scraft` in private storage. Every saved
  session, bounce and export is then copied to `Downloads/SoundCraft/<name>` (the status line
  says so). The same name saved again in one run overwrites that copy.
- Preferences (`ui.json`), autosaves and plugin presets live in the private files directory.
- The first launch opens the demo session; later launches start empty, like the desktop app.

## Known limits (first version)

- **No audio playback or recording yet.** The app runs without an audio device: the transport,
  meters and automation run on a simulated clock (as in tests), so Play moves the playhead but
  nothing is heard, and Record captures nothing. Setup › Playback Engine › Reconnect audio
  device is untested on Android. Bounce Mix works (offline) and lands in Downloads.
- No third-party plugins (CLAP/VST3 need desktop plugin binaries); the built-in plugins work.
- Path prompts default to the app's private files directory (`$HOME` there); typed paths outside
  it are not writable on Android.
- No file association: opening a `.scraft` from a file manager does not launch the app.
- The desktop layout needs a tablet-sized screen; on a phone's cover screen it is cramped.
