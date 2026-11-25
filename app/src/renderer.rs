//! The window is drawn with OpenGL. (Until v0.4.3 Settings › Window offered wgpu, i.e.
//! Vulkan, as well: for a 2D UI that redraws a few batches of triangles it saved no
//! processor time and cost ~75 MB more memory and several times the video memory.)

use std::path::Path;

/// The Vulkan choice was kept in `<data dir>/renderer`: gone with the option.
pub fn forget_old_choice(data_dir: &Path) {
    let file = data_dir.join("renderer");
    if file.exists()
        && let Err(e) = std::fs::remove_file(&file)
    {
        log::warn!("couldn't remove the old renderer choice: {e}");
    }
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
