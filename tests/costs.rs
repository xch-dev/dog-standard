//! Compares the cost of DOG spends with the equivalent CAT spends.
//!
//! Spending a DOG coin takes two coin spends (the DOG coin and its execution coin) and creates two
//! coins, so it costs more than a CAT. These tests measure by how much, and fail if a change to the
//! puzzles makes that noticeably worse. Run with `--nocapture` to see the numbers.

mod common;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::protocol::CoinSpend,
    driver::{Cat, CatSpend, SpendContext, StandardLayer, spend_bundle_cost},
    test::{BlsPairWithCoin, Simulator},
};
use common::*;
use dog::{Dog, DogSpend, DogUris};

/// Issues a CAT and splits it into coins with the given amounts.
fn cat_coins(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    amounts: &[u64],
) -> Result<Vec<Cat>> {
    let total = amounts.iter().sum();
    let issuer = sim.bls(total);
    let conditions = send(ctx, owner.puzzle_hash, total)?;
    let (parent_conditions, cats) =
        Cat::single_issuance(ctx, issuer.coin.coin_id(), None, total, conditions)?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, parent_conditions)?;
    submit(sim, ctx, &[issuer.sk])?;

    let outputs: Vec<_> = amounts
        .iter()
        .map(|&amount| (owner.puzzle_hash, amount))
        .collect();
    let conditions = send_many(ctx, &outputs)?;
    let inner_spend = p2(ctx, owner, conditions)?;
    let children = Cat::spend_all(ctx, &[CatSpend::new(cats[0], inner_spend)])?;
    submit(sim, ctx, &[owner.sk.clone()])?;
    Ok(children)
}

/// Issues a DOG and splits it into coins with the given amounts.
fn dog_coins(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    amounts: &[u64],
) -> Result<Vec<Dog>> {
    let total = amounts.iter().sum();
    let dog = issue_single(sim, ctx, owner, total)?;
    let outputs: Vec<_> = amounts
        .iter()
        .map(|&amount| (owner.puzzle_hash, amount))
        .collect();
    transfer(sim, ctx, owner, dog, &outputs)
}

/// The cost of spending all of the coins together, creating the given outputs from the first one.
fn measure(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    coin_spends: impl FnOnce(&mut SpendContext) -> Result<()>,
) -> Result<u64> {
    coin_spends(ctx)?;
    let coin_spends: Vec<CoinSpend> = ctx.take();
    let cost = spend_bundle_cost(&coin_spends)?;
    sim.spend_coins(coin_spends, &[owner.sk.clone()])?;
    Ok(cost)
}

fn compare(inputs: &[u64], outputs: usize) -> Result<(u64, u64)> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();
    let owner = sim.bls(0);
    let bob = sim.bls(0);

    let total: u64 = inputs.iter().sum();
    let mut amounts = vec![total / outputs as u64; outputs];
    amounts[0] += total % outputs as u64;
    // Each output goes to a different puzzle hash, so that outputs with the same amount differ.
    let targets: Vec<_> = amounts
        .iter()
        .enumerate()
        .map(|(index, &amount)| {
            let mut puzzle_hash = bob.puzzle_hash.to_bytes();
            puzzle_hash[0] = u8::try_from(index).expect("few outputs");
            (puzzle_hash.into(), amount)
        })
        .collect();

    let cats = cat_coins(&mut sim, ctx, &owner, inputs)?;
    let cat_cost = measure(&mut sim, ctx, &owner, |ctx| {
        let mut spends = Vec::new();
        for (index, cat) in cats.iter().enumerate() {
            let conditions = if index == 0 {
                send_many(ctx, &targets)?
            } else {
                Conditions::new()
            };
            spends.push(CatSpend::new(*cat, p2(ctx, &owner, conditions)?));
        }
        Cat::spend_all(ctx, &spends)?;
        Ok(())
    })?;

    let dogs = dog_coins(&mut sim, ctx, &owner, inputs)?;
    let dog_cost = measure(&mut sim, ctx, &owner, |ctx| {
        let mut spends = Vec::new();
        for (index, dog) in dogs.iter().enumerate() {
            let conditions = if index == 0 {
                send_many(ctx, &targets)?
            } else {
                Conditions::new()
            };
            spends.push(DogSpend::new(*dog, p2(ctx, &owner, conditions)?));
        }
        Dog::spend_all(ctx, &spends)?;
        Ok(())
    })?;

    #[allow(clippy::cast_precision_loss)]
    let ratio = dog_cost as f64 / cat_cost as f64;
    println!(
        "{} inputs, {outputs} outputs: CAT {cat_cost}, DOG {dog_cost} ({ratio:.2}x)",
        inputs.len()
    );

    Ok((cat_cost, dog_cost))
}

#[test]
fn cost_compared_to_cat() -> Result<()> {
    for (inputs, outputs) in [
        (&[1000][..], 1),
        (&[1000][..], 2),
        (&[100, 200, 300][..], 1),
        (&[100, 200, 300, 400, 500][..], 5),
    ] {
        let (cat_cost, dog_cost) = compare(inputs, outputs)?;
        assert!(
            dog_cost * 100 < cat_cost * 108,
            "a DOG spend should cost less than 1.08 times a CAT spend ({dog_cost} vs {cat_cost})"
        );
    }

    Ok(())
}

/// The costs of a revocable CAT and a revocable DOG, for the owner's spend and the issuer's
/// revocation spend, each sending one coin to one coin. Both wrap the p2 puzzle in the same
/// revocation layer, so the difference is the same as for unrevocable assets.
fn compare_revocable(revoke: bool) -> Result<(u64, u64)> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();
    let issuer = sim.bls(1000);
    let owner = sim.bls(0);
    let bob = sim.bls(0);
    let hidden_puzzle_hash = issuer.puzzle_hash;
    // The key that signs the measured spend.
    let signer = if revoke { &issuer } else { &owner };

    let conditions = send(ctx, owner.puzzle_hash, 1000)?;
    let (parent_conditions, cats) = Cat::single_issuance(
        ctx,
        issuer.coin.coin_id(),
        Some(hidden_puzzle_hash),
        1000,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, parent_conditions)?;
    submit(&mut sim, ctx, &[issuer.sk.clone()])?;

    let cat_cost = measure(&mut sim, ctx, signer, |ctx| {
        let conditions = send(ctx, bob.puzzle_hash, 1000)?;
        let spend = p2(ctx, signer, conditions)?;
        let cat_spend = if revoke {
            CatSpend::revoke(cats[0], spend)
        } else {
            CatSpend::new(cats[0], spend)
        };
        Cat::spend_all(ctx, &[cat_spend])?;
        Ok(())
    })?;

    let parent = sim.bls(0);
    let conditions = send(ctx, owner.puzzle_hash, 1000)?;
    let issuance = Dog::single_issuance_revocable(
        ctx,
        parent.coin.coin_id(),
        hidden_puzzle_hash,
        &metadata(),
        &DogUris::default(),
        1000,
        conditions,
    )?;
    StandardLayer::new(parent.pk).spend(ctx, parent.coin, issuance.parent_conditions)?;
    submit(&mut sim, ctx, &[parent.sk])?;

    let dog_cost = measure(&mut sim, ctx, signer, |ctx| {
        let conditions = send(ctx, bob.puzzle_hash, 1000)?;
        let spend = p2(ctx, signer, conditions)?;
        let dog_spend = if revoke {
            DogSpend::revoke(issuance.children[0], spend)
        } else {
            DogSpend::new(issuance.children[0], spend)
        };
        Dog::spend_all(ctx, &[dog_spend])?;
        Ok(())
    })?;

    #[allow(clippy::cast_precision_loss)]
    let ratio = dog_cost as f64 / cat_cost as f64;
    let kind = if revoke { "revocation" } else { "owner" };
    println!("revocable, {kind} spend: CAT {cat_cost}, DOG {dog_cost} ({ratio:.2}x)");

    Ok((cat_cost, dog_cost))
}

#[test]
fn revocable_cost_compared_to_cat() -> Result<()> {
    for revoke in [false, true] {
        let (cat_cost, dog_cost) = compare_revocable(revoke)?;
        assert!(
            dog_cost * 100 < cat_cost * 150,
            "a revocable DOG spend should cost less than 1.5 times a revocable CAT spend ({dog_cost} vs {cat_cost})"
        );
    }

    Ok(())
}
