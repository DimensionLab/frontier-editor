// SPDX-License-Identifier: Apache-2.0
//! The wgpu side: one instanced-quad pipeline, the atlas texture, and a `render`
//! that draws into a render pass the host already began (the same pass list the
//! 3D viewers use), clipped to the view's rectangle.

use crate::text::GlyphAtlas;
use bytemuck::{Pod, Zeroable};

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable, PartialEq)]
pub struct Instance {
    pub rect: [f32; 4],
    pub uv: [f32; 4],
    pub color: [f32; 4],
    pub kind: u32,
    pub _pad: [u32; 3],
}

impl Instance {
    pub const SOLID: u32 = 0;
    pub const GLYPH: u32 = 1;
    /// A glyph with its own colours (emoji): drawn as it is, not tinted.
    pub const COLOR_GLYPH: u32 = 2;

    pub fn solid(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> Self {
        Self {
            rect: [x, y, w, h],
            uv: [0.0; 4],
            color,
            kind: Self::SOLID,
            _pad: [0; 3],
        }
    }

    pub fn glyph(x: f32, y: f32, w: f32, h: f32, uv: [f32; 4], color: [f32; 4]) -> Self {
        Self {
            rect: [x, y, w, h],
            uv,
            color,
            kind: Self::GLYPH,
            _pad: [0; 3],
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct Globals {
    transform: [f32; 4],
    viewport: [f32; 4],
}

/// How the host places the view: device-pixel rectangle on the target, a host
/// transform (canvas zoom/pan) applied on top, and the target's size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    /// Scale applied to the view's device pixels (host canvas zoom); 1 = none.
    pub scale: f32,
    /// Translation in target device pixels (the view's origin on the target).
    pub x: f32,
    pub y: f32,
    pub target_w: u32,
    pub target_h: u32,
    /// Scissor rectangle in target device pixels; `None` = the view's own rect.
    pub clip: Option<(u32, u32, u32, u32)>,
}

impl Placement {
    pub fn fill(target_w: u32, target_h: u32) -> Self {
        Self {
            scale: 1.0,
            x: 0.0,
            y: 0.0,
            target_w,
            target_h,
            clip: None,
        }
    }
}

pub struct Renderer {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    bind_group: wgpu::BindGroup,
    globals: wgpu::Buffer,
    sampler: wgpu::Sampler,
    atlas_tex: wgpu::Texture,
    atlas_size: u32,
    atlas_generation: u64,
    instances: wgpu::Buffer,
    instance_capacity: usize,
    instance_count: u32,
    scissor: (u32, u32, u32, u32),
    pub format: wgpu::TextureFormat,
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Renderer")
            .field("format", &self.format)
            .field("atlas_size", &self.atlas_size)
            .field("instances", &self.instance_count)
            .finish()
    }
}

impl Renderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat, sample_count: u32) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("frontier-editor-view"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("editor-view globals"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("editor-view"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("editor-view quads"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<Instance>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Uint32],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: sample_count,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("editor-view globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("editor-view atlas"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let atlas_size = 1024;
        let atlas_tex = Self::make_atlas(device, atlas_size);
        let bind_group =
            Self::make_bind_group(device, &bind_layout, &globals, &atlas_tex, &sampler);
        let instance_capacity = 4096;
        let instances = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("editor-view instances"),
            size: (instance_capacity * std::mem::size_of::<Instance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            pipeline,
            bind_layout,
            bind_group,
            globals,
            sampler,
            atlas_tex,
            atlas_size,
            atlas_generation: 0,
            instances,
            instance_capacity,
            instance_count: 0,
            scissor: (0, 0, 0, 0),
            format,
        }
    }

    fn make_atlas(device: &wgpu::Device, size: u32) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some("editor-view glyph atlas"),
            size: wgpu::Extent3d {
                width: size,
                height: size,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    }

    fn make_bind_group(
        device: &wgpu::Device,
        layout: &wgpu::BindGroupLayout,
        globals: &wgpu::Buffer,
        atlas: &wgpu::Texture,
        sampler: &wgpu::Sampler,
    ) -> wgpu::BindGroup {
        let view = atlas.create_view(&Default::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("editor-view"),
            layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(sampler),
                },
            ],
        })
    }

    /// Uploads this frame's instances, pending atlas bitmaps and the placement.
    #[allow(clippy::too_many_arguments)]
    /// `view_w`/`view_h` are the view's own size in device pixels.
    pub fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        atlas: &mut GlyphAtlas,
        instances: &[Instance],
        placement: &Placement,
        view_w: u32,
        view_h: u32,
    ) {
        if atlas.generation != self.atlas_generation || atlas.size != self.atlas_size {
            self.atlas_size = atlas.size;
            self.atlas_generation = atlas.generation;
            self.atlas_tex = Self::make_atlas(device, self.atlas_size);
            self.bind_group = Self::make_bind_group(
                device,
                &self.bind_layout,
                &self.globals,
                &self.atlas_tex,
                &self.sampler,
            );
        }
        for (x, y, w, h, data) in atlas.take_pending() {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas_tex,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                &data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(w * 4),
                    rows_per_image: Some(h),
                },
                wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
            );
        }
        if instances.len() > self.instance_capacity {
            self.instance_capacity = instances.len().next_power_of_two();
            self.instances = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("editor-view instances"),
                size: (self.instance_capacity * std::mem::size_of::<Instance>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
        }
        if !instances.is_empty() {
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(instances));
        }
        self.instance_count = instances.len() as u32;
        let g = Globals {
            transform: [placement.scale, placement.scale, placement.x, placement.y],
            viewport: [
                2.0 / placement.target_w.max(1) as f32,
                2.0 / placement.target_h.max(1) as f32,
                0.0,
                0.0,
            ],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&g));
        let (vw, vh) = (
            (view_w as f32 * placement.scale) as u32,
            (view_h as f32 * placement.scale) as u32,
        );
        let clip = placement.clip.unwrap_or((
            placement.x.max(0.0) as u32,
            placement.y.max(0.0) as u32,
            vw,
            vh,
        ));
        let x = clip.0.min(placement.target_w);
        let y = clip.1.min(placement.target_h);
        let w = clip.2.min(placement.target_w - x);
        let h = clip.3.min(placement.target_h - y);
        self.scissor = (x, y, w, h);
    }

    /// Draws the prepared frame into `pass`. The pass must target `self.format`.
    pub fn render(&self, pass: &mut wgpu::RenderPass<'_>) {
        if self.instance_count == 0 || self.scissor.2 == 0 || self.scissor.3 == 0 {
            return;
        }
        pass.set_scissor_rect(
            self.scissor.0,
            self.scissor.1,
            self.scissor.2,
            self.scissor.3,
        );
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.instances.slice(..));
        pass.draw(0..6, 0..self.instance_count);
    }

    pub fn instance_count(&self) -> u32 {
        self.instance_count
    }
}
