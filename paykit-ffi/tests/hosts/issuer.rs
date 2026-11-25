//! Return a genuine approval for the generated-language consumer's own signed request.
use base64::{engine::general_purpose::STANDARD, Engine};
use pubky_dispatch_core::{
    records::{AppRequest, AuthRequest},
    sig::SignedDoc,
};
use std::error::Error;
mod backup;
mod service;
mod signer;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("paid-wallet") => return backup::print_wallet(false).await,
        Some("seller-wallet") => return backup::print_wallet(true).await,
        Some("wallet-awaiting-identity") => return backup::print_resume_refusal(false).await,
        Some("departed-wallet") => return backup::print_resume_refusal(true).await,
        Some("notice-wallet") => return backup::print_notice(false).await,
        Some("cleared-wallet") => return backup::print_notice(true).await,
        _ => {}
    }
    let signed: SignedDoc = serde_json::from_reader(std::io::stdin())?;
    let request = AppRequest::verify(&signed)?;
    let auth = AuthRequest {
        client_id: request.body().client_id.clone(),
        client_key: request.body().client_key,
        caps: request.body().caps.clone(),
        secret: [9; 32],
    };
    let world = service::start()?;
    let identity = world.identity("buyer")?.public();
    let mut signer = signer::serving(&world, "buyer").await?;
    let review = signer.review(&signed, &auth)?;
    let approved = signer.approve(review).await?;
    println!("{}", STANDARD.encode(identity));
    println!("{}", STANDARD.encode(approved.claim));
    world.close().await?;
    Ok(())
}
