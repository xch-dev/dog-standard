//! Issuing and melting tokens with the TAIL, and the asset's metadata.

mod common;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{
        consensus::validation_error::ErrorCode,
        protocol::{Bytes32, CoinSpend},
    },
    clvm_traits::{FromClvm, clvm_list, clvm_quote},
    clvm_utils::ToTreeHash,
    clvmr::{Allocator, NodePtr, serde::node_from_bytes},
    driver::{Spend, SpendContext, StandardLayer},
    test::{Simulator, SimulatorError},
    types::{Condition, Conditions, run_puzzle},
};
use common::*;
use dog::{
    Dog, DogMetadata, DogMetadataRemark, DogSpend, DogUris, EverythingWithSignatureTailArgs,
    GenesisByCoinIdTailArgs, asset_id,
};

/// Issues tokens with the signature TAIL, and returns the issued coin and the TAIL.
fn issue_multi(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    issuer: &chia_wallet_sdk::test::BlsPairWithCoin,
    owner: Bytes32,
    amount: u64,
) -> Result<Dog> {
    let parent = sim.bls(0);
    let conditions = send(ctx, owner, amount)?;
    let issuance = Dog::multi_issuance(
        ctx,
        parent.coin.coin_id(),
        issuer.pk,
        &metadata(),
        &DogUris::default(),
        amount,
        conditions,
    )?;
    StandardLayer::new(parent.pk).spend(ctx, parent.coin, issuance.parent_conditions)?;
    submit(sim, ctx, &[parent.sk, issuer.sk.clone()])?;
    Ok(issuance.children[0])
}

fn signature_tail(
    ctx: &mut SpendContext,
    issuer: &chia_wallet_sdk::test::BlsPairWithCoin,
) -> Result<Spend> {
    let tail = ctx.curry(EverythingWithSignatureTailArgs::new(issuer.pk))?;
    Ok(Spend::new(tail, NodePtr::NIL))
}

fn metadata_hash() -> Bytes32 {
    metadata().tree_hash().into()
}

#[test]
fn issue_more_into_existing_coin() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let dog = issue_multi(&mut sim, ctx, &issuer, alice.puzzle_hash, 1000)?;

    // Alice's coin is increased to 1500 by the issuer, and she sends it all to herself.
    let conditions = send(ctx, alice.puzzle_hash, 1500)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    let tail = signature_tail(ctx, &issuer)?;
    let spend = DogSpend::with_tail(dog, inner_spend, tail, metadata_hash(), 500);
    let children = Dog::spend_all(ctx, &[spend])?;

    // The issuer has to sign the supply change.
    let coin_spends = ctx.take();
    let result = sim.spend_coins(coin_spends.clone(), &[alice.sk.clone()]);
    assert!(
        matches!(result, Err(SimulatorError::MissingKey)),
        "{result:?}"
    );

    for coin_spend in coin_spends {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[alice.sk, issuer.sk])?;

    assert_eq!(children[0].amount, 1500);
    assert_unspent(&sim, &children[0]);

    Ok(())
}

#[test]
fn melt_tokens() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let dog = issue_multi(&mut sim, ctx, &issuer, alice.puzzle_hash, 1000)?;

    // Melt 300 of the tokens.
    let conditions = send(ctx, alice.puzzle_hash, 700)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    let tail = signature_tail(ctx, &issuer)?;
    let spend = DogSpend::with_tail(dog, inner_spend, tail, metadata_hash(), -300);
    let children = Dog::spend_all(ctx, &[spend])?;
    submit(&mut sim, ctx, &[alice.sk.clone(), issuer.sk.clone()])?;
    assert_eq!(children[0].amount, 700);

    // Melt all of the rest, which leaves an execution coin with an amount of 0.
    let inner_spend = p2(ctx, &alice, Conditions::new())?;
    let tail = signature_tail(ctx, &issuer)?;
    let spend = DogSpend::with_tail(children[0], inner_spend, tail, metadata_hash(), -700);
    let children = Dog::spend_all(ctx, &[spend])?;
    submit(&mut sim, ctx, &[alice.sk.clone(), issuer.sk.clone()])?;
    assert_eq!(children, [] as [dog::Dog; 0]);

    Ok(())
}

#[test]
fn cant_melt_more_than_the_amount() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let dog = issue_multi(&mut sim, ctx, &issuer, alice.puzzle_hash, 1000)?;

    let inner_spend = p2(ctx, &alice, Conditions::new())?;
    let tail = signature_tail(ctx, &issuer)?;
    let spend = DogSpend::with_tail(dog, inner_spend, tail, metadata_hash(), -1001);

    // The driver rejects it.
    assert!(Dog::spend_all(ctx, &[spend]).is_err());

    // So does the puzzle, since the execution coin's amount would be negative.
    let item = RawRingItem {
        tail_spend: spend.tail_spend,
        exec_amount: 0,
        ..RawRingItem::new(dog, inner_spend, 0)
    };
    spend_ring(ctx, &[item])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk, issuer.sk]),
        PUZZLE_FAILED
    );

    Ok(())
}

#[test]
fn single_issuance_is_final() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    // The issued coin's grandparent is the eve DOG coin, whose parent is the genesis coin.
    let eve_coin_id = dog
        .lineage_proof
        .expect("the issued coin has a lineage proof")
        .parent_parent_coin_info;
    let eve_state = sim.coin_state(eve_coin_id).expect("the eve coin exists");
    let genesis = eve_state.coin.parent_coin_info;
    let tail = ctx.curry(GenesisByCoinIdTailArgs::new(genesis))?;
    let tail_spend = Spend::new(tail, NodePtr::NIL);
    assert_eq!(
        asset_id(ctx.tree_hash(tail), metadata().tree_hash()),
        dog.asset_id
    );

    // The TAIL can't issue more into an existing coin, or melt it.
    for delta in [1, -1, 0] {
        let created = i128::from(dog.amount) + delta;
        let conditions = send(ctx, alice.puzzle_hash, u64::try_from(created)?)?;
        let inner_spend = p2(ctx, &alice, conditions)?;
        Dog::spend_all(
            ctx,
            &[DogSpend::with_tail(
                dog,
                inner_spend,
                tail_spend,
                metadata_hash(),
                delta,
            )],
        )?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[alice.sk.clone()]),
            ErrorCode::AssertMyParentIdFailed,
            "a delta of {delta} should be rejected"
        );
    }

    // Another coin can't issue tokens of the same asset.
    let other = sim.bls(0);
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let issuance = Dog::issue(
        ctx,
        other.coin.coin_id(),
        tail_spend,
        &metadata(),
        &DogUris::default(),
        1000,
        conditions,
    )?;
    assert_eq!(issuance.eve.asset_id, dog.asset_id);
    StandardLayer::new(other.pk).spend(ctx, other.coin, issuance.parent_conditions)?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[other.sk]),
        ErrorCode::AssertMyParentIdFailed
    );

    // The coin can still be spent normally.
    transfer(&mut sim, ctx, &alice, dog, &[(alice.puzzle_hash, 1000)])?;

    Ok(())
}

/// A TAIL that allows anything and outputs `(REMARK supply_change)`, so tests can see what it's
/// called with.
fn echo_tail(ctx: &mut SpendContext) -> Result<NodePtr> {
    Ok(ctx.alloc(&clvm_list!(
        4,
        clvm_list!(4, (1, 1), clvm_list!(4, 2, ())),
        ()
    ))?)
}

/// Runs the spend of the DOG coin, and returns the supply change that the echo TAIL saw.
fn echoed_supply_change(coin_spends: &[CoinSpend], dog: &Dog) -> Result<i128> {
    let mut allocator = Allocator::new();
    let coin_spend = coin_spends
        .iter()
        .find(|cs| cs.coin == dog.coin)
        .expect("the DOG coin is spent");
    let puzzle = node_from_bytes(&mut allocator, coin_spend.puzzle_reveal.as_ref())?;
    let solution = node_from_bytes(&mut allocator, coin_spend.solution.as_ref())?;
    let output = run_puzzle(&mut allocator, puzzle, solution)?;
    let conditions = Vec::<Condition>::from_clvm(&allocator, output)?;
    let remark = conditions
        .into_iter()
        .find_map(Condition::into_remark)
        .expect("the TAIL outputs a remark");
    let (supply_change, ()) = <(i128, ())>::from_clvm(&allocator, remark.rest)?;
    Ok(supply_change)
}

#[test]
fn tail_sees_supply_change() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let tail = echo_tail(ctx)?;
    let tail_spend = Spend::new(tail, NodePtr::NIL);

    // Issuing from an eve coin is a supply change of the whole amount.
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let issuance = Dog::issue(
        ctx,
        issuer.coin.coin_id(),
        tail_spend,
        &metadata(),
        &DogUris::default(),
        1000,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;
    let coin_spends = ctx.take();
    assert_eq!(echoed_supply_change(&coin_spends, &issuance.eve)?, 1000);
    for coin_spend in coin_spends {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[issuer.sk])?;
    let dog = issuance.children[0];

    // With a lineage proof, it's only the delta.
    for delta in [250, -250, 0] {
        let created = u64::try_from(1000 + delta)?;
        let conditions = send(ctx, alice.puzzle_hash, created)?;
        let inner_spend = p2(ctx, &alice, conditions)?;
        Dog::spend_all(
            ctx,
            &[DogSpend::with_tail(
                dog,
                inner_spend,
                tail_spend,
                metadata_hash(),
                delta,
            )],
        )?;
        let coin_spends = ctx.take();
        assert_eq!(echoed_supply_change(&coin_spends, &dog)?, delta);
    }

    // Without one, it's the whole amount plus the delta, since none of it is proven.
    let unproven = Dog {
        lineage_proof: None,
        ..dog
    };
    let conditions = send(ctx, alice.puzzle_hash, 1100)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(
        ctx,
        &[DogSpend::with_tail(
            unproven,
            inner_spend,
            tail_spend,
            metadata_hash(),
            100,
        )],
    )?;
    let coin_spends = ctx.take();
    assert_eq!(echoed_supply_change(&coin_spends, &unproven)?, 1100);

    // That's still a valid spend, since this TAIL allows anything.
    for coin_spend in coin_spends {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[alice.sk])?;

    Ok(())
}

#[test]
fn issue_in_the_same_ring_as_transfers() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_multi(&mut sim, ctx, &issuer, alice.puzzle_hash, 1000)?;
    let coins = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(alice.puzzle_hash, 300), (alice.puzzle_hash, 700)],
    )?;

    // The first coin issues 5000 more, and the second coin creates all of the outputs.
    let tail = signature_tail(ctx, &issuer)?;
    let first = p2(ctx, &alice, Conditions::new())?;
    let conditions = send_many(ctx, &[(bob.puzzle_hash, 5500), (alice.puzzle_hash, 500)])?;
    let second = p2(ctx, &alice, conditions)?;
    let children = Dog::spend_all(
        ctx,
        &[
            DogSpend::with_tail(coins[0], first, tail, metadata_hash(), 5000),
            DogSpend::new(coins[1], second),
        ],
    )?;
    submit(&mut sim, ctx, &[alice.sk, issuer.sk])?;

    assert_eq!(
        children
            .iter()
            .map(|child| child.amount)
            .collect::<Vec<_>>(),
        [5500, 500]
    );

    Ok(())
}

#[test]
fn issue_maximum_supply() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, u64::MAX)?;
    assert_eq!(dog.amount, u64::MAX);

    let children = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(bob.puzzle_hash, u64::MAX - 1), (alice.puzzle_hash, 1)],
    )?;
    let children = transfer(
        &mut sim,
        ctx,
        &bob,
        children[0],
        &[(bob.puzzle_hash, u64::MAX - 1)],
    )?;
    assert_unspent(&sim, &children[0]);

    Ok(())
}

#[test]
fn issuance_needs_no_mojos() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    // The issuer's coin has no mojos, and the transaction doesn't pay a fee.
    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let conditions = send(ctx, alice.puzzle_hash, 1_000_000)?;
    let issuance = Dog::single_issuance(
        ctx,
        issuer.coin.coin_id(),
        &metadata(),
        &DogUris::default(),
        1_000_000,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;

    let coin_spends = ctx.take();
    let balance = balance(&coin_spends)?;
    assert_eq!(balance.removals, balance.additions);
    // The only mojos are those of the execution coin, which is created and spent.
    assert_eq!(balance.removals, 1_000_000);

    for coin_spend in coin_spends {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[issuer.sk])?;
    assert_unspent(&sim, &issuance.children[0]);

    Ok(())
}

#[test]
fn metadata_is_published_and_verifiable() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let uris = DogUris {
        image_uris: vec!["https://example.com/doge.png".to_string()],
        data_uris: vec!["https://example.com/doge.json".to_string()],
    };
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let issuance = Dog::single_issuance(
        ctx,
        issuer.coin.coin_id(),
        &metadata(),
        &uris,
        1000,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;
    submit(&mut sim, ctx, &[issuer.sk])?;

    // An indexer finds the remark in the spend of the eve coin's execution coin.
    let exec_coin = issuance.eve.exec_coin(1000);
    let coin_spend = sim
        .coin_spend(exec_coin.coin_id())
        .expect("the spend is saved");
    let mut allocator = Allocator::new();
    let puzzle = node_from_bytes(&mut allocator, coin_spend.puzzle_reveal.as_ref())?;
    let solution = node_from_bytes(&mut allocator, coin_spend.solution.as_ref())?;
    let output = run_puzzle(&mut allocator, puzzle, solution)?;
    let conditions = Vec::<Condition>::from_clvm(&allocator, output)?;
    let remark = conditions
        .into_iter()
        .filter_map(Condition::into_remark)
        .find_map(|remark| DogMetadataRemark::from_clvm(&allocator, remark.rest).ok())
        .expect("the metadata is published");

    assert!(remark.is_valid());
    assert_eq!(remark.metadata, metadata());
    assert_eq!(remark.image_uris, uris.image_uris);
    assert_eq!(remark.data_uris, uris.data_uris);
    assert_eq!(remark.asset_id(), issuance.eve.asset_id);
    assert_eq!(remark.asset_id(), issuance.children[0].asset_id);

    Ok(())
}

#[test]
fn metadata_is_part_of_the_asset_id() -> Result<()> {
    let ctx = &mut SpendContext::new();
    let tail = ctx.alloc(&clvm_quote!(()))?;
    let tail_hash = ctx.tree_hash(tail);

    let base = DogMetadata::new("Dogecoin", "DOGE", 8);
    let variants = [
        base.clone(),
        DogMetadata::new("Dogecoin", "DOGE", 3),
        DogMetadata::new("Dogecoin", "DOG", 8),
        DogMetadata::new("Dog", "DOGE", 8),
        base.clone().with_image_hash(Bytes32::new([1; 32])),
        base.clone().with_data_hash(Bytes32::new([1; 32])),
    ];

    let mut asset_ids: Vec<Bytes32> = variants
        .iter()
        .map(|metadata| asset_id(tail_hash, metadata.tree_hash()))
        .collect();
    asset_ids.sort();
    asset_ids.dedup();
    assert_eq!(asset_ids.len(), variants.len());

    // The asset id is the tree hash of `(TAIL . METADATA)`.
    let metadata_ptr = ctx.alloc(&base)?;
    let pair = ctx.new_pair(tail, metadata_ptr)?;
    assert_eq!(
        Bytes32::from(ctx.tree_hash(pair)),
        asset_id(tail_hash, base.tree_hash())
    );

    Ok(())
}
