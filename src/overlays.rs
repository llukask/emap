//! wgpu render pipeline + lyon tessellation for overlay shapes.
//!
//! Shapes carry geographic coordinates; the caller projects them to
//! viewport-local pixel space before passing them in (so this module is
//! pure 2D rasterization). Each frame's overlays are tessellated into a
//! single vertex+index buffer and drawn with one draw call.

use lyon::math::point;
use lyon::path::Path;
use lyon::tessellation::{
    BuffersBuilder, FillOptions, FillTessellator, FillVertex, StrokeOptions,
    StrokeTessellator, StrokeVertex, VertexBuffers,
};
use wgpu::util::DeviceExt;

use crate::{Color, Stroke};

/// One vertex of an overlay primitive. Position in viewport-local pixels,
/// color is straight (non-premultiplied) sRGB.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct OverlayVertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    data: [f32; 4],
}

const OVERLAY_SHADER: &str = r#"
struct VsIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

struct Viewport {
    data: vec4<f32>,
};

@group(0) @binding(0) var<uniform> viewport: Viewport;

@vertex
fn vs_main(v: VsIn) -> VsOut {
    var out: VsOut;
    let x = v.pos.x / viewport.data.x * 2.0 - 1.0;
    let y = 1.0 - v.pos.y / viewport.data.y * 2.0;
    out.position = vec4<f32>(x, y, 0.0, 1.0);
    out.color = v.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

/// Either a vertex/fragment description of a primitive to draw.
pub enum OverlayShape<'a> {
    /// Polyline through pixel-space points. `closed = true` joins the last
    /// point back to the first; used for the outline of a polygon.
    Polyline {
        points: &'a [glam::Vec2],
        stroke: Stroke,
        closed: bool,
    },
    /// Filled (possibly non-convex) polygon through pixel-space points.
    PolygonFill {
        points: &'a [glam::Vec2],
        fill: Color,
    },
    /// Circle anchored at a pixel-space center with a pixel radius.
    CircleStroke {
        center: glam::Vec2,
        radius: f32,
        stroke: Stroke,
    },
    CircleFill {
        center: glam::Vec2,
        radius: f32,
        fill: Color,
    },
}

pub struct OverlayRenderer {
    pipeline: wgpu::RenderPipeline,
    viewport_uniform: wgpu::Buffer,
    viewport_bind_group: wgpu::BindGroup,
}

impl OverlayRenderer {
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("emap.overlay.shader"),
            source: wgpu::ShaderSource::Wgsl(OVERLAY_SHADER.into()),
        });

        let viewport_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("emap.overlay.viewport_bgl"),
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

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("emap.overlay.pl"),
            bind_group_layouts: &[&viewport_bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("emap.overlay.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<OverlayVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
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
            label: Some("emap.overlay.viewport_uniform"),
            size: std::mem::size_of::<ViewportUniform>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("emap.overlay.viewport_bg"),
            layout: &viewport_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_uniform.as_entire_binding(),
            }],
        });

        Self {
            pipeline,
            viewport_uniform,
            viewport_bind_group,
        }
    }

    /// Tessellate and draw all `shapes` in a single draw call.
    pub fn render(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport_size: glam::Vec2,
        shapes: &[OverlayShape<'_>],
        pass: &mut wgpu::RenderPass<'_>,
    ) {
        if shapes.is_empty() {
            return;
        }

        let mut buffers: VertexBuffers<OverlayVertex, u32> = VertexBuffers::new();
        let mut fill_tess = FillTessellator::new();
        let mut stroke_tess = StrokeTessellator::new();

        for shape in shapes {
            match shape {
                OverlayShape::Polyline { points, stroke, closed } => {
                    if points.len() < 2 || stroke.width <= 0.0 {
                        continue;
                    }
                    let path = build_path(points, *closed);
                    let color = color_to_array(stroke.color);
                    let opts = StrokeOptions::default().with_line_width(stroke.width);
                    let _ = stroke_tess.tessellate_path(
                        &path,
                        &opts,
                        &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| OverlayVertex {
                            pos: [v.position().x, v.position().y],
                            color,
                        }),
                    );
                }
                OverlayShape::PolygonFill { points, fill } => {
                    if points.len() < 3 {
                        continue;
                    }
                    let path = build_path(points, true);
                    let color = color_to_array(*fill);
                    let _ = fill_tess.tessellate_path(
                        &path,
                        &FillOptions::default(),
                        &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| OverlayVertex {
                            pos: [v.position().x, v.position().y],
                            color,
                        }),
                    );
                }
                OverlayShape::CircleStroke { center, radius, stroke } => {
                    if stroke.width <= 0.0 || *radius <= 0.0 {
                        continue;
                    }
                    let color = color_to_array(stroke.color);
                    let opts = StrokeOptions::default().with_line_width(stroke.width);
                    let _ = stroke_tess.tessellate_circle(
                        point(center.x, center.y),
                        *radius,
                        &opts,
                        &mut BuffersBuilder::new(&mut buffers, |v: StrokeVertex| OverlayVertex {
                            pos: [v.position().x, v.position().y],
                            color,
                        }),
                    );
                }
                OverlayShape::CircleFill { center, radius, fill } => {
                    if *radius <= 0.0 {
                        continue;
                    }
                    let color = color_to_array(*fill);
                    let _ = fill_tess.tessellate_circle(
                        point(center.x, center.y),
                        *radius,
                        &FillOptions::default(),
                        &mut BuffersBuilder::new(&mut buffers, |v: FillVertex| OverlayVertex {
                            pos: [v.position().x, v.position().y],
                            color,
                        }),
                    );
                }
            }
        }

        if buffers.indices.is_empty() {
            return;
        }

        queue.write_buffer(
            &self.viewport_uniform,
            0,
            bytemuck::cast_slice(&[ViewportUniform {
                data: [viewport_size.x, viewport_size.y, 0.0, 0.0],
            }]),
        );

        let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("emap.overlay.vbuf"),
            contents: bytemuck::cast_slice(&buffers.vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("emap.overlay.ibuf"),
            contents: bytemuck::cast_slice(&buffers.indices),
            usage: wgpu::BufferUsages::INDEX,
        });

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.viewport_bind_group, &[]);
        pass.set_vertex_buffer(0, vbuf.slice(..));
        pass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..buffers.indices.len() as u32, 0, 0..1);
    }
}

fn build_path(points: &[glam::Vec2], closed: bool) -> Path {
    let mut builder = Path::builder();
    let mut iter = points.iter();
    if let Some(first) = iter.next() {
        builder.begin(point(first.x, first.y));
        for p in iter {
            builder.line_to(point(p.x, p.y));
        }
        builder.end(closed);
    }
    builder.build()
}

fn color_to_array(c: Color) -> [f32; 4] {
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        c.a as f32 / 255.0,
    ]
}
