mod common;

use anyhow::Result;
use chia_wallet_sdk::{clvmr::NodePtr, driver::SpendContext, test::Simulator};
use common::*;
use dog::{Dog, DogSpend};

#[test]
fn issue_and_transfer() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);

    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    assert_unspent(&sim, &dog);
    assert_eq!(dog.amount, 1000);
    assert_eq!(dog.p2_puzzle_hash, alice.puzzle_hash);

    let children = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(bob.puzzle_hash, 600), (alice.puzzle_hash, 400)],
    )?;

    assert_eq!(children.len(), 2);
    for child in &children {
        assert_unspent(&sim, child);
    }
    assert_eq!(children[0].amount, 600);
    assert_eq!(children[1].amount, 400);

    // Bob can spend his coin.
    let children = transfer(
        &mut sim,
        ctx,
        &bob,
        children[0],
        &[(alice.puzzle_hash, 600)],
    )?;
    assert_unspent(&sim, &children[0]);

    Ok(())
}

#[test]
fn merge_coins() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let coins = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[
            (alice.puzzle_hash, 100),
            (alice.puzzle_hash, 200),
            (alice.puzzle_hash, 700),
        ],
    )?;

    // The first coin creates all of the outputs, and the others create none.
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let spends = vec![
        DogSpend::new(coins[0], p2(ctx, &alice, conditions)?),
        DogSpend::new(coins[1], p2(ctx, &alice, Conditions::new())?),
        DogSpend::new(coins[2], p2(ctx, &alice, Conditions::new())?),
    ];
    let children = Dog::spend_all(ctx, &spends)?;
    submit(&mut sim, ctx, &[alice.sk])?;

    assert_eq!(children.len(), 1);
    assert_eq!(children[0].amount, 1000);
    assert_unspent(&sim, &children[0]);

    Ok(())
}

#[test]
fn ring_with_uneven_inputs_and_outputs() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 10_000)?;
    let coins = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[
            (alice.puzzle_hash, 1),
            (alice.puzzle_hash, 2999),
            (alice.puzzle_hash, 7000),
        ],
    )?;

    // The smallest coin creates most of the outputs, and the largest creates none.
    let first = send_many(
        ctx,
        &[
            (bob.puzzle_hash, 5000),
            (bob.puzzle_hash, 4000),
            (alice.puzzle_hash, 1),
        ],
    )?;
    let second = send(ctx, alice.puzzle_hash, 999)?;
    let spends = [
        DogSpend::new(coins[0], p2(ctx, &alice, first)?),
        DogSpend::new(coins[1], p2(ctx, &alice, second)?),
        DogSpend::new(coins[2], p2(ctx, &alice, Conditions::new())?),
    ];
    let children = Dog::spend_all(ctx, &spends)?;
    submit(&mut sim, ctx, &[alice.sk])?;

    let amounts: Vec<u64> = children.iter().map(|child| child.amount).collect();
    assert_eq!(amounts, [5000, 4000, 1, 999]);
    for child in &children {
        assert_unspent(&sim, child);
    }

    Ok(())
}

#[test]
fn zero_amount_coins() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let children = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(alice.puzzle_hash, 1000), (alice.puzzle_hash, 0)],
    )?;
    assert_eq!(children[1].amount, 0);

    // A coin with no tokens can be spent on its own, or with others.
    transfer(
        &mut sim,
        ctx,
        &alice,
        children[1],
        &[(alice.puzzle_hash, 0)],
    )?;

    Ok(())
}

#[test]
fn different_assets_in_one_transaction() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let first = issue_single(&mut sim, ctx, &alice, 100)?;
    let second = issue_single(&mut sim, ctx, &alice, 200)?;
    assert_ne!(first.asset_id, second.asset_id);

    // Each asset has its own ring.
    let conditions = send(ctx, bob.puzzle_hash, 100)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    let first_children = Dog::spend_all(ctx, &[DogSpend::new(first, inner_spend)])?;

    let conditions = send(ctx, bob.puzzle_hash, 200)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    let second_children = Dog::spend_all(ctx, &[DogSpend::new(second, inner_spend)])?;

    submit(&mut sim, ctx, &[alice.sk.clone()])?;
    assert_unspent(&sim, &first_children[0]);
    assert_unspent(&sim, &second_children[0]);

    // They can't share a ring, since the ring only links execution coins of the same asset.
    let conditions = send(ctx, alice.puzzle_hash, 300)?;
    let spends = [
        DogSpend::new(first_children[0], p2(ctx, &bob, conditions)?),
        DogSpend::new(second_children[0], p2(ctx, &bob, Conditions::new())?),
    ];
    assert!(matches!(
        Dog::spend_all(ctx, &spends),
        Err(dog::DogError::AssetIdMismatch)
    ));

    Ok(())
}

#[test]
fn driver_rejects_invalid_spends() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    // The outputs must add up to the input.
    let conditions = send(ctx, alice.puzzle_hash, 999)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    assert!(matches!(
        Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)]),
        Err(dog::DogError::UnbalancedSpend(1))
    ));

    // The inner puzzle must match the coin.
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let inner_spend = p2(ctx, &bob, conditions)?;
    assert!(matches!(
        Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)]),
        Err(dog::DogError::InnerPuzzleHashMismatch)
    ));

    assert_eq!(Dog::spend_all(ctx, &[])?, [] as [dog::Dog; 0]);
    assert_eq!(ctx.take(), [] as [chia_wallet_sdk::prelude::CoinSpend; 0]);

    Ok(())
}

#[test]
fn pays_fee_with_xch() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(100);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    // The inner puzzle reserves a fee, which an XCH coin in the same transaction pays. The
    // execution coin's mojos can't be used for it, since they're all needed to cancel out.
    let conditions = send(ctx, alice.puzzle_hash, 1000)?.reserve_fee(100);
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    chia_wallet_sdk::driver::StandardLayer::new(alice.pk).spend(
        ctx,
        alice.coin,
        chia_wallet_sdk::types::Conditions::new(),
    )?;

    let coin_spends = ctx.take();
    let balance = balance(&coin_spends)?;
    assert_eq!(balance.removals - balance.additions, 100);
    for coin_spend in coin_spends {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[alice.sk.clone()])?;

    // Without the XCH coin, there's nothing to pay the fee with.
    let child = dog.child(1000, alice.puzzle_hash, 1000);
    let conditions = send(ctx, alice.puzzle_hash, 1000)?.reserve_fee(1);
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(child, inner_spend)])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk]),
        chia_wallet_sdk::chia::consensus::validation_error::ErrorCode::ReserveFeeConditionFailed
    );

    Ok(())
}

#[test]
fn transfers_dont_need_mojos() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;

    // The execution coin's 1000 mojos are both created and spent, so they cancel out.
    let coin_spends = ctx.take();
    assert_eq!(coin_spends.len(), 2);
    let balance = balance(&coin_spends)?;
    assert_eq!(balance.removals, 1000);
    assert_eq!(balance.additions, 1000);

    for coin_spend in coin_spends {
        ctx.insert(coin_spend);
    }
    submit(&mut sim, ctx, &[alice.sk])?;

    Ok(())
}

#[test]
fn wallet_can_sync_coins_from_hints() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let expected = transfer(
        &mut sim,
        ctx,
        &alice,
        dog,
        &[(bob.puzzle_hash, 250), (alice.puzzle_hash, 750)],
    )?;

    // Bob's wallet only knows his puzzle hash. Since the puzzle hash of a DOG coin includes the
    // amount, he finds the coin by its hint instead.
    let hinted = sim.hinted_coins(bob.puzzle_hash);
    let mut found = Vec::new();

    for coin_id in hinted {
        let coin = sim.coin_state(coin_id).expect("the coin exists").coin;
        if coin.amount != 0 {
            continue;
        }

        // The parent is the execution coin, whose spend reveals the outputs.
        let parent = sim
            .coin_state(coin.parent_coin_info)
            .expect("the parent exists")
            .coin;
        let (puzzle, solution) = sim
            .puzzle_and_solution(parent.coin_id())
            .expect("the parent is spent");

        let mut allocator = chia_wallet_sdk::clvmr::Allocator::new();
        let puzzle = chia_wallet_sdk::clvmr::serde::node_from_bytes(&mut allocator, &puzzle)?;
        let solution = chia_wallet_sdk::clvmr::serde::node_from_bytes(&mut allocator, &solution)?;
        let children = Dog::parse_children(&mut allocator, parent, puzzle, solution)?
            .expect("the parent is an execution coin");

        found.extend(children.into_iter().filter(|child| child.coin == coin));
    }

    assert_eq!(found, [expected[0]]);
    assert_eq!(found[0].amount, 250);

    // The parsed coin can be spent with its lineage proof.
    transfer(&mut sim, ctx, &bob, found[0], &[(bob.puzzle_hash, 250)])?;

    Ok(())
}

#[test]
fn parse_children_ignores_other_puzzles() -> Result<()> {
    let ctx = &mut SpendContext::new();
    let puzzle = ctx.alloc(&chia_wallet_sdk::clvm_traits::clvm_quote!(()))?;
    let coin = chia_wallet_sdk::chia::protocol::Coin::new(
        chia_wallet_sdk::chia::protocol::Bytes32::default(),
        ctx.tree_hash(puzzle).into(),
        1,
    );
    assert!(Dog::parse_children(ctx, coin, puzzle, NodePtr::NIL)?.is_none());
    assert!(Dog::parse_puzzle(ctx, puzzle)?.is_none());
    Ok(())
}

/// The ring values in an execution coin's solution aren't signed, and only the differences between
/// subtotals matter, so anyone can make them huge. Wallets must still be able to parse the coins.
#[test]
fn wallet_can_parse_coins_with_huge_subtotals() -> Result<()> {
    use chia_wallet_sdk::{chia::protocol::Bytes, clvm_traits::clvm_list, driver::Spend};
    use dog::DogExecArgs;

    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    // Spend the coin normally, then replace the execution coin's subtotal with 2^200.
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    let exec_coin = dog.exec_coin(1000);
    Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    let mut coin_spends = ctx.take();
    coin_spends.retain(|cs| cs.coin != exec_coin);
    for coin_spend in coin_spends {
        ctx.insert(coin_spend);
    }

    let mut huge = vec![0; 26];
    huge[0] = 1;
    let exec_puzzle = ctx.curry(DogExecArgs::new(dog.asset_id, inner_spend.puzzle))?;
    let exec_solution = ctx.alloc(&clvm_list!(
        inner_spend.solution,
        1000,
        1000,
        Bytes::new(huge),
        alice.puzzle_hash
    ))?;
    ctx.spend(exec_coin, Spend::new(exec_puzzle, exec_solution))?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;

    let (puzzle, solution) = sim
        .puzzle_and_solution(exec_coin.coin_id())
        .expect("the execution coin is spent");
    let mut allocator = chia_wallet_sdk::clvmr::Allocator::new();
    let puzzle = chia_wallet_sdk::clvmr::serde::node_from_bytes(&mut allocator, &puzzle)?;
    let solution = chia_wallet_sdk::clvmr::serde::node_from_bytes(&mut allocator, &solution)?;
    let children = Dog::parse_children(&mut allocator, exec_coin, puzzle, solution)?
        .expect("the parent is an execution coin");

    let expected = dog.child(1000, alice.puzzle_hash, 1000);
    assert_eq!(children, [expected]);
    transfer(
        &mut sim,
        ctx,
        &alice,
        children[0],
        &[(alice.puzzle_hash, 1000)],
    )?;

    Ok(())
}

/// The execution coin wraps anything with the `CREATE_COIN` opcode, so the driver can't skip
/// conditions it doesn't understand, or the amounts wouldn't add up.
#[test]
fn driver_rejects_malformed_create_coin() -> Result<()> {
    use chia_wallet_sdk::{chia::protocol::Bytes, clvm_traits::clvm_list};

    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    let conditions = ctx.alloc(&clvm_list!(
        clvm_list!(51, alice.puzzle_hash, 600),
        clvm_list!(51, Bytes::new(vec![1; 31]), 400)
    ))?;
    let inner_spend = raw_p2(ctx, &alice, conditions)?;
    assert!(matches!(
        Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)]),
        Err(dog::DogError::UnsupportedCreateCoin)
    ));

    Ok(())
}
