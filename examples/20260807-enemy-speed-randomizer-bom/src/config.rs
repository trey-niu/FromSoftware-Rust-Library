use std::{
    ffi::c_void,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

pub const CONFIG_FILE_NAME: &str = "enemy_speed_randomizer_bom.toml";
const CONFIG_AUTHOR: &str = "梅琳娜的刀";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModPaths {
    pub config_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct EnemySpeedRandomizerBomConfig {
    pub speed: EnemySpeedConfig,
    pub overlay: OverlayConfig,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub struct OverlayConfig {
    /// Whether the in-game overlay is drawn. The DX12 hook remains installed
    /// so this can be changed through config hot reload.
    pub enable: bool,
    /// One of: top_left, top_right, bottom_left, bottom_right.
    pub position: OverlayPosition,
    /// Horizontal distance from the selected left/right edge, in pixels.
    pub offset_x: f32,
    /// Vertical distance from the selected top/bottom edge, in pixels.
    pub offset_y: f32,
    /// ImGui font/window scale. Values are clamped to 0.25..=4.0 when drawn.
    pub scale: f32,
    /// Delay after FromSoftware system initialization before installing the
    /// DX12 hook. The game creates its device and swap chain later.
    pub startup_delay_ms: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OverlayPosition {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Default for OverlayPosition {
    fn default() -> Self {
        Self::TopLeft
    }
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            enable: true,
            position: OverlayPosition::TopLeft,
            offset_x: 24.0,
            offset_y: 24.0,
            scale: 1.0,
            startup_delay_ms: 10_000,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub struct EnemySpeedConfig {
    pub enable: bool,
    pub pool_1_min_percent: u32,
    pub pool_1_max_percent: u32,
    pub pool_2_min_percent: u32,
    pub pool_2_max_percent: u32,
    /// Minimum delay between randomizations, in milliseconds.
    pub randomize_interval_min_ms: u64,
    /// Maximum delay between randomizations, in milliseconds.
    pub randomize_interval_max_ms: u64,
    /// Relative weight used when choosing pool 1. Weights do not need to add up to 100.
    pub pool_1_weight: u32,
    /// Relative weight used when choosing pool 2. Weights do not need to add up to 100.
    pub pool_2_weight: u32,
    pub randomize_each_enemy: bool,
    pub toggle_virtual_key: i32,
}

impl Default for EnemySpeedRandomizerBomConfig {
    fn default() -> Self {
        Self {
            speed: EnemySpeedConfig::default(),
            overlay: OverlayConfig::default(),
        }
    }
}

impl Default for EnemySpeedConfig {
    fn default() -> Self {
        Self {
            enable: true,
            pool_1_min_percent: 50,
            pool_1_max_percent: 150,
            pool_2_min_percent: 50,
            pool_2_max_percent: 150,
            randomize_interval_min_ms: 3_000,
            randomize_interval_max_ms: 7_000,
            pool_1_weight: 50,
            pool_2_weight: 50,
            randomize_each_enemy: false,
            toggle_virtual_key: 0x72,
        }
    }
}

pub fn resolve_paths(hmodule_raw: usize) -> ModPaths {
    let dll_path = dll_path_from_module(hmodule_raw);
    mod_paths_from_dll_path(&dll_path)
}

pub fn load_or_create_config(path: &Path) -> EnemySpeedRandomizerBomConfig {
    if !path.exists() {
        let config = EnemySpeedRandomizerBomConfig::default();
        write_default_config(path, &config);
        return config;
    }

    match load_config(path) {
        Some(config) => config,
        None => {
            let config = EnemySpeedRandomizerBomConfig::default();
            write_default_config(path, &config);
            config
        }
    }
}

pub fn load_config(path: &Path) -> Option<EnemySpeedRandomizerBomConfig> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<EnemySpeedRandomizerBomConfig>(&text).ok())
}

pub fn config_modified_time(path: &Path) -> Option<SystemTime> {
    fs::metadata(path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
}

fn write_default_config(path: &Path, config: &EnemySpeedRandomizerBomConfig) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }

    let mut text = format!("author = {CONFIG_AUTHOR:?}\n\n");
    if let Ok(config_text) = toml::to_string_pretty(config) {
        text.push_str(&config_text);
    }
    let _ = fs::write(path, text);
}

fn dll_path_from_module(hmodule_raw: usize) -> PathBuf {
    let hmodule = HMODULE(hmodule_raw as *mut c_void);
    let mut path_buffer = vec![0u16; 260];

    loop {
        let len = unsafe { GetModuleFileNameW(Some(hmodule), &mut path_buffer) } as usize;
        if len == 0 {
            return PathBuf::from(".");
        }
        if len < path_buffer.len() {
            return PathBuf::from(String::from_utf16_lossy(&path_buffer[..len]));
        }
        if path_buffer.len() >= 32_768 {
            return PathBuf::from(".");
        }
        path_buffer.resize((path_buffer.len() * 2).min(32_768), 0);
    }
}

fn mod_paths_from_dll_path(dll_path: &Path) -> ModPaths {
    let dll_dir = dll_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    ModPaths {
        config_path: dll_dir.join(CONFIG_FILE_NAME),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_config_contains_speed_and_overlay_settings() {
        let path = std::env::temp_dir().join("enemy-speed-randomizer-bom-config-test.toml");
        let _ = fs::remove_file(&path);
        write_default_config(&path, &EnemySpeedRandomizerBomConfig::default());
        let text = fs::read_to_string(&path).unwrap();

        assert!(text.starts_with("author = \"梅琳娜的刀\"\n\n[speed]\n"));
        assert!(text.contains("enable = true"));
        assert!(text.contains("pool_1_min_percent = 50"));
        assert!(text.contains("pool_1_max_percent = 150"));
        assert!(text.contains("pool_2_min_percent = 50"));
        assert!(text.contains("pool_2_max_percent = 150"));
        assert!(text.contains("randomize_interval_min_ms = 3000"));
        assert!(text.contains("randomize_interval_max_ms = 7000"));
        assert!(text.contains("pool_1_weight = 50"));
        assert!(text.contains("pool_2_weight = 50"));
        assert!(text.contains("randomize_each_enemy = false"));
        assert!(text.contains("toggle_virtual_key = 114"));
        assert!(text.contains("[overlay]"));
        assert!(text.contains("position = \"top_left\""));
        assert!(text.contains("offset_x = 24.0"));
        assert!(text.contains("offset_y = 24.0"));
        assert!(text.contains("scale = 1.0"));
        assert!(text.contains("startup_delay_ms = 10000"));
        assert!(!text.contains("global"));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn paths_use_dll_directory() {
        let dll_path = Path::new(r"D:\Games\EldenRing\mods\enemy_speed_randomizer_bom.dll");
        let paths = mod_paths_from_dll_path(dll_path);

        assert_eq!(
            paths.config_path,
            PathBuf::from(r"D:\Games\EldenRing\mods\enemy_speed_randomizer_bom.toml")
        );
    }
}
