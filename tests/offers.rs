//! Trading DOGs in offers, with the settlement payments puzzle as the inner puzzle.
//!
//! Requested payments are asserted with a puzzle announcement from the settlement puzzle, wrapped
//! in the asset's outer puzzle. For DOGs, that's the execution coin, whose puzzle hash doesn't
//! depend on the amount, so the maker can calculate it without knowing which coins will be used.

mod common;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{
        consensus::validation_error::ErrorCode,
        protocol::{Bytes32, Coin},
        puzzle_types::{
            Memos,
            offer::{NotarizedPayment, Payment, SettlementPaymentsSolution},
        },
    },
    clvmr::NodePtr,
    driver::{Layer, SettlementLayer, Spend, SpendContext, StandardLayer},
    puzzles::SETTLEMENT_PAYMENT_HASH,
    test::{BlsPairWithCoin, Simulator},
    types::{Conditions, payment_assertion, tree_hash_notarized_payment},
};
use common::*;
use dog::{Dog, DogExecArgs, DogSpend};

fn settlement_puzzle_hash() -> Bytes32 {
    SETTLEMENT_PAYMENT_HASH.into()
}

/// A notarized payment of `amount` to `puzzle_hash`, hinted to the recipient.
fn payment(
    ctx: &mut SpendContext,
    nonce: Bytes32,
    puzzle_hash: Bytes32,
    amount: u64,
) -> Result<NotarizedPayment<NodePtr>> {
    let memos = ctx.hint(puzzle_hash)?;
    Ok(NotarizedPayment {
        nonce,
        payments: vec![Payment::new(puzzle_hash, amount, memos)],
    })
}

/// The condition that asserts a requested payment of a DOG asset.
fn assert_dog_payment(
    ctx: &mut SpendContext,
    asset_id: Bytes32,
    notarized_payment: &NotarizedPayment<NodePtr>,
) -> Conditions {
    let settlement = DogExecArgs::puzzle_hash(asset_id, settlement_puzzle_hash().into());
    let hash = tree_hash_notarized_payment(ctx, notarized_payment);
    Conditions::new().with(payment_assertion(settlement.into(), hash))
}

/// The condition that asserts a requested payment of XCH.
fn assert_xch_payment(
    ctx: &mut SpendContext,
    notarized_payment: &NotarizedPayment<NodePtr>,
) -> Conditions {
    let hash = tree_hash_notarized_payment(ctx, notarized_payment);
    Conditions::new().with(payment_assertion(settlement_puzzle_hash(), hash))
}

/// The inner spend of the settlement puzzle that makes the payments.
fn settle(ctx: &mut SpendContext, payments: Vec<NotarizedPayment<NodePtr>>) -> Result<Spend> {
    Ok(SettlementLayer.construct_spend(ctx, SettlementPaymentsSolution::new(payments))?)
}

/// Sends the DOG coin to the settlement puzzle, and returns the settlement DOG coin.
fn offer_dog(
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    dog: Dog,
    extra: Conditions,
) -> Result<Dog> {
    let conditions = Conditions::new()
        .create_coin(settlement_puzzle_hash(), dog.amount, Memos::None)
        .extend(extra);
    let inner_spend = p2(ctx, owner, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    Ok(children[0])
}

/// Spends a DOG coin held by the settlement puzzle.
fn take_dog(
    ctx: &mut SpendContext,
    dog: Dog,
    payment: NotarizedPayment<NodePtr>,
) -> Result<Vec<Dog>> {
    let inner_spend = settle(ctx, vec![payment])?;
    Ok(Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?)
}

#[test]
fn offer_xch_for_dog() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1000);
    let bob = sim.bls(0);
    let bob_dog = issue_single(&mut sim, ctx, &bob, 500)?;

    // Alice offers 1000 mojos, and requests 500 DOG tokens.
    let requested = payment(ctx, alice.coin.coin_id(), alice.puzzle_hash, 500)?;
    let conditions = Conditions::new()
        .create_coin(settlement_puzzle_hash(), 1000, Memos::None)
        .extend(assert_dog_payment(ctx, bob_dog.asset_id, &requested));
    StandardLayer::new(alice.pk).spend(ctx, alice.coin, conditions)?;

    // Bob takes it, by paying with his DOG coin through the settlement puzzle.
    let settlement_dog = offer_dog(ctx, &bob, bob_dog, Conditions::new())?;
    let alice_dogs = take_dog(ctx, settlement_dog, requested)?;

    // And claiming the offered XCH.
    let settlement_xch = Coin::new(alice.coin.coin_id(), settlement_puzzle_hash(), 1000);
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 1000)?;
    let claim_spend = settle(ctx, vec![claim])?;
    ctx.spend(settlement_xch, claim_spend)?;

    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;

    assert_eq!(alice_dogs.len(), 1);
    assert_eq!(alice_dogs[0].p2_puzzle_hash, alice.puzzle_hash);
    assert_eq!(alice_dogs[0].amount, 500);
    assert_unspent(&sim, &alice_dogs[0]);
    assert!(
        sim.unspent_coins(bob.puzzle_hash, false)
            .iter()
            .any(|coin| coin.amount == 1000)
    );

    Ok(())
}

#[test]
fn offer_dog_for_xch() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(1000);
    let alice_dog = issue_single(&mut sim, ctx, &alice, 500)?;

    // Alice offers 500 DOG tokens, and requests 1000 mojos.
    let requested = payment(ctx, alice_dog.coin.coin_id(), alice.puzzle_hash, 1000)?;
    let assertion = assert_xch_payment(ctx, &requested);
    let settlement_dog = offer_dog(ctx, &alice, alice_dog, assertion)?;

    // Bob pays the XCH through the settlement puzzle.
    let conditions = Conditions::new().create_coin(settlement_puzzle_hash(), 1000, Memos::None);
    StandardLayer::new(bob.pk).spend(ctx, bob.coin, conditions)?;
    let settlement_xch = Coin::new(bob.coin.coin_id(), settlement_puzzle_hash(), 1000);
    let pay_spend = settle(ctx, vec![requested])?;
    ctx.spend(settlement_xch, pay_spend)?;

    // And takes the DOG tokens.
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 500)?;
    let bob_dogs = take_dog(ctx, settlement_dog, claim)?;

    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;

    assert_eq!(bob_dogs[0].p2_puzzle_hash, bob.puzzle_hash);
    assert_eq!(bob_dogs[0].amount, 500);
    assert_unspent(&sim, &bob_dogs[0]);
    assert!(
        sim.unspent_coins(alice.puzzle_hash, false)
            .iter()
            .any(|coin| coin.amount == 1000)
    );

    Ok(())
}

#[test]
fn offer_dog_for_dog() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let alice_dog = issue_single(&mut sim, ctx, &alice, 300)?;
    let bob_dog = issue_single(&mut sim, ctx, &bob, 7000)?;
    assert_ne!(alice_dog.asset_id, bob_dog.asset_id);

    // Alice offers 300 of her asset, and requests 7000 of Bob's.
    let requested = payment(ctx, alice_dog.coin.coin_id(), alice.puzzle_hash, 7000)?;
    let assertion = assert_dog_payment(ctx, bob_dog.asset_id, &requested);
    let alice_settlement = offer_dog(ctx, &alice, alice_dog, assertion)?;

    let bob_settlement = offer_dog(ctx, &bob, bob_dog, Conditions::new())?;
    let alice_dogs = take_dog(ctx, bob_settlement, requested)?;
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 300)?;
    let bob_dogs = take_dog(ctx, alice_settlement, claim)?;

    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;

    assert_eq!(alice_dogs[0].asset_id, bob_dog.asset_id);
    assert_eq!(alice_dogs[0].amount, 7000);
    assert_eq!(bob_dogs[0].asset_id, alice_dog.asset_id);
    assert_eq!(bob_dogs[0].amount, 300);
    assert_unspent(&sim, &alice_dogs[0]);
    assert_unspent(&sim, &bob_dogs[0]);

    Ok(())
}

#[test]
fn offer_cant_be_underpaid() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1000);
    let bob = sim.bls(0);
    let bob_dog = issue_single(&mut sim, ctx, &bob, 500)?;

    let requested = payment(ctx, alice.coin.coin_id(), alice.puzzle_hash, 500)?;
    let conditions = Conditions::new()
        .create_coin(settlement_puzzle_hash(), 1000, Memos::None)
        .extend(assert_dog_payment(ctx, bob_dog.asset_id, &requested));
    StandardLayer::new(alice.pk).spend(ctx, alice.coin, conditions)?;

    // Bob only pays 499, and keeps 1 for himself.
    let conditions = Conditions::new()
        .create_coin(settlement_puzzle_hash(), 499, Memos::None)
        .create_coin(bob.puzzle_hash, 1, Memos::None);
    let inner_spend = p2(ctx, &bob, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(bob_dog, inner_spend)])?;
    let underpayment = payment(ctx, alice.coin.coin_id(), alice.puzzle_hash, 499)?;
    take_dog(ctx, children[0], underpayment)?;

    let settlement_xch = Coin::new(alice.coin.coin_id(), settlement_puzzle_hash(), 1000);
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 1000)?;
    let claim_spend = settle(ctx, vec![claim])?;
    ctx.spend(settlement_xch, claim_spend)?;

    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk, bob.sk]),
        ErrorCode::AssertPuzzleAnnouncementFailed
    );

    Ok(())
}

#[test]
fn payment_from_the_wrong_asset_is_rejected() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(1000);
    let bob = sim.bls(0);
    let wanted = issue_single(&mut sim, ctx, &alice, 1)?;
    let bob_dog = issue_single(&mut sim, ctx, &bob, 500)?;
    assert_ne!(wanted.asset_id, bob_dog.asset_id);

    // Alice requests 500 of an asset that Bob doesn't have.
    let requested = payment(ctx, alice.coin.coin_id(), alice.puzzle_hash, 500)?;
    let conditions = Conditions::new()
        .create_coin(settlement_puzzle_hash(), 1000, Memos::None)
        .extend(assert_dog_payment(ctx, wanted.asset_id, &requested));
    StandardLayer::new(alice.pk).spend(ctx, alice.coin, conditions)?;

    // Bob pays with a different asset.
    let settlement_dog = offer_dog(ctx, &bob, bob_dog, Conditions::new())?;
    take_dog(ctx, settlement_dog, requested)?;

    let settlement_xch = Coin::new(alice.coin.coin_id(), settlement_puzzle_hash(), 1000);
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 1000)?;
    let claim_spend = settle(ctx, vec![claim])?;
    ctx.spend(settlement_xch, claim_spend)?;

    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk, bob.sk]),
        ErrorCode::AssertPuzzleAnnouncementFailed
    );

    Ok(())
}
