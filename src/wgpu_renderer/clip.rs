use super::{GammaParams, GlobalParams, InstanceBindings, PodBounds, WgpuPipelines, WgpuRenderer};
use bytemuck::{Pod, Zeroable};
use gpui::{Bounds, Composite, ScaledPixels};
use std::num::NonZeroU64;
use wgpu::util::DeviceExt;

#[derive(Clone)]
pub(super) struct Target {
    pub view: wgpu::TextureView,
    pub globals: wgpu::BindGroup,
    pub path_globals: wgpu::BindGroup,
    pub paths: Option<wgpu::TextureView>,
    pub path_msaa: Option<wgpu::TextureView>,
    pub isolated: bool,
    pub bounds: Bounds<ScaledPixels>,
}

pub(super) struct Bindings {
    pub instances: InstanceBindings,
    pub batches: Vec<gpui::PrimitiveBatch>,
    pub paths: Vec<(usize, super::PathBindings)>,
    pub children: Vec<Bindings>,
}

impl Target {
    pub fn pass<'a>(
        &self,
        encoder: &'a mut wgpu::CommandEncoder,
        clear: bool,
    ) -> wgpu::RenderPass<'a> {
        encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("scene"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: if clear {
                        wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT)
                    } else {
                        wgpu::LoadOp::Load
                    },
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            ..Default::default()
        })
    }

    pub fn scissor(&self, bounds: Bounds<ScaledPixels>, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_scissor_rect(
            (bounds.left() - self.bounds.left()).0 as u32,
            (bounds.top() - self.bounds.top()).0 as u32,
            bounds.size.width.0 as u32,
            bounds.size.height.0 as u32,
        );
    }
}

pub(super) struct Surface {
    size: (u32, u32),
    _texture: wgpu::Texture,
    uniforms: wgpu::Buffer,
    pub target: Target,
}

pub(super) struct Prepared {
    pub target: Target,
    pub params: wgpu::BindGroup,
    pub texture: wgpu::BindGroup,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct Params {
    bounds: PodBounds,
    clip_bounds: PodBounds,
    radii: [f32; 4],
}

impl WgpuRenderer {
    pub(super) fn target_pipelines(&self, target: &Target) -> &WgpuPipelines {
        if target.isolated {
            self.resources()
                .isolated_pipelines
                .as_ref()
                .expect("isolated pipelines prepared")
        } else {
            &self.resources().pipelines
        }
    }

    pub(super) fn prepare_composite(
        &mut self,
        composite: &Composite,
        bounds: Bounds<ScaledPixels>,
        ix: usize,
    ) -> Prepared {
        debug_assert!(!bounds.is_empty());
        let origin = bounds.origin;
        let width = bounds.size.width.0 as u32;
        let height = bounds.size.height.0 as u32;
        let format = self.surface_config.format;
        let samples = self.rendering_params.path_sample_count;
        let path_offset = self.path_globals_offset;
        let gamma_offset = self.gamma_offset;
        let webgl = self.uses_webgl_instance_data;
        let gamma = GammaParams {
            gamma_ratios: self.rendering_params.gamma_ratios,
            grayscale_enhanced_contrast: self.rendering_params.grayscale_enhanced_contrast,
            subpixel_enhanced_contrast: self.rendering_params.subpixel_enhanced_contrast,
            is_bgr: self.is_bgr as u32,
            _pad: 0,
        };
        let resources = self.resources_mut();
        if resources.isolated_pipelines.is_none() {
            resources.isolated_pipelines = Some(Self::create_pipelines(
                &resources.device,
                &resources.bind_group_layouts,
                format,
                wgpu::CompositeAlphaMode::PreMultiplied,
                samples,
                false,
                webgl,
            ));
        }
        while resources.clip_surfaces.len() <= ix {
            resources.clip_surfaces.push(None);
        }
        if resources.clip_surfaces[ix]
            .as_ref()
            .is_none_or(|surface| surface.size != (width, height))
        {
            resources.clip_surfaces[ix] = None;
            let device = &resources.device;
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("rounded_clip"),
                size: wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let view = texture.create_view(&Default::default());
            let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rounded_clip_globals"),
                size: gamma_offset + std::mem::size_of::<GammaParams>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind = |offset| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("rounded_clip_globals"),
                    layout: &resources.bind_group_layouts.globals,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &uniforms,
                                offset,
                                size: NonZeroU64::new(std::mem::size_of::<GlobalParams>() as u64),
                            }),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &uniforms,
                                offset: gamma_offset,
                                size: NonZeroU64::new(std::mem::size_of::<GammaParams>() as u64),
                            }),
                        },
                    ],
                })
            };
            resources.clip_surfaces[ix] = Some(Surface {
                size: (width, height),
                _texture: texture,
                target: Target {
                    view,
                    globals: bind(0),
                    path_globals: bind(path_offset),
                    paths: None,
                    path_msaa: None,
                    isolated: true,
                    bounds,
                },
                uniforms,
            });
        }
        let surface = resources.clip_surfaces[ix]
            .as_mut()
            .expect("surface prepared");
        surface.target.bounds = bounds;
        if composite.scene().has_paths() && surface.target.paths.is_none() {
            surface.target.paths =
                Some(Self::create_path_intermediate(&resources.device, format, width, height).1);
            surface.target.path_msaa =
                Self::create_msaa_if_needed(&resources.device, format, width, height, samples)
                    .map(|(_, view)| view);
        }
        if !composite.scene().has_paths() {
            surface.target.paths = None;
            surface.target.path_msaa = None;
        }
        let globals = GlobalParams {
            viewport_size: [width as f32, height as f32],
            viewport_origin: [origin.x.0, origin.y.0],
            premultiplied_alpha: 1,
            pad: [0; 3],
        };
        resources
            .queue
            .write_buffer(&surface.uniforms, 0, bytemuck::bytes_of(&globals));
        resources.queue.write_buffer(
            &surface.uniforms,
            path_offset,
            bytemuck::bytes_of(&GlobalParams {
                premultiplied_alpha: 0,
                ..globals
            }),
        );
        resources
            .queue
            .write_buffer(&surface.uniforms, gamma_offset, bytemuck::bytes_of(&gamma));
        let target = surface.target.clone();
        let radii = composite.radii();
        let params = Params {
            bounds: bounds.into(),
            clip_bounds: composite.bounds().into(),
            radii: [
                radii.top_left.0,
                radii.top_right.0,
                radii.bottom_right.0,
                radii.bottom_left.0,
            ],
        };
        let buffer = resources
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("rounded_clip_params"),
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });
        let params = resources
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("rounded_clip_params"),
                layout: &resources.bind_group_layouts.composite,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
        let texture = self.create_texture_bind_group("rounded_clip_texture", &target.view);
        Prepared {
            target,
            params,
            texture,
        }
    }
}

#[cfg(test)]
mod tests;
