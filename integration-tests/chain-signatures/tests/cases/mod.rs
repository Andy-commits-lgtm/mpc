use std::str::FromStr;

use crate::actions::{self, add_latency, wait_for};
use crate::with_multichain_nodes;

use cait_sith::protocol::Participant;
use cait_sith::triples::{TriplePub, TripleShare};
use cait_sith::PresignOutput;
use crypto_shared::{self, derive_epsilon, derive_key, x_coordinate, ScalarExt};
use deadpool_redis::Runtime;
use elliptic_curve::CurveArithmetic;
use integration_tests_chain_signatures::containers::{self, DockerClient};
use integration_tests_chain_signatures::MultichainConfig;
use k256::elliptic_curve::point::AffineCoordinates;
use k256::Secp256k1;
use mpc_contract::config::Config;
use mpc_contract::update::ProposeUpdateArgs;
use mpc_node::kdf::into_eth_sig;
use mpc_node::protocol::presignature::{Presignature, PresignatureId, PresignatureManager};
use mpc_node::protocol::triple::{Triple, TripleManager};
use mpc_node::storage;
use mpc_node::util::NearPublicKeyExt;
use near_account_id::AccountId;
use test_log::test;
use url::Url;

pub mod nightly;

#[test(tokio::test)]
async fn test_multichain_reshare() -> anyhow::Result<()> {
    let config = MultichainConfig::default();
    with_multichain_nodes(config.clone(), |mut ctx| {
        Box::pin(async move {
            let state = wait_for::running_mpc(&ctx, Some(0)).await?;
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_signature_production(&ctx, &state).await?;

            tracing::info!("!!! Add participant 3");
            assert!(ctx.add_participant(None).await.is_ok());
            let state = wait_for::running_mpc(&ctx, None).await?;
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_signature_production(&ctx, &state).await?;

            tracing::info!("!!! Remove participant 0 and participant 2");
            let account_2 = near_workspaces::types::AccountId::from_str(
                state.participants.keys().nth(2).unwrap().clone().as_ref(),
            )
            .unwrap();
            assert!(ctx.remove_participant(Some(&account_2)).await.is_ok());
            let account_0 = near_workspaces::types::AccountId::from_str(
                state.participants.keys().next().unwrap().clone().as_ref(),
            )
            .unwrap();
            let node_cfg_0 = ctx.remove_participant(Some(&account_0)).await;
            assert!(node_cfg_0.is_ok());
            let node_cfg_0 = node_cfg_0.unwrap();
            let state = wait_for::running_mpc(&ctx, None).await?;
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_signature_production(&ctx, &state).await?;

            tracing::info!("!!! Try remove participant 3, should fail due to threshold");
            assert!(ctx.remove_participant(None).await.is_err());

            tracing::info!("!!! Add participant 5");
            assert!(ctx.add_participant(None).await.is_ok());
            let state = wait_for::running_mpc(&ctx, None).await?;
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_signature_production(&ctx, &state).await?;

            tracing::info!("!!! Add back participant 0");
            assert!(ctx.add_participant(Some(node_cfg_0)).await.is_ok());
            let state = wait_for::running_mpc(&ctx, None).await?;
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_signature_production(&ctx, &state).await
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_triples_and_presignatures() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |ctx| {
        Box::pin(async move {
            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            Ok(())
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_signature_basic() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |ctx| {
        Box::pin(async move {
            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_signature_rogue_responder(&ctx, &state_0).await
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_signature_offline_node() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |mut ctx| {
        Box::pin(async move {
            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            wait_for::has_at_least_triples(&ctx, 6).await?;
            wait_for::has_at_least_mine_triples(&ctx, 2).await?;

            // Kill the node then have presignatures and signature generation only use the active set of nodes
            // to start generating presignatures and signatures.
            let account_id = near_workspaces::types::AccountId::from_str(
                state_0.participants.keys().last().unwrap().clone().as_ref(),
            )
            .unwrap();
            ctx.nodes.kill_node(&account_id).await;

            // This could potentially fail and timeout the first time if the participant set picked up is the
            // one with the offline node. This is expected behavior for now if a user submits a request in between
            // a node going offline and the system hasn't detected it yet.
            let presig_res = wait_for::has_at_least_mine_presignatures(&ctx, 1).await;
            let sig_res = actions::single_signature_production(&ctx, &state_0).await;

            // Try again if the first attempt failed. This second portion should not be needed when the NEP
            // comes in for resumeable MPC.
            if presig_res.is_err() || sig_res.is_err() {
                // Retry if the first attempt failed.
                wait_for::has_at_least_mine_presignatures(&ctx, 1).await?;
                actions::single_signature_production(&ctx, &state_0).await?;
            }

            Ok(())
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_key_derivation() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |ctx| {
        Box::pin(async move {
            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            wait_for::has_at_least_triples(&ctx, 6).await?;
            wait_for::has_at_least_presignatures(&ctx, 3).await?;

            for _ in 0..3 {
                let mpc_pk: k256::AffinePoint = state_0.public_key.clone().into_affine_point();
                let (_, payload_hashed, account, status) = actions::request_sign(&ctx).await?;
                let sig = wait_for::signature_responded(status).await?;

                let hd_path = "test";
                let derivation_epsilon = derive_epsilon(account.id(), hd_path);
                let user_pk = derive_key(mpc_pk, derivation_epsilon);
                let multichain_sig = into_eth_sig(
                    &user_pk,
                    &sig.big_r,
                    &sig.s,
                    k256::Scalar::from_bytes(payload_hashed).unwrap(),
                )
                .unwrap();

                // start recovering the address and compare them:
                let user_pk_x = x_coordinate(&user_pk);
                let user_pk_y_parity = match user_pk.y_is_odd().unwrap_u8() {
                    1 => secp256k1::Parity::Odd,
                    0 => secp256k1::Parity::Even,
                    _ => unreachable!(),
                };
                let user_pk_x =
                    secp256k1::XOnlyPublicKey::from_slice(&user_pk_x.to_bytes()).unwrap();
                let user_secp_pk =
                    secp256k1::PublicKey::from_x_only_public_key(user_pk_x, user_pk_y_parity);
                let user_addr = actions::public_key_to_address(&user_secp_pk);
                let r = x_coordinate(&multichain_sig.big_r.affine_point);
                let s = multichain_sig.s;
                let signature_for_recovery: [u8; 64] = {
                    let mut signature = [0u8; 64];
                    signature[..32].copy_from_slice(&r.to_bytes());
                    signature[32..].copy_from_slice(&s.scalar.to_bytes());
                    signature
                };
                let recovered_addr = web3::signing::recover(
                    &payload_hashed,
                    &signature_for_recovery,
                    multichain_sig.recovery_id as i32,
                )
                .unwrap();
                assert_eq!(user_addr, recovered_addr);
            }

            Ok(())
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_triple_persistence() -> anyhow::Result<()> {
    let docker_client = DockerClient::default();
    let docker_network = "test-triple-persistence";
    docker_client.create_network(docker_network).await?;
    let redis = containers::Redis::run(&docker_client, docker_network).await?;
    let redis_url = Url::parse(redis.internal_address.as_str())?;
    let redis_cfg = deadpool_redis::Config::from_url(redis_url);
    let redis_pool = redis_cfg.create_pool(Some(Runtime::Tokio1)).unwrap();
    let triple_storage =
        storage::triple_storage::init(&redis_pool, &AccountId::from_str("test.near").unwrap());

    let mut triple_manager = TripleManager::new(
        Participant::from(0),
        5,
        123,
        &AccountId::from_str("test.near").unwrap(),
        &triple_storage,
    );

    let triple_id_1: u64 = 1;
    let triple_1 = dummy_triple(triple_id_1);
    let triple_id_2: u64 = 2;
    let triple_2 = dummy_triple(triple_id_2);

    // Check that the storage is empty at the start
    assert!(!triple_manager.contains(&triple_id_1).await);
    assert!(!triple_manager.contains_mine(&triple_id_1).await);
    assert_eq!(triple_manager.len_generated().await, 0);
    assert_eq!(triple_manager.len_mine().await, 0);
    assert!(triple_manager.is_empty().await);
    assert_eq!(triple_manager.len_potential().await, 0);

    triple_manager.insert(triple_1).await;
    triple_manager.insert(triple_2).await;

    // Check that the storage contains the foreign triple
    assert!(triple_manager.contains(&triple_id_1).await);
    assert!(triple_manager.contains(&triple_id_2).await);
    assert!(!triple_manager.contains_mine(&triple_id_1).await);
    assert!(!triple_manager.contains_mine(&triple_id_2).await);
    assert_eq!(triple_manager.len_generated().await, 2);
    assert_eq!(triple_manager.len_mine().await, 0);
    assert_eq!(triple_manager.len_potential().await, 2);

    // Take triple and check that it is removed from the storage
    triple_manager
        .take_two(triple_id_1, triple_id_2)
        .await
        .unwrap();
    assert!(!triple_manager.contains(&triple_id_1).await);
    assert!(!triple_manager.contains(&triple_id_2).await);
    assert!(!triple_manager.contains_mine(&triple_id_1).await);
    assert!(!triple_manager.contains_mine(&triple_id_2).await);
    assert_eq!(triple_manager.len_generated().await, 0);
    assert_eq!(triple_manager.len_mine().await, 0);
    assert_eq!(triple_manager.len_potential().await, 0);

    let mine_id_1: u64 = 3;
    let mine_triple_1 = dummy_triple(mine_id_1);
    let mine_id_2: u64 = 4;
    let mine_triple_2 = dummy_triple(mine_id_2);

    // Add mine triple and check that it is in the storage
    triple_manager.insert_mine(mine_triple_1).await;
    triple_manager.insert_mine(mine_triple_2).await;
    assert!(triple_manager.contains(&mine_id_1).await);
    assert!(triple_manager.contains(&mine_id_2).await);
    assert!(triple_manager.contains_mine(&mine_id_1).await);
    assert!(triple_manager.contains_mine(&mine_id_2).await);
    assert_eq!(triple_manager.len_generated().await, 2);
    assert_eq!(triple_manager.len_mine().await, 2);
    assert_eq!(triple_manager.len_potential().await, 2);

    // Take mine triple and check that it is removed from the storage
    triple_manager.take_two_mine().await.unwrap();
    assert!(!triple_manager.contains(&mine_id_1).await);
    assert!(!triple_manager.contains(&mine_id_2).await);
    assert!(!triple_manager.contains_mine(&mine_id_1).await);
    assert!(!triple_manager.contains_mine(&mine_id_2).await);
    assert_eq!(triple_manager.len_generated().await, 0);
    assert_eq!(triple_manager.len_mine().await, 0);
    assert!(triple_manager.is_empty().await);
    assert_eq!(triple_manager.len_potential().await, 0);

    Ok(())
}

#[test(tokio::test)]
async fn test_presignature_persistence() -> anyhow::Result<()> {
    let docker_client = DockerClient::default();
    let docker_network = "test-presignature-persistence";
    docker_client.create_network(docker_network).await?;
    let redis = containers::Redis::run(&docker_client, docker_network).await?;
    let redis_url = Url::parse(redis.internal_address.as_str())?;
    let redis_cfg = deadpool_redis::Config::from_url(redis_url);
    let redis_pool = redis_cfg.create_pool(Some(Runtime::Tokio1)).unwrap();
    let presignature_storage = storage::presignature_storage::init(
        &redis_pool,
        &AccountId::from_str("test.near").unwrap(),
    );
    let mut presignature_manager = PresignatureManager::new(
        Participant::from(0),
        5,
        123,
        &AccountId::from_str("test.near").unwrap(),
        &presignature_storage,
    );

    let presignature = dummy_presignature();
    let presignature_id: PresignatureId = presignature.id;

    // Check that the storage is empty at the start
    assert!(!presignature_manager.contains(&presignature_id).await);
    assert!(!presignature_manager.contains_mine(&presignature_id).await);
    assert_eq!(presignature_manager.len_generated().await, 0);
    assert_eq!(presignature_manager.len_mine().await, 0);
    assert!(presignature_manager.is_empty().await);
    assert_eq!(presignature_manager.len_potential().await, 0);

    presignature_manager.insert(presignature).await;

    // Check that the storage contains the foreign presignature
    assert!(presignature_manager.contains(&presignature_id).await);
    assert!(!presignature_manager.contains_mine(&presignature_id).await);
    assert_eq!(presignature_manager.len_generated().await, 1);
    assert_eq!(presignature_manager.len_mine().await, 0);
    assert_eq!(presignature_manager.len_potential().await, 1);

    // Take presignature and check that it is removed from the storage
    presignature_manager.take(presignature_id).await.unwrap();
    assert!(!presignature_manager.contains(&presignature_id).await);
    assert!(!presignature_manager.contains_mine(&presignature_id).await);
    assert_eq!(presignature_manager.len_generated().await, 0);
    assert_eq!(presignature_manager.len_mine().await, 0);
    assert_eq!(presignature_manager.len_potential().await, 0);

    let mine_presignature = dummy_presignature();
    let mine_presig_id: PresignatureId = mine_presignature.id;

    // Add mine presignature and check that it is in the storage
    presignature_manager.insert_mine(mine_presignature).await;
    assert!(presignature_manager.contains(&mine_presig_id).await);
    assert!(presignature_manager.contains_mine(&mine_presig_id).await);
    assert_eq!(presignature_manager.len_generated().await, 1);
    assert_eq!(presignature_manager.len_mine().await, 1);
    assert_eq!(presignature_manager.len_potential().await, 1);

    // Take mine presignature and check that it is removed from the storage
    presignature_manager.take_mine().await.unwrap();
    assert!(!presignature_manager.contains(&mine_presig_id).await);
    assert!(!presignature_manager.contains_mine(&mine_presig_id).await);
    assert_eq!(presignature_manager.len_generated().await, 0);
    assert_eq!(presignature_manager.len_mine().await, 0);
    assert!(presignature_manager.is_empty().await);
    assert_eq!(presignature_manager.len_potential().await, 0);

    Ok(())
}

fn dummy_presignature() -> Presignature {
    Presignature {
        id: 1,
        output: PresignOutput {
            big_r: <Secp256k1 as CurveArithmetic>::AffinePoint::default(),
            k: <Secp256k1 as CurveArithmetic>::Scalar::ZERO,
            sigma: <Secp256k1 as CurveArithmetic>::Scalar::ONE,
        },
        participants: vec![Participant::from(1), Participant::from(2)],
    }
}

fn dummy_triple(id: u64) -> Triple {
    Triple {
        id,
        share: TripleShare {
            a: <Secp256k1 as CurveArithmetic>::Scalar::ZERO,
            b: <Secp256k1 as CurveArithmetic>::Scalar::ZERO,
            c: <Secp256k1 as CurveArithmetic>::Scalar::ZERO,
        },
        public: TriplePub {
            big_a: <k256::Secp256k1 as CurveArithmetic>::AffinePoint::default(),
            big_b: <k256::Secp256k1 as CurveArithmetic>::AffinePoint::default(),
            big_c: <k256::Secp256k1 as CurveArithmetic>::AffinePoint::default(),
            participants: vec![Participant::from(1), Participant::from(2)],
            threshold: 5,
        },
    }
}

#[test(tokio::test)]
async fn test_signature_offline_node_back_online() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |mut ctx| {
        Box::pin(async move {
            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            wait_for::has_at_least_triples(&ctx, 6).await?;
            wait_for::has_at_least_mine_triples(&ctx, 2).await?;
            wait_for::has_at_least_mine_presignatures(&ctx, 1).await?;

            // Kill node 2
            let account_id = near_workspaces::types::AccountId::from_str(
                state_0.participants.keys().last().unwrap().clone().as_ref(),
            )
            .unwrap();
            let killed_node_config = ctx.nodes.kill_node(&account_id).await;

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // Start the killed node again
            ctx.nodes.restart_node(killed_node_config).await?;

            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            wait_for::has_at_least_mine_triples(&ctx, 2).await?;
            wait_for::has_at_least_mine_presignatures(&ctx, 1).await?;
            // retry the same payload multiple times because we might pick many presignatures not present in node 2 repeatedly until yield/resume time out
            actions::single_payload_signature_production(&ctx, &state_0).await?;

            Ok(())
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_lake_congestion() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |ctx| {
        Box::pin(async move {
            // Currently, with a 10+-1 latency it cannot generate enough tripplets in time
            // with a 5+-1 latency it fails to wait for signature response
            add_latency(&ctx.nodes.proxy_name_for_node(0), true, 1.0, 2_000, 200).await?;
            add_latency(&ctx.nodes.proxy_name_for_node(1), true, 1.0, 2_000, 200).await?;
            add_latency(&ctx.nodes.proxy_name_for_node(2), true, 1.0, 2_000, 200).await?;

            // Also mock lake indexer in high load that it becomes slower to finish process
            // sig req and write to s3
            // with a 1s latency it fails to wait for signature response in time
            add_latency("lake-s3", false, 1.0, 100, 10).await?;

            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_signature_rogue_responder(&ctx, &state_0).await?;
            Ok(())
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_multichain_reshare_with_lake_congestion() -> anyhow::Result<()> {
    let config = MultichainConfig::default();
    with_multichain_nodes(config.clone(), |mut ctx| {
        Box::pin(async move {
            let state = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert!(state.threshold == 2);
            assert!(state.participants.len() == 3);

            // add latency to node1->rpc, but not node0->rpc
            add_latency(&ctx.nodes.proxy_name_for_node(1), true, 1.0, 1_000, 100).await?;
            // remove node2, node0 and node1 should still reach concensus
            // this fails if the latency above is too long (10s)
            assert!(ctx.remove_participant(None).await.is_ok());
            let state = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert!(state.participants.len() == 2);
            // Going below T should error out
            assert!(ctx.remove_participant(None).await.is_err());
            let state = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert!(state.participants.len() == 2);
            assert!(ctx.add_participant(None).await.is_ok());
            // add latency to node2->rpc
            add_latency(&ctx.nodes.proxy_name_for_node(2), true, 1.0, 1_000, 100).await?;
            let state = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert!(state.participants.len() == 3);
            assert!(ctx.remove_participant(None).await.is_ok());
            let state = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert!(state.participants.len() == 2);
            // make sure signing works after reshare
            let new_state = wait_for::running_mpc(&ctx, None).await?;
            wait_for::has_at_least_triples(&ctx, 2).await?;
            wait_for::has_at_least_presignatures(&ctx, 2).await?;
            actions::single_payload_signature_production(&ctx, &new_state).await
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_multichain_update_contract() -> anyhow::Result<()> {
    let config = MultichainConfig::default();
    with_multichain_nodes(config.clone(), |ctx| {
        Box::pin(async move {
            // Get into running state and produce a singular signature.
            let state = wait_for::running_mpc(&ctx, Some(0)).await?;
            wait_for::has_at_least_mine_triples(&ctx, 2).await?;
            wait_for::has_at_least_mine_presignatures(&ctx, 1).await?;
            actions::single_payload_signature_production(&ctx, &state).await?;

            // Perform update to the contract and see that the nodes are still properly running and picking
            // up the new contract by first upgrading the contract, then trying to generate a new signature.
            let id = ctx.propose_update_contract_default().await;
            ctx.vote_update(id).await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            wait_for::has_at_least_mine_presignatures(&ctx, 1).await?;
            actions::single_payload_signature_production(&ctx, &state).await?;

            // Now do a config update and see if that also updates the same:
            let id = ctx
                .propose_update(ProposeUpdateArgs {
                    code: None,
                    config: Some(Config::default()),
                })
                .await;
            ctx.vote_update(id).await;
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            wait_for::has_at_least_mine_presignatures(&ctx, 1).await?;
            actions::single_payload_signature_production(&ctx, &state).await?;

            Ok(())
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_batch_random_signature() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |ctx| {
        Box::pin(async move {
            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            actions::batch_random_signature_production(&ctx, &state_0).await?;
            Ok(())
        })
    })
    .await
}

#[test(tokio::test)]
async fn test_batch_duplicate_signature() -> anyhow::Result<()> {
    with_multichain_nodes(MultichainConfig::default(), |ctx| {
        Box::pin(async move {
            let state_0 = wait_for::running_mpc(&ctx, Some(0)).await?;
            assert_eq!(state_0.participants.len(), 3);
            actions::batch_duplicate_signature_production(&ctx, &state_0).await?;
            Ok(())
        })
    })
    .await
}
