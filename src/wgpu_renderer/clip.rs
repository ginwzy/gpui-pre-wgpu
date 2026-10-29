use super::{GammaParams, GlobalParams, PodBounds, WgpuPipelines, WgpuRenderer};
use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use gpui::{Bounds, Composite, ScaledPixels, point, size};
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
        ix: usize,
    ) -> Result<Option<Prepared>> {
        let visible = composite
            .bounds()
            .intersect(&composite.content_mask().bounds);
        // Clip to the actual drawable, including when the device clamps a huge window.
        let viewport = Bounds::new(
            point(ScaledPixels(0.), ScaledPixels(0.)),
            size(
                ScaledPixels(self.surface_config.width as f32),
                ScaledPixels(self.surface_config.height as f32),
            ),
        );
        let visible = visible.intersect(&viewport);
        if visible.is_empty() {
            return Ok(None);
        }
        let origin = point(
            ScaledPixels(visible.left().0.floor()),
            ScaledPixels(visible.top().0.floor()),
        );
        let width = (visible.right().0.ceil() - origin.x.0) as u32;
        let height = (visible.bottom().0.ceil() - origin.y.0) as u32;
        let bounds = Bounds::new(
            origin,
            size(ScaledPixels(width as f32), ScaledPixels(height as f32)),
        );
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
                },
                uniforms,
            });
        }
        let surface = resources.clip_surfaces[ix]
            .as_mut()
            .expect("surface prepared");
        if !composite.scene().paths.is_empty() && surface.target.paths.is_none() {
            surface.target.paths =
                Some(Self::create_path_intermediate(&resources.device, format, width, height).1);
            surface.target.path_msaa =
                Self::create_msaa_if_needed(&resources.device, format, width, height, samples)
                    .map(|(_, view)| view);
        }
        if composite.scene().paths.is_empty() {
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
        Ok(Some(Prepared {
            target,
            params,
            texture,
        }))
    }
}

#[cfg(test)]
mod tests;
