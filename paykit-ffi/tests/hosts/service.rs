use pubky_switch_harness::{Backend, World};
use std::{error::Error, io::Write};

pub(super) fn start() -> Result<World, Box<dyn Error>> {
    let backend = if std::env::var_os("PUBKY_SWITCH_REQUIRE_DB").is_some() {
        Backend::Service
    } else {
        Backend::Scripted
    };
    let world = World::start(backend)?.ok_or("required wallet test service unavailable")?;
    if world.backend() != backend {
        return Err("wallet test started with a different backend".into());
    }
    let name = match world.backend() {
        Backend::Service => "PostgreSQL",
        Backend::Scripted => "Scripted",
    };
    writeln!(std::io::stderr().lock(), "Wallet test backend: {name}")?;
    Ok(world)
}
