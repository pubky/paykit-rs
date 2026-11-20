use pubky_switch_harness::{Backend, World};
use std::error::Error;

pub(super) fn start() -> Result<World, Box<dyn Error>> {
    let backend = if std::env::var_os("PUBKY_SWITCH_REQUIRE_DB").is_some() {
        Backend::Service
    } else {
        Backend::Scripted
    };
    World::start(backend)?.ok_or_else(|| "required wallet test service unavailable".into())
}
