pub mod console;
pub mod glyph;
pub mod hud;
pub mod layout;
pub mod menu;
pub mod quad;

use crate::{
    client::{
        menu::Menu,
        render::{
            ui::{
                console::ConsoleRenderer,
                glyph::{GlyphRenderer, GlyphRendererCommand},
                hud::{HudRenderer, HudState},
                menu::MenuRenderer,
                quad::{QuadRenderer, QuadRendererCommand},
            },
            Extent2d, GraphicsState,
        },
    },
    common::{console::Console, vfs::Vfs},
};

use bevy::{
    ecs::{system::Resource, world::FromWorld},
    render::renderer::{RenderDevice, RenderQueue},
};
use cgmath::{Matrix4, Vector2};
use chrono::Duration;

pub fn screen_space_vertex_translate(
    display_w: u32,
    display_h: u32,
    pos_x: i32,
    pos_y: i32,
) -> Vector2<f32> {
    // rescale from [0, DISPLAY_*] to [-1, 1] (NDC)
    Vector2::new(
        (pos_x * 2 - display_w as i32) as f32 / display_w as f32,
        (pos_y * 2 - display_h as i32) as f32 / display_h as f32,
    )
}

pub fn screen_space_vertex_scale(
    display_w: u32,
    display_h: u32,
    quad_w: u32,
    quad_h: u32,
) -> Vector2<f32> {
    Vector2::new(
        (quad_w * 2) as f32 / display_w as f32,
        (quad_h * 2) as f32 / display_h as f32,
    )
}

pub fn screen_space_vertex_transform(
    display_w: u32,
    display_h: u32,
    quad_w: u32,
    quad_h: u32,
    pos_x: i32,
    pos_y: i32,
) -> Matrix4<f32> {
    let Vector2 { x: ndc_x, y: ndc_y } =
        screen_space_vertex_translate(display_w, display_h, pos_x, pos_y);

    let Vector2 {
        x: scale_x,
        y: scale_y,
    } = screen_space_vertex_scale(display_w, display_h, quad_w, quad_h);

    Matrix4::from_translation([ndc_x, ndc_y, 0.0].into())
        * Matrix4::from_nonuniform_scale(scale_x, scale_y, 1.0)
}

pub enum UiOverlay<'a> {
    Menu(&'a Menu),
    Console(&'a Console),
}

pub enum UiState<'a> {
    Title {
        overlay: UiOverlay<'a>,
    },
    InGame {
        hud: HudState<'a>,
        overlay: Option<UiOverlay<'a>>,
    },
}

#[derive(Resource)]
pub struct UiRenderer {
    console_renderer: ConsoleRenderer,
    menu_renderer: MenuRenderer,
    hud_renderer: HudRenderer,
    glyph_renderer: GlyphRenderer,
    quad_renderer: QuadRenderer,
}

impl FromWorld for UiRenderer {
    fn from_world(world: &mut bevy::prelude::World) -> Self {
        let state = world.resource::<GraphicsState>();
        let vfs = world.resource::<Vfs>();
        let device = world.resource::<RenderDevice>();
        let queue = world.resource::<RenderQueue>();
        let menu = world.resource::<Menu>();

        UiRenderer::new(state, vfs, device, queue, menu)
    }
}

impl UiRenderer {
    pub fn new(
        state: &GraphicsState,
        vfs: &Vfs,
        device: &RenderDevice,
        queue: &RenderQueue,
        menu: &Menu,
    ) -> UiRenderer {
        UiRenderer {
            console_renderer: ConsoleRenderer::new(state, vfs, device, queue),
            menu_renderer: MenuRenderer::new(state, vfs, device, queue, menu),
            hud_renderer: HudRenderer::new(state, vfs, device, queue),
            glyph_renderer: GlyphRenderer::new(state, device, queue),
            quad_renderer: QuadRenderer::new(state, device),
        }
    }

    pub fn render_pass<'this, 'a>(
        &'this self,
        state: &'this GraphicsState,
        queue: &'a RenderQueue,
        pass: &'a mut wgpu::RenderPass<'this>,
        target_size: Extent2d,
        time: Duration,
        ui_state: &'a UiState<'this>,
        quad_commands: &'a mut Vec<QuadRendererCommand<'this>>,
        glyph_commands: &'a mut Vec<GlyphRendererCommand>,
    ) {
        let (hud_state, overlay) = match ui_state {
            UiState::Title { overlay } => (None, Some(overlay)),
            UiState::InGame { hud, overlay } => (Some(hud), overlay.as_ref()),
        };

        if let Some(hstate) = hud_state {
            self.hud_renderer
                .generate_commands(hstate, time, quad_commands, glyph_commands);
        }

        if let Some(o) = overlay {
            match o {
                UiOverlay::Menu(menu) => {
                    self.menu_renderer
                        .generate_commands(menu, time, quad_commands, glyph_commands);
                }
                UiOverlay::Console(console) => {
                    // TODO: take in-game console proportion as cvar
                    let proportion = match hud_state {
                        Some(_) => 0.33,
                        None => 1.0,
                    };

                    self.console_renderer.generate_commands(
                        console,
                        time,
                        quad_commands,
                        glyph_commands,
                        proportion,
                    );
                }
            }
        }

        self.quad_renderer
            .update_uniforms(state, queue, target_size, quad_commands);
        self.quad_renderer
            .record_draw(state, queue, pass, quad_commands);
        self.glyph_renderer
            .record_draw(state, queue, pass, target_size, glyph_commands);
    }
}
