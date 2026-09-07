mod config;
mod speed_randomizer;

use eldenring::{
    cs::{CSTaskGroupIndex, CSTaskImp},
    fd4::FD4TaskData,
    util::system::wait_for_system_init,
};
use fromsoftware_shared::{FromStatic, program::Program, task::*};
use hudhook::hooks::dx12::ImguiDx12Hooks;
use hudhook::imgui::{
    Condition, Context, FontConfig, FontGlyphRanges, FontSource, StyleColor, StyleVar, Ui,
};
use hudhook::{Hudhook, ImguiRenderLoop, RenderContext, eject, windows};
use std::{
    net::{SocketAddr, ToSocketAddrs, UdpSocket},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::{
        EnemySpeedRandomizerBomConfig, OverlayConfig, OverlayPosition, config_modified_time,
        load_config, load_or_create_config, resolve_paths,
    },
    speed_randomizer::{EnemySpeedRandomizer, SpeedStatus},
};

const CONFIG_RELOAD_INTERVAL: Duration = Duration::from_secs(1);
const INPUT_CHECK_INTERVAL: Duration = Duration::from_millis(500);
const EXPIRATION_UNIX_SECONDS: u64 = 1_789_084_800; // 2026-09-11 00:00:00 UTC
const NTP_UNIX_OFFSET_SECONDS: u64 = 2_208_988_800;
const NTP_SERVERS: [&str; 2] = ["time.cloudflare.com:123", "pool.ntp.org:123"];

#[unsafe(no_mangle)]
/// # Safety
/// This entry point is called by Windows LoadLibrary. Do not call it directly.
pub unsafe extern "C" fn DllMain(hmodule: usize, reason: u32) -> bool {
    if reason == 0 {
        return true;
    }
    if reason != 1 {
        return true;
    }

    std::thread::spawn(move || {
        let Some(expires_at) = network_expiration_instant() else {
            return;
        };
        if Instant::now() >= expires_at {
            return;
        }

        let paths = resolve_paths(hmodule);
        let config = load_or_create_config(&paths.config_path);
        let shared = Arc::new(Mutex::new(OverlayShared {
            config: config.overlay.clone(),
            status: SpeedStatus::default(),
        }));

        if wait_for_system_init(&Program::current(), Duration::MAX).is_err() {
            return;
        }
        if Instant::now() >= expires_at {
            return;
        }

        let Ok(cs_task) = (unsafe { CSTaskImp::instance() }) else {
            return;
        };

        let startup_delay_ms = config.overlay.startup_delay_ms;
        let mut state = State::new(config, paths.config_path, Arc::clone(&shared), expires_at);
        state.publish_status();

        // Start the gameplay task immediately. Only the render hook is delayed:
        // the game DX12 device/swap chain may still be coming up after
        // FromSoftware system initialization is available.
        cs_task.run_recurring(
            move |_: &FD4TaskData| {
                let _ = catch_unwind(AssertUnwindSafe(|| state.tick()));
            },
            CSTaskGroupIndex::ChrIns_PostPhysics,
        );

        if startup_delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(startup_delay_ms));
        }
        if Instant::now() >= expires_at {
            return;
        }

        let hook_result = Hudhook::builder()
            .with::<ImguiDx12Hooks>(EnemySpeedOverlay {
                shared: Arc::clone(&shared),
            })
            .with_hmodule(windows::Win32::Foundation::HINSTANCE(hmodule as _))
            .build()
            .apply();
        if hook_result.is_err() {
            eject();
            return;
        }
    });

    true
}

struct OverlayShared {
    config: OverlayConfig,
    status: SpeedStatus,
}

struct EnemySpeedOverlay {
    shared: Arc<Mutex<OverlayShared>>,
}

impl ImguiRenderLoop for EnemySpeedOverlay {
    fn initialize(&mut self, ctx: &mut Context, _render: &mut dyn RenderContext) {
        ctx.style_mut().use_dark_colors();

        // The default ImGui bitmap font is very small. Scaling it up with
        // SetWindowFontScale makes the overlay look blurred/pixelated. Replace
        // it with a rasterized system font so scale=1.0 remains as large as the
        // old scale=2.5 overlay, but with substantially better glyph quality.
        if let Ok(font_data) = std::fs::read(r"C:\Windows\Fonts\simfang.ttf") {
            ctx.fonts().clear();
            ctx.fonts().add_font(&[FontSource::TtfData {
                data: &font_data,
                size_pixels: 32.0,
                config: Some(FontConfig {
                    glyph_ranges: FontGlyphRanges::chinese_simplified_common(),
                    ..FontConfig::default()
                }),
            }]);
        }
    }

    fn render(&mut self, ui: &mut Ui) {
        let (config, status) = match self.shared.lock() {
            Ok(shared) => (shared.config.clone(), shared.status),
            Err(poisoned) => {
                let shared = poisoned.into_inner();
                (shared.config.clone(), shared.status)
            }
        };

        if config.enable {
            draw_overlay(ui, &config, status);
        }
    }
}

fn network_expiration_instant() -> Option<Instant> {
    let network_time = fetch_network_time()?;
    let expiration = UNIX_EPOCH + Duration::from_secs(EXPIRATION_UNIX_SECONDS);
    let remaining = expiration.duration_since(network_time).ok()?;
    Instant::now().checked_add(remaining)
}

fn fetch_network_time() -> Option<SystemTime> {
    let request = [0x1b_u8; 48];

    for server in NTP_SERVERS {
        let Ok(addresses) = server.to_socket_addrs() else {
            continue;
        };

        for address in addresses {
            let bind_address = match address {
                SocketAddr::V4(_) => "0.0.0.0:0",
                SocketAddr::V6(_) => "[::]:0",
            };
            let Ok(socket) = UdpSocket::bind(bind_address) else {
                continue;
            };
            if socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .is_err()
                || socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .is_err()
            {
                continue;
            }
            if socket.send_to(&request, address).is_err() {
                continue;
            }

            let mut response = [0_u8; 48];
            let Ok((length, _)) = socket.recv_from(&mut response) else {
                continue;
            };
            if length < response.len() || response[1] == 0 || response[0] & 0x07 != 4 {
                continue;
            }

            let ntp_seconds = u32::from_be_bytes(response[40..44].try_into().ok()?) as u64;
            let ntp_fraction = u32::from_be_bytes(response[44..48].try_into().ok()?);
            let unix_seconds = ntp_seconds.checked_sub(NTP_UNIX_OFFSET_SECONDS)?;
            let nanoseconds = ((ntp_fraction as u64 * 1_000_000_000) >> 32) as u32;
            return Some(UNIX_EPOCH + Duration::new(unix_seconds, nanoseconds));
        }
    }

    None
}

fn draw_overlay(ui: &Ui, config: &OverlayConfig, status: SpeedStatus) {
    // Keep ImGui and the DX12 renderer in the same pixel coordinate space.
    unsafe {
        (*hudhook::imgui::sys::igGetIO()).DisplayFramebufferScale =
            hudhook::imgui::sys::ImVec2 { x: 1.0, y: 1.0 };
    }

    let screen = ui.io().display_size;
    if screen[0] <= 0.0 || screen[1] <= 0.0 {
        return;
    }

    // Config scale 1.0 keeps the previous practical 2.5 appearance.
    // Scale is applied to the explicit layout and font size together.
    let scale = config.scale.clamp(0.25, 4.0);
    let offset_x = config.offset_x.max(0.0);
    let offset_y = config.offset_y.max(0.0);
    let (position, pivot) = match config.position {
        OverlayPosition::TopLeft => ([offset_x, offset_y], [0.0, 0.0]),
        OverlayPosition::TopRight => ([screen[0] - offset_x, offset_y], [1.0, 0.0]),
        OverlayPosition::BottomLeft => ([offset_x, screen[1] - offset_y], [0.0, 1.0]),
        OverlayPosition::BottomRight => ([screen[0] - offset_x, screen[1] - offset_y], [1.0, 1.0]),
    };

    let padding = [12.0 * scale, 9.0 * scale];
    let window_padding = ui.push_style_var(StyleVar::WindowPadding(padding));
    let bg = ui.push_style_color(StyleColor::WindowBg, [0.035, 0.045, 0.07, 0.86]);
    let border = ui.push_style_var(StyleVar::WindowBorderSize(1.0 * scale));
    let rounding = ui.push_style_var(StyleVar::WindowRounding(8.0 * scale));

    ui.window("##enemy_speed_randomizer_overlay")
        .position(position, Condition::Always)
        .position_pivot(pivot)
        // SetWindowFontScale is applied after ImGui computes the content size.
        // Reserve the scaled rectangle explicitly so large text is not clipped.
        .size([560.0 * scale, 220.0 * scale], Condition::Always)
        .no_decoration()
        .no_inputs()
        .movable(false)
        .resizable(false)
        .focus_on_appearing(false)
        .bring_to_front_on_focus(false)
        .build(|| {
            ui.set_window_font_scale(scale);
            ui.text_colored([0.92, 0.94, 0.98, 1.0], "敌人速度随机 TomH");
            ui.separator();

            let state = if status.speed_enabled {
                "开启"
            } else {
                "关闭"
            };
            ui.text_colored([0.68, 0.74, 0.84, 1.0], format!("状态：{state}"));
            if status.individual_enemy_speed {
                ui.text_colored([0.56, 0.78, 1.0, 1.0], "当前速度：每个敌人独立随机");
                ui.text_colored([0.56, 0.78, 1.0, 1.0], "下次速度：每个敌人独立随机");
            } else {
                ui.text_colored(
                    [0.56, 0.78, 1.0, 1.0],
                    format!("当前速度：{:.2}x", status.multiplier),
                );
                let next_speed = status
                    .next_multiplier
                    .map(|multiplier| format!("{multiplier:.2}x"))
                    .unwrap_or_else(|| "--".to_string());
                ui.text_colored([0.56, 0.78, 1.0, 1.0], format!("下次速度：{next_speed}"));
            }
            ui.text_colored(
                [0.68, 0.74, 0.84, 1.0],
                format!("随机倒计时：{}", format_countdown(status.countdown_ms)),
            );
        });

    rounding.end();
    border.end();
    bg.end();
    window_padding.end();
}

fn format_countdown(countdown_ms: Option<u64>) -> String {
    let Some(countdown_ms) = countdown_ms else {
        return "--:--.--".to_string();
    };
    let total_seconds = countdown_ms / 1_000;
    let centiseconds = (countdown_ms % 1_000) / 10;
    format!(
        "{:02}:{:02}.{:02}",
        total_seconds / 60,
        total_seconds % 60,
        centiseconds
    )
}

#[cfg(test)]
mod tests {
    use super::format_countdown;

    #[test]
    fn countdown_is_formatted_to_centiseconds() {
        assert_eq!(format_countdown(Some(3_890)), "00:03.89");
        assert_eq!(format_countdown(Some(65_430)), "01:05.43");
        assert_eq!(format_countdown(None), "--:--.--");
    }
}

struct State {
    config_path: PathBuf,
    config_last_modified: Option<SystemTime>,
    last_config_check: Instant,
    randomizer: EnemySpeedRandomizer,
    overlay: Arc<Mutex<OverlayShared>>,
    expires_at: Instant,
    expired: bool,
}

impl State {
    fn new(
        config: EnemySpeedRandomizerBomConfig,
        config_path: PathBuf,
        overlay: Arc<Mutex<OverlayShared>>,
        expires_at: Instant,
    ) -> Self {
        Self {
            config_last_modified: config_modified_time(&config_path),
            config_path,
            last_config_check: Instant::now(),
            randomizer: EnemySpeedRandomizer::new(&config.speed, INPUT_CHECK_INTERVAL),
            overlay,
            expires_at,
            expired: false,
        }
    }

    fn tick(&mut self) {
        if self.expired || Instant::now() >= self.expires_at {
            self.expired = true;
            self.randomizer.disable();
            if let Ok(mut shared) = self.overlay.lock() {
                shared.config.enable = false;
                shared.status = SpeedStatus::default();
            }
            return;
        }

        self.reload_config_if_changed();
        self.randomizer.tick(INPUT_CHECK_INTERVAL);
        self.publish_status();
    }

    fn publish_status(&self) {
        let status = self.randomizer.status();
        if let Ok(mut shared) = self.overlay.lock() {
            shared.status = status;
        }
    }

    fn reload_config_if_changed(&mut self) {
        if self.last_config_check.elapsed() < CONFIG_RELOAD_INTERVAL {
            return;
        }
        self.last_config_check = Instant::now();
        let modified = config_modified_time(&self.config_path);
        if modified == self.config_last_modified {
            return;
        }
        let Some(config) = (if modified.is_none() {
            Some(load_or_create_config(&self.config_path))
        } else {
            load_config(&self.config_path)
        }) else {
            return;
        };
        self.randomizer.update_config(&config.speed);
        if let Ok(mut shared) = self.overlay.lock() {
            shared.config = config.overlay;
        }
        self.config_last_modified = config_modified_time(&self.config_path);
    }
}
