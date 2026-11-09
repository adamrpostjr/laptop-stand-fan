use std::collections::VecDeque;
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use eframe::egui;
use fanstand_ipc::{Link, Mode, Request, Response, Snapshot};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::icon::{self, Tone};
use crate::single::ShowSignal;

mod ui;

const POLL: Duration = Duration::from_secs(1);
const HISTORY: usize = 120; // samples ≈ 2 min at the service's 1 s cadence

#[derive(Debug)]
enum UiEvent {
    Show,
    Quit,
}

/// What the poller thread and command threads share with the UI.
#[derive(Default)]
struct State {
    snap: Option<Result<Snapshot, String>>,
    history: VecDeque<Sample>,
    last_sample_ms: u64,
    /// Result of the last command, shown under the controls for a while.
    cmd: Option<(String, bool, Instant)>,
    busy: bool,
}

#[derive(Clone, Copy)]
struct Sample {
    rpm: [u16; 2],
}

type Shared = Arc<Mutex<State>>;

fn lock(s: &Shared) -> MutexGuard<'_, State> {
    s.lock().unwrap_or_else(|p| p.into_inner())
}

pub fn run(minimized: bool, show_signal: ShowSignal) -> eframe::Result {
    let window_icon = egui::IconData {
        rgba: icon::rgba(64, Tone::Ok),
        width: 64,
        height: 64,
    };
    // Render with Direct3D 12 only. wgpu's default set includes OpenGL, and
    // merely probing it crashes inside some AMD OpenGL drivers (atio6axx.dll).
    // WGPU_BACKEND still overrides this for troubleshooting.
    let mut wgpu_options = eframe::egui_wgpu::WgpuConfiguration::default();
    if std::env::var_os("WGPU_BACKEND").is_none()
        && let eframe::egui_wgpu::WgpuSetup::CreateNew(new) = &mut wgpu_options.wgpu_setup
    {
        new.instance_descriptor.backends = eframe::wgpu::Backends::DX12;
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("FanStand")
            .with_inner_size([460.0, 640.0])
            .with_min_inner_size([420.0, 520.0])
            .with_icon(window_icon)
            .with_visible(!minimized),
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native(
        "FanStand",
        options,
        Box::new(move |cc| Ok(Box::new(FanApp::new(cc, !minimized, show_signal)?))),
    )
}

struct FanApp {
    shared: Shared,
    events: Receiver<UiEvent>,
    tray: TrayIcon,
    tray_tone: Tone,
    tray_tip: String,
    visible: bool,
    quitting: bool,
    /// Slider value; follows the stand unless the user is mid-change.
    slider: u8,
    slider_held_until: Option<Instant>,
    /// Fan glyph rotation per fan, and the clock that drives it.
    spin: [f32; 2],
    last_frame: Instant,
}

impl FanApp {
    fn new(
        cc: &eframe::CreationContext<'_>,
        visible: bool,
        show_signal: ShowSignal,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let ctx = cc.egui_ctx.clone();
        ui::apply_theme(&ctx);
        let shared: Shared = Arc::default();
        let (tx, events) = channel();

        // Tray (must be created on this, the event-loop thread).
        let menu = Menu::new();
        menu.append_items(&[
            &MenuItem::with_id("open", "Open FanStand", true, None),
            &MenuItem::with_id("auto", "Back to auto (fan curve)", true, None),
            &PredefinedMenuItem::separator(),
            &MenuItem::with_id("quit", "Quit", true, None),
        ])?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("FanStand")
            .with_icon(tray_icon::Icon::from_rgba(icon::rgba(32, Tone::Idle), 32, 32)?)
            .build()?;

        // Tray events arrive on this thread's message loop; forward them and
        // wake eframe (App::logic runs even while the window is hidden).
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                let show = matches!(
                    e,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick { .. }
                );
                if show {
                    let _ = tx.send(UiEvent::Show);
                    ctx.request_repaint();
                }
            }));
        }
        {
            let (tx, ctx, shared) = (tx.clone(), ctx.clone(), shared.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                match e.id.0.as_str() {
                    "open" => {
                        let _ = tx.send(UiEvent::Show);
                    }
                    "auto" => send(&shared, &ctx, Request::SetAuto),
                    "quit" => {
                        let _ = tx.send(UiEvent::Quit);
                    }
                    _ => {}
                }
                ctx.request_repaint();
            }));
        }

        // A second launch asks us to come forward.
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                loop {
                    show_signal.wait();
                    let _ = tx.send(UiEvent::Show);
                    ctx.request_repaint();
                }
            });
        }

        // Poll the service.
        {
            let (shared, ctx) = (shared.clone(), ctx.clone());
            std::thread::spawn(move || {
                loop {
                    poll_once(&shared);
                    ctx.request_repaint();
                    std::thread::sleep(POLL);
                }
            });
        }

        Ok(Self {
            shared,
            events,
            tray,
            tray_tone: Tone::Idle,
            tray_tip: String::new(),
            visible,
            quitting: false,
            slider: 50,
            slider_held_until: None,
            spin: [0.0, 0.8],
            last_frame: Instant::now(),
        })
    }

    fn update_tray(&mut self) {
        let (tone, tip) = {
            let st = lock(&self.shared);
            summarize(st.snap.as_ref())
        };
        if tone != self.tray_tone
            && let Ok(i) = tray_icon::Icon::from_rgba(icon::rgba(32, tone), 32, 32)
        {
            let _ = self.tray.set_icon(Some(i));
            self.tray_tone = tone;
        }
        if tip != self.tray_tip {
            let _ = self.tray.set_tooltip(Some(&tip));
            self.tray_tip = tip;
        }
    }
}

fn poll_once(shared: &Shared) {
    let r = match fanstand_ipc::call(&Request::Snapshot) {
        Ok(Response::Snapshot(s)) => Ok(*s),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(pipe_error(&e)),
    };
    let mut st = lock(shared);
    if let Ok(s) = &r
        && let Some(stand) = &s.stand
        && stand.updated_ms != st.last_sample_ms
    {
        st.last_sample_ms = stand.updated_ms;
        st.history.push_back(Sample {
            rpm: stand.rpm,
        });
        while st.history.len() > HISTORY {
            st.history.pop_front();
        }
    }
    st.snap = Some(r);
}

fn pipe_error(e: &std::io::Error) -> String {
    match e.raw_os_error() {
        Some(2) => "The FanStand service isn't running.".into(),
        Some(5) => "Access to the FanStand service was denied.".into(),
        _ => format!("Can't reach the FanStand service: {e}"),
    }
}

/// Fire a command at the service without blocking the UI.
fn send(shared: &Shared, ctx: &egui::Context, req: Request) {
    lock(shared).busy = true;
    let (shared, ctx) = (shared.clone(), ctx.clone());
    std::thread::spawn(move || {
        let what = match &req {
            Request::SetAuto => "Auto: following the fan curve".to_string(),
            Request::SetManual { pct } => format!("Manual: {pct}%"),
            Request::Reconnect => "Reconnecting…".to_string(),
            Request::Snapshot => String::new(),
        };
        let res = match fanstand_ipc::call(&req) {
            Ok(Response::Ok) => (what, false),
            Ok(Response::Error { message }) => (message, true),
            Ok(other) => (format!("unexpected reply: {other:?}"), true),
            Err(e) => (pipe_error(&e), true),
        };
        poll_once(&shared);
        {
            let mut st = lock(&shared);
            st.busy = false;
            st.cmd = Some((res.0, res.1, Instant::now()));
        }
        ctx.request_repaint();
    });
}

fn summarize(snap: Option<&Result<Snapshot, String>>) -> (Tone, String) {
    let Some(Ok(s)) = snap else {
        return (Tone::Idle, "FanStand: service not running".into());
    };
    let Some(st) = &s.stand else {
        let what = match s.link {
            Link::Suspended => "paused for sleep",
            _ => "searching for the stand…",
        };
        return (Tone::Idle, format!("FanStand: {what}"));
    };
    let tone = if st.stalled.iter().any(|&x| x) {
        Tone::Alarm
    } else if st.mode == Mode::Manual || st.failsafe {
        Tone::Attention
    } else {
        Tone::Ok
    };
    let cpu = s
        .cpu
        .map_or(String::new(), |c| format!(" · CPU {:.0} °C", c.celsius));
    let mode = match st.mode {
        Mode::Auto => "auto".to_string(),
        Mode::Manual => format!("manual {}%", st.manual_pct),
    };
    (
        tone,
        format!(
            "FanStand: {} / {} RPM · {:.0}% ({mode}){cpu}",
            st.rpm[0], st.rpm[1], st.duty_pct
        ),
    )
}

fn log_dir() -> std::path::PathBuf {
    std::env::var_os("ProgramData")
        .map_or_else(|| r"C:\ProgramData".into(), std::path::PathBuf::from)
        .join("fanstand")
        .join("logs")
}
