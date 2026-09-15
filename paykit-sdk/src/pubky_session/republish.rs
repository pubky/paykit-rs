use pubky::pkarr::{errors::ResolveError, ResolvePolicy, SignedPacket};

use super::{map_pubky_identity_error, PubkySessionBootstrap};
use crate::{PubkyPublicKey, Result};

impl PubkySessionBootstrap {
    /// Rebroadcast the newest signed identity record found on the configured
    /// PKARR networks or in their caches, without changing or signing it.
    ///
    /// Returns `true` if at least one publishing backend accepted the record,
    /// or `false` if no record was found. Operational failures return errors.
    /// No secret key or session is required. Cached records can be used when
    /// discovery fails, but a reported newer invalid DHT item is not overwritten.
    /// Missing records are never reconstructed.
    ///
    /// This is a one-shot operation, not a background republisher. Reuse this
    /// helper to retain its client cache. Scheduling, throttling, retries and
    /// timeouts belong to the caller/Pubky client; session creation, capability
    /// scope and key rotation remain the caller's responsibility.
    pub async fn republish_identity(&self, public_key: &PubkyPublicKey) -> Result<bool> {
        let public_key = public_key.to_public_key()?;
        let pkarr = self.pubky.client().pkarr();
        // Read relay caches before network resolution can populate the local cache.
        let cached = pkarr.resolve(&public_key, ResolvePolicy::CacheOnly).await;
        let network = pkarr.resolve(&public_key, ResolvePolicy::NetworkOnly).await;
        let packet = match newest_identity_packet(network, cached) {
            Ok(packet) => packet,
            Err(ResolveError::NotFound) => return Ok(false),
            Err(err) => {
                return Err(map_pubky_identity_error(
                    "resolve Pubky identity record",
                    err.into(),
                ));
            }
        };
        pkarr.publish(&packet).await.map_err(|err| {
            map_pubky_identity_error("republish Pubky identity record", err.into())
        })?;
        Ok(true)
    }
}

fn newest_identity_packet(
    network: std::result::Result<SignedPacket, ResolveError>,
    cached: std::result::Result<SignedPacket, ResolveError>,
) -> std::result::Result<SignedPacket, ResolveError> {
    match (network, cached) {
        (Ok(network), Ok(cached)) if cached.more_recent_than(&network) => Ok(cached),
        (Err(ResolveError::InvalidSignedPacket { seq }), Ok(cached))
            if cached.timestamp().as_u64() as i64 >= seq =>
        {
            Ok(cached)
        }
        (Err(err @ ResolveError::InvalidSignedPacket { .. }), _) => Err(err),
        (Ok(packet), _) | (_, Ok(packet)) => Ok(packet),
        (Err(ResolveError::NotFound), Err(err)) | (Err(err), _) => Err(err),
    }
}

#[cfg(test)]
mod tests;
