//! Updatable metadata: issuing with a metadata updater singleton, updating, transferring and
//! locking it, and resolving the current metadata like a wallet would.

mod common;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{
        consensus::validation_error::ErrorCode,
        protocol::{Bytes32, CoinSpend},
        puzzle_types::singleton::LauncherSolution,
    },
    clvm_traits::FromClvm,
    clvm_utils::ToTreeHash,
    clvmr::{Allocator, serde::node_from_bytes},
    driver::{SpendContext, StandardLayer},
    puzzles::SINGLETON_LAUNCHER_HASH,
    test::{BlsPairWithCoin, Simulator},
    types::{Condition, run_puzzle},
};
use common::*;
use dog::{
    Dog, DogError, DogInfo, DogMetadata, DogMetadataRemark, DogMetadataState, DogUris,
    METADATA_UPDATE_PREFIX, MetadataMutability, MetadataUpdater, MetadataUpdaterLaunch,
    ResolvedMetadata, asset_id, resolve_metadata,
};

fn uris() -> DogUris {
    DogUris {
        image_uris: vec!["https://example.com/doge.png".to_string()],
        data_uris: vec!["https://example.com/doge.json".to_string()],
    }
}

fn new_info() -> DogInfo {
    DogInfo::new("Dogecoin Classic", "DOGC").with_image_hash(Bytes32::new([9; 32]))
}

fn new_state() -> DogMetadataState {
    DogMetadataState::new(
        new_info(),
        vec!["ipfs://bafkreidoge".to_string()],
        Vec::new(),
    )
}

/// An asset issued with an updater owned by `owner`.
struct Updatable {
    metadata: DogMetadata,
    dog: Dog,
    updater: MetadataUpdater,
    cost: u64,
}

fn issue_updatable(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
) -> Result<Updatable> {
    // The singleton needs 1 mojo, which is the only XCH the issuance needs.
    let issuer = sim.bls(1);
    let conditions = send(ctx, owner.puzzle_hash, 1000)?;
    let issuance = Dog::single_issuance_with_updater(
        ctx,
        issuer.coin.coin_id(),
        &metadata(),
        &uris(),
        owner.puzzle_hash,
        1000,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;
    let cost = submit(sim, ctx, &[issuer.sk.clone()])?;

    let updater = issuance.updater.expect("the updater is launched");
    let metadata = metadata().with_updater(updater.launcher_id);
    assert_eq!(
        updater.launcher_id,
        MetadataUpdater::launcher_id(issuer.coin.coin_id())
    );
    assert_eq!(issuance.eve.asset_id, asset_id_of(ctx, &issuer, &metadata)?);

    Ok(Updatable {
        metadata,
        dog: issuance.children[0],
        updater,
        cost,
    })
}

/// The asset id of the single issuance TAIL from the issuer's coin, with the metadata.
fn asset_id_of(
    ctx: &mut SpendContext,
    issuer: &BlsPairWithCoin,
    metadata: &DogMetadata,
) -> Result<Bytes32> {
    let tail = ctx.curry(dog::GenesisByCoinIdTailArgs::new(issuer.coin.coin_id()))?;
    Ok(asset_id(ctx.tree_hash(tail), metadata.tree_hash()))
}

/// Fetches the spends of the updater's lineage from the launcher, like a wallet would from a full
/// node: each spend reveals the next coin, until one is unspent or melted.
fn lineage(sim: &Simulator, launcher_id: Bytes32) -> Result<Vec<CoinSpend>> {
    let mut spends = Vec::new();
    let mut coin_id = launcher_id;
    let mut allocator = Allocator::new();

    while let Some(coin_spend) = sim.coin_spend(coin_id) {
        let puzzle = node_from_bytes(&mut allocator, coin_spend.puzzle_reveal.as_ref())?;
        let solution = node_from_bytes(&mut allocator, coin_spend.solution.as_ref())?;
        let revision = MetadataUpdater::parse_spend(
            &mut allocator,
            launcher_id,
            coin_spend.coin,
            puzzle,
            solution,
        )?;
        spends.push(coin_spend);
        let Some(child) = revision.child else {
            break;
        };
        coin_id = child.coin.coin_id();
    }

    Ok(spends)
}

fn resolve(metadata: &DogMetadata, spends: &[CoinSpend]) -> Result<ResolvedMetadata, DogError> {
    resolve_metadata(&mut Allocator::new(), metadata, spends)
}

#[test]
fn immutable_assets_resolve_to_genesis() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    assert_eq!(metadata().updater, None);

    let resolved = resolve(&metadata(), &[])?;
    assert_eq!(resolved.mutability, MetadataMutability::Immutable);
    assert_eq!(resolved.info, metadata().genesis);
    assert_eq!(resolved.decimals, 8);
    assert_eq!(resolved.history, []);

    // The coins are as usual.
    assert_unspent(&sim, &dog);

    Ok(())
}

#[test]
fn issue_with_updater_in_one_transaction() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;
    assert_unspent(&sim, &asset.dog);
    assert!(
        sim.coin_state(asset.updater.coin.coin_id())
            .is_some_and(|state| state.spent_height.is_none())
    );

    // The issuance remark publishes the metadata with the updater.
    let exec_coin_spend = sim
        .coin_spend(asset.dog.lineage_proof.unwrap().parent_parent_coin_info)
        .and_then(|eve| sim.children(eve.coin.coin_id()).first().copied())
        .and_then(|exec| sim.coin_spend(exec.coin.coin_id()))
        .expect("the eve coin's execution coin is spent");
    let mut allocator = Allocator::new();
    let puzzle = node_from_bytes(&mut allocator, exec_coin_spend.puzzle_reveal.as_ref())?;
    let solution = node_from_bytes(&mut allocator, exec_coin_spend.solution.as_ref())?;
    let output = run_puzzle(&mut allocator, puzzle, solution)?;
    let remark = Vec::<Condition>::from_clvm(&allocator, output)?
        .into_iter()
        .filter_map(Condition::into_remark)
        .find_map(|remark| DogMetadataRemark::from_clvm(&allocator, remark.rest).ok())
        .expect("the metadata is published");
    assert_eq!(remark.metadata, asset.metadata);
    assert_eq!(remark.asset_id(), asset.dog.asset_id);

    // Before the singleton is spent, its state is revealed by the launcher's spend.
    let spends = lineage(&sim, asset.updater.launcher_id)?;
    assert_eq!(spends.len(), 1);
    let resolved = resolve(&asset.metadata, &spends)?;
    assert_eq!(resolved.info, metadata().genesis);
    assert_eq!(resolved.image_uris, uris().image_uris);
    assert_eq!(resolved.data_uris, uris().data_uris);
    assert_eq!(
        resolved.mutability,
        MetadataMutability::Updatable(Box::new(asset.updater.clone()))
    );

    // Without the launcher's spend, the updater is unknown.
    let resolved = resolve(&asset.metadata, &[])?;
    assert_eq!(resolved.mutability, MetadataMutability::Unlaunched);
    assert_eq!(resolved.info, metadata().genesis);

    Ok(())
}

#[test]
fn launcher_spend_cant_be_replaced() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let mallory = sim.bls(0);
    let issuer = sim.bls(1);
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let issuance = Dog::single_issuance_with_updater(
        ctx,
        issuer.coin.coin_id(),
        &metadata(),
        &uris(),
        alice.puzzle_hash,
        1000,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;
    let mut coin_spends = ctx.take();

    // The launcher can be spent by anyone, so Mallory replaces its solution with one that makes
    // them the owner. The parent asserts the launcher's announcement, so that's rejected.
    let launcher = coin_spends
        .iter_mut()
        .find(|cs| cs.coin.puzzle_hash == SINGLETON_LAUNCHER_HASH.into())
        .expect("the launcher is spent");
    let state = DogMetadataState::new(metadata().genesis, Vec::new(), Vec::new());
    let inner_puzzle_hash =
        dog::MetadataUpdaterArgs::puzzle_hash(state.tree_hash(), mallory.puzzle_hash);
    let solution = ctx.serialize(&LauncherSolution {
        singleton_puzzle_hash:
            chia_wallet_sdk::chia::puzzle_types::singleton::SingletonArgs::curry_tree_hash(
                launcher.coin.coin_id(),
                inner_puzzle_hash,
            )
            .into(),
        amount: 1,
        key_value_list: MetadataUpdaterLaunch {
            state,
            owner_puzzle_hash: mallory.puzzle_hash,
        },
    })?;
    launcher.solution = solution;

    assert_eq!(
        submit_bundle_err(&mut sim, coin_spends, &[issuer.sk]),
        ErrorCode::AssertCoinAnnouncementFailed
    );

    Ok(())
}

#[test]
fn owner_updates_with_history() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;
    let owner = StandardLayer::new(alice.pk);

    // Alice renames the asset and changes its icon.
    let updater = asset
        .updater
        .update(ctx, &owner, &new_state(), Conditions::new())?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;
    assert_eq!(updater.state, new_state());

    // A wallet sees the new metadata, with the full history.
    let spends = lineage(&sim, asset.updater.launcher_id)?;
    assert_eq!(spends.len(), 2);
    let resolved = resolve(&asset.metadata, &spends)?;
    assert_eq!(resolved.info, new_info());
    assert_eq!(resolved.decimals, 8);
    assert_eq!(resolved.image_uris, new_state().image_uris);
    assert_eq!(
        resolved.mutability,
        MetadataMutability::Updatable(Box::new(updater.clone()))
    );
    assert_eq!(
        resolved
            .history
            .iter()
            .map(|revision| (revision.state.info.name.as_str(), revision.updated))
            .collect::<Vec<_>>(),
        [("Dogecoin", true), ("Dogecoin Classic", true)]
    );

    // The current state only needs the latest spend, the parent of the unspent coin.
    let latest = resolve(&asset.metadata, &spends[1..])?;
    assert_eq!(latest.info, new_info());
    assert_eq!(latest.mutability, resolved.mutability);

    // Every coin after the eve coin is hinted with the launcher id.
    assert_eq!(
        sim.hinted_coins(asset.updater.launcher_id),
        [updater.coin.coin_id()]
    );

    // A spend that doesn't follow the previous one in the lineage is rejected.
    assert!(matches!(
        resolve(&asset.metadata, &[spends[1].clone(), spends[0].clone()]),
        Err(DogError::InvalidUpdaterSpend)
    ));

    Ok(())
}

#[test]
fn non_owners_cant_update() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;

    // Bob's own puzzle isn't the owner's.
    let bob_owned = MetadataUpdater {
        owner_puzzle_hash: bob.puzzle_hash,
        ..asset.updater.clone()
    };
    // The driver can't parse the spend, since the puzzle doesn't match the coin.
    assert!(matches!(
        bob_owned.update(
            ctx,
            &StandardLayer::new(bob.pk),
            &new_state(),
            Conditions::new(),
        ),
        Err(DogError::InvalidUpdaterSpend)
    ));
    assert_eq!(
        submit_err(&mut sim, ctx, &[bob.sk.clone()]),
        ErrorCode::WrongPuzzleHash
    );

    // Alice's puzzle needs Alice's signature.
    asset.updater.update(
        ctx,
        &StandardLayer::new(alice.pk),
        &new_state(),
        Conditions::new(),
    )?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[bob.sk]),
        ErrorCode::BadAggregateSignature
    );

    // The metadata hasn't changed.
    let resolved = resolve(&asset.metadata, &lineage(&sim, asset.updater.launcher_id)?)?;
    assert_eq!(resolved.info, metadata().genesis);

    Ok(())
}

#[test]
fn only_one_update_per_spend() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;

    let other = DogMetadataState::new(DogInfo::new("Other", "OTHER"), Vec::new(), Vec::new());
    let conditions = MetadataUpdater::update_condition(ctx, &new_state())?
        .extend(MetadataUpdater::update_condition(ctx, &other)?)
        .extend(asset.updater.recreate_condition(ctx, alice.puzzle_hash)?);
    let owner_spend = p2(ctx, &alice, conditions)?;
    assert!(matches!(
        asset.updater.spend(ctx, owner_spend),
        Err(DogError::InvalidUpdaterSpend)
    ));
    assert_eq!(submit_err(&mut sim, ctx, &[alice.sk]), PUZZLE_FAILED);

    Ok(())
}

#[test]
fn transfer_ownership() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;

    let updater = asset.updater.transfer(
        ctx,
        &StandardLayer::new(alice.pk),
        bob.puzzle_hash,
        Conditions::new(),
    )?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;
    assert_eq!(updater.owner_puzzle_hash, bob.puzzle_hash);
    assert_eq!(updater.state, asset.updater.state);

    // Alice can't update it anymore.
    let alice_owned = MetadataUpdater {
        owner_puzzle_hash: alice.puzzle_hash,
        ..updater.clone()
    };
    assert!(
        alice_owned
            .update(
                ctx,
                &StandardLayer::new(alice.pk),
                &new_state(),
                Conditions::new(),
            )
            .is_err()
    );
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk]),
        ErrorCode::WrongPuzzleHash
    );

    // Bob can.
    let updater = updater.update(
        ctx,
        &StandardLayer::new(bob.pk),
        &new_state(),
        Conditions::new(),
    )?;
    submit(&mut sim, ctx, &[bob.sk])?;

    let resolved = resolve(&asset.metadata, &lineage(&sim, asset.updater.launcher_id)?)?;
    assert_eq!(resolved.info, new_info());
    assert_eq!(
        resolved.mutability,
        MetadataMutability::Updatable(Box::new(updater))
    );
    assert_eq!(
        resolved
            .history
            .iter()
            .map(|revision| revision.updated)
            .collect::<Vec<_>>(),
        [true, false, true]
    );

    Ok(())
}

#[test]
fn locked_metadata_cant_change() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;
    let owner = StandardLayer::new(alice.pk);

    // The child that an update would create, if the singleton weren't locked.
    let would_be = asset.updater.update(
        &mut SpendContext::new(),
        &owner,
        &new_state(),
        Conditions::new(),
    )?;

    // Alice updates and locks the metadata in the same spend, so the update is final.
    let update = MetadataUpdater::update_condition(ctx, &new_state())?;
    let revision = asset.updater.lock(ctx, &owner, update)?;
    assert_eq!(revision.child, None);
    assert_eq!(revision.state, new_state());
    submit(&mut sim, ctx, &[alice.sk.clone()])?;

    // The singleton is gone, which a wallet sees from the spend of the latest coin.
    assert_eq!(sim.children(asset.updater.coin.coin_id()).len(), 0);
    let spends = lineage(&sim, asset.updater.launcher_id)?;
    for spends in [&spends[..], &spends[1..]] {
        let resolved = resolve(&asset.metadata, spends)?;
        assert_eq!(resolved.mutability, MetadataMutability::Locked);
        assert_eq!(resolved.info, new_info());
    }

    // Nobody, including Alice, can spend the singleton again.
    asset.updater.update(
        ctx,
        &owner,
        &metadata_state_with("Again"),
        Conditions::new(),
    )?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        ErrorCode::DoubleSpend
    );

    // The child it would have had doesn't exist.
    would_be.update(
        ctx,
        &owner,
        &metadata_state_with("Again"),
        Conditions::new(),
    )?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk]),
        ErrorCode::UnknownUnspent
    );

    Ok(())
}

fn metadata_state_with(name: &str) -> DogMetadataState {
    DogMetadataState::new(DogInfo::new(name, "X"), Vec::new(), Vec::new())
}

#[test]
fn fake_updaters_are_rejected() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let mallory = sim.bls(1);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;

    // Mallory launches an updater with the same state, and updates it.
    let (conditions, fake) = MetadataUpdater::launch(
        ctx,
        mallory.coin.coin_id(),
        asset.updater.state.clone(),
        mallory.puzzle_hash,
    )?;
    StandardLayer::new(mallory.pk).spend(ctx, mallory.coin, conditions)?;
    submit(&mut sim, ctx, &[mallory.sk.clone()])?;
    fake.update(
        ctx,
        &StandardLayer::new(mallory.pk),
        &new_state(),
        Conditions::new(),
    )?;
    submit(&mut sim, ctx, &[mallory.sk])?;

    // Its spends aren't accepted for the asset, whether from the launcher or the latest one.
    let fake_spends = lineage(&sim, fake.launcher_id)?;
    assert_eq!(fake_spends.len(), 2);
    for spends in [&fake_spends[..], &fake_spends[1..]] {
        assert!(matches!(
            resolve(&asset.metadata, spends),
            Err(DogError::InvalidUpdaterSpend)
        ));
    }

    // The asset still resolves to the real updater's state.
    let resolved = resolve(&asset.metadata, &lineage(&sim, asset.updater.launcher_id)?)?;
    assert_eq!(resolved.info, metadata().genesis);

    Ok(())
}

#[test]
fn decimals_cant_be_updated() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let asset = issue_updatable(&mut sim, ctx, &alice)?;

    // The state has no decimals, so an update keeps the committed ones.
    let updater = asset.updater.update(
        ctx,
        &StandardLayer::new(alice.pk),
        &new_state(),
        Conditions::new(),
    )?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;
    let resolved = resolve(&asset.metadata, &lineage(&sim, asset.updater.launcher_id)?)?;
    assert_eq!(resolved.decimals, asset.metadata.decimals);

    // The puzzle doesn't look at the state, so the owner can publish one with decimals in it, but
    // it isn't a valid state, so wallets reject it rather than reading decimals from it.
    let rest = ctx.alloc(&(
        METADATA_UPDATE_PREFIX,
        (
            new_info(),
            (Vec::<String>::new(), (Vec::<String>::new(), (2, ()))),
        ),
    ))?;
    let conditions = Conditions::new()
        .remark(rest)
        .extend(updater.recreate_condition(ctx, alice.puzzle_hash)?);
    let owner_spend = p2(ctx, &alice, conditions)?;
    assert!(matches!(
        updater.spend(ctx, owner_spend),
        Err(DogError::InvalidMetadataState)
    ));
    submit(&mut sim, ctx, &[alice.sk])?;
    let spends = sim
        .coin_spend(updater.coin.coin_id())
        .expect("the singleton is spent");
    assert!(matches!(
        resolve(&asset.metadata, &[spends]),
        Err(DogError::InvalidMetadataState)
    ));

    Ok(())
}

#[test]
fn asset_id_commits_to_decimals_and_updater() -> Result<()> {
    let ctx = &mut SpendContext::new();
    let tail = ctx.alloc(&chia_wallet_sdk::clvm_traits::clvm_quote!(()))?;
    let tail_hash = ctx.tree_hash(tail);

    let base = metadata();
    let variants = [
        base.clone(),
        DogMetadata {
            decimals: 3,
            ..base.clone()
        },
        base.clone().with_updater(Bytes32::new([1; 32])),
        base.clone().with_updater(Bytes32::new([2; 32])),
        DogMetadata {
            decimals: 3,
            ..base.clone()
        }
        .with_updater(Bytes32::new([1; 32])),
    ];

    let mut asset_ids: Vec<Bytes32> = variants
        .iter()
        .map(|metadata| asset_id(tail_hash, metadata.tree_hash()))
        .collect();
    asset_ids.sort();
    asset_ids.dedup();
    assert_eq!(asset_ids.len(), variants.len());

    Ok(())
}

#[test]
fn issuance_and_update_costs() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);

    let issuer = sim.bls(0);
    let conditions = send(ctx, alice.puzzle_hash, 1000)?;
    let issuance = Dog::single_issuance(
        ctx,
        issuer.coin.coin_id(),
        &metadata(),
        &uris(),
        1000,
        conditions,
    )?;
    StandardLayer::new(issuer.pk).spend(ctx, issuer.coin, issuance.parent_conditions)?;
    let immutable = submit(&mut sim, ctx, &[issuer.sk])?;

    let asset = issue_updatable(&mut sim, ctx, &alice)?;
    let owner = StandardLayer::new(alice.pk);

    let updater = asset
        .updater
        .update(ctx, &owner, &new_state(), Conditions::new())?;
    let eve_update = submit(&mut sim, ctx, &[alice.sk.clone()])?;

    let updater = updater.update(ctx, &owner, &metadata_state_with("Doge"), Conditions::new())?;
    let update = submit(&mut sim, ctx, &[alice.sk.clone()])?;

    let bob = sim.bls(0);
    let updater = updater.transfer(ctx, &owner, bob.puzzle_hash, Conditions::new())?;
    let transfer = submit(&mut sim, ctx, &[alice.sk.clone()])?;

    updater.lock(ctx, &StandardLayer::new(bob.pk), Conditions::new())?;
    let lock = submit(&mut sim, ctx, &[bob.sk])?;

    println!("issuance without an updater: {immutable}");
    println!("issuance with an updater: {}", asset.cost);
    println!("first update (eve singleton): {eve_update}");
    println!("update: {update}");
    println!("transfer: {transfer}");
    println!("lock: {lock}");

    Ok(())
}
