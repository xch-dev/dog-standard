#![allow(dead_code)]

use anyhow::{Result, ensure};
pub use chia_wallet_sdk::types::Conditions;
use chia_wallet_sdk::{
    chia::{
        bls::SecretKey,
        consensus::validation_error::ErrorCode,
        protocol::{Bytes32, CoinSpend, SpendBundle},
    },
    clvm_traits::FromClvm,
    clvmr::{Allocator, NodePtr, serde::node_from_bytes},
    driver::{SpendContext, SpendWithConditions, StandardLayer, spend_bundle_cost},
    test::{BlsPairWithCoin, Simulator, SimulatorError, sign_transaction},
    types::{Condition, run_puzzle},
};
pub use dog::DogUris;
use dog::{Dog, DogIssuance, DogMetadata, DogSpend};

/// The total amount of the coins spent and created by a set of coin spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    pub removals: u128,
    pub additions: u128,
}

/// Runs every coin spend to add up the removals and additions, since the simulator doesn't check
/// that a transaction doesn't create more mojos than it spends.
pub fn balance(coin_spends: &[CoinSpend]) -> Result<Balance> {
    let mut allocator = Allocator::new();
    let mut removals = 0;
    let mut additions = 0;

    for coin_spend in coin_spends {
        removals += u128::from(coin_spend.coin.amount);

        let puzzle = node_from_bytes(&mut allocator, coin_spend.puzzle_reveal.as_ref())?;
        let solution = node_from_bytes(&mut allocator, coin_spend.solution.as_ref())?;
        let output = run_puzzle(&mut allocator, puzzle, solution)?;
        let conditions = Vec::<Condition>::from_clvm(&allocator, output)?;

        for create_coin in conditions
            .into_iter()
            .filter_map(Condition::into_create_coin)
        {
            additions += u128::from(create_coin.amount);
        }
    }

    Ok(Balance {
        removals,
        additions,
    })
}

/// Submits the coin spends in the context to the simulator, after checking that they don't mint
/// mojos. Returns the cost of the transaction.
pub fn submit(sim: &mut Simulator, ctx: &mut SpendContext, keys: &[SecretKey]) -> Result<u64> {
    let coin_spends = ctx.take();

    let balance = balance(&coin_spends)?;
    ensure!(
        balance.removals >= balance.additions,
        "the transaction mints {} mojos",
        balance.additions - balance.removals
    );

    let cost = spend_bundle_cost(&coin_spends)?;
    sim.spend_coins(coin_spends, keys)?;
    Ok(cost)
}

/// Submits the coin spends in the context, expecting them to be rejected, and returns the error.
///
/// If the signatures can't be calculated because a puzzle raises, the bundle is submitted with an
/// empty signature instead, so that the error comes from consensus rather than the signer.
pub fn submit_err(sim: &mut Simulator, ctx: &mut SpendContext, keys: &[SecretKey]) -> ErrorCode {
    submit_bundle_err(sim, ctx.take(), keys)
}

/// Like [`submit_err`], but for coin spends that aren't in a context.
pub fn submit_bundle_err(
    sim: &mut Simulator,
    coin_spends: Vec<CoinSpend>,
    keys: &[SecretKey],
) -> ErrorCode {
    let signature = sign_transaction(&coin_spends, keys).unwrap_or_default();

    match sim.new_transaction(SpendBundle::new(coin_spends, signature)) {
        Ok(_) => panic!("the transaction should have been rejected"),
        Err(SimulatorError::Validation(code)) => code,
        Err(error) => panic!("unexpected error: {error}"),
    }
}

/// The error code when a puzzle raises, or when a coin spend can't be run.
pub const PUZZLE_FAILED: ErrorCode = ErrorCode::GeneratorRuntimeError;

pub fn metadata() -> DogMetadata {
    DogMetadata::new("Dogecoin", "DOGE", 8)
        .with_image_hash(Bytes32::new([7; 32]))
        .with_data_hash(Bytes32::new([8; 32]))
}

/// The conditions to send `amount` to `puzzle_hash`, with a hint so the recipient can find it.
pub fn send(ctx: &mut SpendContext, puzzle_hash: Bytes32, amount: u64) -> Result<Conditions> {
    Ok(Conditions::new().create_coin(puzzle_hash, amount, ctx.hint(puzzle_hash)?))
}

/// The conditions to create several coins.
pub fn send_many(ctx: &mut SpendContext, outputs: &[(Bytes32, u64)]) -> Result<Conditions> {
    let mut conditions = Conditions::new();
    for &(puzzle_hash, amount) in outputs {
        conditions = conditions.create_coin(puzzle_hash, amount, ctx.hint(puzzle_hash)?);
    }
    Ok(conditions)
}

/// Issues tokens with the single issuance TAIL to the owner, and returns the issued DOG coin.
pub fn issue_single(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    amount: u64,
) -> Result<Dog> {
    let issuer = sim.bls(0);
    let conditions = send(ctx, owner.puzzle_hash, amount)?;
    let DogIssuance {
        parent_conditions,
        children,
        ..
    } = Dog::single_issuance(
        ctx,
        issuer.coin.coin_id(),
        &metadata(),
        &DogUris::default(),
        amount,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, parent_conditions)?;
    submit(sim, ctx, &[issuer.sk])?;

    ensure!(children.len() == 1);
    Ok(children[0])
}

/// The inner spend of a standard puzzle that outputs the conditions.
pub fn p2(
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    conditions: Conditions,
) -> Result<chia_wallet_sdk::driver::Spend> {
    Ok(StandardLayer::new(owner.pk).spend_with_conditions(ctx, conditions)?)
}

/// Spends a single DOG coin owned by `owner`, creating the given outputs.
pub fn transfer(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    dog: Dog,
    outputs: &[(Bytes32, u64)],
) -> Result<Vec<Dog>> {
    let conditions = send_many(ctx, outputs)?;
    let inner_spend = p2(ctx, owner, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    submit(sim, ctx, &[owner.sk.clone()])?;
    Ok(children)
}

/// Checks that the DOG coin exists and is unspent in the simulator.
pub fn assert_unspent(sim: &Simulator, dog: &Dog) {
    let state = sim
        .coin_state(dog.coin.coin_id())
        .expect("the DOG coin should exist");
    assert_eq!(state.coin, dog.coin);
    assert_eq!(state.coin.amount, 0);
    assert!(
        state.spent_height.is_none(),
        "the DOG coin should be unspent"
    );
}

pub fn nil() -> NodePtr {
    NodePtr::NIL
}

/// One coin in a ring that's built by hand, for tests that need spends that the driver rejects.
#[derive(Debug, Clone, Copy)]
pub struct RawRingItem {
    pub dog: Dog,
    pub inner_spend: chia_wallet_sdk::driver::Spend,
    pub tail_spend: Option<dog::TailSpend<NodePtr, NodePtr>>,
    pub exec_amount: u64,
    /// The total amount of the coins that the inner puzzle creates.
    pub created: i128,
}

impl RawRingItem {
    pub fn new(dog: Dog, inner_spend: chia_wallet_sdk::driver::Spend, created: i128) -> Self {
        Self {
            dog,
            inner_spend,
            tail_spend: None,
            exec_amount: dog.amount,
            created,
        }
    }
}

/// Links the coins in a ring like [`Dog::spend_all`], but without checking that it balances.
pub fn spend_ring(ctx: &mut SpendContext, items: &[RawRingItem]) -> Result<()> {
    let mut subtotal = 0;
    let mut prev_subtotals = Vec::new();

    for item in items {
        prev_subtotals.push(subtotal);
        subtotal += i128::from(item.exec_amount) - item.created;
    }

    for (index, item) in items.iter().enumerate() {
        let next = &items[(index + 1) % items.len()];

        item.dog.spend(
            ctx,
            DogSpend {
                dog: item.dog,
                inner_spend: item.inner_spend,
                tail_spend: item.tail_spend,
                hidden: false,
            },
            item.exec_amount,
            prev_subtotals[index],
            next.dog.inner_puzzle_hash().into(),
        )?;
    }

    Ok(())
}

/// The inner spend of a standard puzzle that outputs arbitrary CLVM as its conditions.
pub fn raw_p2(
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    conditions: NodePtr,
) -> Result<chia_wallet_sdk::driver::Spend> {
    let delegated_puzzle = ctx.alloc(&chia_wallet_sdk::clvm_traits::clvm_quote!(conditions))?;
    Ok(StandardLayer::new(owner.pk).delegated_inner_spend(
        ctx,
        chia_wallet_sdk::driver::Spend::new(delegated_puzzle, NodePtr::NIL),
    )?)
}
