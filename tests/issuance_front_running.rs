//! Front-running an issuance in the mempool to inflate the supply.
//!
//! The eve coin's spend is unauthenticated, so anyone who sees an issuance in the mempool can
//! rebuild it with a larger delta, and take the difference with another coin of the same asset in
//! the same ring, since the ring only checks the total for the asset. Two things stop this:
//! - The driver's eve coins assert their own amount, so the execution coin's amount is fixed.
//! - TAILs commit to the new amount. `genesis_by_coin_id` does so with a message from the genesis
//!   coin, so it's safe even for eve coins that don't assert their amount.

mod common;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{consensus::validation_error::ErrorCode, puzzle_types::Memos},
    clvm_utils::ToTreeHash,
    clvmr::NodePtr,
    driver::{Spend, SpendContext, StandardLayer},
    test::{BlsPairWithCoin, Simulator},
};
use common::*;
use dog::{
    DelegatedTailArgs, DelegatedTailSolution, Dog, DogMetadataRemark, DogSpend,
    GenesisByCoinIdTailArgs, asset_id,
};

/// The amount Mallory tries to mint for herself.
const STOLEN: u64 = 1_000_000_000;

fn delegated(ctx: &mut SpendContext, tail: NodePtr, delegated_tail: NodePtr) -> Result<Spend> {
    let solution = ctx.alloc(&DelegatedTailSolution {
        delegated_tail,
        delegated_solution: NodePtr::NIL,
    })?;
    Ok(Spend::new(tail, solution))
}

/// An asset whose issuer delegates to `genesis_by_coin_id` for each issuance, and Mallory's coin
/// of it from the first issuance.
struct Setup {
    sim: Simulator,
    issuer: BlsPairWithCoin,
    mallory: BlsPairWithCoin,
    bob: BlsPairWithCoin,
    tail: NodePtr,
    mallory_dog: Dog,
}

fn setup(ctx: &mut SpendContext) -> Result<Setup> {
    let mut sim = Simulator::new();
    let issuer = sim.bls(0);
    let mallory = sim.bls(0);
    let bob = sim.bls(0);
    let parent = sim.bls(0);

    let tail = ctx.curry(DelegatedTailArgs::new(issuer.pk))?;
    let genesis = ctx.curry(GenesisByCoinIdTailArgs::new(parent.coin.coin_id()))?;
    let spend = delegated(ctx, tail, genesis)?;
    let conditions = send(ctx, mallory.puzzle_hash, 1000)?;
    let issuance = Dog::issue(
        ctx,
        parent.coin.coin_id(),
        spend,
        &metadata(),
        &DogUris::default(),
        1000,
        conditions,
    )?;
    let authorization =
        GenesisByCoinIdTailArgs::authorization(ctx, issuance.eve.coin.coin_id(), 1000, 1000)?;
    StandardLayer::new(parent.pk).spend(
        ctx,
        parent.coin,
        issuance.parent_conditions.extend(authorization),
    )?;
    submit(&mut sim, ctx, &[parent.sk, issuer.sk.clone()])?;

    Ok(Setup {
        sim,
        issuer,
        mallory,
        bob,
        tail,
        mallory_dog: issuance.children[0],
    })
}

#[test]
fn issuance_cant_be_front_run() -> Result<()> {
    let ctx = &mut SpendContext::new();
    let Setup {
        mut sim,
        issuer,
        mallory,
        bob,
        tail,
        mallory_dog,
    } = setup(ctx)?;

    // The issuer issues 1000 more to Bob from another genesis coin.
    let parent = sim.bls(0);
    let genesis = ctx.curry(GenesisByCoinIdTailArgs::new(parent.coin.coin_id()))?;
    let conditions = send(ctx, bob.puzzle_hash, 1000)?;
    let spend = delegated(ctx, tail, genesis)?;
    let issuance = Dog::issue(
        ctx,
        parent.coin.coin_id(),
        spend,
        &metadata(),
        &DogUris::default(),
        1000,
        conditions.clone(),
    )?;
    let honest = ctx.take();
    let authorization =
        GenesisByCoinIdTailArgs::authorization(ctx, issuance.eve.coin.coin_id(), 1000, 1000)?;
    StandardLayer::new(parent.pk).spend(
        ctx,
        parent.coin,
        issuance.parent_conditions.extend(authorization),
    )?;
    let parent_spend = ctx.take();

    // Mallory rebuilds the eve coin's spend with the same inner puzzle and a huge delta, and takes
    // the difference with her own coin. The eve coin asserts its amount, so it fails.
    let tail_hash = ctx.tree_hash(tail);
    let remark = ctx.alloc(&DogMetadataRemark::new(
        tail_hash.into(),
        metadata(),
        vec![],
        vec![],
    ))?;
    let eve_inner = ctx.delegated_spend(conditions.remark(remark).assert_my_amount(1000))?;
    assert_eq!(
        ctx.tree_hash(eve_inner.puzzle),
        issuance.eve.p2_puzzle_hash.into()
    );
    let spend = delegated(ctx, tail, genesis)?;
    let eve = DogSpend::with_tail(
        issuance.eve,
        eve_inner,
        spend,
        metadata().tree_hash().into(),
        STOLEN.into(),
    );
    let theft = send(ctx, mallory.puzzle_hash, 1000 + STOLEN)?;
    let mallory_inner = p2(ctx, &mallory, theft)?;
    Dog::spend_all(ctx, &[eve, DogSpend::new(mallory_dog, mallory_inner)])?;
    for coin_spend in parent_spend.iter().cloned() {
        ctx.insert(coin_spend);
    }
    assert_eq!(
        submit_err(
            &mut sim,
            ctx,
            &[parent.sk.clone(), issuer.sk.clone(), mallory.sk.clone()]
        ),
        ErrorCode::AssertMyAmountFailed
    );

    // The honest issuance still works.
    for coin_spend in honest.into_iter().chain(parent_spend) {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[parent.sk, issuer.sk])?;
    assert_unspent(&sim, &issuance.children[0]);

    Ok(())
}

/// Even if the eve coin doesn't assert its amount (for example, if it was built by another driver),
/// the genesis coin's message commits to the new amount, so the TAIL rejects a different one.
#[test]
fn genesis_tail_commits_to_the_amount() -> Result<()> {
    let ctx = &mut SpendContext::new();
    let Setup {
        mut sim,
        issuer,
        mallory,
        bob,
        tail,
        mallory_dog,
    } = setup(ctx)?;

    let parent = sim.bls(0);
    let genesis = ctx.curry(GenesisByCoinIdTailArgs::new(parent.coin.coin_id()))?;
    let asset = asset_id(ctx.tree_hash(tail), metadata().tree_hash());
    assert_eq!(asset, mallory_dog.asset_id);

    // An eve coin whose inner puzzle only fixes its outputs.
    let conditions = send(ctx, bob.puzzle_hash, 1000)?;
    let eve_inner = ctx.delegated_spend(conditions)?;
    let eve = Dog::new(
        parent.coin.coin_id(),
        asset,
        ctx.tree_hash(eve_inner.puzzle).into(),
        1000,
        None,
    );
    let authorization =
        GenesisByCoinIdTailArgs::authorization(ctx, eve.coin.coin_id(), 1000, 1000)?;
    StandardLayer::new(parent.pk).spend(
        ctx,
        parent.coin,
        Conditions::new()
            .create_coin(eve.coin.puzzle_hash, 0, Memos::None)
            .extend(authorization),
    )?;
    let parent_spend = ctx.take();

    // Mallory adds a delta, and takes the difference.
    let spend = delegated(ctx, tail, genesis)?;
    let eve_spend = DogSpend::with_tail(
        eve,
        eve_inner,
        spend,
        metadata().tree_hash().into(),
        STOLEN.into(),
    );
    let theft = send(ctx, mallory.puzzle_hash, 1000 + STOLEN)?;
    let mallory_inner = p2(ctx, &mallory, theft)?;
    Dog::spend_all(ctx, &[eve_spend, DogSpend::new(mallory_dog, mallory_inner)])?;
    for coin_spend in parent_spend.iter().cloned() {
        ctx.insert(coin_spend);
    }
    assert_eq!(
        submit_err(
            &mut sim,
            ctx,
            &[parent.sk.clone(), issuer.sk.clone(), mallory.sk.clone()]
        ),
        ErrorCode::MessageNotSentOrReceived
    );

    // Without the delta, the issuance works.
    let spend = delegated(ctx, tail, genesis)?;
    let children = Dog::spend_all(
        ctx,
        &[DogSpend::with_tail(
            eve,
            eve_inner,
            spend,
            metadata().tree_hash().into(),
            0,
        )],
    )?;
    for coin_spend in parent_spend {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[parent.sk, issuer.sk])?;
    assert_unspent(&sim, &children[0]);

    Ok(())
}
