//! Revocable DOGs, which wrap the p2 puzzle in the Wallet SDK's revocation layer (the same puzzle
//! that revocable CATs use), so the issuer's hidden puzzle can claw back tokens.
//!
//! The revocation layer is an ordinary inner puzzle as far as the DOG puzzles are concerned, so
//! the ring still conserves supply, and the TAIL is still the only way to issue or melt tokens.

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
    clvm_utils::ToTreeHash,
    clvmr::{Allocator, NodePtr, serde::node_from_bytes},
    driver::{Layer, RevocationLayer, SettlementLayer, Spend, SpendContext, StandardLayer},
    puzzles::SETTLEMENT_PAYMENT_HASH,
    test::{BlsPairWithCoin, Simulator, SimulatorError},
    types::{
        Conditions,
        conditions::{ReceiveMessage, SendMessage},
        payment_assertion,
        puzzles::RevocationSolution,
        tree_hash_notarized_payment,
    },
};
use common::*;
use dog::{
    Dog, DogError, DogExecArgs, DogSpend, DogUris, EverythingWithSignatureTailArgs,
    inner_puzzle_hash,
};

/// Issues revocable tokens with the single issuance TAIL to the owner. The hidden puzzle is the
/// issuer's standard puzzle.
fn issue_revocable(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    issuer: &BlsPairWithCoin,
    outputs: &[(Bytes32, u64)],
) -> Result<Vec<Dog>> {
    let parent = sim.bls(0);
    let amount = outputs.iter().map(|(_, amount)| amount).sum();
    let conditions = send_many(ctx, outputs)?;
    let issuance = Dog::single_issuance_revocable(
        ctx,
        parent.coin.coin_id(),
        issuer.puzzle_hash,
        &metadata(),
        &DogUris::default(),
        amount,
        conditions,
    )?;
    StandardLayer::new(parent.pk).spend(ctx, parent.coin, issuance.parent_conditions)?;
    submit(sim, ctx, &[parent.sk])?;
    Ok(issuance.children)
}

/// The issuer revokes the coins with its hidden puzzle, outputting the conditions from the first.
fn revoke_all(
    ctx: &mut SpendContext,
    issuer: &BlsPairWithCoin,
    dogs: &[Dog],
    mut conditions: Conditions,
) -> Result<Vec<Dog>> {
    let mut spends = Vec::new();
    for dog in dogs {
        let conditions = std::mem::take(&mut conditions);
        spends.push(DogSpend::revoke(*dog, p2(ctx, issuer, conditions)?));
    }
    Ok(Dog::spend_all(ctx, &spends)?)
}

/// Parses the children of the execution coin that created `dog`, from the simulator.
fn parse_from_parent(sim: &Simulator, dog: &Dog) -> Result<Vec<Dog>> {
    let parent = sim
        .coin_state(dog.coin.parent_coin_info)
        .expect("the parent exists")
        .coin;
    let (puzzle, solution) = sim
        .puzzle_and_solution(parent.coin_id())
        .expect("the parent is spent");
    let mut allocator = Allocator::new();
    let puzzle = node_from_bytes(&mut allocator, &puzzle)?;
    let solution = node_from_bytes(&mut allocator, &solution)?;
    Ok(
        Dog::parse_children(&mut allocator, parent, puzzle, solution)?
            .expect("the parent is an execution coin"),
    )
}

#[test]
fn issue_revocable_dogs() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dogs = issue_revocable(
        &mut sim,
        ctx,
        &issuer,
        &[(alice.puzzle_hash, 600), (bob.puzzle_hash, 400)],
    )?;

    assert_eq!(dogs.len(), 2);
    for (dog, (owner, amount)) in dogs
        .iter()
        .zip([(alice.puzzle_hash, 600), (bob.puzzle_hash, 400)])
    {
        assert_eq!(dog.p2_puzzle_hash, owner);
        assert_eq!(dog.amount, amount);
        assert_eq!(dog.hidden_puzzle_hash, Some(issuer.puzzle_hash));
        assert_eq!(
            dog.inner_puzzle_hash(),
            RevocationLayer::new(issuer.puzzle_hash, owner).tree_hash()
        );
        assert_unspent(&sim, dog);
    }

    // The coins are hinted to the owner's p2 puzzle hash, like any other DOG.
    assert!(
        sim.hinted_coins(alice.puzzle_hash)
            .contains(&dogs[0].coin.coin_id())
    );

    // Revocability isn't part of the asset id, which only depends on the TAIL and metadata. The
    // same issuance without a hidden puzzle has the same asset id.
    let parent = sim.bls(0);
    let to_alice = send(ctx, alice.puzzle_hash, 1)?;
    let unrevocable = Dog::single_issuance(
        ctx,
        parent.coin.coin_id(),
        &metadata(),
        &DogUris::default(),
        1,
        to_alice.clone(),
    )?;
    let revocable = Dog::single_issuance_revocable(
        ctx,
        parent.coin.coin_id(),
        issuer.puzzle_hash,
        &metadata(),
        &DogUris::default(),
        1,
        to_alice.clone(),
    )?;
    assert_eq!(unrevocable.eve.asset_id, revocable.eve.asset_id);
    assert_ne!(unrevocable.eve.coin, revocable.eve.coin);

    Ok(())
}

#[test]
fn owner_transfers_revocable_dog() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 1000)])?[0];

    // Alice sends some to Bob, without the issuer.
    let children = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(bob.puzzle_hash, 300), (alice.puzzle_hash, 700)],
    )?;

    // The coins created by the p2 puzzle stay revocable.
    assert_eq!(children[0].p2_puzzle_hash, bob.puzzle_hash);
    assert_eq!(children[1].p2_puzzle_hash, alice.puzzle_hash);
    for child in &children {
        assert_eq!(child.hidden_puzzle_hash, Some(issuer.puzzle_hash));
        assert_unspent(&sim, child);
    }

    // Bob can spend his coin, and merge it with Alice's in the same ring.
    let to_bob = send(ctx, bob.puzzle_hash, 1000)?;
    let spends = [
        DogSpend::new(children[0], p2(ctx, &bob, to_bob)?),
        DogSpend::new(children[1], p2(ctx, &alice, Conditions::new())?),
    ];
    let merged = Dog::spend_all(ctx, &spends)?;
    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;

    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].amount, 1000);
    assert_eq!(merged[0].hidden_puzzle_hash, Some(issuer.puzzle_hash));
    assert_unspent(&sim, &merged[0]);

    Ok(())
}

#[test]
fn owner_cant_escape_the_revocation_layer() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 1000)])?[0];

    // Alice tries to create an unrevocable coin by paying to her own puzzle hash. The revocation
    // layer wraps it, so the coin is revocable anyway.
    let children = transfer(&mut sim, ctx, &alice, dog, &[(alice.puzzle_hash, 1000)])?;
    let unrevocable = Dog::new(
        children[0].coin.parent_coin_info,
        dog.asset_id,
        alice.puzzle_hash,
        1000,
        children[0].lineage_proof,
    );
    assert!(sim.coin_state(unrevocable.coin.coin_id()).is_none());
    assert_unspent(&sim, &children[0]);

    // And the issuer can still revoke it.
    let to_issuer = send(ctx, issuer.puzzle_hash, 1000)?;
    let revoked = revoke_all(ctx, &issuer, &children, to_issuer)?;
    submit(&mut sim, ctx, &[issuer.sk])?;
    assert_unspent(&sim, &revoked[0]);

    Ok(())
}

#[test]
fn issuer_revokes_without_holder_signature() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 1000)])?[0];

    // Only the issuer signs. Alice's p2 puzzle isn't run at all.
    let to_issuer = send(ctx, issuer.puzzle_hash, 1000)?;
    let revoked = revoke_all(ctx, &issuer, &[dog], to_issuer)?;
    submit(&mut sim, ctx, &[issuer.sk.clone()])?;

    // The hidden puzzle's outputs aren't wrapped in the revocation layer.
    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].p2_puzzle_hash, issuer.puzzle_hash);
    assert_eq!(revoked[0].hidden_puzzle_hash, None);
    assert_eq!(revoked[0].amount, 1000);
    assert_unspent(&sim, &revoked[0]);

    // Alice's coin is gone, and the issuer can spend the clawed back tokens normally.
    let state = sim.coin_state(dog.coin.coin_id()).expect("the coin exists");
    assert!(state.spent_height.is_some());
    let children = transfer(
        &mut sim,
        ctx,
        &issuer,
        revoked[0],
        &[(alice.puzzle_hash, 1), (issuer.puzzle_hash, 999)],
    )?;
    assert_unspent(&sim, &children[0]);

    Ok(())
}

#[test]
fn issuer_revokes_into_a_revocable_coin() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let treasury = sim.bls(0);
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 1000)])?[0];

    // The hidden puzzle can keep the revocation layer by wrapping the puzzle hash itself. The hint
    // tells the driver and wallets that it did.
    let wrapped: Bytes32 = inner_puzzle_hash(Some(issuer.puzzle_hash), treasury.puzzle_hash).into();
    let conditions = Conditions::new().create_coin(wrapped, 1000, ctx.hint(treasury.puzzle_hash)?);
    let revoked = revoke_all(ctx, &issuer, &[dog], conditions)?;
    submit(&mut sim, ctx, &[issuer.sk])?;

    assert_eq!(revoked[0].p2_puzzle_hash, treasury.puzzle_hash);
    assert_eq!(revoked[0].hidden_puzzle_hash, Some(issuer.puzzle_hash));
    assert_unspent(&sim, &revoked[0]);
    assert_eq!(parse_from_parent(&sim, &revoked[0])?, revoked);

    // The treasury can spend it like any revocable DOG.
    let children = transfer(
        &mut sim,
        ctx,
        &treasury,
        revoked[0],
        &[(alice.puzzle_hash, 1000)],
    )?;
    assert_eq!(children[0].hidden_puzzle_hash, Some(issuer.puzzle_hash));

    Ok(())
}

#[test]
fn revoke_several_coins_in_one_ring() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dogs = issue_revocable(
        &mut sim,
        ctx,
        &issuer,
        &[
            (alice.puzzle_hash, 100),
            (alice.puzzle_hash, 200),
            (bob.puzzle_hash, 300),
        ],
    )?;

    // All three coins, from two holders, are revoked into one coin.
    let to_issuer = send(ctx, issuer.puzzle_hash, 600)?;
    let revoked = revoke_all(ctx, &issuer, &dogs, to_issuer)?;
    submit(&mut sim, ctx, &[issuer.sk])?;

    assert_eq!(revoked.len(), 1);
    assert_eq!(revoked[0].amount, 600);
    assert_unspent(&sim, &revoked[0]);

    Ok(())
}

#[test]
fn revocation_and_owner_spends_share_a_ring() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dogs = issue_revocable(
        &mut sim,
        ctx,
        &issuer,
        &[(alice.puzzle_hash, 100), (bob.puzzle_hash, 300)],
    )?;

    // The issuer revokes Alice's coin, and pays the tokens to Bob's output, while Bob spends his
    // own coin normally in the same ring.
    let to_issuer = send(ctx, issuer.puzzle_hash, 50)?;
    let to_bob = send(ctx, bob.puzzle_hash, 350)?;
    let spends = [
        DogSpend::revoke(dogs[0], p2(ctx, &issuer, to_issuer)?),
        DogSpend::new(dogs[1], p2(ctx, &bob, to_bob)?),
    ];
    let children = Dog::spend_all(ctx, &spends)?;
    submit(&mut sim, ctx, &[issuer.sk, bob.sk])?;

    assert_eq!(children[0].p2_puzzle_hash, issuer.puzzle_hash);
    assert_eq!(children[0].hidden_puzzle_hash, None);
    assert_eq!(children[1].p2_puzzle_hash, bob.puzzle_hash);
    assert_eq!(children[1].hidden_puzzle_hash, Some(issuer.puzzle_hash));
    assert_eq!(children[1].amount, 350);
    for child in &children {
        assert_unspent(&sim, child);
    }

    Ok(())
}

/// A payment from the settlement puzzle, hinted to the recipient.
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

fn settle(ctx: &mut SpendContext, payments: Vec<NotarizedPayment<NodePtr>>) -> Result<Spend> {
    Ok(SettlementLayer.construct_spend(ctx, SettlementPaymentsSolution::new(payments))?)
}

/// Alice offers her revocable DOG coin for 1000 mojos, and returns the settlement DOG coin and the
/// requested payment.
fn offer_revocable_dog_for_xch(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    issuer: &BlsPairWithCoin,
    alice: &BlsPairWithCoin,
) -> Result<(Dog, NotarizedPayment<NodePtr>)> {
    let dog = issue_revocable(sim, ctx, issuer, &[(alice.puzzle_hash, 500)])?[0];

    let requested = payment(ctx, dog.coin.coin_id(), alice.puzzle_hash, 1000)?;
    let hash = tree_hash_notarized_payment(ctx, &requested);
    let conditions = Conditions::new()
        .create_coin(SETTLEMENT_PAYMENT_HASH.into(), 500, Memos::None)
        .with(payment_assertion(SETTLEMENT_PAYMENT_HASH.into(), hash));
    let inner_spend = p2(ctx, alice, conditions)?;
    let settlement = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?[0];

    // The settlement coin is revocable too, so the issuer can still claw it back.
    assert_eq!(settlement.p2_puzzle_hash, SETTLEMENT_PAYMENT_HASH.into());
    assert_eq!(settlement.hidden_puzzle_hash, Some(issuer.puzzle_hash));

    Ok((settlement, requested))
}

#[test]
fn take_offer_of_revocable_dog_for_xch() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(1000);
    let (settlement, requested) = offer_revocable_dog_for_xch(&mut sim, ctx, &issuer, &alice)?;

    // Bob pays the XCH through the settlement puzzle.
    let conditions =
        Conditions::new().create_coin(SETTLEMENT_PAYMENT_HASH.into(), 1000, Memos::None);
    StandardLayer::new(bob.pk).spend(ctx, bob.coin, conditions)?;
    let settlement_xch = Coin::new(bob.coin.coin_id(), SETTLEMENT_PAYMENT_HASH.into(), 1000);
    let pay_spend = settle(ctx, vec![requested])?;
    ctx.spend(settlement_xch, pay_spend)?;

    // And takes the DOG tokens, which stay revocable.
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 500)?;
    let inner_spend = settle(ctx, vec![claim])?;
    let bob_dogs = Dog::spend_all(ctx, &[DogSpend::new(settlement, inner_spend)])?;

    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;

    assert_eq!(bob_dogs[0].p2_puzzle_hash, bob.puzzle_hash);
    assert_eq!(bob_dogs[0].hidden_puzzle_hash, Some(issuer.puzzle_hash));
    assert_unspent(&sim, &bob_dogs[0]);
    assert_eq!(parse_from_parent(&sim, &bob_dogs[0])?, bob_dogs);

    Ok(())
}

#[test]
fn offer_xch_for_revocable_dog() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(1000);
    let bob = sim.bls(0);
    let bob_dog = issue_revocable(&mut sim, ctx, &issuer, &[(bob.puzzle_hash, 500)])?[0];

    // Alice requests 500 tokens of the revocable DOG. The payment is asserted from the execution
    // coin of the settlement puzzle wrapped in the revocation layer, so a payment of unrevocable
    // tokens of the same asset id (or ones with a different hidden puzzle) wouldn't satisfy it.
    let requested = payment(ctx, alice.coin.coin_id(), alice.puzzle_hash, 500)?;
    let settlement_exec = DogExecArgs::puzzle_hash(
        bob_dog.asset_id,
        inner_puzzle_hash(Some(issuer.puzzle_hash), SETTLEMENT_PAYMENT_HASH.into()),
    );
    let hash = tree_hash_notarized_payment(ctx, &requested);
    let conditions = Conditions::new()
        .create_coin(SETTLEMENT_PAYMENT_HASH.into(), 1000, Memos::None)
        .with(payment_assertion(settlement_exec.into(), hash));
    StandardLayer::new(alice.pk).spend(ctx, alice.coin, conditions)?;

    // Bob sends his DOG coin to the settlement puzzle, and pays Alice from it.
    let to_settlement =
        Conditions::new().create_coin(SETTLEMENT_PAYMENT_HASH.into(), 500, Memos::None);
    let inner_spend = p2(ctx, &bob, to_settlement)?;
    let settlement = Dog::spend_all(ctx, &[DogSpend::new(bob_dog, inner_spend)])?[0];
    let inner_spend = settle(ctx, vec![requested])?;
    let alice_dogs = Dog::spend_all(ctx, &[DogSpend::new(settlement, inner_spend)])?;

    // And claims the XCH.
    let settlement_xch = Coin::new(alice.coin.coin_id(), SETTLEMENT_PAYMENT_HASH.into(), 1000);
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 1000)?;
    let claim_spend = settle(ctx, vec![claim])?;
    ctx.spend(settlement_xch, claim_spend)?;

    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;

    assert_eq!(alice_dogs[0].p2_puzzle_hash, alice.puzzle_hash);
    assert_eq!(alice_dogs[0].hidden_puzzle_hash, Some(issuer.puzzle_hash));
    assert_unspent(&sim, &alice_dogs[0]);

    Ok(())
}

#[test]
fn unrevocable_payment_doesnt_satisfy_a_revocable_request() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(1000);
    let bob = sim.bls(0);

    // Bob only has tokens that the issuer revoked into an unrevocable coin, which has the same
    // asset id as the revocable ones.
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(bob.puzzle_hash, 500)])?[0];
    let to_bob = send(ctx, bob.puzzle_hash, 500)?;
    let bob_dog = revoke_all(ctx, &issuer, &[dog], to_bob)?[0];
    submit(&mut sim, ctx, &[issuer.sk])?;
    assert_eq!(bob_dog.hidden_puzzle_hash, None);

    let requested = payment(ctx, alice.coin.coin_id(), alice.puzzle_hash, 500)?;
    let settlement_exec = DogExecArgs::puzzle_hash(
        bob_dog.asset_id,
        inner_puzzle_hash(Some(issuer.puzzle_hash), SETTLEMENT_PAYMENT_HASH.into()),
    );
    let hash = tree_hash_notarized_payment(ctx, &requested);
    let conditions = Conditions::new()
        .create_coin(SETTLEMENT_PAYMENT_HASH.into(), 1000, Memos::None)
        .with(payment_assertion(settlement_exec.into(), hash));
    StandardLayer::new(alice.pk).spend(ctx, alice.coin, conditions)?;

    let to_settlement =
        Conditions::new().create_coin(SETTLEMENT_PAYMENT_HASH.into(), 500, Memos::None);
    let inner_spend = p2(ctx, &bob, to_settlement)?;
    let settlement = Dog::spend_all(ctx, &[DogSpend::new(bob_dog, inner_spend)])?[0];
    let inner_spend = settle(ctx, vec![requested])?;
    Dog::spend_all(ctx, &[DogSpend::new(settlement, inner_spend)])?;

    let settlement_xch = Coin::new(alice.coin.coin_id(), SETTLEMENT_PAYMENT_HASH.into(), 1000);
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
fn revoke_coin_held_by_settlement_payments() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(0);

    // Alice's coin is held by the settlement puzzle on chain, in the middle of a trade.
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 500)])?[0];
    let to_settlement =
        Conditions::new().create_coin(SETTLEMENT_PAYMENT_HASH.into(), 500, Memos::None);
    let inner_spend = p2(ctx, &alice, to_settlement)?;
    let settlement = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?[0];
    submit(&mut sim, ctx, &[alice.sk.clone()])?;
    assert_eq!(settlement.hidden_puzzle_hash, Some(issuer.puzzle_hash));
    assert_unspent(&sim, &settlement);

    // The issuer revokes it, without the settlement puzzle or Alice.
    let to_issuer = send(ctx, issuer.puzzle_hash, 500)?;
    let revoked = revoke_all(ctx, &issuer, &[settlement], to_issuer)?;
    submit(&mut sim, ctx, &[issuer.sk])?;
    assert_eq!(revoked[0].p2_puzzle_hash, issuer.puzzle_hash);
    assert_unspent(&sim, &revoked[0]);

    // Now nobody can take it.
    let claim = payment(ctx, Bytes32::default(), bob.puzzle_hash, 500)?;
    let inner_spend = settle(ctx, vec![claim])?;
    Dog::spend_all(ctx, &[DogSpend::new(settlement, inner_spend)])?;
    assert_eq!(submit_err(&mut sim, ctx, &[]), ErrorCode::DoubleSpend);

    Ok(())
}

#[test]
fn revoke_coin_in_the_middle_of_an_offer() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(1000);
    let alice = sim.bls(0);
    let (settlement, requested) = offer_revocable_dog_for_xch(&mut sim, ctx, &issuer, &alice)?;
    let maker_spends = ctx.take();

    // The issuer revokes the offered settlement coin instead of taking it with the settlement
    // puzzle. Alice's spend still asserts that she's paid, so it can't be confirmed without that.
    for coin_spend in maker_spends.clone() {
        ctx.insert(coin_spend);
    }
    let to_issuer = send(ctx, issuer.puzzle_hash, 500)?;
    revoke_all(ctx, &issuer, &[settlement], to_issuer)?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone(), issuer.sk.clone()]),
        ErrorCode::AssertPuzzleAnnouncementFailed
    );

    // If the issuer pays Alice, it can claw back the offered tokens in the same transaction.
    for coin_spend in maker_spends {
        ctx.insert(coin_spend);
    }
    let conditions =
        Conditions::new().create_coin(SETTLEMENT_PAYMENT_HASH.into(), 1000, Memos::None);
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, conditions)?;
    let settlement_xch = Coin::new(issuer.coin.coin_id(), SETTLEMENT_PAYMENT_HASH.into(), 1000);
    let pay_spend = settle(ctx, vec![requested])?;
    ctx.spend(settlement_xch, pay_spend)?;
    let to_issuer = send(ctx, issuer.puzzle_hash, 500)?;
    let revoked = revoke_all(ctx, &issuer, &[settlement], to_issuer)?;
    submit(&mut sim, ctx, &[alice.sk.clone(), issuer.sk.clone()])?;

    assert_eq!(revoked[0].p2_puzzle_hash, issuer.puzzle_hash);
    assert_eq!(revoked[0].hidden_puzzle_hash, None);
    assert_unspent(&sim, &revoked[0]);
    assert!(
        sim.unspent_coins(alice.puzzle_hash, false)
            .iter()
            .any(|coin| coin.amount == 1000)
    );

    Ok(())
}

#[test]
fn non_issuer_cant_revoke() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let attacker = sim.bls(0);
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 1000)])?[0];

    // The driver won't revoke with a puzzle that isn't the hidden puzzle.
    let to_attacker = send(ctx, attacker.puzzle_hash, 1000)?;
    let attacker_spend = p2(ctx, &attacker, to_attacker)?;
    assert!(matches!(
        Dog::spend_all(ctx, &[DogSpend::revoke(dog, attacker_spend)]),
        Err(DogError::InnerPuzzleHashMismatch)
    ));

    // Neither does the revocation layer, as the hidden puzzle or as the p2 puzzle.
    for hidden in [true, false] {
        let spend = DogSpend {
            hidden,
            ..DogSpend::new(dog, attacker_spend)
        };
        dog.spend(ctx, spend, 1000, 0, dog.inner_puzzle_hash().into())?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[attacker.sk.clone()]),
            PUZZLE_FAILED
        );
    }

    // The attacker can reveal the issuer's hidden puzzle, but can't sign for it.
    let to_attacker = send(ctx, attacker.puzzle_hash, 1000)?;
    let hidden_spend = p2(ctx, &issuer, to_attacker)?;
    Dog::spend_all(ctx, &[DogSpend::revoke(dog, hidden_spend)])?;
    let result = sim.spend_coins(ctx.take(), &[attacker.sk.clone()]);
    assert!(
        matches!(result, Err(SimulatorError::MissingKey)),
        "{result:?}"
    );
    let to_attacker = send(ctx, attacker.puzzle_hash, 1000)?;
    let hidden_spend = p2(ctx, &issuer, to_attacker)?;
    Dog::spend_all(ctx, &[DogSpend::revoke(dog, hidden_spend)])?;
    let coin_spends = ctx.take();
    let signature = chia_wallet_sdk::test::sign_transaction(&coin_spends, &[attacker.sk.clone()])
        .unwrap_or_default();
    let result = sim.new_transaction(chia_wallet_sdk::chia::protocol::SpendBundle::new(
        coin_spends,
        signature,
    ));
    assert!(
        matches!(
            result,
            Err(SimulatorError::Validation(ErrorCode::BadAggregateSignature))
        ),
        "{result:?}"
    );

    // An unrevocable DOG can't be spent with a hidden puzzle at all.
    let plain = issue_single(&mut sim, ctx, &alice, 1000)?;
    let to_issuer = send(ctx, issuer.puzzle_hash, 1000)?;
    let hidden_spend = p2(ctx, &issuer, to_issuer)?;
    assert!(matches!(
        Dog::spend_all(ctx, &[DogSpend::revoke(plain, hidden_spend)]),
        Err(DogError::NotRevocable)
    ));

    // Alice's coin is untouched.
    assert_unspent(&sim, &dog);

    Ok(())
}

#[test]
fn revocation_cant_change_supply() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 1000)])?[0];

    // The driver rejects a revocation that creates more or fewer tokens than the coin holds.
    for amount in [999, 1001] {
        let conditions = send(ctx, issuer.puzzle_hash, amount)?;
        let hidden_spend = p2(ctx, &issuer, conditions)?;
        assert!(matches!(
            Dog::spend_all(ctx, &[DogSpend::revoke(dog, hidden_spend)]),
            Err(DogError::UnbalancedSpend(_))
        ));
    }

    // And so does the ring, if the spend is built by hand.
    for amount in [999, 1001, 2_000_000] {
        let conditions = send(ctx, issuer.puzzle_hash, amount)?;
        let hidden_spend = p2(ctx, &issuer, conditions)?;
        let created = i128::from(amount);
        dog.spend(
            ctx,
            DogSpend::revoke(dog, hidden_spend),
            1000,
            1000 - created,
            dog.inner_puzzle_hash().into(),
        )?;
        // The subtotal it sends is 0, but the one it receives was claimed to be nonzero, or vice
        // versa, so the ring doesn't close.
        assert_eq!(
            submit_err(&mut sim, ctx, &[issuer.sk.clone()]),
            ErrorCode::MessageNotSentOrReceived
        );
    }

    // The hidden puzzle can't take part in the ring of another coin, since ring messages from
    // inner puzzles are rejected.
    let exec_puzzle_hash: Bytes32 = dog.exec_puzzle_hash().into();
    let fake_ring = Conditions::new()
        .with(SendMessage::new(
            0b00_0010,
            vec![0xd0].into(),
            vec![ctx.alloc(&exec_puzzle_hash)?],
        ))
        .with(ReceiveMessage::new(0b00_0010, vec![0xd0].into(), vec![]))
        .extend(send(ctx, issuer.puzzle_hash, 1000)?);
    let hidden_spend = p2(ctx, &issuer, fake_ring)?;
    Dog::spend_all(ctx, &[DogSpend::revoke(dog, hidden_spend)])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[issuer.sk.clone()]),
        PUZZLE_FAILED
    );

    assert_unspent(&sim, &dog);

    Ok(())
}

#[test]
fn issuer_revokes_and_melts_with_the_tail() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let parent = sim.bls(0);
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let issuance = Dog::multi_issuance_revocable(
        ctx,
        parent.coin.coin_id(),
        issuer.pk,
        issuer.puzzle_hash,
        &metadata(),
        &DogUris::default(),
        1000,
        conditions,
    )?;
    StandardLayer::new(parent.pk).spend(ctx, parent.coin, issuance.parent_conditions)?;
    submit(&mut sim, ctx, &[parent.sk, issuer.sk.clone()])?;
    let dog = issuance.children[0];
    assert_eq!(dog.hidden_puzzle_hash, Some(issuer.puzzle_hash));

    // Revoking and melting are separate permissions: the hidden puzzle moves tokens, and the TAIL
    // changes the supply. The issuer holds both here, so it can claw back and burn 400 tokens.
    let tail = ctx.curry(EverythingWithSignatureTailArgs::new(issuer.pk))?;
    let to_issuer = send(ctx, issuer.puzzle_hash, 600)?;
    let hidden_spend = p2(ctx, &issuer, to_issuer)?;
    let spend = DogSpend {
        hidden: true,
        ..DogSpend::with_tail(
            dog,
            hidden_spend,
            Spend::new(tail, NodePtr::NIL),
            metadata().tree_hash().into(),
            -400,
        )
    };
    let children = Dog::spend_all(ctx, &[spend])?;
    submit(&mut sim, ctx, &[issuer.sk])?;

    assert_eq!(children[0].amount, 600);
    assert_eq!(children[0].hidden_puzzle_hash, None);
    assert_unspent(&sim, &children[0]);

    Ok(())
}

#[test]
fn wallet_parses_revocable_children() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let bob = sim.bls(0);

    // The eve coin's children.
    let eve_children = issue_revocable(
        &mut sim,
        ctx,
        &issuer,
        &[(alice.puzzle_hash, 700), (alice.puzzle_hash, 300)],
    )?;
    assert_eq!(parse_from_parent(&sim, &eve_children[0])?, eve_children);

    // The children of a normal spend are revocable, with the p2 puzzle hashes from the p2 spend.
    let sent = transfer(
        &mut sim,
        ctx,
        &alice,
        eve_children[0],
        &[(bob.puzzle_hash, 200), (alice.puzzle_hash, 500)],
    )?;
    let parsed = parse_from_parent(&sim, &sent[0])?;
    assert_eq!(parsed, sent);
    assert_eq!(parsed[0].p2_puzzle_hash, bob.puzzle_hash);
    assert_eq!(parsed[0].hidden_puzzle_hash, Some(issuer.puzzle_hash));

    // Bob finds his coin by its hint, and can spend the parsed coin.
    assert!(
        sim.hinted_coins(bob.puzzle_hash)
            .contains(&parsed[0].coin.coin_id())
    );
    transfer(&mut sim, ctx, &bob, parsed[0], &[(bob.puzzle_hash, 200)])?;

    // The children of a revocation spend are unrevocable, unless the hidden puzzle wrapped them
    // and hinted the p2 puzzle hash. A wrapped coin without that hint can't be recognized, and is
    // assumed to be unrevocable, like with revocable CATs.
    let wrapped: Bytes32 = inner_puzzle_hash(Some(issuer.puzzle_hash), bob.puzzle_hash).into();
    let conditions = Conditions::new()
        .create_coin(issuer.puzzle_hash, 100, ctx.hint(issuer.puzzle_hash)?)
        .create_coin(wrapped, 150, ctx.hint(bob.puzzle_hash)?)
        .create_coin(wrapped, 50, Memos::None);
    let hidden_spend = p2(ctx, &issuer, conditions)?;
    let revoked = Dog::spend_all(ctx, &[DogSpend::revoke(eve_children[1], hidden_spend)])?;
    submit(&mut sim, ctx, &[issuer.sk.clone()])?;

    let parsed = parse_from_parent(&sim, &revoked[0])?;
    assert_eq!(parsed, revoked);
    assert_eq!(parsed[0].hidden_puzzle_hash, None);
    assert_eq!(parsed[0].p2_puzzle_hash, issuer.puzzle_hash);
    assert_eq!(parsed[1].hidden_puzzle_hash, Some(issuer.puzzle_hash));
    assert_eq!(parsed[1].p2_puzzle_hash, bob.puzzle_hash);
    assert_eq!(parsed[2].hidden_puzzle_hash, None);
    assert_eq!(parsed[2].p2_puzzle_hash, wrapped);
    for child in &parsed {
        assert_unspent(&sim, child);
    }

    // The coins it recognized can be spent.
    transfer(
        &mut sim,
        ctx,
        &issuer,
        parsed[0],
        &[(issuer.puzzle_hash, 100)],
    )?;
    transfer(&mut sim, ctx, &bob, parsed[1], &[(bob.puzzle_hash, 150)])?;

    Ok(())
}

/// The revocation layer's solution is unsigned. Anyone who sees a revocation spend in the mempool
/// can't turn it into a spend of the p2 puzzle, since that needs the owner's signature, and can't
/// change the hidden puzzle's output, since that's signed by the issuer.
#[test]
fn revocation_spend_cant_be_tampered_with() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let issuer = sim.bls(0);
    let alice = sim.bls(0);
    let attacker = sim.bls(0);
    let dog = issue_revocable(&mut sim, ctx, &issuer, &[(alice.puzzle_hash, 1000)])?[0];

    let to_issuer = send(ctx, issuer.puzzle_hash, 1000)?;
    revoke_all(ctx, &issuer, &[dog], to_issuer)?;
    let coin_spends = ctx.take();
    let signature = chia_wallet_sdk::test::sign_transaction(&coin_spends, &[issuer.sk.clone()])?;

    // Replace the hidden puzzle's delegated spend with one that pays the attacker, keeping the
    // issuer's signature.
    let exec_index = coin_spends
        .iter()
        .position(|coin_spend| coin_spend.coin == dog.exec_coin(1000))
        .expect("the execution coin is spent");
    let mut tampered = coin_spends.clone();
    let solution = node_from_bytes(ctx, &tampered[exec_index].solution)?;
    let (inner_solution, ring) = ctx.extract::<(NodePtr, NodePtr)>(solution)?;
    let revocation = ctx.extract::<RevocationSolution<NodePtr, NodePtr>>(inner_solution)?;
    let to_attacker = send(ctx, attacker.puzzle_hash, 1000)?;
    let attacker_hidden = p2(ctx, &issuer, to_attacker)?;
    let new_inner = ctx.alloc(&RevocationSolution::new(
        true,
        revocation.puzzle,
        attacker_hidden.solution,
    ))?;
    let new_solution = ctx.alloc(&(new_inner, ring))?;
    tampered[exec_index].solution = ctx.serialize(&new_solution)?;

    let result = sim.new_transaction(chia_wallet_sdk::chia::protocol::SpendBundle::new(
        tampered,
        signature.clone(),
    ));
    assert!(
        matches!(
            result,
            Err(SimulatorError::Validation(ErrorCode::BadAggregateSignature))
        ),
        "{result:?}"
    );

    // The original spend is still valid.
    sim.new_transaction(chia_wallet_sdk::chia::protocol::SpendBundle::new(
        coin_spends,
        signature,
    ))?;

    Ok(())
}
