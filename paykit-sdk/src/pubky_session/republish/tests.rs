use std::{net::Ipv4Addr, num::NonZeroUsize, sync::Arc, time::Duration};

use pubky::{
    pkarr::{dns::rdata::SVCB, Cache, InMemoryCache, Keypair},
    Pubky, PubkyHttpClient,
};

use super::*;
use crate::PaykitSdkError;

fn identity_packet(keypair: &Keypair, timestamp: u64) -> SignedPacket {
    SignedPacket::builder()
        .timestamp(timestamp.into())
        .https(
            "_pubky".try_into().unwrap(),
            SVCB::new(
                0,
                format!("hs{timestamp}.example")
                    .as_str()
                    .try_into()
                    .unwrap(),
            ),
            3600,
        )
        .txt(
            "extra".try_into().unwrap(),
            "preserve me".try_into().unwrap(),
            3600,
        )
        .sign(keypair)
        .unwrap()
}

fn testnet_pubky(
    testnet: &mainline::Testnet,
    cache: Arc<InMemoryCache>,
    relay: Option<&url::Url>,
) -> Pubky {
    let client = PubkyHttpClient::builder()
        .pkarr(|builder| {
            builder
                .no_default_network()
                .bootstrap(&testnet.bootstrap)
                .dht(|config| {
                    config.bind_address = Some(Ipv4Addr::LOCALHOST);
                    config
                })
                .cache(cache)
                .request_timeout(Duration::from_secs(1));
            if let Some(relay) = relay {
                builder.no_dht().relays(&[relay.as_str()]).unwrap();
            }
            builder
        })
        .build()
        .unwrap();
    Pubky::with_client(client)
}

fn empty_cache() -> Arc<InMemoryCache> {
    Arc::new(InMemoryCache::new(NonZeroUsize::new(8).unwrap()))
}

#[tokio::test]
async fn test_republish_identity_preserves_newest_packet_on_dht_and_relay() {
    let testnet = mainline::Testnet::builder(5).build().unwrap();
    let relay = pkarr_relay::Relay::run_test(&testnet.bootstrap)
        .await
        .unwrap();
    let relay_url = relay.local_url();
    for relay in [None, Some(&relay_url)] {
        for (cached_timestamp, network_timestamp) in [
            (Some(10), None),
            (None, Some(10)),
            (Some(10), Some(20)),
            (Some(20), Some(10)),
        ] {
            let keypair = Keypair::random();
            let public_key = keypair.public_key();
            let cache = empty_cache();
            if let Some(timestamp) = cached_timestamp {
                cache.put(
                    &public_key.clone().into(),
                    &identity_packet(&keypair, timestamp),
                );
            }
            if let Some(timestamp) = network_timestamp {
                testnet_pubky(&testnet, empty_cache(), relay)
                    .client()
                    .pkarr()
                    .publish(&identity_packet(&keypair, timestamp))
                    .await
                    .unwrap();
            }
            let expected =
                identity_packet(&keypair, cached_timestamp.max(network_timestamp).unwrap());
            let bootstrap = PubkySessionBootstrap::with_pubky(
                testnet_pubky(&testnet, cache, relay),
                "paykit.test",
            )
            .unwrap();

            assert!(bootstrap
                .republish_identity(&PubkyPublicKey::new(public_key.to_z32()).unwrap())
                .await
                .unwrap());
            let resolved = testnet_pubky(&testnet, empty_cache(), None)
                .client()
                .pkarr()
                .resolve(&public_key, ResolvePolicy::NetworkOnly)
                .await
                .unwrap();
            assert_eq!(resolved.as_bytes(), expected.as_bytes());
        }
    }
    relay.shutdown();
}

#[tokio::test]
async fn test_republish_identity_missing_record_is_not_created() {
    let testnet = mainline::Testnet::builder(5).build().unwrap();
    let bootstrap = PubkySessionBootstrap::with_pubky(
        testnet_pubky(&testnet, empty_cache(), None),
        "paykit.test",
    )
    .unwrap();
    let public_key = PubkyPublicKey::new(Keypair::random().public_key().to_z32()).unwrap();

    assert!(!bootstrap.republish_identity(&public_key).await.unwrap());
    assert_eq!(
        testnet_pubky(&testnet, empty_cache(), None)
            .client()
            .pkarr()
            .resolve(
                &public_key.to_public_key().unwrap(),
                ResolvePolicy::NetworkOnly
            )
            .await,
        Err(ResolveError::NotFound)
    );
}

#[tokio::test]
async fn test_republish_identity_reports_resolution_and_publication_failures() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let relay = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let keypair = Keypair::random();
    for cached in [false, true] {
        let cache = empty_cache();
        if cached {
            cache.put(&keypair.public_key().into(), &identity_packet(&keypair, 10));
        }
        let client = PubkyHttpClient::builder()
            .pkarr(|builder| {
                builder
                    .no_default_network()
                    .relays(&[&relay])
                    .unwrap()
                    .cache(cache)
                    .request_timeout(Duration::from_millis(100))
            })
            .build()
            .unwrap();
        let bootstrap =
            PubkySessionBootstrap::with_pubky(Pubky::with_client(client), "paykit.test").unwrap();

        let error = bootstrap
            .republish_identity(&PubkyPublicKey::new(keypair.public_key().to_z32()).unwrap())
            .await
            .unwrap_err();

        let PaykitSdkError::Identity { context, source } = error else {
            panic!("expected an identity error");
        };
        assert_eq!(
            context,
            if cached {
                "republish Pubky identity record"
            } else {
                "resolve Pubky identity record"
            }
        );
        assert!(source.is_some());
    }
}

#[test]
fn test_republish_identity_does_not_hide_resolution_errors() {
    let invalid = ResolveError::InvalidSignedPacket { seq: 20 };
    let keypair = Keypair::random();
    let packet = identity_packet(&keypair, 10);
    for (network, cached, expected) in [
        (Err(invalid.clone()), Ok(packet.clone()), Err(invalid)),
        (
            Err(ResolveError::InvalidSignedPacket { seq: 9 }),
            Ok(packet.clone()),
            Ok(packet.clone()),
        ),
        (
            Err(ResolveError::InvalidSignedPacket { seq: 10 }),
            Ok(packet.clone()),
            Ok(packet),
        ),
        (
            Err(ResolveError::NotFound),
            Err(ResolveError::NoResponses),
            Err(ResolveError::NoResponses),
        ),
        (
            Err(ResolveError::NoResponses),
            Err(ResolveError::NotFound),
            Err(ResolveError::NoResponses),
        ),
    ] {
        assert_eq!(newest_identity_packet(network, cached), expected);
    }
}
