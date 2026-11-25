use pubky_dispatch_client::store::MemoryStore;
use pubky_dispatch_harness::World;
use pubky_dispatch_signer::{Protocols, Signer};
use std::{error::Error, sync::Arc};

pub(super) async fn serving(world: &World, name: &str) -> Result<Signer, Box<dyn Error>> {
    let id = world.identity(name)?;
    let origin = format!("https://{name}.payments.test");
    world.add_switch(&origin, &[&id]).await?;
    let mut env = world.signer_env(&id, Arc::new(MemoryStore::new()))?;
    env.protocols = Protocols::new(vec![Arc::new(paykit_lib::signer::PaykitProtocol)])?;
    let mut signer = Signer::create(env, world.keyring(&id)?, &origin).await?;
    signer.resume().await?;
    Ok(signer)
}
