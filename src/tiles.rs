//! wgpu render pipeline + GPU texture cache for slippy-map tiles.
//!
//! The pipeline draws axis-aligned textured quads in viewport-local pixel
//! space. The vertex shader does the trivial pixel → clip-space transform;
//! the host sets the wgpu viewport so this module only needs to know the
//! viewport's pixel size.

use std::{collections::HashMap, num::NonZeroU32};

use wgpu::util::DeviceExt;

use crate::{
    coords::{TileId, UvRect},
    tile_loader::TileImage,
};

/// A single tile uploaded to the GPU.
struct TileGpu {
    bind_group: wgpu::BindGroup,
    // Texture stays alive via the bind group's view ref counts; we only
    // keep the handle here so eviction (`retain`) drops it explicitly.
    _texture: wgpu::Texture,
}

/// One vertex of a textured tile quad. Position in viewport-local pixels.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct TileVertex {
    pub pos: [f32; 2],
    pub uv: [f32; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    /// `[viewport_w, viewport_h, _, _]` — last two slots are padding so the
    /// uniform's size meets wgpu's 16-byte minimum binding alignment.
    data: [f32; 4],
}

/// wgpu state for the tile pipeline plus the per-tile texture cache.
pub struct TileRenderer {
    pipeline: wgpu::RenderPipeline,
    tile_bind_group_layout: wgpu::BindGroupLayout,
    /// Texture format used for uploaded tile textures. Chosen at construction
    /// to match the render target's color space so the round-trip (decode on
    /// sample + RT encode on write) leaves the pixel values unchanged.
    tile_texture_format: wgpu::TextureFormat,
    viewport_uniform: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
    sampler: wgpu::Sampler,
    /// Reusable index buffer for one quad (6 indices). Vertex buffers are
    /// reallocated per frame to size with the visible tile count.
    quad_index_buffer: wgpu::Buffer,
    /// GPU-resident tile textures keyed by id. Evicted by
    /// [`TileRenderer::retain`].
    tiles: HashMap<TileId, TileGpu>,
}

const TILE_SHADER: &str = r#"
struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) uv:  vec2<f32>,
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Viewport {
    data: vec4<f32>, // x, y = viewport size in pixels
};

@group(0) @binding(0) var<uniform> viewport: Viewport;
@group(1) @binding(0) var tile_texture: texture_2d<f32>;
@group(1) @binding(1) var tile_sampler: sampler;

@vertex
fn vs_main(v: VsIn) -> VsOut {
    var out: VsOut;
    let x = v.pos.x / viewport.data.x * 2.0 - 1.0;
    let y = 1.0 - v.pos.y / viewport.data.y * 2.0;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.uv = v.uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tile_texture, tile_sampler, in.uv);
}
"#;

impl TileRenderer {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("emap.tile.shader"),
            source: wgpu::ShaderSource::Wgsl(TILE_SHADER.into()),
        });

        let viewport_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("emap.tile.viewport_bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let tile_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("emap.tile.tile_bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("emap.tile.pl"),
            bind_group_layouts: &[&viewport_bind_group_layout, &tile_bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("emap.tile.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<TileVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let viewport_uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("emap.tile.viewport_uniform"),
            size: std::mem::size_of::<ViewportUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("emap.tile.viewport_bg"),
            layout: &viewport_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_uniform.as_entire_binding(),
            }],
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("emap.tile.sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        // Quad indices: two triangles forming a quad. TL=0, TR=1, BR=2, BL=3.
        let quad_index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("emap.tile.indices"),
            contents: bytemuck::cast_slice::<u16, _>(&[0, 1, 2, 0, 2, 3]),
            usage: wgpu::BufferUsages::INDEX,
        });

        // Pick a tile-texture format matched to the render target's color
        // space. With sRGB target the GPU does sRGB→linear on sample and
        // linear→sRGB on write, so storing sRGB-encoded PNG bytes as
        // Rgba8UnormSrgb round-trips exactly. With a UNorm target (eframe
        // does manual gamma in its own shader) no RT-side conversion runs,
        // so we mustn't decode on sample either — use plain Rgba8Unorm so
        // sampled values equal the stored bytes.
        let tile_texture_format = if target_format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };

        Self {
            pipeline,
            tile_bind_group_layout,
            tile_texture_format,
            viewport_uniform,
            viewport_bind_group,
            sampler,
            quad_index_buffer,
            tiles: HashMap::new(),
        }
    }

    /// Whether a tile is already resident on the GPU.
    pub fn contains(&self, tile: &TileId) -> bool {
        self.tiles.contains_key(tile)
    }

    /// Upload a decoded tile image to a fresh GPU texture and register it.
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tile: TileId,
        image: &TileImage,
    ) {
        let size = wgpu::Extent3d {
            width: image.width,
            height: image.height,
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("emap.tile.texture"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.tile_texture_format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });

        let bytes_per_row =
            NonZeroU32::new(4 * image.width).map(|v| v.get()).unwrap_or(0);
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &image.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(image.height),
            },
            size,
        );

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("emap.tile.bg"),
            layout: &self.tile_bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        self.tiles.insert(
            tile,
            TileGpu {
                _texture: texture,
                bind_group,
            },
        );
    }

    /// Evict GPU textures for tiles outside `retain`.
    pub fn retain<'a>(&mut self, retain: impl IntoIterator<Item = &'a TileId>) {
        let set = retain
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        self.tiles.retain(|k, _| set.contains(k));
    }

    /// Draw one frame's worth of tiles.
    ///
    /// `draws` carries one entry per visible tile: the screen-space rect to
    /// paint into (viewport-local pixels), the tile id to sample from, and
    /// the UV sub-rect within that texture (full image except when the
    /// caller falls back to a pyramid parent).
    pub fn render(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport_size: glam::Vec2,
        draws: &[TileDraw],
        pass: &mut wgpu::RenderPass<'_>,
    ) {
        if draws.is_empty() {
            return;
        }

        queue.write_buffer(
            &self.viewport_uniform,
            0,
            bytemuck::cast_slice(&[ViewportUniform {
                data: [viewport_size.x, viewport_size.y, 0.0, 0.0],
            }]),
        );

        let mut verts: Vec<TileVertex> = Vec::with_capacity(draws.len() * 4);
        for d in draws {
            let min = d.rect_min;
            let max = d.rect_max;
            let uv_min = d.uv_min;
            let uv_max = d.uv_max;
            // TL, TR, BR, BL
            verts.push(TileVertex {
                pos: [min.x, min.y],
                uv: [uv_min.x, uv_min.y],
            });
            verts.push(TileVertex {
                pos: [max.x, min.y],
                uv: [uv_max.x, uv_min.y],
            });
            verts.push(TileVertex {
                pos: [max.x, max.y],
                uv: [uv_max.x, uv_max.y],
            });
            verts.push(TileVertex {
                pos: [min.x, max.y],
                uv: [uv_min.x, uv_max.y],
            });
        }

        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("emap.tile.vbuf"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.viewport_bind_group, &[]);
        pass.set_index_buffer(self.quad_index_buffer.slice(..), wgpu::IndexFormat::Uint16);

        for (i, d) in draws.iter().enumerate() {
            let Some(gpu) = self.tiles.get(&d.tile) else {
                continue;
            };
            let vstart = (i * 4) as u64 * std::mem::size_of::<TileVertex>() as u64;
            let vend = vstart + 4 * std::mem::size_of::<TileVertex>() as u64;
            pass.set_vertex_buffer(0, vbuf.slice(vstart..vend));
            pass.set_bind_group(1, &gpu.bind_group, &[]);
            pass.draw_indexed(0..6, 0, 0..1);
        }

        // Keep the vertex buffer alive until the queue submission completes
        // by leaking it into the pass's frame lifetime; wgpu's command
        // encoder retains the reference.
        // NOTE: rust will drop `vbuf` at end of function and wgpu reference
        // counts will keep it alive for the in-flight frame.
        let _ = vbuf;
    }
}

/// One tile to paint this frame.
pub struct TileDraw {
    pub tile: TileId,
    pub rect_min: glam::Vec2,
    pub rect_max: glam::Vec2,
    pub uv_min: glam::Vec2,
    pub uv_max: glam::Vec2,
}

impl TileDraw {
    pub fn full_uv(tile: TileId, rect_min: glam::Vec2, rect_max: glam::Vec2) -> Self {
        Self {
            tile,
            rect_min,
            rect_max,
            uv_min: glam::Vec2::ZERO,
            uv_max: glam::Vec2::ONE,
        }
    }
    pub fn with_uv(
        tile: TileId,
        rect_min: glam::Vec2,
        rect_max: glam::Vec2,
        uv: UvRect,
    ) -> Self {
        Self {
            tile,
            rect_min,
            rect_max,
            uv_min: uv.min,
            uv_max: uv.max,
        }
    }
}
