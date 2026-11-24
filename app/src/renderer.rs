//! Which GPU API draws the window. OpenGL unless the user picks wgpu (Vulkan; Metal on
//! macOS) in Settings › Window. The UI is a few thousand 2D triangles that only redraw
//! when something changes, so wgpu brings no visible speedup here, and it was measured
//! at about three times the memory and five times the video memory (issue #42). It's
//! there for drivers whose OpenGL misdraws, or for whoever prefers it.

use std::path::Path;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    OpenGl,
    Wgpu,
    /// Drawn on the processor, for the least memory: no GPU driver is loaded. Mesa's
    /// llvmpipe through OpenGL on Linux, Windows' own WARP through DirectX 12 on Windows.
    /// The animated background is left out (it would cost the processor too much).
    Software,
}

static ACTIVE: OnceLock<Renderer> = OnceLock::new();

/// The saved choice; wgpu falls back to OpenGL when no adapter for it answers, since a
/// window that fails to open can't be switched back from inside the app.
pub fn init(data_dir: &Path) -> Renderer {
    let mut r = load(data_dir);
    if r == Renderer::Software {
        log::info!("renderer: software");
    }
    if r == Renderer::Wgpu && !available() {
        log::warn!("renderer: no {} adapter; drawing with OpenGL", api_name());
        r = Renderer::OpenGl;
    }
    let _ = ACTIVE.set(r);
    r
}

/// The renderer this run draws with.
pub fn active() -> Renderer {
    *ACTIVE.get_or_init(|| Renderer::OpenGl)
}

pub fn load(data_dir: &Path) -> Renderer {
    match std::fs::read_to_string(data_dir.join("renderer")).unwrap_or_default().trim() {
        "wgpu" => Renderer::Wgpu,
        "software" => Renderer::Software,
        _ => Renderer::OpenGl,
    }
}

pub fn save(data_dir: &Path, r: Renderer) -> std::io::Result<()> {
    std::fs::write(data_dir.join("renderer"), match r {
        Renderer::Wgpu => "wgpu",
        Renderer::Software => "software",
        Renderer::OpenGl => "opengl",
    })
}

/// eframe's renderer for `r`: software is OpenGL on Linux (Mesa) and wgpu on Windows (WARP).
pub fn eframe_renderer(r: Renderer) -> eframe::Renderer {
    match r {
        Renderer::OpenGl => eframe::Renderer::Glow,
        Renderer::Wgpu => eframe::Renderer::Wgpu,
        Renderer::Software if cfg!(windows) => eframe::Renderer::Wgpu,
        Renderer::Software => eframe::Renderer::Glow,
    }
}

/// Linux, software rendering: OpenGL through Mesa's llvmpipe, whatever the graphics card.
/// Set before anything starts (the environment is read when the window's GL loads).
pub fn prepare(data_dir: &Path) {
    if cfg!(target_os = "linux") && load(data_dir) == Renderer::Software {
        // SAFETY: called first thing in `main`, before any other thread exists.
        unsafe {
            std::env::set_var("LIBGL_ALWAYS_SOFTWARE", "1");
            std::env::set_var("__GLX_VENDOR_LIBRARY_NAME", "mesa");
            let mesa = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";
            if Path::new(mesa).exists() {
                std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", mesa);
            }
        }
    }
}

/// The API wgpu draws through here.
pub fn api_name() -> &'static str {
    if cfg!(target_os = "macos") { "Metal" } else { "Vulkan" }
}

pub fn backends() -> wgpu::Backends {
    if cfg!(target_os = "macos") { wgpu::Backends::METAL } else { wgpu::Backends::VULKAN }
}

/// eframe's wgpu settings: only [`backends`], so "Vulkan" never quietly means DX12 or GL.
/// And memory over speed (#154): wgpu's default sets aside big blocks of video memory up
/// front, and eframe's keeps three window-sized images in flight. At a Retina window's
/// size that was over 1.2 GB of video memory, which a Mac counts as the app's own; the
/// UI needs neither.
pub fn wgpu_options() -> eframe::egui_wgpu::WgpuConfiguration {
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.instance_descriptor.backends = backends();
    // Windows, software rendering: DirectX 12's WARP, the processor adapter Windows ships.
    if cfg!(windows) && active() == Renderer::Software {
        setup.instance_descriptor.backends = wgpu::Backends::DX12;
        setup.native_adapter_selector = Some(std::sync::Arc::new(|adapters: &[wgpu::Adapter], _surface: Option<&wgpu::Surface<'_>>| {
            adapters.iter().find(|a| a.get_info().device_type == wgpu::DeviceType::Cpu).cloned().ok_or_else(|| "no WARP adapter".to_owned())
        }));
    }
    let device = setup.device_descriptor.clone();
    setup.device_descriptor = std::sync::Arc::new(move |adapter| wgpu::DeviceDescriptor { memory_hints: wgpu::MemoryHints::MemoryUsage, ..device(adapter) });
    eframe::egui_wgpu::WgpuConfiguration {
        surface: eframe::egui_wgpu::SurfaceConfig::LOW_LATENCY,
        wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

fn available() -> bool {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = backends();
    let instance = wgpu::Instance::new(desc);
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().build() else { return false };
    let adapter = rt.block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()));
    adapter.is_ok()
}
