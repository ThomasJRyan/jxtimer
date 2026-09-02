#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Align2, Color32, FontId, Frame, RichText, Stroke, Vec2};
use global_hotkey::hotkey::{Code, HotKey, Modifiers as HotKeyModifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use serde::{Deserialize, Serialize};

const WINDOW_SIZE: [f32; 2] = [520.0, 200.0];
const MIN_WINDOW_SIZE: [f32; 2] = [380.0, 200.0];
const SETTINGS_VERSION: u32 = 1;
const DEFAULT_SHORTCUTS: [&str; 4] = [
    "control+alt+KeyS",
    "control+alt+KeyT",
    "control+alt+KeyR",
    "control+alt+KeyO",
];
fn settings_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("focus-timer-settings")
}

fn normal_viewport_size() -> [f32; 2] {
    WINDOW_SIZE
}
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

mod tokens {
    pub const PANEL: [u8; 4] = [27, 20, 83, 255];
    pub const GREEN: [u8; 4] = [92, 255, 120, 255];
    pub const GREEN_HIGHLIGHT: [u8; 4] = [168, 255, 112, 255];
    pub const CHROMA: [u8; 4] = [39, 24, 132, 255];
    pub const SECONDARY: [u8; 4] = [67, 53, 130, 255];
    pub const OUTLINE: [u8; 4] = [105, 88, 190, 255];
    pub const DARK_TEXT: [u8; 4] = [8, 24, 16, 255];
    pub const PANEL_OUTLINE: [u8; 4] = [68, 57, 145, 255];
    pub const RESET_TEXT: [u8; 4] = [215, 210, 255, 255];
    pub const RADIUS: f32 = 14.0;
    pub const CONTROL_SIZE: super::Vec2 = super::Vec2::new(112.0, 42.0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerAction {
    Start,
    Stop,
    Reset,
    ToggleOverlay,
}

impl TimerAction {
    fn name(self) -> &'static str {
        match self {
            Self::Start => "Start",
            Self::Stop => "Stop",
            Self::Reset => "Reset",
            Self::ToggleOverlay => "Toggle overlay",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Appearance {
    text_color: [u8; 4],
    gradient: bool,
    gradient_end: [u8; 4],
    gradient_angle: f32,
    chroma_key: [u8; 4],
    native_transparency: bool,
    font_size: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            text_color: tokens::GREEN,
            gradient: false,
            gradient_end: tokens::GREEN_HIGHLIGHT,
            gradient_angle: 0.0,
            chroma_key: tokens::CHROMA,
            native_transparency: true,
            font_size: 72.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Settings {
    show_milliseconds: bool,
    fractional_digits: u8,
    overlay_mode: bool,
    appearance: Appearance,
    shortcuts: [String; 4],
}

#[derive(Debug, Clone)]
struct SettingsSurfaceState {
    settings: Settings,
    status: String,
    capture: Option<CaptureState>,
    wayland: bool,
}

#[derive(Debug, Clone)]
enum SettingsIntent {
    Close,
    Preference(PreferenceChange),
    CaptureRequested(usize),
    CaptureEvents(Vec<egui::Event>, egui::Modifiers),
}

#[derive(Clone)]
struct SettingsIntentSink {
    sender: Sender<SettingsIntent>,
    root_ctx: egui::Context,
}

impl SettingsIntentSink {
    fn send(&self, intent: SettingsIntent) {
        if self.sender.send(intent).is_ok() {
            self.root_ctx.request_repaint_of(egui::ViewportId::ROOT);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum PreferenceChange {
    ShowMilliseconds(bool),
    FractionalDigits(u8),
    OverlayMode(bool),
    NativeTransparency(bool),
    ChromaKey([u8; 4]),
    TextColor([u8; 4]),
    Gradient(bool),
    GradientEnd([u8; 4]),
    GradientAngle(f32),
    FontSize(f32),
}

fn apply_preference_change(settings: &mut Settings, change: PreferenceChange) {
    match change {
        PreferenceChange::ShowMilliseconds(value) => settings.show_milliseconds = value,
        PreferenceChange::FractionalDigits(value) => settings.fractional_digits = value.min(2),
        PreferenceChange::OverlayMode(value) => settings.overlay_mode = value,
        PreferenceChange::NativeTransparency(value) => {
            settings.appearance.native_transparency = value
        }
        PreferenceChange::ChromaKey(mut value) => {
            value[3] = 255;
            settings.appearance.chroma_key = value;
        }
        PreferenceChange::TextColor(value) => settings.appearance.text_color = value,
        PreferenceChange::Gradient(value) => settings.appearance.gradient = value,
        PreferenceChange::GradientEnd(value) => settings.appearance.gradient_end = value,
        PreferenceChange::GradientAngle(value) => {
            settings.appearance.gradient_angle = value.clamp(0.0, 360.0)
        }
        PreferenceChange::FontSize(value) => {
            settings.appearance.font_size = value.clamp(32.0, 180.0)
        }
    }
}
fn save_status(result: Result<(), String>) -> String {
    match result {
        Ok(()) => "Preferences saved".into(),
        Err(error) => format!("Preferences kept in memory; save failed: {error}"),
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            show_milliseconds: true,
            fractional_digits: 2,
            overlay_mode: false,
            appearance: Appearance::default(),
            shortcuts: DEFAULT_SHORTCUTS.map(str::to_owned),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct RawSettings {
    show_milliseconds: Option<bool>,
    fractional_digits: Option<u8>,
    overlay_mode: Option<bool>,
    text_color: Option<String>,
    gradient: Option<bool>,
    gradient_end: Option<String>,
    gradient_angle: Option<f32>,
    chroma_key: Option<String>,
    native_transparency: Option<bool>,
    font_size: Option<f32>,
    shortcuts: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SettingsFile {
    schema_version: u32,
    #[serde(default)]
    settings: RawSettings,
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("global_timer").join("settings.json"))
}

fn parse_color(value: &str) -> Option<[u8; 4]> {
    let value = value.strip_prefix('#').unwrap_or(value);
    if value.len() != 6 && value.len() != 8 {
        return None;
    }
    let mut out = [0, 0, 0, 255];
    for (i, slot) in out
        .iter_mut()
        .enumerate()
        .take(if value.len() == 6 { 3 } else { 4 })
    {
        *slot = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

fn color_hex(color: [u8; 4]) -> String {
    format!(
        "#{:02X}{:02X}{:02X}{:02X}",
        color[0], color[1], color[2], color[3]
    )
}
fn color32(color: [u8; 4]) -> Color32 {
    Color32::from_rgba_unmultiplied(color[0], color[1], color[2], color[3])
}
fn array_from_color(color: Color32) -> [u8; 4] {
    [color.r(), color.g(), color.b(), color.a()]
}

fn normalize(raw: RawSettings) -> Settings {
    let defaults = Settings::default();
    let shortcuts = raw
        .shortcuts
        .and_then(|values| values.try_into().ok())
        .and_then(|values: [String; 4]| {
            parse_shortcuts(&values)
                .ok()
                .map(|keys| keys.map(|key| key.to_string()))
        })
        .unwrap_or(defaults.shortcuts);
    let appearance = Appearance {
        text_color: raw
            .text_color
            .as_deref()
            .and_then(parse_color)
            .unwrap_or(defaults.appearance.text_color),
        gradient: raw.gradient.unwrap_or(defaults.appearance.gradient),
        gradient_end: raw
            .gradient_end
            .as_deref()
            .and_then(parse_color)
            .unwrap_or(defaults.appearance.gradient_end),
        gradient_angle: raw
            .gradient_angle
            .filter(|angle| angle.is_finite())
            .unwrap_or(defaults.appearance.gradient_angle)
            .clamp(0.0, 360.0),
        chroma_key: raw
            .chroma_key
            .as_deref()
            .and_then(parse_color)
            .map(|mut color| {
                color[3] = 255;
                color
            })
            .unwrap_or(defaults.appearance.chroma_key),
        native_transparency: raw
            .native_transparency
            .unwrap_or(defaults.appearance.native_transparency),
        font_size: raw
            .font_size
            .filter(|size| (32.0..=180.0).contains(size))
            .unwrap_or(defaults.appearance.font_size),
    };
    Settings {
        show_milliseconds: raw.show_milliseconds.unwrap_or(defaults.show_milliseconds),
        fractional_digits: raw
            .fractional_digits
            .unwrap_or(defaults.fractional_digits)
            .min(2),
        overlay_mode: raw.overlay_mode.unwrap_or(defaults.overlay_mode),
        appearance,
        shortcuts,
    }
}

fn to_file(settings: &Settings) -> SettingsFile {
    SettingsFile {
        schema_version: SETTINGS_VERSION,
        settings: RawSettings {
            show_milliseconds: Some(settings.show_milliseconds),
            fractional_digits: Some(settings.fractional_digits.min(2)),
            overlay_mode: Some(settings.overlay_mode),
            text_color: Some(color_hex(settings.appearance.text_color)),
            gradient: Some(settings.appearance.gradient),
            gradient_end: Some(color_hex(settings.appearance.gradient_end)),
            gradient_angle: Some(settings.appearance.gradient_angle.clamp(0.0, 360.0)),
            chroma_key: Some(color_hex(settings.appearance.chroma_key)),
            native_transparency: Some(settings.appearance.native_transparency),
            font_size: Some(settings.appearance.font_size),
            shortcuts: Some(settings.shortcuts.to_vec()),
        },
    }
}

#[derive(Debug, Clone, PartialEq)]
struct LoadedSettings {
    settings: Settings,
    status: String,
    writes_blocked: bool,
}

#[derive(Debug)]
enum DecodeSettingsError {
    Parse(String),
    Unsupported,
}

fn decode_settings(bytes: &[u8]) -> Result<Settings, DecodeSettingsError> {
    let file: SettingsFile = serde_json::from_slice(bytes)
        .map_err(|error| DecodeSettingsError::Parse(error.to_string()))?;
    if file.schema_version != SETTINGS_VERSION {
        return Err(DecodeSettingsError::Unsupported);
    }
    Ok(normalize(file.settings))
}
fn initial_runtime_state(settings: Settings) -> (Settings, Timer) {
    (settings, Timer::default())
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings");
    path.with_file_name(format!("{name}.backup"))
}

fn load_settings_from(path: &Path) -> LoadedSettings {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::read(backup_path(path)) {
                Ok(bytes) => match decode_settings(&bytes) {
                    Ok(settings) => {
                        return LoadedSettings {
                            settings,
                            status: "Preferences restored from backup".into(),
                            writes_blocked: false,
                        }
                    }
                    Err(DecodeSettingsError::Unsupported) => {
                        return LoadedSettings {
                            settings: Settings::default(),
                            status: "Preferences backup version unsupported; using defaults".into(),
                            writes_blocked: true,
                        }
                    }
                    Err(DecodeSettingsError::Parse(error)) => {
                        return LoadedSettings {
                            settings: Settings::default(),
                            status: format!(
                                "Preferences and backup unreadable; using defaults ({error})"
                            ),
                            writes_blocked: false,
                        }
                    }
                },
                Err(backup_error) if backup_error.kind() == std::io::ErrorKind::NotFound => {
                    return LoadedSettings {
                        settings: Settings::default(),
                        status: "Using default preferences".into(),
                        writes_blocked: false,
                    }
                }
                Err(backup_error) => {
                    return LoadedSettings {
                        settings: Settings::default(),
                        status: format!("Preferences unavailable; using defaults ({backup_error})"),
                        writes_blocked: false,
                    }
                }
            }
        }
        Err(error) => {
            return LoadedSettings {
                settings: Settings::default(),
                status: format!("Preferences unavailable; using defaults ({error})"),
                writes_blocked: false,
            }
        }
    };
    match decode_settings(&bytes) {
        Ok(settings) => LoadedSettings {
            settings,
            status: "Preferences restored".into(),
            writes_blocked: false,
        },
        Err(DecodeSettingsError::Parse(error)) => LoadedSettings {
            settings: Settings::default(),
            status: format!("Preferences unreadable; using defaults ({error})"),
            writes_blocked: false,
        },
        Err(DecodeSettingsError::Unsupported) => LoadedSettings {
            settings: Settings::default(),
            status: "Preferences version unsupported; using defaults".into(),
            writes_blocked: true,
        },
    }
}

fn save_settings_to(path: &Path, settings: &Settings) -> Result<(), String> {
    save_settings_to_with_hooks(
        path,
        settings,
        |settings| serde_json::to_vec_pretty(&to_file(settings)).map_err(|error| error.to_string()),
        |dir| fs::create_dir_all(dir),
        write_temp_file,
        replace_config_file,
    )
}

fn save_settings_to_with_hooks<SerializeSettings, CreateDir, WriteTemp, Replace>(
    path: &Path,
    settings: &Settings,
    serialize: SerializeSettings,
    create_dir: CreateDir,
    write_temp: WriteTemp,
    replace: Replace,
) -> Result<(), String>
where
    SerializeSettings: FnOnce(&Settings) -> Result<Vec<u8>, String>,
    CreateDir: FnOnce(&Path) -> std::io::Result<()>,
    WriteTemp: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
    Replace: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    let data = serialize(settings)?;
    let dir = path
        .parent()
        .ok_or_else(|| "No preferences directory".to_owned())?;
    create_dir(dir).map_err(|error| error.to_string())?;
    if path.exists()
        && fs::symlink_metadata(path)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
    {
        return Err("preferences path is a symlink".into());
    }
    if has_unsupported_schema(path)?
        || (!path.exists() && has_unsupported_schema(&backup_path(path))?)
    {
        return Err("preferences belong to a newer version; save is disabled".into());
    }
    let temporary = dir.join(format!(
        ".settings-{}-{}.tmp",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    persist_temp_and_replace(&temporary, path, &data, write_temp, replace)
}

fn has_unsupported_schema(path: &Path) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    if fs::symlink_metadata(path)
        .map_err(|error| error.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("preferences path is a symlink".into());
    }
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    Ok(matches!(
        decode_settings(&bytes),
        Err(DecodeSettingsError::Unsupported)
    ))
}

fn write_temp_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

fn persist_temp_and_replace<WriteTemp, Replace>(
    temporary: &Path,
    destination: &Path,
    data: &[u8],
    write_temp: WriteTemp,
    replace: Replace,
) -> Result<(), String>
where
    WriteTemp: FnOnce(&Path, &[u8]) -> std::io::Result<()>,
    Replace: FnOnce(&Path, &Path) -> std::io::Result<()>,
{
    if let Err(error) = write_temp(temporary, data) {
        let _ = fs::remove_file(temporary);
        return Err(error.to_string());
    }
    if let Err(error) = replace(temporary, destination) {
        let _ = fs::remove_file(temporary);
        return Err(error.to_string());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_config_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_config_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    // Windows cannot rename over an existing file. Copy the old canonical file
    // to a stable same-directory backup before removing it, and retain that
    // backup after a successful replacement so an interrupted two-step replace
    // is recoverable on the next load.
    let backup = backup_path(destination);
    if destination.exists() {
        if backup.exists() && fs::symlink_metadata(&backup)?.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "preferences backup path is a symlink",
            ));
        }
        fs::copy(destination, &backup)?;
        OpenOptions::new().read(true).open(&backup)?.sync_all()?;
        fs::remove_file(destination)?;
    }
    fs::rename(temporary, destination)
}

#[derive(Debug, Clone)]
struct CaptureState {
    target: usize,
    modifiers: HotKeyModifiers,
    primary: Option<(egui::Key, Code)>,
    held: HashSet<egui::Key>,
    invalid: Option<String>,
}
impl CaptureState {
    fn new(target: usize) -> Self {
        Self {
            target,
            modifiers: HotKeyModifiers::empty(),
            primary: None,
            held: HashSet::new(),
            invalid: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum CaptureResult {
    Continue,
    Cancelled,
    Committed(HotKey),
    Rejected(String),
}

fn map_key(key: egui::Key) -> Option<Code> {
    Some(match key {
        egui::Key::A => Code::KeyA,
        egui::Key::B => Code::KeyB,
        egui::Key::C => Code::KeyC,
        egui::Key::D => Code::KeyD,
        egui::Key::E => Code::KeyE,
        egui::Key::F => Code::KeyF,
        egui::Key::G => Code::KeyG,
        egui::Key::H => Code::KeyH,
        egui::Key::I => Code::KeyI,
        egui::Key::J => Code::KeyJ,
        egui::Key::K => Code::KeyK,
        egui::Key::L => Code::KeyL,
        egui::Key::M => Code::KeyM,
        egui::Key::N => Code::KeyN,
        egui::Key::O => Code::KeyO,
        egui::Key::P => Code::KeyP,
        egui::Key::Q => Code::KeyQ,
        egui::Key::R => Code::KeyR,
        egui::Key::S => Code::KeyS,
        egui::Key::T => Code::KeyT,
        egui::Key::U => Code::KeyU,
        egui::Key::V => Code::KeyV,
        egui::Key::W => Code::KeyW,
        egui::Key::X => Code::KeyX,
        egui::Key::Y => Code::KeyY,
        egui::Key::Z => Code::KeyZ,
        egui::Key::Num0 => Code::Digit0,
        egui::Key::Num1 => Code::Digit1,
        egui::Key::Num2 => Code::Digit2,
        egui::Key::Num3 => Code::Digit3,
        egui::Key::Num4 => Code::Digit4,
        egui::Key::Num5 => Code::Digit5,
        egui::Key::Num6 => Code::Digit6,
        egui::Key::Num7 => Code::Digit7,
        egui::Key::Num8 => Code::Digit8,
        egui::Key::Num9 => Code::Digit9,
        egui::Key::F1 => Code::F1,
        egui::Key::F2 => Code::F2,
        egui::Key::F3 => Code::F3,
        egui::Key::F4 => Code::F4,
        egui::Key::F5 => Code::F5,
        egui::Key::F6 => Code::F6,
        egui::Key::F7 => Code::F7,
        egui::Key::F8 => Code::F8,
        egui::Key::F9 => Code::F9,
        egui::Key::F10 => Code::F10,
        egui::Key::F11 => Code::F11,
        egui::Key::F12 => Code::F12,
        egui::Key::ArrowDown => Code::ArrowDown,
        egui::Key::ArrowLeft => Code::ArrowLeft,
        egui::Key::ArrowRight => Code::ArrowRight,
        egui::Key::ArrowUp => Code::ArrowUp,
        egui::Key::Backspace => Code::Backspace,
        egui::Key::Delete => Code::Delete,
        egui::Key::Enter => Code::Enter,
        egui::Key::Space => Code::Space,
        egui::Key::Tab => Code::Tab,
        egui::Key::Backslash => Code::Backslash,
        egui::Key::Backtick => Code::Backquote,
        egui::Key::Comma => Code::Comma,
        egui::Key::Equals => Code::Equal,
        egui::Key::Minus => Code::Minus,
        egui::Key::Period => Code::Period,
        egui::Key::Quote => Code::Quote,
        egui::Key::Semicolon => Code::Semicolon,
        egui::Key::Slash => Code::Slash,
        _ => return None,
    })
}
fn event_mods(m: egui::Modifiers) -> HotKeyModifiers {
    let mut out = HotKeyModifiers::empty();
    if m.ctrl {
        out |= HotKeyModifiers::CONTROL
    }
    if m.alt {
        out |= HotKeyModifiers::ALT
    }
    if m.shift {
        out |= HotKeyModifiers::SHIFT
    }
    if m.mac_cmd {
        out |= HotKeyModifiers::SUPER
    }
    out
}
fn process_capture(state: &mut CaptureState, event: &egui::Event) -> CaptureResult {
    let egui::Event::Key {
        key,
        physical_key,
        pressed,
        repeat,
        modifiers,
    } = event
    else {
        return CaptureResult::Continue;
    };
    if *pressed && *repeat {
        return CaptureResult::Continue;
    }
    state.modifiers |= event_mods(*modifiers);
    if *pressed && *key == egui::Key::Escape {
        return CaptureResult::Cancelled;
    }
    if is_modifier_key(*key) || physical_key.is_some_and(is_modifier_key) {
        return CaptureResult::Continue;
    }
    let key = physical_key.unwrap_or(*key);
    let mapped = map_key(key);
    if *pressed {
        if let Some(code) = mapped {
            if let Some((old, _)) = state.primary {
                if old != key {
                    state.invalid = Some("Use one primary key".into());
                }
            } else {
                state.primary = Some((key, code));
            }
            state.held.insert(key);
        } else {
            state.invalid = Some("Unsupported key".into());
        }
    } else {
        state.held.remove(&key);
    }
    CaptureResult::Continue
}

fn is_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

fn take_capture_key_events(events: &mut Vec<egui::Event>) -> Vec<egui::Event> {
    let mut captured = Vec::new();
    events.retain(|event| {
        if matches!(event, egui::Event::Key { .. }) {
            captured.push(event.clone());
            false
        } else {
            true
        }
    });
    captured
}

fn finish_capture(state: &CaptureState, modifiers: egui::Modifiers) -> CaptureResult {
    if !state.held.is_empty() || event_mods(modifiers).intersects(state.modifiers) {
        return CaptureResult::Continue;
    }
    if state.primary.is_none() && state.modifiers.is_empty() {
        return CaptureResult::Continue;
    }
    if let Some(error) = &state.invalid {
        return CaptureResult::Rejected(error.clone());
    }
    let Some((_, code)) = state.primary else {
        return CaptureResult::Rejected("Add a modifier and one primary key".into());
    };
    if state.modifiers.is_empty() {
        return CaptureResult::Rejected("Add at least one modifier".into());
    };
    CaptureResult::Committed(HotKey::new(Some(state.modifiers), code))
}

fn parse_shortcuts(values: &[String; 4]) -> Result<[HotKey; 4], String> {
    let parsed: Vec<HotKey> = values
        .iter()
        .enumerate()
        .map(|(i, value)| {
            let key: HotKey = value
                .parse()
                .map_err(|error| format!("Shortcut {}: {error}", i + 1))?;
            if key.mods.is_empty() {
                Err(format!("Shortcut {} needs a modifier", i + 1))
            } else {
                Ok(key)
            }
        })
        .collect::<Result<_, String>>()?;
    let ids: HashSet<u32> = parsed.iter().map(HotKey::id).collect();
    if ids.len() != 4 {
        return Err("Shortcuts must be distinct".into());
    }
    Ok([parsed[0], parsed[1], parsed[2], parsed[3]])
}

fn default_hotkeys() -> [HotKey; 4] {
    let modifiers = HotKeyModifiers::CONTROL | HotKeyModifiers::ALT;
    [
        HotKey::new(Some(modifiers), Code::KeyS),
        HotKey::new(Some(modifiers), Code::KeyT),
        HotKey::new(Some(modifiers), Code::KeyR),
        HotKey::new(Some(modifiers), Code::KeyO),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransactionState {
    NewActive,
    PreviousActive,
    Inactive,
    Unknown,
}

trait ShortcutBackend {
    fn register(&mut self, key: HotKey) -> Result<(), String>;
    fn unregister(&mut self, key: HotKey) -> Result<(), String>;
}

#[derive(Debug)]
struct ShortcutTransaction {
    state: TransactionState,
    message: String,
    known_active: Vec<HotKey>,
    uncertain: Vec<HotKey>,
}

struct ShortcutStartupState {
    active: Option<[HotKey; 4]>,
    known_active: Vec<HotKey>,
    uncertain: Vec<HotKey>,
    state: TransactionState,
    message: String,
}

fn cleanup_shortcuts<B: ShortcutBackend>(backend: &mut B, keys: &[HotKey]) -> Vec<HotKey> {
    let mut uncertain: Vec<HotKey> = Vec::new();
    for key in keys.iter().rev() {
        if backend.unregister(*key).is_err() && !uncertain.iter().any(|item| item.id() == key.id())
        {
            uncertain.push(*key);
        }
    }
    uncertain
}

fn replace_shortcut_set<B: ShortcutBackend>(
    backend: &mut B,
    previous: &[HotKey],
    next: &[HotKey; 4],
) -> ShortcutTransaction {
    let mut release_failures = Vec::new();
    for key in previous {
        if let Err(error) = backend.unregister(*key) {
            release_failures.push((*key, error));
        }
    }
    if !release_failures.is_empty() {
        let failed_keys: Vec<HotKey> = release_failures.iter().map(|(key, _)| *key).collect();
        let uncertain = cleanup_shortcuts(backend, &failed_keys);
        let detail = release_failures
            .first()
            .map(|(_, error)| error.as_str())
            .unwrap_or("unknown error");
        if uncertain.is_empty() {
            return ShortcutTransaction {
                state: TransactionState::Inactive,
                message: format!(
                    "Could not release shortcuts: {detail}; cleanup confirmed none active"
                ),
                known_active: Vec::new(),
                uncertain,
            };
        }
        return ShortcutTransaction {
            state: TransactionState::Unknown,
            message: format!(
                "Could not release shortcuts: {detail}; shortcut state unknown/degraded"
            ),
            known_active: Vec::new(),
            uncertain,
        };
    }
    let mut registered = Vec::new();
    for key in next {
        if let Err(error) = backend.register(*key) {
            // A failed registration is an ordinary conflict/error and does not
            // prove that this process owns the key. Never unregister that key:
            // it may belong to another application. Only successful local
            // registrations are candidates for cleanup.
            let uncertain = cleanup_shortcuts(backend, &registered);
            if !uncertain.is_empty() {
                return ShortcutTransaction {
                    state: TransactionState::Unknown,
                    message: format!(
                        "Could not register: {error}; cleanup incomplete, shortcut state unknown/degraded"
                    ),
                    known_active: Vec::new(),
                    uncertain,
                };
            }
            if previous.is_empty() {
                return ShortcutTransaction {
                    state: TransactionState::Inactive,
                    message: format!("Could not register: {error}; cleanup confirmed none active"),
                    known_active: Vec::new(),
                    uncertain,
                };
            }
            let mut restored = Vec::new();
            for old in previous {
                if let Err(rollback_error) = backend.register(*old) {
                    let uncertain = cleanup_shortcuts(backend, &restored);
                    if uncertain.is_empty() {
                        return ShortcutTransaction {
                            state: TransactionState::Inactive,
                            message: format!(
                                "Could not register: {error}; rollback failed: {rollback_error}; cleanup confirmed none active"
                            ),
                            known_active: Vec::new(),
                            uncertain,
                        };
                    }
                    return ShortcutTransaction {
                        state: TransactionState::Unknown,
                        message: format!(
                            "Could not register: {error}; rollback failed: {rollback_error}; shortcut state unknown/degraded"
                        ),
                        known_active: Vec::new(),
                        uncertain,
                    };
                }
                restored.push(*old);
            }
            return ShortcutTransaction {
                state: TransactionState::PreviousActive,
                message: format!("Could not register: {error}; previous shortcuts restored"),
                known_active: restored,
                uncertain: Vec::new(),
            };
        }
        registered.push(*key);
    }
    ShortcutTransaction {
        state: TransactionState::NewActive,
        message: "Shortcuts applied".into(),
        known_active: registered,
        uncertain: Vec::new(),
    }
}

fn initialize_shortcuts<B: ShortcutBackend>(
    backend: &mut B,
    defaults: &[HotKey; 4],
) -> ShortcutStartupState {
    let result = replace_shortcut_set(backend, &[], defaults);
    let active = (result.state == TransactionState::NewActive)
        .then(|| result.known_active.clone().try_into().ok())
        .flatten();
    ShortcutStartupState {
        active,
        known_active: result.known_active,
        uncertain: result.uncertain,
        state: result.state,
        message: result.message,
    }
}

struct NativeShortcutBackend<'a> {
    manager: &'a GlobalHotKeyManager,
}
impl ShortcutBackend for NativeShortcutBackend<'_> {
    fn register(&mut self, key: HotKey) -> Result<(), String> {
        self.manager.register(key).map_err(|e| e.to_string())
    }
    fn unregister(&mut self, key: HotKey) -> Result<(), String> {
        self.manager.unregister(key).map_err(|e| e.to_string())
    }
}

fn action_for_event(
    event: GlobalHotKeyEvent,
    shortcuts: Option<&[HotKey; 4]>,
) -> Option<TimerAction> {
    if event.state() != HotKeyState::Pressed {
        return None;
    }
    let index = shortcuts?.iter().position(|key| key.id() == event.id())?;
    Some(
        [
            TimerAction::Start,
            TimerAction::Stop,
            TimerAction::Reset,
            overlay_toggle_action(),
        ][index],
    )
}
fn toggled_overlay(current: bool, action: TimerAction) -> bool {
    if action == TimerAction::ToggleOverlay {
        !current
    } else {
        current
    }
}
fn overlay_toggle_action() -> TimerAction {
    TimerAction::ToggleOverlay
}
fn escape_overlay_action(overlay_active: bool, escape_pressed: bool) -> Option<TimerAction> {
    (overlay_active && escape_pressed).then_some(overlay_toggle_action())
}
fn apply_overlay_transition(settings: &mut Settings) {
    let next = toggled_overlay(settings.overlay_mode, overlay_toggle_action());
    apply_preference_change(settings, PreferenceChange::OverlayMode(next));
}
fn frame_timer_state(timer: &Timer, now: Duration) -> (Duration, bool) {
    (timer.elapsed(now), timer.is_running())
}

/// Timer state accepts only explicit monotonic durations; the UI supplies `Instant::elapsed()`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Timer {
    accumulated: Duration,
    started_at: Option<Duration>,
}
impl Timer {
    pub fn start(&mut self, now: Duration) {
        if self.started_at.is_none() {
            self.started_at = Some(now)
        }
    }
    pub fn stop(&mut self, now: Duration) {
        if let Some(start) = self.started_at.take() {
            self.accumulated += now.saturating_sub(start)
        }
    }
    pub fn reset(&mut self) {
        self.accumulated = Duration::ZERO;
        self.started_at = None
    }
    pub fn elapsed(&self, now: Duration) -> Duration {
        self.accumulated
            + self
                .started_at
                .map_or(Duration::ZERO, |start| now.saturating_sub(start))
    }
    pub fn is_running(&self) -> bool {
        self.started_at.is_some()
    }
}

fn format_duration(
    duration: Duration,
    fractions: bool,
    fractional_digits: u8,
    running: bool,
) -> String {
    let total = duration.as_secs();
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    let show_hours = !running || total >= 3_600;
    let show_minutes = !running || total >= 60;
    let hours = if !show_hours {
        "  ".into()
    } else {
        format!("{h:02}")
    };
    let minutes = if !show_minutes {
        "  ".into()
    } else {
        format!("{m:02}")
    };
    let hour_minute_separator = if show_hours { ":" } else { " " };
    let minute_second_separator = if show_minutes { ":" } else { " " };
    if fractions {
        let hundredths = duration.subsec_millis() / 10;
        match fractional_digits.min(2) {
            0 => format!("{hours}{hour_minute_separator}{minutes}{minute_second_separator}{s:02}"),
            1 => format!(
                "{hours}{hour_minute_separator}{minutes}{minute_second_separator}{s:02}.{}",
                hundredths / 10
            ),
            _ => format!(
                "{hours}{hour_minute_separator}{minutes}{minute_second_separator}{s:02}.{hundredths:02}"
            ),
        }
    } else {
        format!("{hours}{hour_minute_separator}{minutes}{minute_second_separator}{s:02}")
    }
}
fn wayland_session() -> bool {
    cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some()
}
fn transparent_background(preference: bool, overlay: bool, wayland: bool) -> bool {
    preference && overlay && !wayland
}

fn decorations_for_mode(_overlay: bool) -> bool {
    // Keep native chrome in both modes. The overlay client area is still
    // timer-only; stream software can crop the title bar when needed.
    true
}
fn transparency_status(requested: bool, wayland: bool) -> Option<&'static str> {
    if !requested {
        None
    } else if wayland {
        Some("Native Wayland transparency is unsupported; uniform chroma-key fallback is active.")
    } else {
        Some("Transparent composition requested (best effort, unconfirmed); chroma-key remains available.")
    }
}
fn chroma_color(color: [u8; 4]) -> Color32 {
    Color32::from_rgb(color[0], color[1], color[2])
}
fn readout_layout(available: Vec2, measured: Vec2, requested: f32) -> (f32, Vec2) {
    let scale = (available.x / measured.x.max(1.0))
        .min(available.y / measured.y.max(1.0))
        .min(1.0);
    let scale = scale.max(0.0);
    let size = requested * scale;
    (size, measured * scale)
}
fn centered_readout_rect(
    available: egui::Rect,
    measured: Vec2,
    requested: f32,
) -> (f32, egui::Rect) {
    let (font_size, size) = readout_layout(available.size(), measured, requested);
    (
        font_size,
        egui::Rect::from_center_size(available.center(), size),
    )
}

fn projected_gradient_factor(rect: egui::Rect, point: egui::Pos2, angle: f32) -> f32 {
    let radians = angle.to_radians();
    let direction = Vec2::new(radians.cos(), radians.sin());
    let corners = [
        rect.left_top(),
        rect.right_top(),
        rect.left_bottom(),
        rect.right_bottom(),
    ];
    let min = corners
        .iter()
        .map(|corner| corner.to_vec2().dot(direction))
        .fold(f32::INFINITY, f32::min);
    let max = corners
        .iter()
        .map(|corner| corner.to_vec2().dot(direction))
        .fold(f32::NEG_INFINITY, f32::max);
    ((point.to_vec2().dot(direction) - min) / (max - min).max(f32::EPSILON)).clamp(0.0, 1.0)
}

fn readout_field_size(available_width: f32, measured: Vec2) -> Vec2 {
    let content_width = (available_width - 16.0).max(1.0);
    let scale = (content_width / measured.x.max(1.0)).min(1.0);
    Vec2::new(measured.x * scale + 16.0, measured.y * scale + 12.0)
}

struct TimerApp {
    timer: Timer,
    clock: Instant,
    settings: Settings,
    config_file: Option<PathBuf>,
    status: String,
    settings_open: bool,
    capture: Option<CaptureState>,
    hotkey_manager: Option<GlobalHotKeyManager>,
    active_shortcuts: Option<[HotKey; 4]>,
    known_active_shortcuts: Vec<HotKey>,
    uncertain_shortcuts: Vec<HotKey>,
    shortcut_state: TransactionState,
    global_events: Receiver<GlobalHotKeyEvent>,
    global_sender: Sender<GlobalHotKeyEvent>,
    wake_started: bool,
    last_transparency: Option<bool>,
    last_decorations: Option<bool>,
    config_writes_blocked: bool,
    test_mode: bool,
    settings_surface: Arc<Mutex<SettingsSurfaceState>>,
    settings_intents: Sender<SettingsIntent>,
    settings_events: Receiver<SettingsIntent>,
    wayland: bool,
    #[cfg(test)]
    save_override: Option<fn(&Path, &Settings) -> Result<(), String>>,
    #[cfg(test)]
    shortcut_transaction_override: Option<fn(&[HotKey], &[HotKey; 4]) -> ShortcutTransaction>,
    #[cfg(test)]
    settings_snapshot: bool,
}
impl Default for TimerApp {
    fn default() -> Self {
        let config_file = config_path();
        let (settings, mut status, config_writes_blocked) = config_file
            .as_deref()
            .map(load_settings_from)
            .map(|loaded| (loaded.settings, loaded.status, loaded.writes_blocked))
            .unwrap_or((
                Settings::default(),
                "Using default preferences (config path unavailable)".into(),
                false,
            ));
        let (global_sender, global_events) = mpsc::channel();
        let defaults = parse_shortcuts(&settings.shortcuts).unwrap_or_else(|_| default_hotkeys());
        let (manager, startup) = match GlobalHotKeyManager::new() {
            Ok(manager) => {
                let mut backend = NativeShortcutBackend { manager: &manager };
                let startup = initialize_shortcuts(&mut backend, &defaults);
                status = format!("{status}; {}", startup.message);
                (Some(manager), startup)
            }
            Err(error) => {
                status = format!("{status}; global shortcuts unavailable: {error}");
                (
                    None,
                    ShortcutStartupState {
                        active: None,
                        known_active: Vec::new(),
                        uncertain: Vec::new(),
                        state: TransactionState::Inactive,
                        message: format!("global shortcuts unavailable: {error}"),
                    },
                )
            }
        };
        let (settings, timer) = initial_runtime_state(settings);
        let (settings_intents, settings_events) = mpsc::channel();
        let settings_surface = Arc::new(Mutex::new(SettingsSurfaceState {
            settings: settings.clone(),
            status: status.clone(),
            capture: None,
            wayland: wayland_session(),
        }));
        Self {
            timer,
            clock: Instant::now(),
            settings,
            config_file,
            status,
            settings_open: false,
            capture: None,
            hotkey_manager: manager,
            active_shortcuts: startup.active,
            known_active_shortcuts: startup.known_active,
            uncertain_shortcuts: startup.uncertain,
            shortcut_state: startup.state,
            global_events,
            global_sender,
            wake_started: false,
            last_transparency: None,
            last_decorations: None,
            config_writes_blocked,
            test_mode: false,
            settings_surface,
            settings_intents,
            settings_events,
            wayland: wayland_session(),
            #[cfg(test)]
            save_override: None,
            #[cfg(test)]
            shortcut_transaction_override: None,
            #[cfg(test)]
            settings_snapshot: false,
        }
    }
}
impl TimerApp {
    #[cfg(test)]
    fn new_for_test() -> Self {
        let (settings_intents, settings_events) = mpsc::channel();
        let (global_sender, global_events) = mpsc::channel();
        let settings = Settings::default();
        Self {
            timer: Timer::default(),
            clock: Instant::now(),
            settings: settings.clone(),
            config_file: None,
            status: String::new(),
            settings_open: false,
            capture: None,
            hotkey_manager: None,
            active_shortcuts: None,
            known_active_shortcuts: Vec::new(),
            uncertain_shortcuts: Vec::new(),
            shortcut_state: TransactionState::Inactive,
            global_events,
            global_sender,
            wake_started: true,
            last_transparency: None,
            last_decorations: None,
            config_writes_blocked: false,
            test_mode: true,
            settings_surface: Arc::new(Mutex::new(SettingsSurfaceState {
                settings,
                status: String::new(),
                capture: None,
                wayland: false,
            })),
            settings_intents,
            settings_events,
            wayland: false,
            save_override: None,
            shortcut_transaction_override: None,
            settings_snapshot: false,
        }
    }
    fn now(&self) -> Duration {
        self.clock.elapsed()
    }
    fn save(&mut self) {
        if self.config_writes_blocked {
            self.status = "Preferences from a newer version kept; save is disabled".into();
            return;
        }
        if let Some(path) = &self.config_file {
            #[cfg(test)]
            let result = self.save_override.map_or_else(
                || save_settings_to(path, &self.settings),
                |save| save(path, &self.settings),
            );
            #[cfg(not(test))]
            let result = save_settings_to(path, &self.settings);
            self.status = save_status(result);
        }
    }
    fn apply(&mut self, action: TimerAction, now: Duration) {
        match action {
            TimerAction::Start => self.timer.start(now),
            TimerAction::Stop => self.timer.stop(now),
            TimerAction::Reset => self.timer.reset(),
            TimerAction::ToggleOverlay => {
                self.transition_overlay(!self.settings.overlay_mode, true);
            }
        }
    }
    fn transition_overlay(&mut self, desired: bool, close_settings: bool) {
        if self.settings.overlay_mode != desired {
            apply_overlay_transition(&mut self.settings);
        }
        if self.settings.overlay_mode && close_settings {
            self.close_settings();
        }
        self.save();
    }
    fn close_settings(&mut self) {
        self.settings_open = false;
        self.capture = None;
        if let Ok(mut surface) = self.settings_surface.lock() {
            surface.capture = None;
        }
    }
    fn open_settings(&mut self, ctx: &egui::Context) {
        self.settings_open = true;
        if let Ok(mut surface) = self.settings_surface.lock() {
            surface.settings = self.settings.clone();
            surface.status = self.status.clone();
            surface.capture = self.capture.clone();
        }
        if !self.test_mode {
            ctx.request_repaint();
        }
    }
    fn drain_settings_intents(&mut self, ctx: &egui::Context) {
        let mut handled = false;
        while let Ok(intent) = self.settings_events.try_recv() {
            handled = true;
            match intent {
                SettingsIntent::Close => self.close_settings(),
                SettingsIntent::Preference(change) => {
                    if let PreferenceChange::OverlayMode(value) = change {
                        self.transition_overlay(value, false);
                    } else {
                        apply_preference_change(&mut self.settings, change);
                        self.save();
                    }
                }
                SettingsIntent::CaptureRequested(target) => {
                    self.capture = Some(CaptureState::new(target));
                    self.status = "Listening… release all keys; Escape cancels".into();
                }
                SettingsIntent::CaptureEvents(events, modifiers) => {
                    let Some(capture) = self.capture.as_mut() else {
                        continue;
                    };
                    let mut result = CaptureResult::Continue;
                    for event in &events {
                        result = process_capture(capture, event);
                        if result != CaptureResult::Continue {
                            break;
                        }
                    }
                    if result == CaptureResult::Continue {
                        result = finish_capture(capture, modifiers);
                    }
                    match result {
                        CaptureResult::Committed(key) => {
                            let target = capture.target;
                            let previous = self.settings.shortcuts[target].clone();
                            self.settings.shortcuts[target] = key.to_string();
                            self.capture = None;
                            if !self.replace_shortcuts() {
                                self.settings.shortcuts[target] = previous;
                            }
                        }
                        CaptureResult::Cancelled => {
                            self.capture = None;
                            self.status = "Capture cancelled".into();
                        }
                        CaptureResult::Rejected(error) => {
                            self.capture = None;
                            self.status = format!("Capture rejected: {error}");
                        }
                        CaptureResult::Continue => {}
                    }
                }
            }
        }
        if let Ok(mut surface) = self.settings_surface.lock() {
            surface.settings = self.settings.clone();
            surface.status = self.status.clone();
            surface.capture = self.capture.clone();
        }
        if handled {
            ctx.request_repaint();
        }
    }
    fn native_settings(&self, ctx: &egui::Context) {
        let surface = Arc::clone(&self.settings_surface);
        let intents = SettingsIntentSink {
            sender: self.settings_intents.clone(),
            root_ctx: ctx.clone(),
        };
        ctx.show_viewport_deferred(
            settings_viewport_id(),
            egui::ViewportBuilder::default()
                .with_title("Settings")
                .with_inner_size([430.0, 560.0]),
            move |ui, _class| {
                // Losing focus briefly is normal when selecting a chord. Only an
                // actual viewport close ends capture; focus is not a cancellation.
                if ui.input(|input| input.viewport().close_requested()) {
                    intents.send(SettingsIntent::Close);
                }
                settings_contents(ui, &surface, &intents)
            },
        );
    }
    fn replace_shortcuts(&mut self) -> bool {
        let drafts = self.settings.shortcuts.clone();
        let parsed = match parse_shortcuts(&drafts) {
            Ok(keys) => keys,
            Err(error) => {
                self.status = error;
                return false;
            }
        };
        let old = self
            .known_active_shortcuts
            .iter()
            .chain(self.uncertain_shortcuts.iter())
            .copied()
            .collect::<Vec<_>>();
        #[cfg(test)]
        let result = if let Some(replace) = self.shortcut_transaction_override {
            replace(&old, &parsed)
        } else {
            let Some(manager) = self.hotkey_manager.as_ref() else {
                self.status = "Global shortcuts unavailable; local controls remain active".into();
                return false;
            };
            let mut backend = NativeShortcutBackend { manager };
            replace_shortcut_set(&mut backend, &old, &parsed)
        };
        #[cfg(not(test))]
        let manager = match self.hotkey_manager.as_ref() {
            Some(manager) => manager,
            None => {
                self.status = "Global shortcuts unavailable; local controls remain active".into();
                return false;
            }
        };
        #[cfg(not(test))]
        let mut backend = NativeShortcutBackend { manager };
        #[cfg(not(test))]
        let result = replace_shortcut_set(&mut backend, &old, &parsed);
        self.known_active_shortcuts = result.known_active.clone();
        self.uncertain_shortcuts = result.uncertain.clone();
        self.shortcut_state = result.state;
        match result.state {
            TransactionState::NewActive => {
                self.active_shortcuts = Some(parsed);
                self.settings.shortcuts = parsed.map(|key| key.to_string());
                self.save();
                true
            }
            TransactionState::PreviousActive => {
                self.active_shortcuts = result.known_active.clone().try_into().ok();
                self.status = result.message;
                false
            }
            TransactionState::Inactive => {
                self.active_shortcuts = None;
                self.status = result.message;
                false
            }
            TransactionState::Unknown => {
                self.active_shortcuts = None;
                self.status = result.message;
                false
            }
        }
    }
    fn set_viewport(&mut self, ctx: &egui::Context) {
        let transparent = transparent_background(
            self.settings.appearance.native_transparency,
            self.settings.overlay_mode,
            self.wayland,
        );
        if self.last_transparency != Some(transparent) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Transparent(transparent));
            self.last_transparency = Some(transparent);
        }
        let decorations = decorations_for_mode(self.settings.overlay_mode);
        if self.last_decorations != Some(decorations) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(decorations));
            self.last_decorations = Some(decorations);
        }
    }
    fn draw_readout(&self, ui: &mut egui::Ui, available: egui::Rect, elapsed: Duration) {
        let text = format_duration(
            elapsed,
            self.settings.show_milliseconds,
            self.settings.fractional_digits,
            self.timer.is_running(),
        );
        let requested_font = FontId::monospace(self.settings.appearance.font_size);
        let measured = ui
            .fonts_mut(|fonts| {
                fonts.layout_no_wrap(text.clone(), requested_font.clone(), Color32::WHITE)
            })
            .size();
        // Reserve a small optical safety margin for glyph side-bearings at the
        // smallest overlay sizes; the measured galley does not include every
        // rasterized edge pixel on all backends.
        let safe_available = available.shrink2(Vec2::splat(8.0));
        let (font_size, rect) =
            centered_readout_rect(safe_available, measured, self.settings.appearance.font_size);
        let allocated = rect.size();
        ui.allocate_rect(rect, egui::Sense::hover());
        let center = rect.center();
        let font = FontId::monospace(font_size);
        let base = color32(self.settings.appearance.text_color);
        let end = color32(self.settings.appearance.gradient_end);
        if self.settings.appearance.gradient {
            let mut galley = ui.fonts_mut(|fonts| {
                fonts.layout_no_wrap(text.clone(), font.clone(), Color32::WHITE)
            });
            let galley_data = Arc::make_mut(&mut galley);
            for row in &mut galley_data.rows {
                let row_origin = rect.min + row.pos.to_vec2();
                let mesh = &mut Arc::make_mut(&mut row.row).visuals.mesh;
                for vertex in &mut mesh.vertices {
                    let point = row_origin + vertex.pos.to_vec2();
                    let t = projected_gradient_factor(
                        rect,
                        point,
                        self.settings.appearance.gradient_angle,
                    );
                    vertex.color = base.lerp_to_gamma(end, t);
                }
            }
            ui.painter().galley(rect.min, galley, Color32::WHITE);
            return;
        }
        let width = allocated.x / text.chars().count().max(1) as f32;
        let start = center - Vec2::new(width * (text.chars().count() as f32) / 2.0, 0.0);
        for (i, ch) in text.chars().enumerate() {
            ui.painter().text(
                start + Vec2::new(width * i as f32, 0.0),
                Align2::LEFT_CENTER,
                ch,
                font.clone(),
                base,
            );
        }
    }
    fn overlay_ui(&self, ui: &mut egui::Ui, elapsed: Duration) {
        let bg = if transparent_background(
            self.settings.appearance.native_transparency,
            true,
            self.wayland,
        ) {
            Color32::TRANSPARENT
        } else {
            chroma_color(self.settings.appearance.chroma_key)
        };
        egui::CentralPanel::default()
            .frame(Frame::new().fill(bg))
            .show(ui, |ui| {
                let available = ui.max_rect();
                self.draw_readout(ui, available, elapsed);
            });
    }
    fn normal_ui(&mut self, ui: &mut egui::Ui, elapsed: Duration, now: Duration) {
        let background = chroma_color(self.settings.appearance.chroma_key);
        egui::CentralPanel::default()
            .frame(Frame::new().fill(background))
            .show(ui, |ui| {
                let compact = ui.available_height() < 360.0;
                ui.add_space(if compact { 8.0 } else { 14.0 });
                ui.vertical_centered(|ui| {
                    let requested_font = FontId::monospace(self.settings.appearance.font_size);
                    let text = format_duration(
                        elapsed,
                        self.settings.show_milliseconds,
                        self.settings.fractional_digits,
                        self.timer.is_running(),
                    );
                    let measured = ui
                        .fonts_mut(|fonts| {
                            fonts.layout_no_wrap(text, requested_font, Color32::WHITE)
                        })
                        .size();
                    let field_size = readout_field_size(ui.available_width(), measured);
                    ui.allocate_ui_with_layout(
                        field_size,
                        egui::Layout::top_down(egui::Align::Center),
                        |ui| {
                            Frame::new()
                                .fill(chroma_color(self.settings.appearance.chroma_key))
                                .corner_radius(tokens::RADIUS)
                                .inner_margin(egui::Margin::symmetric(8, 6))
                                .show(ui, |ui| {
                                    let available = ui.max_rect();
                                    self.draw_readout(ui, available, elapsed);
                                });
                        },
                    );
                    ui.add_space(if compact { 10.0 } else { 18.0 });
                    let group_width =
                        tokens::CONTROL_SIZE.x * 3.0 + ui.spacing().item_spacing.x * 2.0;
                    ui.allocate_ui_with_layout(
                        Vec2::new(group_width, tokens::CONTROL_SIZE.y),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let start = egui::Button::new(
                                RichText::new("START")
                                    .strong()
                                    .color(color32(tokens::DARK_TEXT)),
                            )
                            .fill(color32(tokens::GREEN))
                            .min_size(tokens::CONTROL_SIZE);
                            if ui.add_enabled(!self.timer.is_running(), start).clicked() {
                                self.apply(TimerAction::Start, now)
                            }
                            let stop = egui::Button::new(
                                RichText::new("STOP").strong().color(Color32::WHITE),
                            )
                            .fill(color32(tokens::SECONDARY))
                            .min_size(tokens::CONTROL_SIZE);
                            if ui.add_enabled(self.timer.is_running(), stop).clicked() {
                                self.apply(TimerAction::Stop, now)
                            }
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new("RESET").color(color32(tokens::RESET_TEXT)),
                                    )
                                    .fill(Color32::TRANSPARENT)
                                    .stroke(Stroke::new(1.0, color32(tokens::PANEL_OUTLINE)))
                                    .min_size(tokens::CONTROL_SIZE),
                                )
                                .clicked()
                            {
                                self.apply(TimerAction::Reset, now)
                            }
                        },
                    );
                    ui.add_space(if compact { 8.0 } else { 12.0 });
                    if ui
                        .add_sized(
                            [group_width, 30.0],
                            egui::Button::new(RichText::new("Settings").strong())
                                .fill(color32(tokens::PANEL))
                                .stroke(Stroke::new(1.0, color32(tokens::OUTLINE))),
                        )
                        .clicked()
                    {
                        self.open_settings(ui.ctx())
                    }
                });
            });
        if self.settings_open && self.test_mode {
            let surface = Arc::clone(&self.settings_surface);
            let intents = SettingsIntentSink {
                sender: self.settings_intents.clone(),
                root_ctx: ui.ctx().clone(),
            };
            egui::Window::new("Settings").show(ui.ctx(), |ui| {
                settings_contents(ui, &surface, &intents);
            });
        }
    }
}

fn settings_contents(
    ui: &mut egui::Ui,
    surface: &Arc<Mutex<SettingsSurfaceState>>,
    intents: &SettingsIntentSink,
) {
    let Ok(mut state) = surface.lock() else {
        return;
    };
    // Remove keyboard events before any Settings widgets are built. The same
    // events are retained locally for the active capture, so a captured chord
    // cannot also activate a focused checkbox or button.
    let capture_events = if state.capture.is_some() {
        ui.input_mut(|input| take_capture_key_events(&mut input.events))
    } else {
        Vec::new()
    };
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.heading("Presentation");
        ui.add_space(2.0);
        ui.separator();

        let mut fractional = state.settings.show_milliseconds;
        if ui
            .checkbox(&mut fractional, "Show fractional seconds")
            .changed()
        {
            state.settings.show_milliseconds = fractional;
            intents.send(SettingsIntent::Preference(
                PreferenceChange::ShowMilliseconds(fractional),
            ));
        }
        if fractional {
            let mut digits = state.settings.fractional_digits;
            if ui
                .add(egui::Slider::new(&mut digits, 0..=2).text("Fractional digits"))
                .changed()
            {
                state.settings.fractional_digits = digits.min(2);
                intents.send(SettingsIntent::Preference(
                    PreferenceChange::FractionalDigits(digits),
                ));
            }
        }
        let mut overlay = state.settings.overlay_mode;
        if ui.checkbox(&mut overlay, "Stream overlay mode").changed() {
            state.settings.overlay_mode = overlay;
            intents.send(SettingsIntent::Preference(PreferenceChange::OverlayMode(
                overlay,
            )));
        }
        let mut transparency = state.settings.appearance.native_transparency;
        if ui
            .checkbox(&mut transparency, "Request native transparent background")
            .changed()
        {
            state.settings.appearance.native_transparency = transparency;
            intents.send(SettingsIntent::Preference(
                PreferenceChange::NativeTransparency(transparency),
            ));
        }
        if let Some(message) =
            transparency_status(state.settings.appearance.native_transparency, state.wayland)
        {
            ui.colored_label(Color32::YELLOW, message);
        }
        ui.label("Chroma-key fallback");
        let mut chroma = color32(state.settings.appearance.chroma_key);
        let chroma_response = ui.color_edit_button_srgba(&mut chroma);
        chroma_response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Chroma-key color")
        });
        if chroma_response.changed() {
            state.settings.appearance.chroma_key = array_from_color(chroma);
            intents.send(SettingsIntent::Preference(PreferenceChange::ChromaKey(
                array_from_color(chroma),
            )));
        }
        ui.label("Timer color");
        let mut text = color32(state.settings.appearance.text_color);
        let text_response = ui.color_edit_button_srgba(&mut text);
        text_response.widget_info(|| {
            egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Timer color")
        });
        if text_response.changed() {
            state.settings.appearance.text_color = array_from_color(text);
            intents.send(SettingsIntent::Preference(PreferenceChange::TextColor(
                array_from_color(text),
            )));
        }
        let mut gradient = state.settings.appearance.gradient;
        if ui.checkbox(&mut gradient, "Use text gradient").changed() {
            state.settings.appearance.gradient = gradient;
            intents.send(SettingsIntent::Preference(PreferenceChange::Gradient(
                gradient,
            )));
        }
        if gradient {
            let mut end = color32(state.settings.appearance.gradient_end);
            let end_response = ui.color_edit_button_srgba(&mut end);
            end_response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, "Gradient end color")
            });
            if end_response.changed() {
                state.settings.appearance.gradient_end = array_from_color(end);
                intents.send(SettingsIntent::Preference(PreferenceChange::GradientEnd(
                    array_from_color(end),
                )));
            }
            let mut angle = state.settings.appearance.gradient_angle;
            if ui
                .add(egui::Slider::new(&mut angle, 0.0..=360.0).text("Gradient angle (°)"))
                .changed()
            {
                state.settings.appearance.gradient_angle = angle.clamp(0.0, 360.0);
                intents.send(SettingsIntent::Preference(PreferenceChange::GradientAngle(
                    angle,
                )));
            }
        }
        let mut font_size = state.settings.appearance.font_size;
        if ui
            .add(egui::Slider::new(&mut font_size, 48.0..=140.0).text("Readout size"))
            .changed()
        {
            state.settings.appearance.font_size = font_size.clamp(32.0, 180.0);
            intents.send(SettingsIntent::Preference(PreferenceChange::FontSize(
                font_size,
            )));
        }
        ui.separator();
        ui.heading("Global shortcuts");
        ui.label("Choose a binding, then press a modifier + one key.");
        ui.add_space(2.0);
        for i in 0..4 {
            let action = [
                TimerAction::Start,
                TimerAction::Stop,
                TimerAction::Reset,
                TimerAction::ToggleOverlay,
            ][i];
            ui.horizontal(|ui| {
                ui.add_sized(
                    [112.0, 28.0],
                    egui::Label::new(RichText::new(action.name()).strong()),
                );
                let capture_button = ui.add_enabled_ui(state.capture.is_none(), |ui| {
                    ui.add_sized(
                        [150.0, 28.0],
                        egui::Button::new(state.settings.shortcuts[i].as_str()),
                    )
                });
                if capture_button.inner.clicked() {
                    state.capture = Some(CaptureState::new(i));
                    intents.send(SettingsIntent::CaptureRequested(i));
                }
            });
            ui.add_space(3.0);
        }
        if let Some(capture) = &state.capture {
            ui.label(format!("Capture {}", capture.target + 1));
            let events = capture_events;
            let modifiers = ui.input(|input| input.modifiers);
            if !events.is_empty() || modifiers != egui::Modifiers::default() {
                intents.send(SettingsIntent::CaptureEvents(events, modifiers));
            }
        }
        if ui.button("Close").clicked() {
            intents.send(SettingsIntent::Close);
        }
        if !state.status.is_empty() {
            ui.label(&state.status);
        }
    });
}

impl Drop for TimerApp {
    fn drop(&mut self) {
        if let Some(manager) = &self.hotkey_manager {
            let mut keys = self.known_active_shortcuts.clone();
            for key in &self.uncertain_shortcuts {
                if !keys.iter().any(|known| known.id() == key.id()) {
                    keys.push(*key);
                }
            }
            let mut cleanup_failed = false;
            for key in keys {
                if manager.unregister(key).is_err() {
                    cleanup_failed = true;
                }
            }
            if cleanup_failed || self.shortcut_state == TransactionState::Unknown {
                eprintln!(
                    "Global shortcut shutdown cleanup was non-fatal; native shortcut state remains unknown/degraded"
                );
            }
        }
    }
}
impl eframe::App for TimerApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if transparent_background(
            self.settings.appearance.native_transparency,
            self.settings.overlay_mode,
            self.wayland,
        ) {
            [0.0, 0.0, 0.0, 0.0]
        } else {
            [12.0 / 255.0, 10.0 / 255.0, 45.0 / 255.0, 1.0]
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.wake_started {
            let receiver = GlobalHotKeyEvent::receiver().clone();
            let sender = self.global_sender.clone();
            let repaint = ctx.clone();
            std::thread::spawn(move || {
                while let Ok(event) = receiver.recv() {
                    if sender.send(event).is_err() {
                        break;
                    }
                    repaint.request_repaint_of(egui::ViewportId::ROOT)
                }
            });
            self.wake_started = true
        }
        let now = self.now();
        if let Some(action) = escape_overlay_action(
            self.settings.overlay_mode,
            ctx.input(|input| input.key_pressed(egui::Key::Escape)),
        ) {
            self.apply(action, now);
        }
        while let Ok(event) = self.global_events.try_recv() {
            if self.capture.is_none() {
                if let Some(action) = action_for_event(event, self.active_shortcuts.as_ref()) {
                    self.apply(action, now);
                }
            }
        }
        self.drain_settings_intents(&ctx);
        let (elapsed, running) = frame_timer_state(&self.timer, now);
        if running {
            ctx.request_repaint_after(Duration::from_millis(33))
        }
        self.set_viewport(&ctx);
        #[cfg(test)]
        if self.settings_snapshot {
            let surface = Arc::clone(&self.settings_surface);
            let intents = SettingsIntentSink {
                sender: self.settings_intents.clone(),
                root_ctx: ctx.clone(),
            };
            egui::CentralPanel::default()
                .frame(Frame::new().fill(chroma_color(tokens::CHROMA)))
                .show(ui, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.allocate_ui_with_layout(
                            Vec2::new(430.0, 660.0),
                            egui::Layout::top_down(egui::Align::Min),
                            |ui| {
                                Frame::new()
                                    .fill(color32(tokens::PANEL))
                                    .stroke(Stroke::new(1.0, color32(tokens::OUTLINE)))
                                    .corner_radius(tokens::RADIUS)
                                    .inner_margin(egui::Margin::same(20))
                                    .show(ui, |ui| settings_contents(ui, &surface, &intents));
                            },
                        );
                    });
                });
            return;
        }
        if self.settings.overlay_mode {
            self.overlay_ui(ui, elapsed)
        } else {
            self.normal_ui(ui, elapsed, now)
        }
        if self.settings_open && !self.test_mode {
            self.native_settings(&ctx);
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(normal_viewport_size())
            .with_min_inner_size(MIN_WINDOW_SIZE)
            .with_transparent(true),
        ..Default::default()
    };
    eframe::run_native(
        "JXTimer",
        options,
        Box::new(|_| Ok(Box::new(TimerApp::default()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::{kittest::Queryable, Harness};
    fn d(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    fn ui_harness(size: [f32; 2]) -> Harness<'static, TimerApp> {
        Harness::builder()
            .with_size(size)
            .with_pixels_per_point(1.0)
            .build_eframe(|_| TimerApp::new_for_test())
    }

    #[test]
    fn headless_normal_view_exposes_primary_timer_controls() {
        let mut harness = ui_harness([900.0, 600.0]);
        harness.step();

        harness.get_by_label("START");
        harness.get_by_label("STOP");
        harness.get_by_label("RESET");
        harness.get_by_label("Settings");
        assert!(harness.query_by_label("FOCUS / STREAM TIMER").is_none());
        assert!(harness
            .query_by_label("READY • start when you are")
            .is_none());
    }

    #[test]
    fn normal_view_uses_configured_chroma_background() {
        let mut harness = ui_harness([900.0, 600.0]);
        harness.state_mut().settings.appearance.chroma_key = [1, 2, 3, 255];
        harness.run();
        assert_eq!(harness.render().unwrap().get_pixel(0, 0).0, [1, 2, 3, 255]);
    }

    #[test]
    fn normal_viewport_intent_is_compact_without_changing_minimum_width() {
        let [width, height] = normal_viewport_size();
        assert_eq!([width, height], [520.0, 200.0]);
        assert_eq!(MIN_WINDOW_SIZE, [380.0, 200.0]);
    }

    #[test]
    fn normal_readout_field_has_configured_key_color_and_small_vertical_slack() {
        let measured = Vec2::new(600.0, 90.0);
        let field = readout_field_size(700.0, measured);
        assert!(field.y - measured.y < 13.0);
        assert!(field.x - measured.x < 17.0);

        let mut harness = ui_harness([900.0, 600.0]);
        harness.state_mut().settings.appearance.chroma_key = [7, 8, 9, 255];
        harness.run();
        let image = harness.render().unwrap();
        let first_content_row = (0..600)
            .find(|y| (0..900).any(|x| image.get_pixel(x, *y).0 != [7, 8, 9, 255]))
            .unwrap();
        assert!(
            first_content_row < 100,
            "blank band reaches row {first_content_row}"
        );
        let mut key_pixels_around_readout = 0;
        for y in first_content_row.saturating_sub(4)..120 {
            for x in 150..750 {
                key_pixels_around_readout += (image.get_pixel(x, y).0 == [7, 8, 9, 255]) as usize;
            }
        }
        assert!(key_pixels_around_readout > 1_000);
    }

    #[test]
    fn headless_narrow_view_keeps_primary_controls_queryable() {
        let mut harness = ui_harness([380.0, 300.0]);
        harness.step();

        harness.get_by_label("START");
        harness.get_by_label("STOP");
        harness.get_by_label("RESET");
    }

    #[test]
    fn headless_overlay_view_exposes_timer_only() {
        let mut harness = ui_harness([900.0, 240.0]);
        harness.state_mut().settings.overlay_mode = true;
        harness.state_mut().settings.appearance.native_transparency = false;
        harness.step();

        assert!(harness.query_by_label("Settings").is_none());
        assert!(harness.query_by_label("START").is_none());
        assert!(harness.query_by_label("RESET").is_none());
    }

    #[test]
    fn snapshot_normal_timer_view() {
        let mut harness = ui_harness([900.0, 600.0]);
        harness.run();
        harness.snapshot("normal-timer");
    }

    #[test]
    fn snapshot_narrow_timer_view() {
        let mut harness = ui_harness([380.0, 300.0]);
        harness.run();
        harness.snapshot("narrow-timer");
    }

    #[test]
    fn snapshot_settings_dialog_with_feedback_and_shortcuts() {
        let mut harness = ui_harness([900.0, 700.0]);
        harness.state_mut().settings_open = true;
        harness.state_mut().settings_snapshot = true;
        harness.state_mut().settings.appearance.gradient = true;
        harness.state_mut().status = "Preferences saved".into();
        let settings = harness.state().settings.clone();
        {
            let mut surface = harness.state_mut().settings_surface.lock().unwrap();
            surface.settings = settings;
            surface.status = "Preferences saved".into();
        }
        harness.step();

        // Keep the visual review representative: every binding is visible and
        // feedback is present, but no capture overlay is active.
        for action in ["Start", "Stop", "Reset", "Toggle overlay"] {
            harness.get_by_label(action);
        }
        assert!(harness.state().capture.is_none());
        harness.snapshot("settings-dialog");
    }

    #[test]
    fn snapshot_timer_only_overlay_view() {
        let mut harness = ui_harness([900.0, 240.0]);
        harness.state_mut().settings.overlay_mode = true;
        harness.state_mut().settings.appearance.native_transparency = false;
        harness.run();
        harness.snapshot("overlay-timer-only");
    }

    #[test]
    fn snapshot_narrow_short_timer_only_overlay_view() {
        let mut harness = ui_harness([380.0, 150.0]);
        harness.state_mut().settings.overlay_mode = true;
        harness.state_mut().settings.appearance.native_transparency = false;
        harness.run();
        harness.snapshot("overlay-timer-narrow-short");
    }

    #[test]
    fn rendered_fallback_and_transparent_overlay_samples_are_bounded() {
        let mut fallback = ui_harness([380.0, 150.0]);
        fallback.state_mut().settings.overlay_mode = true;
        fallback.state_mut().settings.appearance.native_transparency = false;
        fallback.run();
        let fallback_image = fallback.render().unwrap();
        let expected = tokens::CHROMA;
        for point in [(0, 0), (379, 0), (0, 149), (379, 149)] {
            assert_eq!(fallback_image.get_pixel(point.0, point.1).0, expected);
        }

        let mut transparent = ui_harness([380.0, 150.0]);
        transparent.state_mut().settings.overlay_mode = true;
        transparent
            .state_mut()
            .settings
            .appearance
            .native_transparency = true;
        transparent.run();
        let transparent_image = transparent.render().unwrap();
        for point in [(0, 0), (379, 0), (0, 149), (379, 149)] {
            assert_eq!(
                transparent_image.get_pixel(point.0, point.1).0,
                [0, 0, 0, 0]
            );
        }
    }

    #[test]
    fn snapshot_gradient_overlay_view() {
        let mut harness = ui_harness([900.0, 240.0]);
        harness.state_mut().settings.overlay_mode = true;
        harness.state_mut().settings.appearance.native_transparency = false;
        harness.state_mut().settings.appearance.gradient = true;
        harness.run();
        harness.snapshot("overlay-timer-gradient");
    }

    #[test]
    fn snapshot_vertical_gradient_overlay_view() {
        let mut harness = ui_harness([900.0, 240.0]);
        harness.state_mut().settings.overlay_mode = true;
        harness.state_mut().settings.appearance.native_transparency = false;
        harness.state_mut().settings.appearance.gradient = true;
        harness.state_mut().settings.appearance.gradient_angle = 90.0;
        harness.run();
        harness.snapshot("overlay-timer-gradient-vertical");
        let image = harness.render().unwrap();
        let mut rows = Vec::new();
        for y in 0..240 {
            for x in 150..750 {
                let pixel = image.get_pixel(x, y).0;
                if pixel != tokens::CHROMA {
                    rows.push((y, pixel));
                    break;
                }
            }
        }
        assert!(rows.len() > 2);
        assert_ne!(rows.first().unwrap().1, rows.last().unwrap().1);
    }

    #[test]
    fn settings_intents_update_preferences_without_changing_timer() {
        let mut app = TimerApp::new_for_test();
        app.timer.start(d(100));
        let before = app.timer.elapsed(d(500));
        for change in [
            PreferenceChange::ShowMilliseconds(false),
            PreferenceChange::NativeTransparency(false),
            PreferenceChange::ChromaKey([1, 2, 3, 0]),
            PreferenceChange::TextColor([4, 5, 6, 255]),
            PreferenceChange::Gradient(true),
            PreferenceChange::GradientEnd([8, 9, 10, 255]),
            PreferenceChange::FontSize(99.0),
        ] {
            app.settings_intents
                .send(SettingsIntent::Preference(change))
                .unwrap();
        }
        app.drain_settings_intents(&egui::Context::default());
        assert!(!app.settings.show_milliseconds);
        assert!(!app.settings.appearance.native_transparency);
        assert_eq!(app.settings.appearance.chroma_key, [1, 2, 3, 255]);
        assert_eq!(app.settings.appearance.text_color, [4, 5, 6, 255]);
        assert!(app.settings.appearance.gradient);
        assert_eq!(app.settings.appearance.gradient_end, [8, 9, 10, 255]);
        assert_eq!(app.settings.appearance.font_size, 99.0);
        assert!(app.timer.is_running());
        assert_eq!(app.timer.elapsed(d(500)), before);
    }

    #[test]
    fn headless_controls_update_timer_and_preferences() {
        let mut harness = ui_harness([900.0, 600.0]);
        harness.step();

        harness.get_by_label("START").click();
        harness.step();
        assert!(harness.state().timer.is_running());

        harness.get_by_label("STOP").click();
        harness.step();
        assert!(!harness.state().timer.is_running());

        harness.get_by_label("Settings").click();
        harness.step();
        harness
            .get_by_role_and_label(accesskit::Role::CheckBox, "Show fractional seconds")
            .click();
        harness.run();
        assert!(!harness.state().settings.show_milliseconds);
    }

    #[test]
    fn settings_workflow_reports_save_restores_and_keeps_overlay_pure() {
        let path = std::env::temp_dir().join(format!(
            "global-timer-settings-workflow-{}.json",
            std::process::id()
        ));
        let mut harness = ui_harness([900.0, 700.0]);
        harness.state_mut().config_file = Some(path.clone());
        harness.state_mut().settings_open = true;
        harness.state_mut().timer.start(d(10));
        harness.step();
        harness
            .get_by_role_and_label(accesskit::Role::CheckBox, "Use text gradient")
            .click();
        harness.step();
        harness.step();
        assert_eq!(harness.state().status, "Preferences saved");
        let elapsed = harness.state().timer.elapsed(d(125));
        assert!(harness.state().timer.is_running());

        let restored = load_settings_from(&path);
        assert!(restored.settings.appearance.gradient);
        assert_eq!(restored.status, "Preferences restored");
        assert_eq!(elapsed, d(115));

        harness.state_mut().settings.overlay_mode = true;
        harness.state_mut().settings.appearance.native_transparency = false;
        harness.step();
        let image = harness.render().unwrap();
        assert_eq!(image.get_pixel(0, 0).0, tokens::CHROMA);
        assert!(harness.query_by_label("Preferences saved").is_none());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn headless_settings_interaction_operates_every_preference_control() {
        let mut harness = ui_harness([900.0, 700.0]);
        harness.state_mut().settings_open = true;
        harness.step();

        harness
            .get_by_role_and_label(accesskit::Role::CheckBox, "Show fractional seconds")
            .click();
        harness.step();
        harness
            .get_by_role_and_label(
                accesskit::Role::CheckBox,
                "Request native transparent background",
            )
            .click();
        harness.step();
        harness
            .get_by_role_and_label(accesskit::Role::CheckBox, "Use text gradient")
            .click();
        harness.step();

        harness
            .get_by_role_and_label(accesskit::Role::Button, "Chroma-key color")
            .click();
        harness.step();
        harness
            .get_all_by_role(accesskit::Role::Slider)
            .next()
            .unwrap()
            .click();
        harness
            .get_by_role_and_label(accesskit::Role::Button, "Chroma-key color")
            .click();
        harness.run();
        harness
            .get_by_role_and_label(accesskit::Role::Button, "Timer color")
            .click();
        harness.step();
        harness.get_by_value("R 92").click();
        harness.get_by_value("R 92").type_text("10");
        harness
            .get_by_role_and_label(accesskit::Role::Button, "Timer color")
            .click();
        harness.run();
        harness
            .get_by_role_and_label(accesskit::Role::Button, "Gradient end color")
            .click();
        harness.step();
        harness.get_by_value("R 168").click();
        harness.get_by_value("R 168").type_text("10");
        harness
            .get_by_role_and_label(accesskit::Role::Button, "Gradient end color")
            .click();
        harness.run();
        harness
            .get_all_by_role(accesskit::Role::Slider)
            .nth(1)
            .unwrap()
            .click();
        harness.run();

        let settings = &harness.state().settings;
        assert!(!settings.show_milliseconds);
        assert!(!settings.appearance.native_transparency);
        assert!(settings.appearance.gradient);
        assert_ne!(
            settings.appearance.chroma_key,
            Appearance::default().chroma_key
        );
        assert_ne!(
            settings.appearance.text_color,
            Appearance::default().text_color
        );
        assert_ne!(
            settings.appearance.gradient_end,
            Appearance::default().gradient_end
        );
        assert_ne!(
            settings.appearance.font_size,
            Appearance::default().font_size
        );

        harness
            .get_by_role_and_label(accesskit::Role::CheckBox, "Stream overlay mode")
            .click();
        harness.run();
        assert!(harness.state().settings.overlay_mode);
        assert!(harness.state().settings_open);
    }

    #[test]
    fn headless_unavailable_shortcuts_keep_direct_controls_and_local_overlay() {
        let mut harness = ui_harness([900.0, 600.0]);
        harness.step();
        assert!(harness.state().active_shortcuts.is_none());

        harness.get_by_label("START").click();
        harness.step();
        assert!(harness.state().timer.is_running());
        harness.get_by_label("STOP").click();
        harness.step();
        assert!(!harness.state().timer.is_running());
        harness.get_by_label("RESET").click();
        harness.step();
        assert_eq!(
            harness.state().timer.elapsed(Duration::ZERO),
            Duration::ZERO
        );

        harness.get_by_label("Settings").click();
        harness.step();
        harness
            .get_by_role_and_label(accesskit::Role::CheckBox, "Stream overlay mode")
            .click();
        harness.step();
        harness.step();
        assert!(harness.state().settings.overlay_mode);
    }

    #[test]
    fn settings_focus_order_and_capture_affordance_are_headless_queryable() {
        let mut harness = ui_harness([900.0, 700.0]);
        harness.state_mut().settings_open = true;
        harness.step();
        let checks: Vec<_> = harness.get_all_by_role(accesskit::Role::CheckBox).collect();
        assert_eq!(checks.len(), 4);
        checks[0].focus();
        harness.step();
        assert!(harness
            .get_all_by_role(accesskit::Role::CheckBox)
            .next()
            .unwrap()
            .is_focused());

        harness.get_by_label("Start");
        harness.get_by_label("control+alt+KeyS");
        harness.get_by_label("control+alt+KeyS").click();
        harness.step();
        harness.step();
        assert_eq!(
            harness
                .state()
                .capture
                .as_ref()
                .map(|capture| capture.target),
            Some(0)
        );
        assert!(harness.state().status.contains("Listening"));
    }

    #[test]
    fn closing_settings_clears_capture_and_restores_global_shortcuts() {
        let mut harness = ui_harness([900.0, 600.0]);
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        harness.state_mut().settings_open = true;
        harness.state_mut().capture = Some(CaptureState::new(3));
        harness.state_mut().active_shortcuts = Some(keys);
        harness.state_mut().close_settings();

        assert!(!harness.state().settings_open);
        assert!(harness.state().capture.is_none());
        harness
            .state()
            .global_sender
            .send(GlobalHotKeyEvent {
                id: keys[3].id(),
                state: HotKeyState::Pressed,
            })
            .unwrap();
        harness.step();
        assert!(harness.state().settings.overlay_mode);
    }

    #[test]
    fn entering_overlay_closes_settings_and_capture() {
        let mut app = TimerApp::new_for_test();
        app.settings_open = true;
        app.capture = Some(CaptureState::new(0));
        app.apply(TimerAction::ToggleOverlay, Duration::ZERO);

        assert!(app.settings.overlay_mode);
        assert!(!app.settings_open);
        assert!(app.capture.is_none());
    }
    #[test]
    fn settings_overlay_toggle_keeps_viewport_and_capture_state_open() {
        let mut app = TimerApp::new_for_test();
        app.settings_open = true;
        app.settings_intents
            .send(SettingsIntent::CaptureRequested(2))
            .unwrap();
        app.settings_intents
            .send(SettingsIntent::Preference(PreferenceChange::OverlayMode(
                true,
            )))
            .unwrap();
        app.drain_settings_intents(&egui::Context::default());

        assert!(app.settings.overlay_mode);
        assert!(app.settings_open);
        assert_eq!(app.capture.as_ref().map(|capture| capture.target), Some(2));
    }
    #[test]
    fn timer_is_duration_driven() {
        let mut t = Timer::default();
        t.start(d(100));
        assert_eq!(t.elapsed(d(225)), d(125));
        t.stop(d(300));
        assert_eq!(t.elapsed(d(900)), d(200));
    }
    #[test]
    fn formatting_is_fractional_and_stable() {
        assert_eq!(format_duration(d(3_723_045), true, 2, false), "01:02:03.04");
        assert_eq!(format_duration(d(7), false, 2, false), "00:00:00");
        assert_eq!(format_duration(d(59_990), true, 2, true), "      59.99");
        assert_eq!(format_duration(d(12_340), true, 2, true), "      12.34");
        assert_eq!(format_duration(d(60_000), true, 2, true), "   01:00.00");
        assert_eq!(format_duration(d(3_600_000), true, 2, true), "01:00:00.00");
        assert_eq!(format_duration(d(3_661_000), true, 2, true), "01:01:01.00");
        assert_eq!(
            format_duration(Duration::ZERO, true, 2, false),
            "00:00:00.00"
        );
        assert_eq!(
            format_duration(Duration::ZERO, true, 2, true),
            "      00.00"
        );
        assert_eq!(format_duration(d(1), true, 2, true), "      00.00");
    }
    #[test]
    fn defaults_are_complete_and_valid() {
        let s = Settings::default();
        assert!(s.show_milliseconds);
        assert_eq!(s.fractional_digits, 2);
        assert_eq!(s.shortcuts.len(), 4);
        assert!(parse_shortcuts(&s.shortcuts).is_ok());
        assert!(parse_color(&color_hex(s.appearance.text_color)).is_some());
        assert!(parse_color(&color_hex(s.appearance.gradient_end)).is_some());
        assert_eq!(s.appearance.chroma_key[3], 255);
        assert!((32.0..=180.0).contains(&s.appearance.font_size));
        assert_eq!(s.appearance.chroma_key, [39, 24, 132, 255]);
    }
    #[test]
    fn malformed_fields_recover_without_losing_valid_values() {
        let raw = RawSettings {
            show_milliseconds: Some(false),
            text_color: Some("nope".into()),
            chroma_key: Some("#123456".into()),
            font_size: Some(999.0),
            fractional_digits: Some(99),
            shortcuts: Some(vec!["bad".into()]),
            ..Default::default()
        };
        let s = normalize(raw);
        assert!(!s.show_milliseconds);
        assert_eq!(s.appearance.chroma_key, [0x12, 0x34, 0x56, 255]);
        assert_eq!(s.appearance.text_color, Appearance::default().text_color);
        assert_eq!(s.shortcuts[0], DEFAULT_SHORTCUTS[0]);
        assert_eq!(s.fractional_digits, 2);
    }
    #[test]
    fn gradient_angle_normalizes_and_projection_changes_with_direction() {
        let mut raw = RawSettings {
            gradient_angle: Some(999.0),
            ..Default::default()
        };
        assert_eq!(normalize(raw.clone()).appearance.gradient_angle, 360.0);
        raw.gradient_angle = Some(-12.0);
        assert_eq!(normalize(raw.clone()).appearance.gradient_angle, 0.0);
        raw.gradient_angle = Some(f32::NAN);
        assert_eq!(normalize(raw).appearance.gradient_angle, 0.0);

        let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(100.0, 40.0));
        let point = egui::pos2(80.0, 10.0);
        assert_ne!(
            projected_gradient_factor(rect, point, 0.0),
            projected_gradient_factor(rect, point, 90.0)
        );
    }
    #[test]
    fn settings_round_trip_preserves_all_fields() {
        let mut s = Settings::default();
        s.overlay_mode = true;
        s.appearance.gradient = true;
        s.appearance.gradient_angle = 135.0;
        s.appearance.text_color = [1, 2, 3, 255];
        let path =
            std::env::temp_dir().join(format!("global-timer-test-{}.json", std::process::id()));
        save_settings_to(&path, &s).unwrap();
        let loaded = load_settings_from(&path);
        assert_eq!(loaded.settings, s);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn initial_settings_restore_does_not_create_or_advance_timer() {
        let mut settings = Settings::default();
        settings.overlay_mode = true;
        settings.show_milliseconds = false;
        settings.appearance.gradient = true;
        let (restored, timer) = initial_runtime_state(settings.clone());
        assert_eq!(restored, settings);
        assert_eq!(timer, Timer::default());
        assert_eq!(timer.elapsed(d(250)), Duration::ZERO);
        assert!(!timer.is_running());
    }
    #[test]
    fn every_settings_preference_change_mutates_only_its_target() {
        let mut settings = Settings::default();
        apply_preference_change(&mut settings, PreferenceChange::ShowMilliseconds(false));
        apply_preference_change(&mut settings, PreferenceChange::OverlayMode(true));
        apply_preference_change(&mut settings, PreferenceChange::NativeTransparency(false));
        apply_preference_change(&mut settings, PreferenceChange::ChromaKey([1, 2, 3, 0]));
        apply_preference_change(&mut settings, PreferenceChange::TextColor([4, 5, 6, 7]));
        apply_preference_change(&mut settings, PreferenceChange::Gradient(true));
        apply_preference_change(&mut settings, PreferenceChange::GradientEnd([8, 9, 10, 11]));
        apply_preference_change(&mut settings, PreferenceChange::GradientAngle(270.0));
        apply_preference_change(&mut settings, PreferenceChange::FontSize(99.0));
        assert!(!settings.show_milliseconds && settings.overlay_mode);
        assert!(!settings.appearance.native_transparency);
        assert_eq!(settings.appearance.chroma_key, [1, 2, 3, 255]);
        assert_eq!(settings.appearance.text_color, [4, 5, 6, 7]);
        assert!(settings.appearance.gradient);
        assert_eq!(settings.appearance.gradient_end, [8, 9, 10, 11]);
        assert_eq!(settings.appearance.gradient_angle, 270.0);
        assert_eq!(settings.appearance.font_size, 99.0);
    }
    #[test]
    fn save_feedback_preserves_success_and_failure_meaning() {
        assert_eq!(save_status(Ok(())), "Preferences saved");
        assert!(save_status(Err("disk full".into())).contains("kept in memory"));
    }
    fn injected_directory_failure(path: &Path, settings: &Settings) -> Result<(), String> {
        save_settings_to_with_hooks(
            path,
            settings,
            |settings| {
                serde_json::to_vec_pretty(&to_file(settings)).map_err(|error| error.to_string())
            },
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected directory failure",
                ))
            },
            write_temp_file,
            replace_config_file,
        )
    }
    fn injected_serialization_failure(path: &Path, settings: &Settings) -> Result<(), String> {
        save_settings_to_with_hooks(
            path,
            settings,
            |_| Err("injected serialization failure".into()),
            |_| panic!("directory creation must not run after serialization failure"),
            write_temp_file,
            replace_config_file,
        )
    }
    fn injected_replacement_failure(path: &Path, settings: &Settings) -> Result<(), String> {
        save_settings_to_with_hooks(
            path,
            settings,
            |settings| {
                serde_json::to_vec_pretty(&to_file(settings)).map_err(|error| error.to_string())
            },
            |dir| fs::create_dir_all(dir),
            write_temp_file,
            |_, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected replacement failure",
                ))
            },
        )
    }
    #[test]
    fn timer_app_save_keeps_memory_and_rejects_partial_file_on_injected_failures() {
        let directory = std::env::temp_dir().join(format!(
            "global-timer-save-injection-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("settings.json");
        let original = br#"previous durable preferences"#;
        let mut expected = Settings::default();
        expected.overlay_mode = true;
        expected.appearance.gradient = true;

        for injected in [
            injected_directory_failure as fn(&Path, &Settings) -> Result<(), String>,
            injected_serialization_failure,
            injected_replacement_failure,
        ] {
            fs::write(&path, original).unwrap();
            let mut app = TimerApp::new_for_test();
            app.config_file = Some(path.clone());
            app.settings = expected.clone();
            app.save_override = Some(injected);
            app.save();

            assert_eq!(app.settings, expected);
            assert!(app.status.contains("kept in memory"));
            assert_eq!(fs::read(&path).unwrap(), original);
            let entries: Vec<_> = fs::read_dir(&directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect();
            assert_eq!(entries, vec![std::ffi::OsString::from("settings.json")]);
        }
        fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn one_frame_snapshot_uses_one_timer_and_elapsed_input() {
        let mut timer = Timer::default();
        timer.start(d(100));
        let first = frame_timer_state(&timer, d(250));
        let second = frame_timer_state(&timer, d(250));
        assert_eq!(first, (d(150), true));
        assert_eq!(first, second);
        assert!(timer.is_running());
    }
    #[test]
    fn unsupported_version_is_safe() {
        let path =
            std::env::temp_dir().join(format!("global-timer-version-{}.json", std::process::id()));
        fs::write(&path, r#"{"schema_version":99,"settings":{}}"#).unwrap();
        let loaded = load_settings_from(&path);
        assert_eq!(loaded.settings, Settings::default());
        assert!(loaded.status.contains("unsupported"));
        assert!(loaded.writes_blocked);
        assert!(path.exists());
        let _ = fs::remove_file(path);
    }
    #[test]
    fn missing_canonical_recovers_valid_backup() {
        let path = std::env::temp_dir().join(format!(
            "global-timer-backup-recovery-{}.json",
            std::process::id()
        ));
        let backup = backup_path(&path);
        let mut settings = Settings::default();
        settings.overlay_mode = true;
        let data = serde_json::to_vec(&to_file(&settings)).unwrap();
        let _ = fs::remove_file(&path);
        fs::write(&backup, data).unwrap();

        let loaded = load_settings_from(&path);
        assert_eq!(loaded.settings, settings);
        assert!(loaded.status.contains("backup"));
        assert!(!loaded.writes_blocked);
        assert!(!path.exists());
        assert!(backup.exists());
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(backup);
    }
    #[test]
    fn future_schema_backup_is_non_destructive_and_blocks_writes() {
        let path = std::env::temp_dir().join(format!(
            "global-timer-future-backup-{}.json",
            std::process::id()
        ));
        let backup = backup_path(&path);
        let original = br#"{"schema_version":99,"settings":{"overlay_mode":true}}"#;
        let _ = fs::remove_file(&path);
        fs::write(&backup, original).unwrap();

        let loaded = load_settings_from(&path);
        assert!(loaded.writes_blocked);
        assert!(save_settings_to(&path, &Settings::default()).is_err());
        assert!(!path.exists());
        assert_eq!(fs::read(&backup).unwrap(), original);
        let _ = fs::remove_file(path);
        let _ = fs::remove_file(backup);
    }
    #[test]
    fn future_schema_is_non_destructive_and_blocks_writes() {
        let path = std::env::temp_dir().join(format!(
            "global-timer-future-schema-{}.json",
            std::process::id()
        ));
        let original = br#"{"schema_version":99,"settings":{"overlay_mode":true}}"#;
        fs::write(&path, original).unwrap();

        let loaded = load_settings_from(&path);
        assert!(loaded.writes_blocked);
        assert!(save_settings_to(&path, &Settings::default()).is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn injected_write_sync_and_replace_failures_preserve_durable_config() {
        let path = std::env::temp_dir().join(format!(
            "global-timer-persist-failures-{}.json",
            std::process::id()
        ));
        let temporary = path.with_file_name("global-timer-persist-failures.tmp");
        let original = b"previous durable preferences";
        fs::write(&path, original).unwrap();

        let write_failure = persist_temp_and_replace(
            &temporary,
            &path,
            b"new preferences",
            |_temporary, _data| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "injected write failure",
                ))
            },
            |_temporary, _destination| panic!("replace must not run after write failure"),
        );
        assert!(write_failure.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);

        let sync_failure = persist_temp_and_replace(
            &temporary,
            &path,
            b"new preferences",
            |_temporary, _data| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected sync failure",
                ))
            },
            |_temporary, _destination| panic!("replace must not run after sync failure"),
        );
        assert!(sync_failure.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);

        let replace_failure = persist_temp_and_replace(
            &temporary,
            &path,
            b"new preferences",
            |temporary, data| fs::write(temporary, data),
            |_temporary, _destination| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected replace failure",
                ))
            },
        );
        assert!(replace_failure.is_err());
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(!temporary.exists());
        let _ = fs::remove_file(path);
    }
    #[test]
    fn loaded_settings_preserve_one_duration_driven_timer() {
        let mut timer = Timer::default();
        timer.start(d(10));
        let settings = Settings::default();
        let loaded = decode_settings(&serde_json::to_vec(&to_file(&settings)).unwrap()).unwrap();
        let elapsed_before = timer.elapsed(d(125));

        assert_eq!(loaded, settings);
        assert_eq!(timer.elapsed(d(125)), elapsed_before);
        assert!(timer.is_running());
    }
    #[test]
    fn persisting_settings_preserves_running_and_stopped_timer_states() {
        let path = std::env::temp_dir().join(format!(
            "global-timer-state-preservation-{}.json",
            std::process::id()
        ));
        let mut running = Timer::default();
        running.start(d(10));
        let running_elapsed = running.elapsed(d(125));
        let mut running_settings = Settings::default();
        running_settings.overlay_mode = true;
        save_settings_to(&path, &running_settings).unwrap();
        assert_eq!(running.elapsed(d(125)), running_elapsed);
        assert!(running.is_running());

        let mut stopped = Timer::default();
        stopped.start(d(0));
        stopped.stop(d(80));
        let stopped_elapsed = stopped.elapsed(d(500));
        let mut stopped_settings = Settings::default();
        stopped_settings.appearance.gradient = true;
        save_settings_to(&path, &stopped_settings).unwrap();
        assert_eq!(stopped.elapsed(d(500)), stopped_elapsed);
        assert!(!stopped.is_running());
        let _ = fs::remove_file(path);
    }
    #[test]
    fn malformed_json_and_existing_file_recover_safely() {
        let path =
            std::env::temp_dir().join(format!("global-timer-existing-{}.json", std::process::id()));
        fs::write(&path, b"not json").unwrap();
        assert_eq!(load_settings_from(&path).settings, Settings::default());
        let mut settings = Settings::default();
        settings.overlay_mode = true;
        save_settings_to(&path, &settings).unwrap();
        assert_eq!(load_settings_from(&path).settings, settings);
        let _ = fs::remove_file(path);
    }
    #[test]
    fn missing_file_uses_safe_defaults() {
        let path =
            std::env::temp_dir().join(format!("global-timer-missing-{}.json", std::process::id()));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
        assert_eq!(load_settings_from(&path).settings, Settings::default());
    }
    #[test]
    fn four_shortcuts_map_overlay_without_affecting_actions() {
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        for (index, expected) in [
            TimerAction::Start,
            TimerAction::Stop,
            TimerAction::Reset,
            TimerAction::ToggleOverlay,
        ]
        .into_iter()
        .enumerate()
        {
            let event = GlobalHotKeyEvent {
                id: keys[index].id(),
                state: HotKeyState::Pressed,
            };
            assert_eq!(action_for_event(event, Some(&keys)), Some(expected));
        }
    }
    #[test]
    fn shortcut_validation_rejects_duplicate_and_bare_key() {
        let mut values = Settings::default().shortcuts;
        values[1] = values[0].clone();
        assert!(parse_shortcuts(&values).is_err());
        values[1] = "KeyA".into();
        assert!(parse_shortcuts(&values).is_err());
    }
    #[test]
    fn overlay_action_does_not_touch_timer() {
        let mut t = Timer::default();
        t.start(d(10));
        let before = t.elapsed(d(50));
        assert_eq!(before, d(40));
        assert!(matches!(
            TimerAction::ToggleOverlay,
            TimerAction::ToggleOverlay
        ));
        assert!(t.is_running());
        assert_eq!(t.elapsed(d(50)), before);
    }
    #[test]
    fn local_and_global_overlay_transitions_share_state_logic() {
        let mut local = Settings::default();
        let mut global = Settings::default();
        apply_overlay_transition(&mut local);
        apply_overlay_transition(&mut global);
        assert_eq!(local.overlay_mode, global.overlay_mode);
        assert!(local.overlay_mode);
        assert!(!toggled_overlay(true, TimerAction::ToggleOverlay));
    }
    #[test]
    fn settings_global_and_escape_overlay_paths_share_transition_behavior() {
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let global_event = GlobalHotKeyEvent {
            id: keys[3].id(),
            state: HotKeyState::Pressed,
        };
        let global_action = action_for_event(global_event, Some(&keys));
        let escape_action = escape_overlay_action(true, true);

        assert_eq!(global_action, Some(overlay_toggle_action()));
        assert_eq!(escape_action, Some(overlay_toggle_action()));

        let mut settings_path = Settings::default();
        let mut global_path = Settings::default();
        let mut escape_path = Settings::default();
        apply_overlay_transition(&mut settings_path);
        if global_action.is_some() {
            apply_overlay_transition(&mut global_path);
        }
        if escape_action.is_some() {
            apply_overlay_transition(&mut escape_path);
        }
        assert_eq!(settings_path.overlay_mode, true);
        assert_eq!(global_path, settings_path);
        assert_eq!(escape_path, settings_path);
    }
    #[test]
    fn local_settings_and_global_overlay_toggles_have_equal_runtime_transition() {
        let mut local = ui_harness([900.0, 700.0]);
        local.state_mut().timer.start(d(100));
        local.state_mut().settings_open = true;
        local.step();
        local
            .get_by_role_and_label(accesskit::Role::CheckBox, "Stream overlay mode")
            .click();
        local.step();
        local.step();
        let local_elapsed = local.state().timer.elapsed(d(500));
        let local_running = local.state().timer.is_running();
        assert!(local.state().settings.overlay_mode);

        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let mut global = ui_harness([900.0, 700.0]);
        global.state_mut().timer.start(d(100));
        global.state_mut().active_shortcuts = Some(keys);
        global.step();
        global
            .state()
            .global_sender
            .send(GlobalHotKeyEvent {
                id: keys[3].id(),
                state: HotKeyState::Pressed,
            })
            .unwrap();
        global.step();

        assert!(global.state().settings.overlay_mode);
        assert_eq!(global.state().timer.elapsed(d(500)), local_elapsed);
        assert_eq!(global.state().timer.is_running(), local_running);
        assert_eq!(
            local.state().settings.overlay_mode,
            global.state().settings.overlay_mode
        );
    }
    #[test]
    fn shortcut_normalization_rejects_complete_set_collisions() {
        let raw = RawSettings {
            shortcuts: Some(vec![
                DEFAULT_SHORTCUTS[0].into(),
                DEFAULT_SHORTCUTS[0].into(),
                DEFAULT_SHORTCUTS[2].into(),
                DEFAULT_SHORTCUTS[3].into(),
            ]),
            ..Default::default()
        };
        assert_eq!(normalize(raw).shortcuts, Settings::default().shortcuts);
    }
    #[test]
    fn shortcut_normalization_canonicalizes_aliases_by_hotkey_identity() {
        let values = vec![
            "ctrl+alt+KeyS".into(),
            "control+alt+KeyT".into(),
            "ctrl+alt+KeyR".into(),
            "control+alt+KeyO".into(),
        ];
        let parsed: [String; 4] = values.clone().try_into().unwrap();
        let expected = parse_shortcuts(&parsed).unwrap().map(|key| key.to_string());
        let normalized = normalize(RawSettings {
            shortcuts: Some(values),
            ..Default::default()
        });
        assert_eq!(normalized.shortcuts, expected);

        let mut collision = parsed;
        collision[1] = "ctrl+alt+KeyS".into();
        assert_eq!(
            normalize(RawSettings {
                shortcuts: Some(collision.to_vec()),
                ..Default::default()
            })
            .shortcuts,
            Settings::default().shortcuts
        );
    }
    fn key_event(key: egui::Key, pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        }
    }
    #[test]
    fn capture_validates_commit_cancel_and_preserves_prior_binding() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let mut capture = CaptureState::new(3);
        process_capture(&mut capture, &key_event(egui::Key::O, true, modifiers));
        process_capture(&mut capture, &key_event(egui::Key::O, false, modifiers));
        assert!(matches!(
            finish_capture(&capture, egui::Modifiers::default()),
            CaptureResult::Committed(_)
        ));
        let mut invalid = CaptureState::new(3);
        process_capture(
            &mut invalid,
            &key_event(egui::Key::O, true, egui::Modifiers::default()),
        );
        process_capture(
            &mut invalid,
            &key_event(egui::Key::O, false, egui::Modifiers::default()),
        );
        assert!(matches!(
            finish_capture(&invalid, egui::Modifiers::default()),
            CaptureResult::Rejected(_)
        ));
        let mut settings = Settings::default();
        settings.shortcuts[3] = "control+alt+KeyP".into();
        let active_before = parse_shortcuts(&settings.shortcuts).unwrap();
        let draft_before = settings.shortcuts.clone();
        assert_eq!(active_before[3].to_string(), "control+alt+KeyP");
        assert_eq!(settings.shortcuts, draft_before);
        // A rejected candidate leaves both the non-default draft and active set
        // unchanged; this models the Settings commit guard.
        assert!(parse_shortcuts(&[
            "control+alt+KeyP".into(),
            "control+alt+KeyP".into(),
            DEFAULT_SHORTCUTS[2].into(),
            DEFAULT_SHORTCUTS[3].into()
        ])
        .is_err());
        assert_eq!(settings.shortcuts, draft_before);
        assert_eq!(
            process_capture(
                &mut CaptureState::new(3),
                &key_event(egui::Key::Escape, true, egui::Modifiers::default())
            ),
            CaptureResult::Cancelled
        );
    }
    #[test]
    fn ctrl_alt_a_capture_normalizes_persists_and_registers() {
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let both = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let events = [
            key_event(egui::Key::ControlLeft, true, ctrl),
            key_event(egui::Key::AltLeft, true, both),
            key_event(egui::Key::A, true, both),
            key_event(egui::Key::A, false, both),
            key_event(egui::Key::AltLeft, false, ctrl),
            key_event(egui::Key::ControlLeft, false, egui::Modifiers::default()),
        ];
        let mut capture = CaptureState::new(0);
        for event in events {
            assert_eq!(
                process_capture(&mut capture, &event),
                CaptureResult::Continue
            );
        }
        let committed = match finish_capture(&capture, egui::Modifiers::default()) {
            CaptureResult::Committed(key) => key,
            other => panic!("Ctrl + Alt + A was not committed: {other:?}"),
        };
        assert_eq!(committed.key, Code::KeyA);
        assert_eq!(
            committed.mods,
            HotKeyModifiers::CONTROL | HotKeyModifiers::ALT
        );

        let mut values = Settings::default().shortcuts;
        values[0] = committed.to_string();
        let normalized = normalize(RawSettings {
            shortcuts: Some(values.to_vec()),
            ..Default::default()
        });
        assert_eq!(normalized.shortcuts[0], "control+alt+KeyA");
        let persisted = decode_settings(&serde_json::to_vec(&to_file(&normalized)).unwrap())
            .expect("normalized Ctrl + Alt + A should persist");
        assert_eq!(persisted.shortcuts, normalized.shortcuts);

        let previous = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let next = parse_shortcuts(&persisted.shortcuts).unwrap();
        let mut backend = FakeBackend {
            registered: previous.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: Vec::new(),
            fail_unregister_at: Vec::new(),
            register_calls: 0,
            unregister_calls: 0,
        };
        let transaction = replace_shortcut_set(&mut backend, &previous, &next);
        assert_eq!(transaction.state, TransactionState::NewActive);
        assert_eq!(transaction.known_active, next.to_vec());
        assert_eq!(backend.registered, next.iter().map(HotKey::id).collect());
    }
    #[test]
    fn genuinely_unsupported_capture_key_is_still_rejected() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let mut capture = CaptureState::new(0);
        process_capture(&mut capture, &key_event(egui::Key::Copy, true, modifiers));
        process_capture(&mut capture, &key_event(egui::Key::Copy, false, modifiers));
        assert!(matches!(
            finish_capture(&capture, egui::Modifiers::default()),
            CaptureResult::Rejected(error) if error.contains("Unsupported")
        ));
    }
    struct FakeBackend {
        registered: HashSet<u32>,
        conflicts: HashSet<u32>,
        unregister_attempts: Vec<u32>,
        fail_register_at: Vec<usize>,
        fail_unregister_at: Vec<usize>,
        register_calls: usize,
        unregister_calls: usize,
    }
    impl ShortcutBackend for FakeBackend {
        fn register(&mut self, key: HotKey) -> Result<(), String> {
            self.register_calls += 1;
            if self.conflicts.contains(&key.id()) {
                return Err("owned by another application".into());
            }
            if self.fail_register_at.contains(&self.register_calls) {
                return Err("injected failure".into());
            }
            self.registered.insert(key.id());
            Ok(())
        }
        fn unregister(&mut self, key: HotKey) -> Result<(), String> {
            self.unregister_calls += 1;
            self.unregister_attempts.push(key.id());
            if self.fail_unregister_at.contains(&self.unregister_calls) {
                return Err("injected unregister failure".into());
            }
            self.registered.remove(&key.id());
            Ok(())
        }
    }
    fn settings_registration_failure(
        previous: &[HotKey],
        next: &[HotKey; 4],
    ) -> ShortcutTransaction {
        let mut backend = FakeBackend {
            registered: previous.iter().map(HotKey::id).collect(),
            conflicts: [next[0].id()].into_iter().collect(),
            unregister_attempts: Vec::new(),
            fail_register_at: Vec::new(),
            fail_unregister_at: Vec::new(),
            register_calls: 0,
            unregister_calls: 0,
        };
        replace_shortcut_set(&mut backend, previous, next)
    }
    fn settings_registration_success(
        previous: &[HotKey],
        next: &[HotKey; 4],
    ) -> ShortcutTransaction {
        let mut backend = FakeBackend {
            registered: previous.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: Vec::new(),
            fail_unregister_at: Vec::new(),
            register_calls: 0,
            unregister_calls: 0,
        };
        replace_shortcut_set(&mut backend, previous, next)
    }
    fn settings_rollback_failure(previous: &[HotKey], next: &[HotKey; 4]) -> ShortcutTransaction {
        let mut backend = FakeBackend {
            registered: previous.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![3, 5],
            fail_unregister_at: Vec::new(),
            register_calls: 0,
            unregister_calls: 0,
        };
        replace_shortcut_set(&mut backend, previous, next)
    }
    fn send_valid_capture(app: &mut TimerApp) {
        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        app.settings_intents
            .send(SettingsIntent::CaptureRequested(0))
            .unwrap();
        app.settings_intents
            .send(SettingsIntent::CaptureEvents(
                vec![
                    key_event(egui::Key::A, true, modifiers),
                    key_event(egui::Key::A, false, modifiers),
                ],
                egui::Modifiers::default(),
            ))
            .unwrap();
    }
    #[test]
    fn settings_capture_survives_transient_unfocus_and_commits_after_release() {
        let mut app = TimerApp::new_for_test();
        app.settings_open = true;
        app.shortcut_transaction_override = Some(settings_registration_success);
        let before = app.settings.shortcuts.clone();
        app.settings_intents
            .send(SettingsIntent::CaptureRequested(1))
            .unwrap();
        app.drain_settings_intents(&egui::Context::default());
        assert_eq!(app.capture.as_ref().map(|capture| capture.target), Some(1));

        // A transiently unfocused Settings frame does not emit Close. Capture
        // therefore remains live while the chord is entered.
        app.drain_settings_intents(&egui::Context::default());
        assert!(app.settings_open);
        assert!(app.capture.is_some());

        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        app.settings_intents
            .send(SettingsIntent::CaptureEvents(
                vec![key_event(egui::Key::A, true, modifiers)],
                modifiers,
            ))
            .unwrap();
        app.drain_settings_intents(&egui::Context::default());
        assert!(app.capture.is_some());
        assert_eq!(app.settings.shortcuts, before);

        app.settings_intents
            .send(SettingsIntent::CaptureEvents(
                vec![key_event(egui::Key::A, false, modifiers)],
                egui::Modifiers::default(),
            ))
            .unwrap();
        app.drain_settings_intents(&egui::Context::default());

        assert!(app.capture.is_none());
        assert_eq!(app.settings.shortcuts[1], "control+alt+KeyA");
        assert_eq!(app.settings.shortcuts[0], before[0]);
    }
    #[test]
    fn settings_capture_ends_on_viewport_close_or_explicit_escape() {
        let mut closed = TimerApp::new_for_test();
        closed.settings_open = true;
        closed
            .settings_intents
            .send(SettingsIntent::CaptureRequested(0))
            .unwrap();
        closed.drain_settings_intents(&egui::Context::default());
        assert!(closed.capture.is_some());
        closed.settings_intents.send(SettingsIntent::Close).unwrap();
        closed.drain_settings_intents(&egui::Context::default());
        assert!(!closed.settings_open);
        assert!(closed.capture.is_none());
        assert!(closed.settings_surface.lock().unwrap().capture.is_none());

        let mut cancelled = TimerApp::new_for_test();
        cancelled.settings_open = true;
        cancelled
            .settings_intents
            .send(SettingsIntent::CaptureRequested(0))
            .unwrap();
        cancelled.drain_settings_intents(&egui::Context::default());
        cancelled
            .settings_intents
            .send(SettingsIntent::CaptureEvents(
                vec![key_event(
                    egui::Key::Escape,
                    true,
                    egui::Modifiers::default(),
                )],
                egui::Modifiers::default(),
            ))
            .unwrap();
        cancelled.drain_settings_intents(&egui::Context::default());
        assert!(cancelled.capture.is_none());
        assert_eq!(cancelled.status, "Capture cancelled");
    }
    #[test]
    fn capture_key_events_are_consumed_before_settings_controls_see_them() {
        let modifiers = egui::Modifiers {
            ctrl: true,
            alt: true,
            ..Default::default()
        };
        let mut events = vec![
            egui::Event::PointerMoved(egui::pos2(4.0, 5.0)),
            key_event(egui::Key::A, true, modifiers),
            key_event(egui::Key::A, false, modifiers),
        ];
        let captured = take_capture_key_events(&mut events);
        assert_eq!(captured.len(), 2);
        assert!(captured
            .iter()
            .all(|event| matches!(event, egui::Event::Key { .. })));
        assert_eq!(
            events,
            vec![egui::Event::PointerMoved(egui::pos2(4.0, 5.0))]
        );
    }
    #[test]
    fn settings_path_surfaces_load_save_registration_rollback_and_fallback_feedback() {
        let path = std::env::temp_dir().join(format!(
            "global-timer-settings-feedback-{}-{}.json",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let mut edited = Settings::default();
        edited.appearance.gradient = true;
        save_settings_to(&path, &edited).unwrap();

        // Startup/load feedback is the same status displayed when Settings opens.
        let loaded = load_settings_from(&path);
        assert_eq!(loaded.status, "Preferences restored");
        fs::write(&path, b"not json").unwrap();
        assert!(load_settings_from(&path)
            .status
            .starts_with("Preferences unreadable"));
        save_settings_to(&path, &edited).unwrap();

        let mut harness = ui_harness([900.0, 700.0]);
        harness.state_mut().config_file = Some(path.clone());
        harness.state_mut().settings_open = true;
        harness.step();
        harness
            .get_by_role_and_label(accesskit::Role::CheckBox, "Use text gradient")
            .click();
        harness.step();
        harness.step();
        assert_eq!(harness.state().status, "Preferences saved");

        // The next startup consumes the durable edit, rather than the in-memory app.
        let restarted = load_settings_from(&path);
        assert!(restarted.settings.appearance.gradient == edited.appearance.gradient);

        harness.state_mut().save_override = Some(injected_directory_failure);
        harness
            .state()
            .settings_intents
            .send(SettingsIntent::Preference(PreferenceChange::FontSize(99.0)))
            .unwrap();
        harness.run();
        assert!(harness.state().status.contains("kept in memory"));

        let previous = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        harness.state_mut().known_active_shortcuts = previous.to_vec();
        harness.state_mut().shortcut_transaction_override = Some(settings_registration_failure);
        send_valid_capture(harness.state_mut());
        harness.run();
        assert!(harness.state().status.contains("Could not register"));
        assert_eq!(harness.state().settings.shortcuts[0], DEFAULT_SHORTCUTS[0]);

        harness.state_mut().shortcut_transaction_override = Some(settings_rollback_failure);
        send_valid_capture(harness.state_mut());
        harness.run();
        assert!(harness.state().status.contains("rollback failed"));
        assert_eq!(harness.state().settings.shortcuts[0], DEFAULT_SHORTCUTS[0]);

        harness.state_mut().settings_surface.lock().unwrap().wayland = true;
        harness.step();
        harness.get_by_label_contains("Native Wayland transparency is unsupported");

        // Feedback and capture are settings-only state: overlay pixels remain byte-for-byte
        // identical to a clean overlay frame.
        harness.state_mut().settings.overlay_mode = true;
        harness.state_mut().settings.appearance.native_transparency = false;
        harness.state_mut().capture = Some(CaptureState::new(0));
        harness.run();
        let dirty = harness.render().unwrap();
        harness.state_mut().capture = None;
        harness.state_mut().status.clear();
        harness.state_mut().settings_surface.lock().unwrap().capture = None;
        harness
            .state_mut()
            .settings_surface
            .lock()
            .unwrap()
            .status
            .clear();
        harness.run();
        let clean_image = harness.render().unwrap();
        for y in 0..700 {
            for x in 0..900 {
                assert_eq!(dirty.get_pixel(x, y), clean_image.get_pixel(x, y));
            }
        }
        let _ = fs::remove_file(path);
    }
    #[test]
    fn transaction_validation_rejects_before_unregister_or_commit() {
        let previous = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let mut invalid = Settings::default().shortcuts;
        invalid[1] = invalid[0].clone();
        let backend = FakeBackend {
            registered: previous.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![],
            fail_unregister_at: vec![],
            register_calls: 0,
            unregister_calls: 0,
        };

        let validation = parse_shortcuts(&invalid);
        assert!(validation.is_err());
        assert_eq!(backend.register_calls, 0);
        assert_eq!(backend.unregister_calls, 0);
        assert!(backend.unregister_attempts.is_empty());
        assert_eq!(
            backend.registered,
            previous.iter().map(HotKey::id).collect::<HashSet<_>>()
        );
    }
    #[test]
    fn transaction_complete_success_replaces_previous_set() {
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let next_values: [String; 4] = [
            "control+alt+KeyA".into(),
            "control+alt+KeyB".into(),
            "control+alt+KeyC".into(),
            "control+alt+KeyD".into(),
        ];
        let next = parse_shortcuts(&next_values).unwrap();
        let mut backend = FakeBackend {
            registered: keys.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![],
            fail_unregister_at: vec![],
            register_calls: 0,
            unregister_calls: 0,
        };
        let result = replace_shortcut_set(&mut backend, &keys, &next);
        assert_eq!(result.state, TransactionState::NewActive);
        assert!(result.uncertain.is_empty());
        assert_eq!(backend.registered.len(), 4);
        assert_eq!(result.known_active, next.to_vec());
    }
    #[test]
    fn transaction_registration_failure_rolls_back_previous_set() {
        let previous = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let next_values: [String; 4] = [
            "control+alt+KeyA".into(),
            "control+alt+KeyB".into(),
            "control+alt+KeyC".into(),
            "control+alt+KeyD".into(),
        ];
        let next = parse_shortcuts(&next_values).unwrap();
        let mut backend = FakeBackend {
            registered: previous.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![3],
            fail_unregister_at: vec![],
            register_calls: 0,
            unregister_calls: 0,
        };

        let result = replace_shortcut_set(&mut backend, &previous, &next);

        assert_eq!(result.state, TransactionState::PreviousActive);
        assert_eq!(result.known_active, previous.to_vec());
        assert!(result.uncertain.is_empty());
        assert_eq!(
            backend.registered,
            previous.iter().map(HotKey::id).collect::<HashSet<_>>()
        );
    }
    #[test]
    fn transaction_conflict_restores_previous_without_unregistering_external_key() {
        let previous = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let next_values: [String; 4] = [
            "control+alt+KeyA".into(),
            "control+alt+KeyB".into(),
            "control+alt+KeyC".into(),
            "control+alt+KeyD".into(),
        ];
        let next = parse_shortcuts(&next_values).unwrap();
        let external = next[1].id();
        let mut backend = FakeBackend {
            registered: previous.iter().map(HotKey::id).collect(),
            conflicts: [external].into_iter().collect(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![],
            fail_unregister_at: vec![],
            register_calls: 0,
            unregister_calls: 0,
        };

        let result = replace_shortcut_set(&mut backend, &previous, &next);

        assert_eq!(result.state, TransactionState::PreviousActive);
        assert_eq!(result.known_active, previous.to_vec());
        assert!(result.uncertain.is_empty());
        assert_eq!(
            backend.registered,
            previous.iter().map(HotKey::id).collect::<HashSet<_>>()
        );
        assert!(!backend.unregister_attempts.contains(&external));
        assert!(!backend.registered.contains(&next[0].id()));
    }
    #[test]
    fn transaction_cleans_new_partial_registration_with_empty_previous() {
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let mut backend = FakeBackend {
            registered: HashSet::new(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![2],
            fail_unregister_at: vec![],
            register_calls: 0,
            unregister_calls: 0,
        };
        let result = replace_shortcut_set(&mut backend, &[], &keys);
        assert_eq!(result.state, TransactionState::Inactive);
        assert!(result.uncertain.is_empty());
        assert!(backend.registered.is_empty());
    }
    #[test]
    fn transaction_reports_unknown_when_partial_cleanup_fails() {
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let mut backend = FakeBackend {
            registered: HashSet::new(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![2],
            fail_unregister_at: vec![1],
            register_calls: 0,
            unregister_calls: 0,
        };
        let result = replace_shortcut_set(&mut backend, &[], &keys);
        assert_eq!(result.state, TransactionState::Unknown);
        assert!(!result.uncertain.is_empty());
        assert!(result.message.contains("unknown"));
    }
    #[test]
    fn transaction_reports_unknown_when_previous_release_cannot_be_confirmed() {
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let mut backend = FakeBackend {
            registered: keys.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![],
            fail_unregister_at: vec![1, 2, 5],
            register_calls: 0,
            unregister_calls: 0,
        };
        let result = replace_shortcut_set(&mut backend, &keys, &keys);
        assert_eq!(result.state, TransactionState::Unknown);
        assert_eq!(result.uncertain.len(), 1);
        assert!(result.known_active.is_empty());
    }
    #[test]
    fn transaction_reports_unknown_when_rollback_cleanup_fails() {
        let keys = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let mut rollback = FakeBackend {
            registered: keys.iter().map(HotKey::id).collect(),
            conflicts: HashSet::new(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![2, 4],
            // Four old-set releases, one partial-new cleanup release, then
            // the first partial rollback cleanup release.
            fail_unregister_at: vec![6],
            register_calls: 0,
            unregister_calls: 0,
        };
        let result = replace_shortcut_set(&mut rollback, &keys, &keys);
        assert_eq!(result.state, TransactionState::Unknown);
        assert!(!result.uncertain.is_empty());
        assert!(result.message.contains("unknown"));
    }
    #[test]
    fn injected_startup_registration_conflict_reaches_normal_ui_with_inactive_globals() {
        let defaults = parse_shortcuts(&Settings::default().shortcuts).unwrap();
        let mut backend = FakeBackend {
            registered: HashSet::new(),
            conflicts: [defaults[1].id()].into_iter().collect(),
            unregister_attempts: Vec::new(),
            fail_register_at: vec![],
            fail_unregister_at: vec![],
            register_calls: 0,
            unregister_calls: 0,
        };
        let startup = initialize_shortcuts(&mut backend, &defaults);
        assert_eq!(startup.state, TransactionState::Inactive);
        assert!(startup.active.is_none());
        assert!(startup.known_active.is_empty());
        assert!(startup.uncertain.is_empty());
        assert!(backend.registered.is_empty());

        let mut harness = ui_harness([900.0, 600.0]);
        harness.state_mut().active_shortcuts = startup.active;
        harness.state_mut().known_active_shortcuts = startup.known_active;
        harness.state_mut().uncertain_shortcuts = startup.uncertain;
        harness.state_mut().shortcut_state = startup.state;
        harness.state_mut().status = startup.message;
        harness.step();

        harness.get_by_label("START");
        harness.get_by_label("Settings");
        assert!(harness.state().active_shortcuts.is_none());
        assert_eq!(harness.state().shortcut_state, TransactionState::Inactive);
        assert!(!harness.state().settings.overlay_mode);
    }
    #[test]
    fn fallback_is_uniform_and_chroma_is_opaque() {
        assert!(transparent_background(true, true, false));
        assert!(!transparent_background(true, true, true));
        assert_eq!(chroma_color([1, 2, 3, 17]).a(), 255);
    }
    #[test]
    fn transparency_status_never_claims_unconfirmed_composition_is_active() {
        assert_eq!(transparency_status(false, false), None);
        assert_eq!(
            transparency_status(true, true),
            Some("Native Wayland transparency is unsupported; uniform chroma-key fallback is active.")
        );
        assert_eq!(
            transparency_status(true, false),
            Some("Transparent composition requested (best effort, unconfirmed); chroma-key remains available.")
        );
    }
    #[test]
    fn overlay_viewport_intent_retains_native_decorations() {
        assert!(decorations_for_mode(false));
        assert!(decorations_for_mode(true));
    }
    #[test]
    fn readout_geometry_fits_narrow_viewports() {
        let available = egui::Rect::from_min_size(egui::pos2(11.0, 7.0), Vec2::new(220.0, 100.0));
        let (size, allocation) = centered_readout_rect(available, Vec2::new(1150.0, 90.0), 72.0);
        assert!(
            allocation.width() <= available.width() && allocation.height() <= available.height()
        );
        assert_eq!(allocation.center(), available.center());
        assert!(allocation.min.x >= available.min.x && allocation.max.x <= available.max.x);
        assert!(size < 72.0);
    }
}
