//! Rounded window corners where the app draws its own frame on Linux: the window is
//! see-through, and once everything is drawn its four corners are cut away, so the desktop
//! shows past a rounded edge (Windows 11 rounds the app's windows itself). OpenGL only:
//! the wgpu renderer leaves them square.
//!
//! The cut is a last draw over the window that keeps what's inside the rounded rectangle
//! and clears what's outside it (`dst *= coverage`), anti-aliased over a pixel.

use std::sync::{Arc, Mutex, OnceLock};

use eframe::glow::{self, HasContext};

const VERTEX: &str = r#"
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"
precision highp float;
uniform vec2 u_size;
uniform float u_radius;
out vec4 frag_color;
void main() {
  // Distance outside the rounded rectangle, in pixels (negative inside).
  vec2 q = abs(gl_FragCoord.xy - u_size * 0.5) - (u_size * 0.5 - vec2(u_radius));
  float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - u_radius;
  frag_color = vec4(0.0, 0.0, 0.0, clamp(0.5 - d, 0.0, 1.0));
}
"#;

struct Cut {
    program: glow::Program,
    size: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    vao: glow::VertexArray,
}

/// The corners' radius, in points.
pub const RADIUS: f32 = 12.0;

static CUT: OnceLock<Mutex<Option<Cut>>> = OnceLock::new();

/// Make the program, once per run. Failing leaves corners square.
pub fn install(gl: &glow::Context) {
    match make(gl) {
        Ok(c) => {
            let _ = CUT.set(Mutex::new(Some(c)));
        }
        Err(e) => log::warn!("rounded window corners unavailable: {e:#}"),
    }
}

/// Whether corners can be cut: the window has to be see-through for it.
pub fn available() -> bool {
    CUT.get().is_some_and(|c| c.lock().unwrap().is_some())
}

fn make(gl: &glow::Context) -> anyhow::Result<Cut> {
    let version = eframe::egui_glow::ShaderVersion::get(gl);
    anyhow::ensure!(version.is_new_shader_interface(), "needs a newer OpenGL");
    let header = version.version_declaration();
    unsafe {
        let program = gl.create_program().map_err(anyhow::Error::msg)?;
        let mut shaders = Vec::new();
        for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
            let s = gl.create_shader(kind).map_err(anyhow::Error::msg)?;
            gl.shader_source(s, &format!("{header}\n{src}"));
            gl.compile_shader(s);
            anyhow::ensure!(gl.get_shader_compile_status(s), "corner shader: {}", gl.get_shader_info_log(s));
            gl.attach_shader(program, s);
            shaders.push(s);
        }
        gl.link_program(program);
        anyhow::ensure!(gl.get_program_link_status(program), "corner link: {}", gl.get_program_info_log(program));
        for s in shaders {
            gl.detach_shader(program, s);
            gl.delete_shader(s);
        }
        let vao = gl.create_vertex_array().map_err(anyhow::Error::msg)?;
        Ok(Cut { size: gl.get_uniform_location(program, "u_size"), radius: gl.get_uniform_location(program, "u_radius"), vao, program })
    }
}

/// Cut the window's corners to `radius` points, after everything else this frame: on the
/// topmost layer, so it's drawn last.
pub fn cut(ctx: &egui::Context, radius: f32) {
    if !available() || radius <= 0.0 {
        return;
    }
    let rect = ctx.content_rect();
    let layer = egui::LayerId::new(egui::Order::Debug, egui::Id::unique("window-corners"));
    let cb = eframe::egui_glow::CallbackFn::new(move |info, _painter| {
        let Some(lock) = CUT.get() else { return };
        let guard = lock.lock().unwrap();
        let Some(c) = guard.as_ref() else { return };
        let gl = _painter.gl();
        let [w, h] = info.screen_size_px.map(|v| v as f32);
        unsafe {
            gl.viewport(0, 0, w as i32, h as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.enable(glow::BLEND);
            // Keep the destination as far as the shape covers it, colour and alpha alike.
            gl.blend_func_separate(glow::ZERO, glow::SRC_ALPHA, glow::ZERO, glow::SRC_ALPHA);
            gl.use_program(Some(c.program));
            gl.uniform_2_f32(c.size.as_ref(), w, h);
            gl.uniform_1_f32(c.radius.as_ref(), radius * info.pixels_per_point);
            gl.bind_vertex_array(Some(c.vao));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            // egui's own blending, which the painter expects back.
            gl.blend_func_separate(glow::ONE, glow::ONE_MINUS_SRC_ALPHA, glow::ONE_MINUS_DST_ALPHA, glow::ONE);
        }
    });
    ctx.layer_painter(layer).add(egui::PaintCallback { rect, callback: Arc::new(cb) });
}
