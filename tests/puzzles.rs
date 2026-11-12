//! Checks that the committed bytecode in `puzzles/dist/` matches the Rue sources.
//!
//! If this fails after changing a puzzle, rebuild it with `rue build --all --hash` using the `rue`
//! CLI from the main branch of Rue.

use std::{collections::HashMap, path::Path};

use anyhow::Result;
use chia_wallet_sdk::{
    clvm_utils::{TreeHash, tree_hash},
    clvmr::{Allocator, serde::node_from_bytes},
    types::compile_rue,
};
use dog::{
    BURNABLE_TAIL, BURNABLE_TAIL_HASH, CAPPED_SUPPLY_TAIL, CAPPED_SUPPLY_TAIL_HASH, DELEGATED_TAIL,
    DELEGATED_TAIL_HASH, DOG_EXEC_PUZZLE, DOG_EXEC_PUZZLE_HASH, DOG_PUZZLE, DOG_PUZZLE_HASH,
    EVERYTHING_WITH_SIGNATURE_TAIL, EVERYTHING_WITH_SIGNATURE_TAIL_HASH,
    EVERYTHING_WITH_SINGLETON_TAIL, EVERYTHING_WITH_SINGLETON_TAIL_HASH, GENESIS_BY_COIN_ID_TAIL,
    GENESIS_BY_COIN_ID_TAIL_HASH, ISSUANCE_DEADLINE_TAIL, ISSUANCE_DEADLINE_TAIL_HASH,
    MULTISIG_TAIL, MULTISIG_TAIL_HASH, SUPPLY_COUNTER_PUZZLE, SUPPLY_COUNTER_PUZZLE_HASH,
};
use rstest::rstest;

#[rstest]
#[case::dog("puzzles/dog.rue", &DOG_PUZZLE, DOG_PUZZLE_HASH)]
#[case::dog_exec("puzzles/dog_exec.rue", &DOG_EXEC_PUZZLE, DOG_EXEC_PUZZLE_HASH)]
#[case::genesis_by_coin_id(
    "puzzles/tails/genesis_by_coin_id.rue",
    &GENESIS_BY_COIN_ID_TAIL,
    GENESIS_BY_COIN_ID_TAIL_HASH
)]
#[case::everything_with_signature(
    "puzzles/tails/everything_with_signature.rue",
    &EVERYTHING_WITH_SIGNATURE_TAIL,
    EVERYTHING_WITH_SIGNATURE_TAIL_HASH
)]
#[case::everything_with_singleton(
    "puzzles/tails/everything_with_singleton.rue",
    &EVERYTHING_WITH_SINGLETON_TAIL,
    EVERYTHING_WITH_SINGLETON_TAIL_HASH
)]
#[case::capped_supply(
    "puzzles/tails/capped_supply.rue",
    &CAPPED_SUPPLY_TAIL,
    CAPPED_SUPPLY_TAIL_HASH
)]
#[case::supply_counter(
    "puzzles/tails/supply_counter.rue",
    &SUPPLY_COUNTER_PUZZLE,
    SUPPLY_COUNTER_PUZZLE_HASH
)]
#[case::burnable("puzzles/tails/burnable.rue", &BURNABLE_TAIL, BURNABLE_TAIL_HASH)]
#[case::issuance_deadline(
    "puzzles/tails/issuance_deadline.rue",
    &ISSUANCE_DEADLINE_TAIL,
    ISSUANCE_DEADLINE_TAIL_HASH
)]
#[case::delegated_tail("puzzles/tails/delegated_tail.rue", &DELEGATED_TAIL, DELEGATED_TAIL_HASH)]
#[case::multisig("puzzles/tails/multisig.rue", &MULTISIG_TAIL, MULTISIG_TAIL_HASH)]
fn compiled_puzzles_are_up_to_date(
    #[case] path: &str,
    #[case] reveal: &[u8],
    #[case] hash: TreeHash,
) -> Result<()> {
    let compilation = compile_rue(Path::new(path), false, None)?;
    assert_eq!(hex::encode(&compilation.reveal), hex::encode(reveal));
    assert_eq!(compilation.hash, hash);

    let mut allocator = Allocator::new();
    let ptr = node_from_bytes(&mut allocator, reveal)?;
    assert_eq!(tree_hash(&allocator, ptr), hash);

    Ok(())
}

/// Every lint is denied in `Rue.toml`, so the puzzles must compile without any diagnostics.
#[test]
fn puzzles_have_no_diagnostics() -> Result<()> {
    let project = rue_options::find_project(Path::new(env!("CARGO_MANIFEST_DIR")), false)?
        .expect("the Rue project should be found");
    assert_eq!(project.unknown_lints, [] as [rue_options::UnknownLint; 0]);
    assert_eq!(
        project.forbidden_lints,
        [] as [rue_options::ForbiddenLint; 0]
    );

    let mut ctx = rue_compiler::Compiler::new(project.options);
    rue_compiler::FileTree::compile_path(&mut ctx, &project.entrypoint, &mut HashMap::new())?;

    let diagnostics = ctx.take_diagnostics();
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");

    Ok(())
}
