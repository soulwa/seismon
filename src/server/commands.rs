use std::path::PathBuf;

use bevy::prelude::*;
use clap::Parser;
use failure::Error;

use crate::{
    client::{input::InputFocus, Connection, ConnectionState},
    common::{
        console::{ExecResult, RegisterCmdExt},
        net::SignOnStage,
    },
};

use super::*;

pub fn register_commands(app: &mut App) {
    app.command(cmd_map.map(|res| -> ExecResult {
        if let Err(e) = res {
            format!("{}", e).into()
        } else {
            default()
        }
    }));
}

#[derive(Parser)]
#[command(name = "map", about = "Load and start a new map")]
struct Map {
    map_name: PathBuf,
}

fn cmd_map(
    In(Map { mut map_name }): In<Map>,
    mut commands: Commands,
    session: Option<ResMut<Session>>,
    mut focus: ResMut<InputFocus>,
    vfs: Res<Vfs>,
    mut registry: ResMut<Registry>,
) -> Result<(), Error> {
    if map_name.extension().is_none() {
        map_name.set_extension("bsp");
    }

    let mut path = PathBuf::from("maps");
    path.push(map_name);

    let bsp = vfs.open(format!("{}", path.display()))?;
    let (models, entmap) = crate::common::bsp::load(bsp)?;
    let progs = vfs.open("progs.dat")?;
    let progs = crate::server::progs::load(progs)?;

    if let Some(mut session) = session {
        session.state = SessionState::Loading;
        session.level = LevelState::new(progs, models, entmap, registry.reborrow(), &*vfs);
    } else {
        // TODO: Make `max_clients` a cvar
        commands.insert_resource(Session::new(
            8,
            registry.reborrow(),
            &*vfs,
            progs,
            models,
            entmap,
        ));
    }

    // TODO: This should not be handled here, server and client should be decoupled
    commands.insert_resource(Connection::new_server());
    commands.insert_resource(ConnectionState::SignOn(SignOnStage::Prespawn));
    *focus = InputFocus::Game;

    Ok(())
}
