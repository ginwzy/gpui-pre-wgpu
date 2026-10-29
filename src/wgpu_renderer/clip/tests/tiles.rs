use super::super::Target;
use crate::wgpu_renderer::{GammaParams, GlobalParams, WgpuRenderer, WgpuSurfaceConfig};
use gpui::{
    Bounds, ContentMask, Corners, DevicePixels, FontId, GlyphId, PathBuilder, PlatformAtlas, Quad,
    RenderGlyphParams, ScaledPixels, Scene, SubpixelSprite, TransformationMatrix, point, px, rgb,
    size, test::with_scene_clip,
};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, WindowHandle,
};
use std::{
    borrow::Cow,
    cell::RefCell,
    ffi::{c_char, c_ulong, c_void},
    ptr::NonNull,
    rc::Rc,
    sync::{Arc, mpsc},
    time::Instant,
};

#[link(name = "X11")]
unsafe extern "C" {
    fn XInitThreads() -> i32;
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XDefaultRootWindow(display: *mut c_void) -> c_ulong;
    fn XCreateSimpleWindow(
        display: *mut c_void,
        parent: c_ulong,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
        border: u32,
        border_pixel: c_ulong,
        background: c_ulong,
    ) -> c_ulong;
    fn XDestroyWindow(display: *mut c_void, window: c_ulong) -> i32;
    fn XCloseDisplay(display: *mut c_void) -> i32;
    fn XSync(display: *mut c_void, discard: i32) -> i32;
}

#[derive(Debug)]
struct XWindow {
    display: NonNull<c_void>,
    window: c_ulong,
}
// XInitThreads enables Xlib's internal serialization before the connection opens.
unsafe impl Send for XWindow {}
unsafe impl Sync for XWindow {}
impl XWindow {
    fn new() -> Arc<Self> {
        unsafe {
            assert_ne!(XInitThreads(), 0);
            let display = NonNull::new(XOpenDisplay(std::ptr::null()))
                .expect("test needs DISPLAY (Xvfb is sufficient)");
            let window = XCreateSimpleWindow(
                display.as_ptr(),
                XDefaultRootWindow(display.as_ptr()),
                0,
                0,
                64,
                64,
                0,
                0,
                0,
            );
            XSync(display.as_ptr(), 0);
            // Never mapped: pixel tests do not interact with the user's desktop.
            Arc::new(Self { display, window })
        }
    }
}
impl Drop for XWindow {
    fn drop(&mut self) {
        unsafe {
            XDestroyWindow(self.display.as_ptr(), self.window);
            XCloseDisplay(self.display.as_ptr());
        }
    }
}
impl HasWindowHandle for XWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        Ok(unsafe {
            WindowHandle::borrow_raw(raw_window_handle::XlibWindowHandle::new(self.window).into())
        })
    }
}
impl HasDisplayHandle for XWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        Ok(unsafe {
            DisplayHandle::borrow_raw(
                raw_window_handle::XlibDisplayHandle::new(Some(self.display), 0).into(),
            )
        })
    }
}

struct Harness {
    renderer: WgpuRenderer,
    _window: Arc<XWindow>,
}
fn rect(x: f32, y: f32, w: f32, h: f32) -> Bounds<ScaledPixels> {
    Bounds::new(
        point(ScaledPixels(x), ScaledPixels(y)),
        size(ScaledPixels(w), ScaledPixels(h)),
    )
}
impl Harness {
    fn new(width: u32, height: u32) -> Self {
        let window = XWindow::new();
        let renderer = WgpuRenderer::new(
            Rc::new(RefCell::new(None)),
            &window,
            WgpuSurfaceConfig {
                size: size(DevicePixels(width as i32), DevicePixels(height as i32)),
                transparent: false,
                preferred_present_mode: None,
            },
            None,
        )
        .unwrap();
        eprintln!("tile test adapter: {:?}", renderer.adapter_info);
        Self {
            renderer,
            _window: window,
        }
    }

    fn texture(&self) -> wgpu::Texture {
        let r = &self.renderer;
        let resources = r.resources();
        let (w, h) = (r.surface_config.width, r.surface_config.height);
        let texture = resources.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("clip_test"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: r.surface_config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        texture
    }

    fn submit(&mut self, scene: &Scene, tiled: bool, texture: &wgpu::Texture) {
        let r = &mut self.renderer;
        let (w, h) = (r.surface_config.width, r.surface_config.height);
        if scene.has_paths() {
            r.ensure_intermediate_textures();
        }
        r.atlas.before_frame();
        let resources = r.resources();
        let globals = GlobalParams {
            viewport_size: [w as f32, h as f32],
            viewport_origin: [0.; 2],
            premultiplied_alpha: u32::from(
                r.surface_config.alpha_mode == wgpu::CompositeAlphaMode::PreMultiplied,
            ),
            pad: [0; 3],
        };
        resources
            .queue
            .write_buffer(&resources.globals_buffer, 0, bytemuck::bytes_of(&globals));
        resources.queue.write_buffer(
            &resources.globals_buffer,
            r.path_globals_offset,
            bytemuck::bytes_of(&GlobalParams {
                premultiplied_alpha: 0,
                ..globals
            }),
        );
        resources.queue.write_buffer(
            &resources.globals_buffer,
            r.gamma_offset,
            bytemuck::bytes_of(&GammaParams {
                gamma_ratios: r.rendering_params.gamma_ratios,
                grayscale_enhanced_contrast: r.rendering_params.grayscale_enhanced_contrast,
                subpixel_enhanced_contrast: r.rendering_params.subpixel_enhanced_contrast,
                is_bgr: 0,
                _pad: 0,
            }),
        );
        let target = Target {
            view: texture.create_view(&Default::default()),
            globals: resources.globals_bind_group.clone(),
            path_globals: resources.path_globals_bind_group.clone(),
            paths: resources.path_intermediate_view.clone(),
            path_msaa: resources.path_msaa_view.clone(),
            isolated: false,
            bounds: rect(0., 0., w as f32, h as f32),
        };
        let mut encoder = resources.device.create_command_encoder(&Default::default());
        let mut offset = 0;
        let bindings = r.prepare_bindings(scene, &mut offset).unwrap();
        let mut surfaces = 0;
        if tiled {
            r.record_scene(
                scene,
                &bindings,
                &target,
                &[target.bounds],
                true,
                &mut encoder,
                &mut surfaces,
            )
            .unwrap();
        } else {
            // Original full-surface algorithm, for a scene with one clipped subtree.
            assert_eq!(scene.composites.len(), 1);
            let composite = &scene.composites[0];
            let visible = composite
                .bounds()
                .intersect(&composite.content_mask().bounds)
                .intersect(&target.bounds);
            let bounds = rect(
                visible.left().0.floor(),
                visible.top().0.floor(),
                visible.right().0.ceil() - visible.left().0.floor(),
                visible.bottom().0.ceil() - visible.top().0.floor(),
            );
            let prepared = r.prepare_composite(composite, bounds, 0);
            surfaces = 1;
            r.record_scene(
                composite.scene(),
                &bindings.children[0],
                &prepared.target,
                &[bounds],
                true,
                &mut encoder,
                &mut surfaces,
            )
            .unwrap();
            let mut pass = target.pass(&mut encoder, true);
            pass.set_pipeline(&r.target_pipelines(&target).composite);
            pass.set_bind_group(0, &target.globals, &[]);
            pass.set_bind_group(1, &prepared.params, &[]);
            pass.set_bind_group(2, &prepared.texture, &[]);
            pass.draw(0..4, 0..1);
        }
        r.resources_mut().clip_surfaces.truncate(surfaces);
        r.resources().queue.submit([encoder.finish()]);
    }

    fn render(&mut self, scene: &Scene, tiled: bool) -> Vec<u8> {
        let texture = self.texture();
        self.submit(scene, tiled, &texture);
        let r = &mut self.renderer;
        let (w, h) = (r.surface_config.width, r.surface_config.height);
        let mut encoder = r
            .resources()
            .device
            .create_command_encoder(&Default::default());
        let row_bytes = (w * 4).next_multiple_of(256);
        let readback = r.resources().device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(row_bytes * h),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row_bytes),
                    rows_per_image: None,
                },
            },
            texture.size(),
        );
        r.resources().queue.submit([encoder.finish()]);
        let (tx, rx) = mpsc::channel();
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |v| tx.send(v).unwrap());
        r.resources()
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();
        rx.recv().unwrap().unwrap();
        let error = r.last_error.lock().unwrap().take();
        assert!(error.is_none(), "GPU validation: {error:?}");
        let data = readback.slice(..).get_mapped_range();
        let pixels = data
            .chunks(row_bytes as usize)
            .flat_map(|row| row[..w as usize * 4].iter().copied())
            .collect();
        drop(data);
        readback.unmap();
        pixels
    }
}

fn paint(scene: &mut Scene, bounds: Bounds<ScaledPixels>, color: gpui::Hsla) {
    scene.insert_primitive(Quad {
        bounds,
        content_mask: ContentMask { bounds },
        background: color.into(),
        ..Default::default()
    });
}

#[test]
#[ignore = "requires X11 and a WGPU adapter"]
fn tiles_match_full_compositing_with_fractional_edges_paths_and_ancestor_clips() {
    let mut harness = Harness::new(128, 128);
    for scale in [1., 1.25, 2.] {
        for offset in [0., 0.3, 0.75] {
            let bounds = rect(
                9. * scale + offset,
                13. * scale + offset,
                43. * scale,
                47. * scale,
            );
            for mask in [rect(0., 0., 128., 128.), rect(20., 30., 65., 60.)] {
                let mut scene = Scene::default();
                with_scene_clip(
                    &mut scene,
                    "clip",
                    bounds,
                    Corners {
                        top_left: ScaledPixels(8. * scale),
                        top_right: ScaledPixels(0.),
                        bottom_left: ScaledPixels(4. * scale),
                        bottom_right: ScaledPixels(12. * scale),
                    },
                    ContentMask { bounds: mask },
                    |s| {
                        paint(
                            s,
                            bounds,
                            gpui::Hsla {
                                h: 0.,
                                s: 1.,
                                l: 0.5,
                                a: 0.35,
                            },
                        );
                        paint(
                            s,
                            bounds,
                            gpui::Hsla {
                                h: 0.6,
                                s: 1.,
                                l: 0.5,
                                a: 0.6,
                            },
                        );
                        let mut path = PathBuilder::fill();
                        path.move_to(point(px(0.), px(0.)));
                        path.line_to(point(px(110.), px(85.)));
                        path.line_to(point(px(0.), px(128.)));
                        path.close();
                        let mut path = path.build().unwrap().scale(1.);
                        path.color = rgb(0x00ff00).into();
                        path.content_mask = ContentMask { bounds };
                        s.insert_primitive(path);
                    },
                );
                scene.finish();
                let expected = harness.render(&scene, false);
                let actual = harness.render(&scene, true);
                let worst = actual
                    .iter()
                    .zip(&expected)
                    .map(|(a, b)| a.abs_diff(*b))
                    .max()
                    .unwrap();
                if worst > 1 {
                    let examples: Vec<_> = actual
                        .chunks_exact(4)
                        .zip(expected.chunks_exact(4))
                        .enumerate()
                        .filter(|(_, (a, b))| a.iter().zip(*b).any(|(a, b)| a.abs_diff(*b) > 1))
                        .take(12)
                        .map(|(i, (a, b))| (i % 128, i / 128, a, b))
                        .collect();
                    panic!(
                        "pixel mismatch {worst}, scale={scale} offset={offset} mask={mask:?}: {examples:?}"
                    );
                }
            }
        }
    }
}

#[test]
#[ignore = "requires X11 and dual-source WGPU blending"]
fn rounded_and_nested_interiors_preserve_lcd_coverage_and_paint_order() {
    let mut harness = Harness::new(128, 128);
    assert!(
        harness.renderer.dual_source_blending,
        "adapter must support LCD blending for this test"
    );
    let key = RenderGlyphParams {
        font_id: FontId(0),
        glyph_id: GlyphId(0),
        font_size: px(15.),
        subpixel_variant: point(0, 0),
        scale_factor: 1.,
        is_emoji: false,
        subpixel_rendering: true,
        dilation: 0,
    }
    .into();
    let tile = harness
        .renderer
        .atlas
        .get_or_insert_with(&key, &mut || {
            Ok(Some((
                size(DevicePixels(4), DevicePixels(4)),
                Cow::Owned([64, 128, 192, 255].repeat(16)),
            )))
        })
        .unwrap()
        .unwrap();
    let bounds = rect(8., 8., 112., 112.);
    let glyph = SubpixelSprite {
        order: 0,
        pad: 0,
        bounds: rect(48., 48., 4., 4.),
        content_mask: ContentMask { bounds },
        color: rgb(0x202020).into(),
        tile,
        transformation: TransformationMatrix::unit(),
    };
    let body = |s: &mut Scene| {
        paint(s, bounds, rgb(0xfafafa).into());
        s.insert_primitive(glyph);
        s.insert_primitive(SubpixelSprite {
            bounds: rect(9., 9., 4., 4.),
            transformation: TransformationMatrix::unit()
                .translate(point(ScaledPixels(51.), ScaledPixels(51.))),
            ..glyph
        });
    };
    let mut plain = Scene::default();
    body(&mut plain);
    plain.finish();
    let direct = harness.render(&plain, true);
    for nested in [false, true] {
        let mut scene = Scene::default();
        with_scene_clip(
            &mut scene,
            "outer",
            bounds,
            Corners::all(ScaledPixels(16.)),
            ContentMask { bounds },
            |s| {
                if nested {
                    with_scene_clip(
                        s,
                        "inner",
                        bounds,
                        Corners::all(ScaledPixels(24.)),
                        ContentMask { bounds },
                        body,
                    );
                } else {
                    body(s);
                }
            },
        );
        // A later primitive must still cover the subtree, even when its batches are split.
        paint(&mut scene, rect(64., 32., 8., 8.), rgb(0x00ff00).into());
        scene.finish();
        let actual = harness.render(&scene, true);
        let pixel = |data: &[u8], x: usize, y: usize| {
            <[u8; 4]>::try_from(&data[(y * 128 + x) * 4..(y * 128 + x + 1) * 4]).unwrap()
        };
        let lcd = pixel(&actual, 49, 49);
        assert_eq!(lcd, pixel(&direct, 49, 49));
        assert_eq!(
            pixel(&actual, 61, 61),
            pixel(&direct, 61, 61),
            "culling must retain translated glyphs"
        );
        assert_ne!(pixel(&actual, 61, 61)[0], pixel(&actual, 61, 61)[2]);
        assert_ne!(
            lcd[0], lcd[2],
            "LCD channel coverage must survive in the interior"
        );
        assert_eq!(pixel(&actual, 65, 33), [0, 255, 0, 255]);
        assert_eq!(pixel(&actual, 8, 8), [0; 4]);
    }
}

#[test]
#[ignore = "4K GPU timing report; requires X11 and a WGPU adapter"]
fn clip_tiles_4k_measurement() {
    let mut harnesses = [Harness::new(3840, 2160), Harness::new(3840, 2160)];
    let bounds = rect(0., 0., 3840., 2160.);
    let mut scene = Scene::default();
    with_scene_clip(
        &mut scene,
        "window",
        bounds,
        Corners::all(ScaledPixels(16.)),
        ContentMask { bounds },
        |s| {
            paint(s, bounds, rgb(0xfafafa).into());
            for y in 0..60 {
                for x in 0..80 {
                    paint(
                        s,
                        rect(x as f32 * 48., y as f32 * 36., 32., 18.),
                        rgb(0x343434).into(),
                    );
                }
            }
        },
    );
    scene.finish();
    // Keep independent resource pools warm; alternating one renderer would measure
    // reallocating between full-sized and corner textures, not steady rendering.
    let textures = [harnesses[0].texture(), harnesses[1].texture()];
    let mut timings = [Vec::new(), Vec::new()];
    for sample in 0..90 {
        let order = if sample % 2 == 0 { [0, 1] } else { [1, 0] };
        for ix in order {
            let start = Instant::now();
            harnesses[ix].submit(&scene, ix == 1, &textures[ix]);
            harnesses[ix]
                .renderer
                .resources()
                .device
                .poll(wgpu::PollType::wait_indefinitely())
                .unwrap();
            if sample >= 10 {
                timings[ix].push(start.elapsed().as_secs_f64() * 1000.);
            }
        }
    }
    for (ix, times) in timings.iter_mut().enumerate() {
        times.sort_by(f64::total_cmp);
        let bytes: u64 = harnesses[ix]
            .renderer
            .resources()
            .clip_surfaces
            .iter()
            .flatten()
            .map(|s| u64::from(s.size.0) * u64::from(s.size.1) * 4)
            .sum();
        eprintln!(
            "4K tiled={}: median={:.3}ms p95={:.3}ms isolated_bytes={bytes} samples=80 (alternating warmed renderers; encoding + GPU completion, no readback/present)",
            ix == 1,
            times[40],
            times[75]
        );
    }
}
