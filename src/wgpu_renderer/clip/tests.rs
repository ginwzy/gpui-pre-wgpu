#[cfg(target_os = "linux")]
mod tiles;

use super::Params;
use crate::wgpu_renderer::{GammaParams, GlobalParams, WgpuRenderer};
use gpui::{Bounds, ContentMask, Quad, ScaledPixels, point, rgb, size};
use std::{
    future::Future,
    sync::{Arc, mpsc},
    task::{Context, Poll, Wake, Waker},
};
use wgpu::util::DeviceExt;

struct ThreadWake(std::thread::Thread);
impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}
fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[test]
#[ignore = "requires a WGPU adapter (a software Vulkan adapter is sufficient)"]
fn composited_edges_have_one_coverage_after_overlapping_backgrounds() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
        .expect("WGPU adapter");
    eprintln!("rounded clip adapter: {:?}", adapter.get_info());
    let (device, queue) =
        block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).expect("WGPU device");
    let layouts = WgpuRenderer::create_bind_group_layouts(&device, false);
    let pipelines = WgpuRenderer::create_pipelines(
        &device,
        &layouts,
        wgpu::TextureFormat::Rgba8Unorm,
        wgpu::CompositeAlphaMode::PreMultiplied,
        1,
        false,
        false,
    );
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor::default());
    let buffer = |data: &[u8], usage| {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: None,
            contents: data,
            usage,
        })
    };
    let gamma = buffer(
        bytemuck::bytes_of(&GammaParams {
            gamma_ratios: [0.; 4],
            grayscale_enhanced_contrast: 0.,
            subpixel_enhanced_contrast: 0.,
            is_bgr: 0,
            _pad: 0,
        }),
        wgpu::BufferUsages::UNIFORM,
    );
    let globals = |size, origin| {
        let params = buffer(
            bytemuck::bytes_of(&GlobalParams {
                viewport_size: size,
                viewport_origin: origin,
                premultiplied_alpha: 1,
                pad: [0; 3],
            }),
            wgpu::BufferUsages::UNIFORM,
        );
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layouts.globals,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: gamma.as_entire_binding(),
                },
            ],
        })
    };
    for scale in [1.0_f32, 1.25, 2.] {
        let extent = (96. * scale) as u32;
        let x = (10. * scale).floor();
        let y = (20. * scale).floor();
        let width = (74. * scale).ceil() - x;
        let height = (84. * scale).ceil() - y;
        let original = Bounds::new(
            point(ScaledPixels(10. * scale), ScaledPixels(20. * scale)),
            size(ScaledPixels(64. * scale), ScaledPixels(64. * scale)),
        );
        let visible = Bounds::new(
            point(ScaledPixels(x), ScaledPixels(y)),
            size(ScaledPixels(width), ScaledPixels(height)),
        );
        let texture = |width, height| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba8Unorm,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            })
        };
        let source = texture(width as u32, height as u32);
        let source_view = source.create_view(&Default::default());
        let destination = texture(extent, extent);
        let destination_view = destination.create_view(&Default::default());
        let source_globals = globals([width, height], [x, y]);
        let destination_globals = globals([extent as f32; 2], [0.; 2]);
        let quads: Vec<_> = [0xff0000, 0x0000ff]
            .into_iter()
            .map(|color| Quad {
                bounds: visible,
                content_mask: ContentMask { bounds: visible },
                background: rgb(color).into(),
                ..Default::default()
            })
            .collect();
        let instances = buffer(
            unsafe { WgpuRenderer::instance_bytes(&quads) },
            wgpu::BufferUsages::STORAGE,
        );
        let instances = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layouts.instances,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: instances.as_entire_binding(),
            }],
        });
        let params = buffer(
            bytemuck::bytes_of(&Params {
                bounds: visible.into(),
                clip_bounds: original.into(),
                radii: [16. * scale; 4],
            }),
            wgpu::BufferUsages::UNIFORM,
        );
        let params = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layouts.composite,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: params.as_entire_binding(),
            }],
        });
        let sampled = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &layouts.texture,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&source_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });
        let row_bytes = (extent * 4).next_multiple_of(256);
        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: u64::from(row_bytes * extent),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut outputs = Vec::new();
        for first_instance in [1, 0] {
            let mut encoder = device.create_command_encoder(&Default::default());
            {
                let attachments = [Some(wgpu::RenderPassColorAttachment {
                    view: &source_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })];
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &attachments,
                    ..Default::default()
                });
                pass.set_pipeline(&pipelines.quads);
                pass.set_bind_group(0, &source_globals, &[]);
                pass.set_bind_group(1, &instances, &[]);
                pass.draw(0..4, first_instance..2);
            }
            {
                let attachments = [Some(wgpu::RenderPassColorAttachment {
                    view: &destination_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })];
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &attachments,
                    ..Default::default()
                });
                pass.set_pipeline(&pipelines.composite);
                pass.set_bind_group(0, &destination_globals, &[]);
                pass.set_bind_group(1, &params, &[]);
                pass.set_bind_group(2, &sampled, &[]);
                pass.draw(0..4, 0..1);
            }
            encoder.copy_texture_to_buffer(
                destination.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row_bytes),
                        rows_per_image: None,
                    },
                },
                wgpu::Extent3d {
                    width: extent,
                    height: extent,
                    depth_or_array_layers: 1,
                },
            );
            queue.submit([encoder.finish()]);
            let (tx, rx) = mpsc::channel();
            readback
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    tx.send(result).unwrap();
                });
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: Some(std::time::Duration::from_secs(30)),
                })
                .unwrap();
            rx.recv().unwrap().unwrap();
            let pixels = readback.slice(..).get_mapped_range().to_vec();
            readback.unmap();
            let pixel = |x: u32, y: u32| {
                &pixels[(y * row_bytes + x * 4) as usize..(y * row_bytes + x * 4 + 4) as usize]
            };
            assert_eq!(
                pixel((40. * scale) as u32, (50. * scale) as u32),
                [0, 0, 255, 255]
            );
            assert_eq!(pixel((10. * scale) as u32, (20. * scale) as u32), [0; 4]);
            assert_eq!(pixel(0, 0), [0; 4]);
            assert!(
                pixels.chunks_exact(4).any(|p| p[3] > 0 && p[3] < 255),
                "rounded edge needs fractional coverage"
            );
            assert!(
                pixels
                    .chunks_exact(4)
                    .all(|p| p[0] == 0 && p[1] == 0 && p[2] == p[3]),
                "edge must remain premultiplied blue without a dark fringe"
            );
            outputs.push(pixels);
        }
        assert_eq!(
            outputs[0], outputs[1],
            "overlapping backgrounds must not accumulate edge coverage at scale {scale}"
        );
    }
}
