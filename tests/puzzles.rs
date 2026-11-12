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
    DOG_EXEC_PUZZLE, DOG_EXEC_PUZZLE_HASH, DOG_PUZZLE, DOG_PUZZLE_HASH,
    EVERYTHING_WITH_SIGNATURE_TAIL, EVERYTHING_WITH_SIGNATURE_TAIL_HASH, GENESIS_BY_COIN_ID_TAIL,
    GENESIS_BY_COIN_ID_TAIL_HASH,
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

/// The execution coin puzzle includes the hash of the DOG coin puzzle as a constant, which has to
/// be updated by hand when the DOG coin puzzle changes.
#[test]
fn dog_mod_hash_constant_is_up_to_date() -> Result<()> {
    let shared = std::fs::read_to_string("puzzles/shared.rue")?;
    let hash = hex::encode(DOG_PUZZLE_HASH.to_bytes());
    assert!(
        shared.contains(&format!("DOG_MOD_HASH: Bytes32 = 0x{hash};"))
            || shared.contains(&format!("DOG_MOD_HASH: Bytes32 =\n    0x{hash};")),
        "update `DOG_MOD_HASH` in `puzzles/shared.rue` to 0x{hash}, then rebuild the puzzles"
    );

    // It's in the compiled execution coin puzzle, as a quoted 32 byte atom.
    let atom = [&[0xa0][..], DOG_PUZZLE_HASH.as_ref()].concat();
    assert!(
        DOG_EXEC_PUZZLE
            .windows(atom.len())
            .any(|window| window == atom.as_slice())
    );

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
