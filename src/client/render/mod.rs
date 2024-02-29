// Copyright © 2020 Cormac O'Brien.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

/// Rendering functionality.
///
/// # Pipeline stages
///
/// The current rendering implementation consists of the following stages:
/// - Initial geometry pass
///   - Inputs:
///     - `AliasPipeline`
///     - `BrushPipeline`
///     - `SpritePipeline`
///   - Output: `InitialPassTarget`
/// - Deferred lighting pass
///   - Inputs:
///     - `DeferredPipeline`
///     - `QuadPipeline`
///     - `GlyphPipeline`
///   - Output: `DeferredPassTarget`
/// - Final pass
///   - Inputs:
///     - `PostProcessPipeline`
///   - Output: `FinalPassTarget`
/// - Blit to swap chain
///   - Inputs:
///     - `BlitPipeline`
///   - Output: `SwapChainTarget`
mod atlas;
mod blit;
mod cvars;
mod error;
mod palette;
mod pipeline;
mod target;
mod ui;
mod uniform;
mod warp;
mod world;

use bevy::{
    app::Plugin,
    core_pipeline::core_3d::graph::{Core3d, Node3d},
    ecs::{
        schedule::ScheduleLabel,
        system::{Res, ResMut, Resource},
        world::FromWorld,
    },
    prelude::*,
    render::{
        extract_resource::{ExtractResource, ExtractResourcePlugin},
        render_graph::{Node, RenderGraph, RenderLabel, SlotInfo},
        render_resource::{BindGroup, BindGroupLayout, Buffer, Sampler, Texture, TextureView},
        renderer::{RenderDevice, RenderQueue},
        view::ViewTarget,
        ExtractSchedule, RenderApp,
    },
    window::PrimaryWindow,
};
pub use cvars::register_cvars;
pub use error::{RenderError, RenderErrorKind};
pub use palette::Palette;
use parking_lot::RwLock;
pub use pipeline::Pipeline;
pub use postprocess::PostProcessRenderer;
pub use target::{PreferredFormat, RenderTarget, RenderTargetResolve, SwapChainTarget};
pub use ui::{hud::HudState, UiOverlay, UiRenderer, UiState};
pub use world::{
    deferred::{DeferredRenderer, DeferredUniforms, PointLight},
    Camera, WorldRenderer,
};

use std::{
    borrow::Cow,
    cell::RefCell,
    mem::size_of,
    num::NonZeroU64,
    ops::{Deref, DerefMut},
};

use crate::{
    client::{
        entity::MAX_LIGHTS,
        input::InputFocus,
        menu::Menu,
        render::{
            target::{DeferredPassTarget, FinalPassTarget, InitPassOutput, InitialPassTarget},
            ui::{glyph::GlyphPipeline, quad::QuadPipeline},
            uniform::DynamicUniformBuffer,
            world::{
                alias::AliasPipeline,
                brush::BrushPipeline,
                deferred::DeferredPipeline,
                particle::ParticlePipeline,
                postprocess::{self, PostProcessPipeline},
                sprite::SpritePipeline,
                EntityUniforms,
            },
        },
    },
    common::{
        console::{cvar_error_handler, Console, CvarRegistry},
        vfs::Vfs,
        wad::Wad,
    },
};

use self::{
    blit::BlitPipeline,
    target::{InitPass, InitPassLabel},
    world::extract_world_renderer,
};

use bumpalo::Bump;
use cgmath::{Deg, Vector3, Zero};
use chrono::{DateTime, Duration, Utc};
use failure::Error;

use super::{state::ClientState, Connection, ConnectionKind, ConnectionState};

pub struct RichterRenderPlugin;

fn extract_now<T: Resource + Clone>(app: &mut App) {
    let res = app.world.resource::<T>().clone();
    let Ok(render_app) = app.get_sub_app_mut(RenderApp) else {
        return;
    };
    render_app.insert_resource(res);
}

impl Plugin for RichterRenderPlugin {
    fn build(&self, app: &mut bevy::prelude::App) {
        #[derive(Hash, Debug, PartialEq, Eq, Copy, Clone, ScheduleLabel)]
        struct RenderSetup;

        app.add_plugins((
            ExtractResourcePlugin::<Console>::default(),
            ExtractResourcePlugin::<CvarRegistry>::default(),
            ExtractResourcePlugin::<Menu>::default(),
            ExtractResourcePlugin::<RenderState>::default(),
            ExtractResourcePlugin::<RenderResolution>::default(),
            ExtractResourcePlugin::<InputFocus>::default(),
            ExtractResourcePlugin::<Fov>::default(),
            ExtractResourcePlugin::<ConnectionState>::default(),
            // TODO: Do all loading on the main thread (this is currently just for the palette and gfx wad)
            ExtractResourcePlugin::<Vfs>::default(),
        ))
        .add_systems(Startup, register_cvars.pipe(cvar_error_handler));

        extract_now::<Console>(app);
        extract_now::<CvarRegistry>(app);
        extract_now::<Menu>(app);
        extract_now::<Vfs>(app);
        extract_now::<ConnectionState>(app);
    }

    fn finish(&self, app: &mut bevy::prelude::App) {
        extract_now::<RenderResolution>(app);

        let Ok(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };

        render_app
            .init_resource::<GraphicsState>()
            .init_resource::<DeferredRenderer>()
            .init_resource::<PostProcessRenderer>()
            .init_resource::<UiRenderer>()
            .add_systems(
                ExtractSchedule,
                (
                    (systems::recreate_graphics_state, systems::update_renderers)
                        .chain()
                        .run_if(resource_changed::<RenderResolution>),
                    extract_world_renderer.run_if(resource_changed::<ConnectionState>),
                    ui::systems::update_ui,
                ),
            );

        let renderer = ClientRenderer::from_world(&mut render_app.world);
        let mut render_graph = render_app.world.resource_mut::<RenderGraph>();
        let render_graph = render_graph.sub_graph_mut(Core3d);
        render_graph.add_node(ClientRenderLabel, renderer);
        render_graph.add_node(InitPassLabel, InitPass);
        render_graph.add_node_edge(Node3d::MainOpaquePass, InitPassLabel);
        render_graph.add_slot_edge(
            InitPassLabel,
            InitPassOutput::Diffuse,
            ClientRenderLabel,
            InitPassOutput::Diffuse,
        );
        render_graph.add_slot_edge(
            InitPassLabel,
            InitPassOutput::Normal,
            ClientRenderLabel,
            InitPassOutput::Normal,
        );
        render_graph.add_slot_edge(
            InitPassLabel,
            InitPassOutput::Light,
            ClientRenderLabel,
            InitPassOutput::Light,
        );
        render_graph.add_slot_edge(
            InitPassLabel,
            InitPassOutput::Depth,
            ClientRenderLabel,
            InitPassOutput::Depth,
        );
        render_graph.add_node_edge(ClientRenderLabel, Node3d::EndMainPass);
    }
}

const DEPTH_ATTACHMENT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
pub const DIFFUSE_ATTACHMENT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
pub const FINAL_ATTACHMENT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const NORMAL_ATTACHMENT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const LIGHT_ATTACHMENT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const DIFFUSE_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;
const FULLBRIGHT_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;
const LIGHTMAP_TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R8Unorm;

/// Create a `wgpu::TextureDescriptor` appropriate for the provided texture data.
pub fn texture_descriptor<'a>(
    label: Option<&'a str>,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::TextureDescriptor {
    wgpu::TextureDescriptor {
        label,
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: Default::default(),
    }
}

pub fn create_texture<'a>(
    device: &RenderDevice,
    queue: &RenderQueue,
    label: Option<&'a str>,
    width: u32,
    height: u32,
    data: &TextureData,
) -> Texture {
    trace!(
        "Creating texture ({:?}: {}x{})",
        data.format(),
        width,
        height
    );

    // It looks like sometimes quake includes textures with at least one zero aspect?
    let texture = device.create_texture(&texture_descriptor(
        label,
        width.max(1),
        height.max(1),
        data.format(),
    ));
    queue.write_texture(
        wgpu::ImageCopyTexture {
            texture: &texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: Default::default(),
        },
        data.data(),
        wgpu::ImageDataLayout {
            offset: 0,
            bytes_per_row: Some(width * data.stride()),
            rows_per_image: None,
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    texture
}

pub struct DiffuseData<'a> {
    pub rgba: Cow<'a, [u8]>,
}

pub struct FullbrightData<'a> {
    pub fullbright: Cow<'a, [u8]>,
}

pub struct LightmapData<'a> {
    pub lightmap: Cow<'a, [u8]>,
}

pub enum TextureData<'a> {
    Diffuse(DiffuseData<'a>),
    Fullbright(FullbrightData<'a>),
    Lightmap(LightmapData<'a>),
}

impl<'a> TextureData<'a> {
    pub fn format(&self) -> wgpu::TextureFormat {
        match self {
            TextureData::Diffuse(_) => DIFFUSE_TEXTURE_FORMAT,
            TextureData::Fullbright(_) => FULLBRIGHT_TEXTURE_FORMAT,
            TextureData::Lightmap(_) => LIGHTMAP_TEXTURE_FORMAT,
        }
    }

    pub fn data(&self) -> &[u8] {
        match self {
            TextureData::Diffuse(d) => &d.rgba,
            TextureData::Fullbright(d) => &d.fullbright,
            TextureData::Lightmap(d) => &d.lightmap,
        }
    }

    pub fn stride(&self) -> u32 {
        use std::mem;
        use wgpu::TextureFormat::*;

        (match self.format() {
            Rg8Unorm | Rg8Snorm | Rg8Uint | Rg8Sint => mem::size_of::<[u8; 2]>(),
            R8Unorm | R8Snorm | R8Uint | R8Sint => mem::size_of::<u8>(),
            Bgra8Unorm | Bgra8UnormSrgb | Rgba8Unorm | Rgba8UnormSrgb => mem::size_of::<[u8; 4]>(),
            R16Uint | R16Sint | R16Unorm | R16Snorm | R16Float => mem::size_of::<u16>(),
            Rg16Uint | Rg16Sint | Rg16Unorm | Rg16Snorm | Rg16Float => mem::size_of::<[u16; 2]>(),
            Rgba16Uint | Rgba16Sint | Rgba16Unorm | Rgba16Snorm | Rgba16Float => {
                mem::size_of::<[u16; 4]>()
            }
            _ => todo!(),
        }) as u32
    }

    pub fn size(&self) -> wgpu::BufferAddress {
        self.data().len() as wgpu::BufferAddress
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Extent2d {
    pub width: u32,
    pub height: u32,
}

impl std::convert::Into<wgpu::Extent3d> for Extent2d {
    fn into(self) -> wgpu::Extent3d {
        wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        }
    }
}

impl std::convert::From<winit::dpi::PhysicalSize<u32>> for Extent2d {
    fn from(other: winit::dpi::PhysicalSize<u32>) -> Extent2d {
        let winit::dpi::PhysicalSize { width, height } = other;
        Extent2d { width, height }
    }
}

#[derive(Resource, ExtractResource, PartialEq, Eq, Clone, Copy)]
pub struct RenderResolution(pub u32, pub u32);

impl FromWorld for RenderResolution {
    fn from_world(world: &mut World) -> Self {
        let res = &world
            .query_filtered::<&Window, With<PrimaryWindow>>()
            .single(world)
            .resolution;

        RenderResolution(res.width() as _, res.height() as _)
    }
}

pub enum RenderConnectionKind {
    Server,
    Demo,
}

#[derive(Resource)]
pub struct RenderState {
    // TODO: Make a stripped-down version of this
    state: ClientState,
    kind: RenderConnectionKind,
}

impl ExtractResource for RenderState {
    type Source = Connection;

    fn extract_resource(source: &Self::Source) -> Self {
        let Connection { state, kind } = source;

        RenderState {
            state: state.clone(),
            kind: match kind {
                ConnectionKind::Server { .. } => RenderConnectionKind::Server,
                ConnectionKind::Demo(_) => RenderConnectionKind::Demo,
            },
        }
    }
}

#[derive(Resource)]
pub struct GraphicsState {
    initial_pass_target: InitialPassTarget,
    deferred_pass_target: DeferredPassTarget,
    final_pass_target: FinalPassTarget,

    world_bind_group_layouts: Vec<BindGroupLayout>,
    world_bind_groups: Vec<BindGroup>,

    frame_uniform_buffer: Buffer,

    // TODO: This probably doesn't need to be a rwlock
    entity_uniform_buffer: RwLock<DynamicUniformBuffer<EntityUniforms>>,

    diffuse_sampler: Sampler,
    nearest_sampler: Sampler,
    lightmap_sampler: Sampler,

    sample_count: u32,

    alias_pipeline: AliasPipeline,
    brush_pipeline: BrushPipeline,
    sprite_pipeline: SpritePipeline,
    deferred_pipeline: DeferredPipeline,
    particle_pipeline: ParticlePipeline,
    postprocess_pipeline: PostProcessPipeline,
    glyph_pipeline: GlyphPipeline,
    quad_pipeline: QuadPipeline,
    blit_pipeline: BlitPipeline,

    default_lightmap: Texture,
    default_lightmap_view: TextureView,

    palette: Palette,
    gfx_wad: Wad,
}

impl FromWorld for GraphicsState {
    fn from_world(world: &mut World) -> Self {
        let vfs = world.resource::<Vfs>();
        let cvars = world.resource::<CvarRegistry>();
        let render_resolution = world.resource::<RenderResolution>();
        let device = world.resource::<RenderDevice>();
        let queue = world.resource::<RenderQueue>();

        let mut sample_count = cvars.get_value("r_msaa_samples").unwrap_or(2.0) as u32;
        if !&[2, 4].contains(&sample_count) {
            sample_count = 2;
        }
        // TODO: Reimplement MSAA
        sample_count = 1;

        match GraphicsState::new(
            &*device,
            &*queue,
            Extent2d {
                width: render_resolution.0,
                height: render_resolution.1,
            },
            sample_count,
            &*vfs,
        ) {
            Ok(state) => state,
            Err(e) => {
                warn!("Failed to create graphics state: {}", e);
                todo!();
            }
        }
    }
}

thread_local! {
    static COMPILER: RefCell<shaderc::Compiler> = shaderc::Compiler::new().unwrap().into();
}

impl GraphicsState {
    pub fn new(
        device: &RenderDevice,
        queue: &RenderQueue,
        size: Extent2d,
        sample_count: u32,
        vfs: &Vfs,
    ) -> Result<GraphicsState, Error> {
        let palette = Palette::load(&vfs, "gfx/palette.lmp");
        let gfx_wad = Wad::load(vfs.open("gfx.wad")?).unwrap();

        let initial_pass_target = InitialPassTarget::new(device, size, sample_count);
        let deferred_pass_target = DeferredPassTarget::new(device, size, sample_count);
        let final_pass_target = FinalPassTarget::new(device, size);

        let frame_uniform_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame uniform buffer"),
            size: size_of::<world::FrameUniforms>() as wgpu::BufferAddress,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let entity_uniform_buffer = DynamicUniformBuffer::new(device);

        let diffuse_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: None,
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            // TODO: these are the OpenGL defaults; see if there's a better choice for us
            lod_max_clamp: 1000.0,
            compare: None,
            ..Default::default()
        });

        let nearest_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: None,
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            address_mode_w: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            // TODO: these are the OpenGL defaults; see if there's a better choice for us
            lod_max_clamp: 1000.0,
            compare: None,
            ..Default::default()
        });

        let lightmap_sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: None,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::FilterMode::Linear,
            // TODO: these are the OpenGL defaults; see if there's a better choice for us
            lod_max_clamp: 1000.0,
            compare: None,
            ..Default::default()
        });

        let world_bind_group_layouts: Vec<BindGroupLayout> = world::BIND_GROUP_LAYOUT_DESCRIPTORS
            .iter()
            .map(|desc| device.create_bind_group_layout(None, desc))
            .collect();
        let world_bind_groups = vec![
            device.create_bind_group(
                Some("per-frame bind group"),
                &world_bind_group_layouts[world::BindGroupLayoutId::PerFrame as usize],
                &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &frame_uniform_buffer,
                        offset: 0,
                        size: None,
                    }),
                }],
            ),
            device.create_bind_group(
                Some("brush per-entity bind group"),
                &world_bind_group_layouts[world::BindGroupLayoutId::PerEntity as usize],
                &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                            buffer: &entity_uniform_buffer.buffer(),
                            offset: 0,
                            size: Some(
                                NonZeroU64::new(size_of::<EntityUniforms>() as u64).unwrap(),
                            ),
                        }),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&diffuse_sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&lightmap_sampler),
                    },
                ],
            ),
        ];

        let (
            alias_pipeline,
            brush_pipeline,
            sprite_pipeline,
            deferred_pipeline,
            particle_pipeline,
            quad_pipeline,
            glyph_pipeline,
            postprocess_pipeline,
            blit_pipeline,
        ) = COMPILER.with_borrow_mut(|compiler| {
            let alias_pipeline =
                AliasPipeline::new(device, compiler, &world_bind_group_layouts, sample_count);
            let brush_pipeline =
                BrushPipeline::new(device, compiler, &world_bind_group_layouts, sample_count);
            let sprite_pipeline =
                SpritePipeline::new(device, compiler, &world_bind_group_layouts, sample_count);
            let deferred_pipeline = DeferredPipeline::new(device, compiler, sample_count);
            let particle_pipeline =
                ParticlePipeline::new(device, &queue, compiler, sample_count, &palette);
            let quad_pipeline =
                QuadPipeline::new(device, compiler, DIFFUSE_ATTACHMENT_FORMAT, sample_count);
            let glyph_pipeline =
                GlyphPipeline::new(device, compiler, DIFFUSE_ATTACHMENT_FORMAT, sample_count);

            let postprocess_pipeline = PostProcessPipeline::new(
                device,
                compiler,
                final_pass_target.format(),
                final_pass_target.sample_count(),
            );

            let blit_pipeline = BlitPipeline::new(
                device,
                compiler,
                final_pass_target.resolve_view(),
                final_pass_target.format(),
            );

            (
                alias_pipeline,
                brush_pipeline,
                sprite_pipeline,
                deferred_pipeline,
                particle_pipeline,
                quad_pipeline,
                glyph_pipeline,
                postprocess_pipeline,
                blit_pipeline,
            )
        });

        let default_lightmap = create_texture(
            device,
            queue,
            None,
            1,
            1,
            &TextureData::Lightmap(LightmapData {
                lightmap: (&[0xFF][..]).into(),
            }),
        );
        let default_lightmap_view = default_lightmap.create_view(&Default::default());

        Ok(GraphicsState {
            initial_pass_target,
            deferred_pass_target,
            final_pass_target,
            frame_uniform_buffer,
            entity_uniform_buffer: entity_uniform_buffer.into(),

            world_bind_group_layouts,
            world_bind_groups,

            sample_count,

            alias_pipeline,
            brush_pipeline,
            sprite_pipeline,
            deferred_pipeline,
            particle_pipeline,
            postprocess_pipeline,
            glyph_pipeline,
            quad_pipeline,
            blit_pipeline,

            diffuse_sampler,
            nearest_sampler,
            lightmap_sampler,

            default_lightmap,
            default_lightmap_view,
            palette,
            gfx_wad,
        })
    }

    pub fn set_format(&mut self, format: wgpu::TextureFormat) {
        self.blit_pipeline.set_format(format);
    }

    pub fn format(&self) -> wgpu::TextureFormat {
        self.blit_pipeline.format()
    }

    pub fn create_texture<'a>(
        &self,
        device: &RenderDevice,
        queue: &RenderQueue,
        label: Option<&'a str>,
        width: u32,
        height: u32,
        data: &TextureData,
    ) -> Texture {
        create_texture(device, queue, label, width, height, data)
    }

    /// Update graphics state with the new framebuffer size and sample count.
    ///
    /// If the framebuffer size has changed, this recreates all render targets with the new size.
    ///
    /// If the framebuffer sample count has changed, this recreates all render targets with the
    /// new sample count and rebuilds the render pipelines to output that number of samples.
    pub fn update(&mut self, device: &RenderDevice, size: Extent2d, sample_count: u32) {
        if self.sample_count != sample_count {
            self.sample_count = sample_count;
            self.recreate_pipelines(device, sample_count);
        }

        if self.initial_pass_target.size() != size
            || self.initial_pass_target.sample_count() != sample_count
        {
            self.initial_pass_target = InitialPassTarget::new(device, size, sample_count);
        }

        if self.deferred_pass_target.size() != size
            || self.deferred_pass_target.sample_count() != sample_count
        {
            self.deferred_pass_target = DeferredPassTarget::new(device, size, sample_count);
        }

        if self.final_pass_target.size() != size
            || self.final_pass_target.sample_count() != sample_count
        {
            self.final_pass_target = FinalPassTarget::new(device, size);

            // TODO: How do we do the final pass?
            // COMPILER.with_borrow_mut(|compiler| {
            //     self.blit_pipeline
            //         .rebuild(device, compiler, self.deferred_pass_target.color_view());
            // });
        }
    }

    /// Rebuild all render pipelines using the new sample count.
    ///
    /// This must be called when the sample count of the render target(s) changes or the program
    /// will panic.
    fn recreate_pipelines(&mut self, device: &RenderDevice, sample_count: u32) {
        COMPILER.with_borrow_mut(|compiler| {
            self.alias_pipeline.rebuild(
                device,
                compiler,
                &self.world_bind_group_layouts,
                sample_count,
            );
            self.brush_pipeline.rebuild(
                device,
                compiler,
                &self.world_bind_group_layouts,
                sample_count,
            );
            self.sprite_pipeline.rebuild(
                device,
                compiler,
                &self.world_bind_group_layouts,
                sample_count,
            );
            self.deferred_pipeline
                .rebuild(device, compiler, sample_count);
            self.postprocess_pipeline
                .rebuild(device, compiler, sample_count);
            self.glyph_pipeline.rebuild(device, compiler, sample_count);
            self.quad_pipeline.rebuild(device, compiler, sample_count);
            self.blit_pipeline
                .rebuild(device, compiler, self.final_pass_target.resolve_view());
        });
    }

    pub fn initial_pass_target(&self) -> &InitialPassTarget {
        &self.initial_pass_target
    }

    pub fn deferred_pass_target(&self) -> &DeferredPassTarget {
        &self.deferred_pass_target
    }

    pub fn final_pass_target(&self) -> &FinalPassTarget {
        &self.final_pass_target
    }

    pub fn frame_uniform_buffer(&self) -> &Buffer {
        &self.frame_uniform_buffer
    }

    pub fn entity_uniform_buffer(
        &self,
    ) -> impl Deref<Target = DynamicUniformBuffer<EntityUniforms>> + '_ {
        self.entity_uniform_buffer.read()
    }

    pub fn entity_uniform_buffer_mut(
        &self,
    ) -> impl DerefMut<Target = DynamicUniformBuffer<EntityUniforms>> + '_ {
        self.entity_uniform_buffer.write()
    }

    pub fn diffuse_sampler(&self) -> &Sampler {
        &self.diffuse_sampler
    }

    pub fn nearest_sampler(&self) -> &Sampler {
        &self.nearest_sampler
    }

    pub fn default_lightmap(&self) -> &Texture {
        &self.default_lightmap
    }

    pub fn default_lightmap_view(&self) -> &TextureView {
        &self.default_lightmap_view
    }

    pub fn lightmap_sampler(&self) -> &Sampler {
        &self.lightmap_sampler
    }

    pub fn world_bind_group_layouts(&self) -> &[BindGroupLayout] {
        &self.world_bind_group_layouts
    }

    pub fn world_bind_groups(&self) -> &[BindGroup] {
        &self.world_bind_groups
    }

    // pipelines

    pub fn alias_pipeline(&self) -> &AliasPipeline {
        &self.alias_pipeline
    }

    pub fn brush_pipeline(&self) -> &BrushPipeline {
        &self.brush_pipeline
    }

    pub fn sprite_pipeline(&self) -> &SpritePipeline {
        &self.sprite_pipeline
    }

    pub fn deferred_pipeline(&self) -> &DeferredPipeline {
        &self.deferred_pipeline
    }

    pub fn particle_pipeline(&self) -> &ParticlePipeline {
        &self.particle_pipeline
    }

    pub fn postprocess_pipeline(&self) -> &PostProcessPipeline {
        &self.postprocess_pipeline
    }

    pub fn glyph_pipeline(&self) -> &GlyphPipeline {
        &self.glyph_pipeline
    }

    pub fn quad_pipeline(&self) -> &QuadPipeline {
        &self.quad_pipeline
    }

    pub fn palette(&self) -> &Palette {
        &self.palette
    }

    pub fn gfx_wad(&self) -> &Wad {
        &self.gfx_wad
    }

    pub fn blit_pipeline(&self) -> &BlitPipeline {
        &self.blit_pipeline
    }
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, RenderLabel)]
struct ClientRenderLabel;

struct ClientRenderer {
    start_time: DateTime<Utc>,
    view_query: QueryState<&'static ViewTarget>,
}

impl FromWorld for ClientRenderer {
    fn from_world(world: &mut World) -> Self {
        Self {
            view_query: world.query(),
            start_time: Utc::now(),
        }
    }
}

#[derive(Resource)]
pub struct Fov(pub Deg<f32>);

impl Node for ClientRenderer {
    fn update(&mut self, world: &mut World) {
        self.view_query.update_archetypes(world);
        let Ok(target) = self.view_query.get_single(world) else {
            return;
        };
        let format = target.main_texture_format();
        let world = world.cell();
        let Some(mut gfx_state) = world.get_resource_mut::<GraphicsState>() else {
            return;
        };

        if format != gfx_state.format() {
            let device = world.resource::<RenderDevice>();
            gfx_state.set_format(format);
            gfx_state.recreate_pipelines(&*device, 1);
        }
    }

    fn run<'w>(
        &self,
        graph: &mut bevy::render::render_graph::RenderGraphContext,
        render_context: &mut bevy::render::renderer::RenderContext<'w>,
        world: &'w bevy::prelude::World,
    ) -> Result<(), bevy::render::render_graph::NodeRunError> {
        thread_local! {
            static BUMP: RefCell<Bump> =Bump::new().into();
        }

        let renderer = world.get_resource::<WorldRenderer>();
        let conn = world.get_resource::<RenderState>();
        let queue = world.resource::<RenderQueue>();
        let device = world.resource::<RenderDevice>();
        let gfx_state = world.resource::<GraphicsState>();
        let postprocess_renderer = world.resource::<PostProcessRenderer>();
        let ui_renderer = world.resource::<UiRenderer>();
        let console = world.get_resource::<Console>();
        let Some(&RenderResolution(width, height)) = world.get_resource::<RenderResolution>()
        else {
            return Ok(());
        };
        let menu = world.get_resource::<Menu>();
        let focus = world.resource::<InputFocus>();
        let fov = world.resource::<Fov>();

        let Some(target) = graph
            .get_view_entity()
            .and_then(|e| self.view_query.get_manual(world, e).ok())
        else {
            return Ok(());
        };

        // TODO: Cache
        let deferred_renderer = DeferredRenderer::new(
            gfx_state,
            device,
            graph.get_input_texture(InitPassOutput::Diffuse)?,
            graph.get_input_texture(InitPassOutput::Normal)?,
            graph.get_input_texture(InitPassOutput::Light)?,
            graph.get_input_texture(InitPassOutput::Depth)?,
        );

        let encoder = render_context.command_encoder();

        BUMP.with_borrow_mut(|bump| bump.reset());
        BUMP.with_borrow(|bump| {
            // quad_commands must outlive final pass
            let mut quad_commands = Vec::new();
            let mut glyph_commands = Vec::new();

            if let (
                Some(RenderState {
                    state: cl_state,
                    kind,
                }),
                Some(world),
            ) = (conn, renderer)
            {
                // if client is fully connected, draw world
                let camera = match kind {
                    RenderConnectionKind::Demo => {
                        cl_state.demo_camera(width as f32 / height as f32, fov.0)
                    }
                    RenderConnectionKind::Server => {
                        cl_state.camera(width as f32 / height as f32, fov.0)
                    }
                };

                // deferred lighting pass
                {
                    let deferred_pass_builder =
                        gfx_state.deferred_pass_target().render_pass_builder();
                    let mut deferred_pass =
                        encoder.begin_render_pass(&deferred_pass_builder.descriptor());

                    let mut lights = [PointLight {
                        origin: Vector3::zero(),
                        radius: 0.0,
                    }; MAX_LIGHTS];

                    let mut light_count = 0;
                    for (light_id, light) in cl_state.iter_lights().enumerate() {
                        light_count += 1;
                        let light_origin = light.origin();
                        let converted_origin =
                            Vector3::new(-light_origin.y, light_origin.z, -light_origin.x);
                        lights[light_id].origin =
                            (camera.view() * converted_origin.extend(1.0)).truncate();
                        lights[light_id].radius = light.radius(cl_state.time());
                    }

                    let uniforms = DeferredUniforms {
                        inv_projection: camera.inverse_projection().into(),
                        light_count,
                        _pad: [0; 3],
                        lights,
                    };

                    deferred_renderer.record_draw(gfx_state, queue, &mut deferred_pass, uniforms);
                }
            }

            // final render pass: postprocess the world and draw the UI
            {
                let final_pass_builder = gfx_state.final_pass_target().render_pass_builder();
                let mut final_pass = encoder.begin_render_pass(&final_pass_builder.descriptor());

                if let Some(RenderState {
                    state: cl_state, ..
                }) = conn
                {
                    // only postprocess if client is in the game
                    if renderer.is_some() {
                        postprocess_renderer.record_draw(
                            &*gfx_state,
                            queue,
                            &mut final_pass,
                            cl_state.color_shift(),
                        );
                    }

                    let ui_state = match conn {
                        Some(RenderState {
                            state: cl_state, ..
                        }) => UiState::InGame {
                            hud: match cl_state.intermission() {
                                Some(kind) => HudState::Intermission {
                                    kind,
                                    completion_duration: cl_state.completion_time().unwrap()
                                        - cl_state.start_time(),
                                    stats: cl_state.stats(),
                                    console,
                                },

                                None => HudState::InGame {
                                    items: cl_state.items(),
                                    item_pickup_time: cl_state.item_pickup_times(),
                                    stats: cl_state.stats(),
                                    face_anim_time: cl_state.face_anim_time(),
                                    console,
                                },
                            },

                            overlay: match (focus, console, menu) {
                                (InputFocus::Game, _, _) => None,
                                (InputFocus::Console, Some(console), _) => {
                                    Some(UiOverlay::Console(console))
                                }
                                (InputFocus::Menu, _, Some(menu)) => Some(UiOverlay::Menu(menu)),
                                _ => None,
                            },
                        },

                        None => UiState::Title {
                            overlay: match (focus, console, menu) {
                                (InputFocus::Console, Some(console), _) => {
                                    UiOverlay::Console(console)
                                }
                                (InputFocus::Menu, _, Some(menu)) => UiOverlay::Menu(menu),
                                (InputFocus::Game, _, _) => unreachable!(),
                                _ => return,
                            },
                        },
                    };

                    let elapsed = self.elapsed(conn.map(|c| c.state.time));
                    ui_renderer.render_pass(
                        &*gfx_state,
                        queue,
                        &mut final_pass,
                        Extent2d { width, height },
                        // use client time when in game, renderer time otherwise
                        elapsed,
                        &ui_state,
                        &mut quad_commands,
                        &mut glyph_commands,
                    );
                }
            }
        });

        {
            let attachment = target.main_texture_view();

            let swap_chain_target = SwapChainTarget::with_swap_chain_view(attachment);
            let blit_pass_builder = swap_chain_target.render_pass_builder();
            let mut blit_pass = render_context
                .command_encoder()
                .begin_render_pass(&blit_pass_builder.descriptor());
            gfx_state.blit_pipeline().blit(&*gfx_state, &mut blit_pass);
        }

        Ok(())
    }

    fn input(&self) -> Vec<SlotInfo> {
        InitPass.output()
    }
}

impl ClientRenderer {
    pub fn elapsed(&self, time: Option<Duration>) -> Duration {
        match time {
            Some(time) => time,
            None => Utc::now().signed_duration_since(self.start_time),
        }
    }
}

mod systems {
    use super::*;

    pub fn update_renderers(
        gfx_state: Res<GraphicsState>,
        device: Res<RenderDevice>,
        mut deferred_renderer: ResMut<DeferredRenderer>,
        mut postprocess_renderer: ResMut<PostProcessRenderer>,
    ) {
        deferred_renderer.rebuild(
            &*gfx_state,
            &*device,
            gfx_state.initial_pass_target().diffuse_view(),
            gfx_state.initial_pass_target().normal_view(),
            gfx_state.initial_pass_target().light_view(),
            gfx_state.initial_pass_target().depth_view(),
        );
        postprocess_renderer.rebuild(
            &*gfx_state,
            &*device,
            gfx_state.deferred_pass_target().color_view(),
        );
    }

    pub fn recreate_graphics_state(
        mut gfx_state: ResMut<GraphicsState>,
        mut commands: Commands,
        device: Res<RenderDevice>,
        queue: Res<RenderQueue>,
        render_resolution: Res<RenderResolution>,
        cvars: Res<CvarRegistry>,
        vfs: Res<Vfs>,
    ) {
        let mut sample_count = cvars.get_value("r_msaa_samples").unwrap_or(2.0) as u32;
        if !&[2, 4].contains(&sample_count) {
            sample_count = 2;
        }
        // TODO: Reimplement MSAA
        sample_count = 1;

        match GraphicsState::new(
            &*device,
            &*queue,
            Extent2d {
                width: render_resolution.0,
                height: render_resolution.1,
            },
            sample_count,
            &*vfs,
        ) {
            Ok(state) => {
                *gfx_state = state;
            }
            Err(e) => {
                warn!("Failed to create graphics state: {}", e);
                commands.remove_resource::<GraphicsState>();
            }
        }
    }
}
