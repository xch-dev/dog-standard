//! Property tests over random rings of coins and outputs.

mod common;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{consensus::validation_error::ErrorCode, protocol::Bytes32, puzzle_types::Memos},
    clvm_traits::clvm_list,
    driver::{SpendContext, StandardLayer},
    test::Simulator,
    types::Conditions,
};
use common::*;
use dog::{Dog, DogSpend, DogUris};
use proptest::prelude::*;

/// A ring of coins with the given amounts, whose outputs are given as `(coin index, amount)`.
#[derive(Debug, Clone)]
struct Ring {
    inputs: Vec<u64>,
    outputs: Vec<(usize, u64)>,
}

impl Ring {
    fn total(&self) -> u128 {
        self.inputs.iter().map(|&amount| u128::from(amount)).sum()
    }
}

/// Splits `total` at the given fractions, then halves any part that doesn't fit in a u64.
fn split(total: u128, cuts: &[u64]) -> Vec<u64> {
    let mut points: Vec<u128> = cuts
        .iter()
        // Only the top 32 bits of the fraction are used, so the product can't overflow.
        .map(|&cut| total * u128::from(cut >> 32) / u128::from(u32::MAX))
        .collect();
    points.push(0);
    points.push(total);
    points.sort_unstable();

    let mut parts: Vec<u128> = points
        .windows(2)
        .map(|window| window[1] - window[0])
        .collect();
    while let Some(index) = parts.iter().position(|&part| part > u128::from(u64::MAX)) {
        let part = parts.remove(index);
        parts.insert(index, part / 2);
        parts.insert(index, part - part / 2);
    }

    parts
        .into_iter()
        .map(|part| u64::try_from(part).expect("every part fits in a u64"))
        .collect()
}

fn amount() -> impl Strategy<Value = u64> {
    prop_oneof![
        Just(0),
        1..1000u64,
        1..=u64::MAX / 8,
        // Large amounts close to the maximum, which must be kept on separate coins.
        (u64::MAX / 2)..=u64::MAX / 2 + 1000,
    ]
}

fn ring() -> impl Strategy<Value = Ring> {
    (
        prop::collection::vec(amount(), 1..=4),
        prop::collection::vec(any::<u64>(), 0..=5),
        prop::collection::vec(any::<prop::sample::Index>(), 1..=6),
    )
        .prop_map(|(inputs, cuts, owners)| {
            let total: u128 = inputs.iter().map(|&amount| u128::from(amount)).sum();

            let outputs = split(total, &cuts)
                .into_iter()
                .zip(owners.iter().cycle())
                .map(|(amount, owner)| (owner.index(inputs.len()), amount))
                .collect();

            Ring { inputs, outputs }
        })
}

/// Issues coins with the ring's input amounts, with the signature TAIL, so that more than
/// `u64::MAX` can exist in total.
fn setup(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    ring: &Ring,
) -> Result<(chia_wallet_sdk::test::BlsPairWithCoin, Vec<Dog>)> {
    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let mut coins = Vec::new();

    for (index, &amount) in ring.inputs.iter().enumerate() {
        let parent = sim.bls(0);
        // Each coin gets a different hint, so that coins with the same amount are distinct.
        let conditions = Conditions::new().create_coin(
            alice.puzzle_hash,
            amount,
            Memos::Some(ctx.alloc(&[Bytes32::new([u8::try_from(index)?; 32])])?),
        );
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
        coins.push(issuance.children[0]);
    }

    Ok((alice, coins))
}

/// A different puzzle hash for every output, so that no two outputs are the same coin.
fn output_puzzle_hash(index: usize) -> Bytes32 {
    let mut bytes = [0xaa; 32];
    bytes[0] = u8::try_from(index).expect("few outputs");
    Bytes32::new(bytes)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 48,
        failure_persistence: None,
        ..ProptestConfig::default()
    })]

    /// Any ring whose outputs add up to its inputs can be spent, and the supply doesn't change.
    #[test]
    fn balanced_rings_conserve_supply(ring in ring()) {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let (alice, coins) = setup(&mut sim, ctx, &ring).unwrap();

        let mut spends = Vec::new();
        for (coin_index, coin) in coins.iter().enumerate() {
            let mut conditions = Conditions::new();
            for (output_index, &(owner, amount)) in ring.outputs.iter().enumerate() {
                if owner == coin_index {
                    conditions = conditions.create_coin(output_puzzle_hash(output_index), amount, Memos::None);
                }
            }
            spends.push(DogSpend::new(*coin, p2(ctx, &alice, conditions).unwrap()));
        }

        let children = Dog::spend_all(ctx, &spends).unwrap();
        submit(&mut sim, ctx, &[alice.sk]).unwrap();

        let total: u128 = children.iter().map(|child| u128::from(child.amount)).sum();
        prop_assert_eq!(total, ring.total());
        prop_assert_eq!(children.len(), ring.outputs.len());
        for child in &children {
            assert_unspent(&sim, child);
        }
    }

    /// If the outputs add up to anything else, the ring can't close, whatever the subtotals are.
    #[test]
    fn unbalanced_rings_are_rejected(ring in ring(), error in prop_oneof![-1000i128..0, 1..=1000i128]) {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let (alice, coins) = setup(&mut sim, ctx, &ring).unwrap();

        let mut items = Vec::new();
        for (coin_index, coin) in coins.iter().enumerate() {
            let mut created = 0;
            let mut conditions = Vec::new();
            for (output_index, &(owner, amount)) in ring.outputs.iter().enumerate() {
                if owner == coin_index {
                    let mut amount = i128::from(amount);
                    // The first output is off by the error.
                    if output_index == 0 {
                        amount += error;
                    }
                    created += amount;
                    conditions.push(ctx.alloc(&clvm_list!(51, output_puzzle_hash(output_index), amount)).unwrap());
                }
            }
            let conditions = ctx.alloc(&conditions).unwrap();
            items.push(RawRingItem::new(*coin, raw_p2(ctx, &alice, conditions).unwrap(), created));
        }

        // The ring is linked with the subtotals as if it were balanced, but it can't be.
        spend_ring(ctx, &items).unwrap();
        let code = submit_err(&mut sim, ctx, &[alice.sk]);

        // If the error makes an output negative or too large, the puzzle rejects it before the
        // ring is checked.
        prop_assert!(
            code == ErrorCode::MessageNotSentOrReceived || code == PUZZLE_FAILED,
            "unexpected error {:?}",
            code
        );
    }
}
