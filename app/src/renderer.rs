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
}

/// Linux: on a PC whose graphics cards are all NVIDIA, OpenGL loads only NVIDIA's driver.
/// The system's OpenGL loader (libglvnd) otherwise opens every installed one, and Mesa,
/// installed alongside on most systems, brings its LLVM: ~100 MB the app never uses
/// (measured: 369 MB → 269 MB). Left alone where any other card is present (a laptop's
/// Intel or AMD chip may be the one showing the window) or the user chose a driver.
/// Must run before any thread starts (it sets the environment) and before EGL loads.
pub fn only_used_gl_driver() {
    if !cfg!(target_os = "linux") || ["__EGL_VENDOR_LIBRARY_FILENAMES", "__EGL_VENDOR_LIBRARY_DIRS"].iter().any(|v| std::env::var_os(v).is_some()) {
        return;
    }
    let drivers: Vec<String> = std::fs::read_dir("/sys/class/drm")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| is_card(&e.file_name().to_string_lossy()))
        .filter_map(|e| std::fs::read_link(e.path().join("device/driver")).ok())
        .filter_map(|d| d.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect();
    if drivers.is_empty() || drivers.iter().any(|d| d != "nvidia") {
        return;
    }
    let Some(json) = ["/etc/glvnd/egl_vendor.d/10_nvidia.json", "/usr/share/glvnd/egl_vendor.d/10_nvidia.json"].into_iter().find(|p| Path::new(p).exists()) else {
        return;
    };
    log::info!("renderer: NVIDIA only; OpenGL loads only its driver ({json})");
    // SAFETY: called first thing in `main`, before any other thread exists.
    unsafe { std::env::set_var("__EGL_VENDOR_LIBRARY_FILENAMES", json) };
}

/// `card0`, `card1`…, not the connectors (`card0-DP-1`) or render nodes.
fn is_card(name: &str) -> bool {
    name.strip_prefix("card").is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

static ACTIVE: OnceLock<Renderer> = OnceLock::new();

/// The saved choice; wgpu falls back to OpenGL when no adapter for it answers, since a
/// window that fails to open can't be switched back from inside the app.
pub fn init(data_dir: &Path) -> Renderer {
    let mut r = load(data_dir);
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
        _ => Renderer::OpenGl,
    }
}

pub fn save(data_dir: &Path, r: Renderer) -> std::io::Result<()> {
    std::fs::write(data_dir.join("renderer"), if r == Renderer::Wgpu { "wgpu" } else { "opengl" })
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

#[cfg(test)]
mod tests {
    #[test]
    fn cards_not_connectors() {
        assert!(super::is_card("card0"));
        assert!(super::is_card("card12"));
        assert!(!super::is_card("card0-DP-1"));
        assert!(!super::is_card("renderD128"));
        assert!(!super::is_card("card"));
    }
}
