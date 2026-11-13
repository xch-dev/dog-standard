//! Attempts to break the rules of the standard, which must all be rejected.
//!
//! Each test that expects a failure also has a valid counterpart nearby (often in the same test),
//! so that a failure can't come from a mistake in the test setup instead of the puzzle.

mod common;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{
        consensus::validation_error::ErrorCode,
        protocol::{Bytes, Bytes32, Coin, SpendBundle},
        puzzle_types::{LineageProof, Memos},
    },
    clvm_traits::clvm_list,
    clvmr::NodePtr,
    driver::{Spend, SpendContext, StandardLayer},
    test::{Simulator, sign_transaction},
    types::Conditions,
};
use common::*;
use dog::{
    Dog, DogArgs, DogExecArgs, DogExecSolution, DogSpend, GenesisByCoinIdTailArgs, TailSpend,
    asset_id,
};

use chia_wallet_sdk::clvm_utils::ToTreeHash;

/// The ring message prefix, which inner puzzles can't use.
const RING_PREFIX: u8 = 0xd0;

fn ring_message(subtotal: i128) -> Bytes {
    let mut message = vec![RING_PREFIX];
    message.extend(int_bytes(subtotal));
    Bytes::new(message)
}

/// The canonical CLVM encoding of an integer.
fn int_bytes(value: i128) -> Vec<u8> {
    let mut bytes = value.to_be_bytes().to_vec();
    while bytes.len() > 1
        && ((bytes[0] == 0 && bytes[1] & 0x80 == 0) || (bytes[0] == 0xff && bytes[1] & 0x80 != 0))
    {
        bytes.remove(0);
    }
    if bytes == [0] { Vec::new() } else { bytes }
}

#[test]
fn tail_must_match_asset_id() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let real_tail = ctx.curry(GenesisByCoinIdTailArgs::new(alice.coin.coin_id()))?;
    let metadata_hash: Bytes32 = metadata().tree_hash().into();
    let asset_id = asset_id(ctx.tree_hash(real_tail), metadata_hash.into());

    // A TAIL that allows anything, which doesn't match the asset id.
    let fake_tail = ctx.alloc(&chia_wallet_sdk::clvm_traits::clvm_quote!(()))?;

    for (tail, metadata_hash) in [
        (fake_tail, metadata_hash),
        // The right TAIL, but different metadata.
        (real_tail, DogMetadataHash::other()),
    ] {
        let conditions = send(ctx, alice.puzzle_hash, 1000)?;
        let inner_spend = ctx.delegated_spend(conditions)?;

        // The parent can be any coin, since the TAIL would be the only thing stopping this.
        let parent = sim.bls(0);
        let eve = Dog::new(
            parent.coin.coin_id(),
            asset_id,
            ctx.tree_hash(inner_spend.puzzle).into(),
            1000,
            None,
        );
        StandardLayer::new(parent.pk).spend(
            ctx,
            parent.coin,
            Conditions::new().create_coin(eve.coin.puzzle_hash, 0, Memos::None),
        )?;
        Dog::spend_all(
            ctx,
            &[DogSpend::with_tail(
                eve,
                inner_spend,
                Spend::new(tail, NodePtr::NIL),
                metadata_hash,
                0,
            )],
        )?;

        assert_eq!(submit_err(&mut sim, ctx, &[parent.sk]), PUZZLE_FAILED);
    }

    Ok(())
}

struct DogMetadataHash;

impl DogMetadataHash {
    fn other() -> Bytes32 {
        let mut metadata = metadata();
        metadata.decimals += 1;
        metadata.tree_hash().into()
    }
}

#[test]
fn negative_output_cant_inflate_supply() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    // The outputs add up to 1000, but one of them is negative.
    let conditions = ctx.alloc(&clvm_list!(
        clvm_list!(51, alice.puzzle_hash, 1_000_000i64),
        clvm_list!(51, Bytes32::new([0; 32]), -999_000i64)
    ))?;
    let inner_spend = raw_p2(ctx, &alice, conditions)?;
    spend_ring(ctx, &[RawRingItem::new(dog, inner_spend, 1000)])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        PUZZLE_FAILED
    );

    // A zero output is fine.
    let conditions = ctx.alloc(&clvm_list!(
        clvm_list!(51, alice.puzzle_hash, 1000),
        clvm_list!(51, Bytes32::new([0; 32]), 0)
    ))?;
    let inner_spend = raw_p2(ctx, &alice, conditions)?;
    spend_ring(ctx, &[RawRingItem::new(dog, inner_spend, 1000)])?;
    submit(&mut sim, ctx, &[alice.sk])?;

    Ok(())
}

#[test]
fn output_amount_cant_exceed_u64() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let issuer = sim.bls(0);

    // Issue two coins whose total is exactly 2^64.
    let half = 1u64 << 63;
    let conditions = send_many(
        ctx,
        &[(alice.puzzle_hash, half), (alice.puzzle_hash, half - 1)],
    )?;
    let issuance = Dog::multi_issuance(
        ctx,
        issuer.coin.coin_id(),
        issuer.pk,
        &metadata(),
        &DogUris::default(),
        u64::MAX,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;
    submit(&mut sim, ctx, &[issuer.sk.clone()])?;
    let [first, second] = issuance.children[..] else {
        panic!("expected two children");
    };

    let more = sim.bls(0);
    let conditions = send(ctx, alice.puzzle_hash, 1)?;
    let issuance = Dog::multi_issuance(
        ctx,
        more.coin.coin_id(),
        issuer.pk,
        &metadata(),
        &DogUris::default(),
        1,
        conditions,
    )?;
    assert_eq!(issuance.eve.asset_id, first.asset_id);
    StandardLayer::new(more.pk).spend(ctx, more.coin, issuance.parent_conditions)?;
    submit(&mut sim, ctx, &[more.sk, issuer.sk])?;
    let third = issuance.children[0];

    // Merging all of them into one coin of 2^64 would make it impossible to spend.
    let total = i128::from(u64::MAX) + 1;
    let conditions = ctx.alloc(&clvm_list!(clvm_list!(51, alice.puzzle_hash, total)))?;
    let items = [
        RawRingItem::new(first, raw_p2(ctx, &alice, conditions)?, total),
        RawRingItem::new(second, raw_p2(ctx, &alice, NodePtr::NIL)?, 0),
        RawRingItem::new(third, raw_p2(ctx, &alice, NodePtr::NIL)?, 0),
    ];
    spend_ring(ctx, &items)?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        PUZZLE_FAILED
    );

    // But two coins of u64::MAX and 1 is fine.
    let conditions = send_many(
        ctx,
        &[(alice.puzzle_hash, u64::MAX), (alice.puzzle_hash, 1)],
    )?;
    let spends = [
        DogSpend::new(first, p2(ctx, &alice, conditions)?),
        DogSpend::new(second, p2(ctx, &alice, Conditions::new())?),
        DogSpend::new(third, p2(ctx, &alice, Conditions::new())?),
    ];
    let children = Dog::spend_all(ctx, &spends)?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;
    assert_eq!(children[0].amount, u64::MAX);

    // The coin with u64::MAX can be spent.
    transfer(
        &mut sim,
        ctx,
        &alice,
        children[0],
        &[(alice.puzzle_hash, u64::MAX)],
    )?;

    Ok(())
}

#[test]
fn non_canonical_output_amount_is_normalized() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    // 1000 with a redundant leading zero.
    let amount = Bytes::new(vec![0x00, 0x03, 0xe8]);
    let conditions = ctx.alloc(&clvm_list!(clvm_list!(51, alice.puzzle_hash, amount)))?;
    let inner_spend = raw_p2(ctx, &alice, conditions)?;
    spend_ring(ctx, &[RawRingItem::new(dog, inner_spend, 1000)])?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;

    // The child has the canonical amount in its puzzle hash, so it can be spent like any other.
    let child = dog.child(1000, alice.puzzle_hash, 1000);
    assert_unspent(&sim, &child);
    transfer(&mut sim, ctx, &alice, child, &[(alice.puzzle_hash, 1000)])?;

    Ok(())
}

#[test]
fn forged_dog_coin_needs_tail() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let real = issue_single(&mut sim, ctx, &alice, 1000)?;

    // Anyone can create a coin with the puzzle hash of a DOG coin, with any amount.
    let attacker = sim.bls(0);
    let forged = Dog::new(
        attacker.coin.coin_id(),
        real.asset_id,
        alice.puzzle_hash,
        1_000_000,
        None,
    );
    let conditions = Conditions::new().create_coin(forged.coin.puzzle_hash, 0, Memos::None);
    StandardLayer::new(attacker.pk).spend(ctx, attacker.coin, conditions)?;
    submit(&mut sim, ctx, &[attacker.sk])?;

    // Without a lineage proof or the TAIL, it can't be spent.
    let conditions = send(ctx, alice.puzzle_hash, 1_000_000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(forged, inner_spend)])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        PUZZLE_FAILED
    );

    // A lineage proof that points to a real execution coin doesn't help, since it isn't the parent.
    let forged = Dog {
        lineage_proof: real.lineage_proof,
        ..forged
    };
    let conditions = send(ctx, alice.puzzle_hash, 1_000_000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(forged, inner_spend)])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        ErrorCode::AssertMyParentIdFailed
    );

    // Neither does a lineage proof that claims the parent is an execution coin.
    let forged = Dog {
        lineage_proof: Some(LineageProof {
            parent_parent_coin_info: attacker.coin.parent_coin_info,
            parent_inner_puzzle_hash: attacker.puzzle_hash,
            parent_amount: attacker.coin.amount,
        }),
        ..forged
    };
    let conditions = send(ctx, alice.puzzle_hash, 1_000_000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(forged, inner_spend)])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        ErrorCode::AssertMyParentIdFailed
    );

    // The real coin can still be spent.
    transfer(&mut sim, ctx, &alice, real, &[(alice.puzzle_hash, 1000)])?;

    Ok(())
}

#[test]
fn forged_exec_coin_is_rejected() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let real = issue_single(&mut sim, ctx, &alice, 1000)?;

    // An attacker funds an execution coin with real mojos, without a DOG coin as its parent. If it
    // could be spent, it would create DOG tokens for 1 mojo each.
    let attacker = sim.bls(500);
    let exec_puzzle_hash: Bytes32 = real.exec_puzzle_hash().into();
    let conditions = Conditions::new().create_coin(exec_puzzle_hash, 500, Memos::None);
    StandardLayer::new(attacker.pk).spend(ctx, attacker.coin, conditions)?;
    submit(&mut sim, ctx, &[attacker.sk.clone()])?;
    let exec_coin = Coin::new(attacker.coin.coin_id(), exec_puzzle_hash, 500);

    // The attacker claims that the parent is a DOG coin with an amount of 500.
    let conditions = send(ctx, attacker.puzzle_hash, 500)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    let exec_puzzle = ctx.curry(DogExecArgs::new(real.asset_id, inner_spend.puzzle))?;
    let exec_solution = ctx.alloc(&DogExecSolution {
        inner_solution: inner_spend.solution,
        my_amount: 500,
        parent_amount: 500,
        prev_subtotal: 0,
        next_inner_puzzle_hash: alice.puzzle_hash,
    })?;
    ctx.spend(exec_coin, Spend::new(exec_puzzle, exec_solution))?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        ErrorCode::MessageNotSentOrReceived
    );

    Ok(())
}

#[test]
fn dog_coin_cant_be_spent_without_exec_coin() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;

    // Remove the execution coin's spend, leaving only the DOG coin. The execution coin would keep
    // its mojos, which don't exist.
    let coin_spends: Vec<_> = ctx
        .take()
        .into_iter()
        .filter(|cs| cs.coin == dog.coin)
        .collect();
    assert_eq!(coin_spends.len(), 1);
    assert_eq!(
        submit_bundle_err(&mut sim, coin_spends.clone(), &[]),
        ErrorCode::MintingCoin
    );

    // Even if someone pays for the mojos, the DOG coin's message to the execution coin must be
    // received, so the execution coin can't be left unspent.
    let funder = sim.bls(1000);
    StandardLayer::new(funder.pk).spend(ctx, funder.coin, Conditions::new())?;
    let mut coin_spends = coin_spends;
    coin_spends.extend(ctx.take());
    assert_eq!(
        submit_bundle_err(&mut sim, coin_spends, &[funder.sk]),
        ErrorCode::MessageNotSentOrReceived
    );

    Ok(())
}

/// A change to the solution of an execution coin.
type Tamper = fn(&mut DogExecSolution<NodePtr>);

#[test]
fn exec_coin_cant_lie_about_amounts() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let coins = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(alice.puzzle_hash, 400), (alice.puzzle_hash, 600)],
    )?;

    let cases: [(&str, Tamper); 4] = [
        ("my_amount", |solution| solution.my_amount += 1),
        ("parent_amount", |solution| solution.parent_amount += 1),
        ("prev_subtotal", |solution| solution.prev_subtotal += 1),
        ("next_inner_puzzle_hash", |solution| {
            solution.next_inner_puzzle_hash = Bytes32::new([7; 32]);
        }),
    ];

    for (name, tamper) in cases {
        let spends = ring_spends(ctx, &alice, &coins)?;
        Dog::spend_all(ctx, &spends)?;

        let mut coin_spends = ctx.take();
        let exec_spend = coin_spends
            .iter_mut()
            .find(|cs| cs.coin == coins[0].exec_coin(400))
            .expect("the execution coin should be spent");

        let mut allocator = chia_wallet_sdk::clvmr::Allocator::new();
        let solution = chia_wallet_sdk::clvmr::serde::node_from_bytes(
            &mut allocator,
            exec_spend.solution.as_ref(),
        )?;
        let mut solution: DogExecSolution<NodePtr> =
            chia_wallet_sdk::clvm_traits::FromClvm::from_clvm(&allocator, solution)?;
        tamper(&mut solution);
        let solution = chia_wallet_sdk::clvm_traits::ToClvm::to_clvm(&solution, &mut allocator)?;
        exec_spend.solution = chia_wallet_sdk::chia::protocol::Program::new(
            chia_wallet_sdk::clvmr::serde::node_to_bytes(&allocator, solution)?.into(),
        );

        assert_eq!(
            submit_bundle_err(&mut sim, coin_spends, &[alice.sk.clone()]),
            ErrorCode::MessageNotSentOrReceived,
            "tampering with {name} should be rejected"
        );
    }

    // The untampered spend works.
    let spends = ring_spends(ctx, &alice, &coins)?;
    Dog::spend_all(ctx, &spends)?;
    submit(&mut sim, ctx, &[alice.sk])?;

    Ok(())
}

/// Spends two coins of 400 and 600 together, sending 700 and 300 back to the owner.
fn ring_spends(
    ctx: &mut SpendContext,
    owner: &chia_wallet_sdk::test::BlsPairWithCoin,
    coins: &[Dog],
) -> Result<Vec<DogSpend>> {
    let first = send(ctx, owner.puzzle_hash, 700)?;
    let second = send(ctx, owner.puzzle_hash, 300)?;
    Ok(vec![
        DogSpend::new(coins[0], p2(ctx, owner, first)?),
        DogSpend::new(coins[1], p2(ctx, owner, second)?),
    ])
}

/// In a ring with a single coin, the coin sends the subtotal to itself, so any starting subtotal
/// works. This isn't a problem, since only the differences between subtotals matter.
#[test]
fn subtotals_are_relative() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    dog.spend(
        ctx,
        DogSpend::new(dog, inner_spend),
        1000,
        12345,
        alice.puzzle_hash,
    )?;
    submit(&mut sim, ctx, &[alice.sk])?;

    Ok(())
}

/// The message mode of the ring, in which the sender commits to nothing and the receiver commits
/// to its puzzle hash.
const RING_MODE: u8 = 0b000_010;

/// An XCH coin that sends a ring message to an execution coin's puzzle hash, and optionally receives
/// one, to try to take part in the ring.
fn ring_attacker(
    ctx: &mut SpendContext,
    attacker: &chia_wallet_sdk::test::BlsPairWithCoin,
    send: (i128, Bytes32),
    receive: Option<i128>,
) -> Result<()> {
    let mut conditions =
        vec![ctx.alloc(&clvm_list!(66, RING_MODE, ring_message(send.0), send.1))?];
    if let Some(subtotal) = receive {
        conditions.push(ctx.alloc(&clvm_list!(67, RING_MODE, ring_message(subtotal)))?);
    }
    let delegated = ctx.alloc(&chia_wallet_sdk::clvm_traits::clvm_quote!(conditions))?;
    let attacker_spend = StandardLayer::new(attacker.pk)
        .delegated_inner_spend(ctx, Spend::new(delegated, NodePtr::NIL))?;
    ctx.spend(attacker.coin, attacker_spend)?;
    Ok(())
}

#[test]
fn ring_cant_be_joined_by_other_coins() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let coins = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(alice.puzzle_hash, 400), (alice.puzzle_hash, 600)],
    )?;
    let exec_puzzle_hash: Bytes32 = coins[0].exec_puzzle_hash().into();
    let attacker = sim.bls(0);

    // An XCH coin sends a ring message to the execution coins' puzzle hash, claiming that the
    // previous subtotal is -5000, so that the first coin could create 5000 more tokens than it has.
    // The first coin sends -10000 on to the second coin, which creates nothing and sends -9400.
    //
    // There are three ring messages sent to the execution coins, but only two received, so it fails
    // whether or not the attacker tries to receive the last one. It can't, since a message sent to
    // the puzzle hash of an execution coin can only be received by an execution coin.
    for receive in [None, Some(-9400)] {
        ring_attacker(ctx, &attacker, (-5000, exec_puzzle_hash), receive)?;

        let conditions = send(ctx, alice.puzzle_hash, 5400)?;
        let first_inner = p2(ctx, &alice, conditions)?;
        let second_inner = p2(ctx, &alice, Conditions::new())?;
        coins[0].spend(
            ctx,
            DogSpend::new(coins[0], first_inner),
            400,
            -5000,
            alice.puzzle_hash,
        )?;
        coins[1].spend(
            ctx,
            DogSpend::new(coins[1], second_inner),
            600,
            -10000,
            alice.puzzle_hash,
        )?;

        assert_eq!(
            submit_err(&mut sim, ctx, &[alice.sk.clone(), attacker.sk.clone()]),
            ErrorCode::MessageNotSentOrReceived,
            "receive: {receive:?}"
        );
    }

    // The same goes for an attacker that tries to take the place of the second coin, by receiving
    // the first coin's message and sending one back that makes up the difference. The first coin's
    // message is sent to an execution coin's puzzle hash, so the attacker can't receive it.
    ring_attacker(ctx, &attacker, (-5000, exec_puzzle_hash), Some(-10000))?;
    let conditions = send(ctx, alice.puzzle_hash, 5400)?;
    let first_inner = p2(ctx, &alice, conditions)?;
    coins[0].spend(
        ctx,
        DogSpend::new(coins[0], first_inner),
        400,
        -5000,
        alice.puzzle_hash,
    )?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone(), attacker.sk.clone()]),
        ErrorCode::MessageNotSentOrReceived
    );

    // The honest ring works.
    let spends = ring_spends(ctx, &alice, &coins)?;
    Dog::spend_all(ctx, &spends)?;
    submit(&mut sim, ctx, &[alice.sk])?;

    Ok(())
}

/// Ring messages are sent to the puzzle hash of the next execution coin, so in a ring of coins with
/// different inner puzzles, each subtotal has to go to the right inner puzzle hash.
#[test]
fn ring_messages_go_to_the_next_inner_puzzle() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let coins = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(alice.puzzle_hash, 400), (bob.puzzle_hash, 600)],
    )?;

    // Alice's coin creates 1000 and Bob's creates nothing, so each one is unbalanced by itself.
    let items = |ctx: &mut SpendContext| -> Result<[RawRingItem; 2]> {
        let conditions = send(ctx, alice.puzzle_hash, 1000)?;
        Ok([
            RawRingItem::new(coins[0], p2(ctx, &alice, conditions)?, 1000),
            RawRingItem::new(coins[1], p2(ctx, &bob, Conditions::new())?, 0),
        ])
    };

    // If each coin sends its subtotal to its own inner puzzle hash, the subtotals that Alice's coin
    // sends and receives would have to be equal, so it fails whatever the subtotals are.
    for (first, second) in [(0, 0), (0, 600), (-600, 0), (600, -600)] {
        let [alice_item, bob_item] = items(ctx)?;
        coins[0].spend(
            ctx,
            DogSpend::new(coins[0], alice_item.inner_spend),
            400,
            first,
            alice.puzzle_hash,
        )?;
        coins[1].spend(
            ctx,
            DogSpend::new(coins[1], bob_item.inner_spend),
            600,
            second,
            bob.puzzle_hash,
        )?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[alice.sk.clone(), bob.sk.clone()]),
            ErrorCode::MessageNotSentOrReceived,
            "subtotals {first} and {second}"
        );
    }

    // Linked to each other, they balance.
    let items = items(ctx)?;
    spend_ring(ctx, &items)?;
    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;
    assert_unspent(&sim, &coins[0].child(400, alice.puzzle_hash, 1000));

    Ok(())
}

#[test]
fn inner_puzzle_cant_use_ring_messages() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let target = Bytes32::new([5; 32]);

    for opcode in [66, 67] {
        for message in [
            ring_message(0),
            ring_message(-5000),
            Bytes::new(vec![RING_PREFIX]),
        ] {
            let conditions = ctx.alloc(&clvm_list!(
                clvm_list!(51, alice.puzzle_hash, 1000),
                clvm_list!(opcode, RING_MODE, message, target)
            ))?;
            let inner_spend = raw_p2(ctx, &alice, conditions)?;
            spend_ring(ctx, &[RawRingItem::new(dog, inner_spend, 1000)])?;
            assert_eq!(
                submit_err(&mut sim, ctx, &[alice.sk.clone()]),
                PUZZLE_FAILED,
                "opcode {opcode} with a ring message should be rejected"
            );
        }
    }

    Ok(())
}

#[test]
fn inner_puzzle_can_use_other_messages() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let exec_coin = dog.exec_coin(1000);

    // Coin to coin messages are fine, even if they start with the prefix, since that isn't the
    // mode of the ring.
    let messages = [
        (63, Bytes::new(vec![0x01, RING_PREFIX])),
        (63, Bytes::new(Vec::new())),
        (63, Bytes::new(b"hello".to_vec())),
        (63, ring_message(0)),
    ];

    let mut inner_conditions = vec![ctx.alloc(&clvm_list!(51, alice.puzzle_hash, 1000))?];
    let mut bob_conditions = Conditions::new();

    for (mode, message) in messages {
        inner_conditions.push(ctx.alloc(&clvm_list!(
            66,
            mode,
            message.clone(),
            bob.coin.coin_id()
        ))?);
        let received = ctx.alloc(&clvm_list!(67, mode, message, exec_coin.coin_id()))?;
        bob_conditions = bob_conditions.with(chia_wallet_sdk::types::Condition::Other(received));
    }

    // A ring-prefixed message with a different mode can't be mistaken for a ring message either.
    let puzzle_mode_message = ring_message(0);
    inner_conditions.push(ctx.alloc(&clvm_list!(
        66,
        0b010_010,
        puzzle_mode_message.clone(),
        bob.puzzle_hash
    ))?);
    let received = ctx.alloc(&clvm_list!(
        67,
        0b010_010,
        puzzle_mode_message,
        exec_coin.puzzle_hash
    ))?;
    bob_conditions = bob_conditions.with(chia_wallet_sdk::types::Condition::Other(received));

    let conditions = ctx.alloc(&inner_conditions)?;
    let inner_spend = raw_p2(ctx, &alice, conditions)?;
    spend_ring(ctx, &[RawRingItem::new(dog, inner_spend, 1000)])?;
    StandardLayer::new(bob.pk).spend(ctx, bob.coin, bob_conditions)?;

    submit(&mut sim, ctx, &[alice.sk, bob.sk])?;

    Ok(())
}

#[test]
fn signature_commits_to_exec_amount() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    // An asset whose TAIL allows anyone to issue or melt, which is the worst case.
    let alice = sim.bls(0);
    let issuer = sim.bls(0);
    let tail = ctx.alloc(&chia_wallet_sdk::clvm_traits::clvm_quote!(()))?;
    let metadata_hash: Bytes32 = metadata().tree_hash().into();

    // The attacker's coin can be spent by anyone, and doesn't create anything.
    let anyone = ctx.delegated_spend(Conditions::new())?;
    let anyone_puzzle_hash: Bytes32 = ctx.tree_hash(anyone.puzzle).into();

    let conditions = send_many(ctx, &[(alice.puzzle_hash, 1000), (anyone_puzzle_hash, 500)])?;
    let issuance = Dog::issue(
        ctx,
        issuer.coin.coin_id(),
        Spend::new(tail, NodePtr::NIL),
        &metadata(),
        &DogUris::default(),
        1500,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;
    submit(&mut sim, ctx, &[issuer.sk])?;
    let [dog, attacker_dog] = issuance.children[..] else {
        panic!("expected two children");
    };

    // Alice signs a spend that keeps the amount the same.
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    let signature = sign_transaction(&ctx.take(), &[alice.sk.clone()])?;

    // Someone else reuses her inner spend and signature, but melts half of her tokens and makes up
    // the difference with their own coin. The ring is balanced, but her signature is tied to the
    // execution coin, whose id includes its amount, so it no longer matches.
    let tampered = RawRingItem {
        dog,
        inner_spend,
        tail_spend: Some(TailSpend {
            tail,
            metadata_hash,
            delta: -500,
            solution: NodePtr::NIL,
        }),
        exec_amount: 500,
        created: 1000,
    };
    spend_ring(ctx, &[tampered, RawRingItem::new(attacker_dog, anyone, 0)])?;
    let result = sim.new_transaction(SpendBundle::new(ctx.take(), signature));
    assert!(
        matches!(
            result,
            Err(chia_wallet_sdk::test::SimulatorError::Validation(
                ErrorCode::BadAggregateSignature
            ))
        ),
        "{result:?}"
    );

    Ok(())
}

#[test]
fn dog_args_match_puzzle() -> Result<()> {
    let ctx = &mut SpendContext::new();
    let args = DogArgs::new(Bytes32::new([1; 32]), Bytes32::new([2; 32]), 42);
    let puzzle = ctx.curry(args)?;
    assert_eq!(Dog::parse_puzzle(ctx, puzzle)?, Some(args));
    assert_eq!(
        ctx.tree_hash(puzzle),
        DogArgs::puzzle_hash(args.asset.asset_id, args.inner_puzzle_hash, args.amount)
    );
    Ok(())
}
