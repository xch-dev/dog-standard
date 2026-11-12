//! The compiled puzzles, and the types of their curried arguments and solutions.
//!
//! The bytecode is compiled from the Rue sources in `puzzles/` with `rue build --all --hash`, and
//! embedded from `puzzles/dist/`. The `compiled_puzzles_are_up_to_date` test checks that it matches
//! the sources.

use std::borrow::Cow;

use chia_wallet_sdk::{
    chia::{
        bls::PublicKey,
        protocol::Bytes32,
        puzzle_types::{LineageProof, singleton::SingletonStruct},
    },
    clvm_traits::{FromClvm, ToClvm},
    clvm_utils::{ToTreeHash, TreeHash},
    puzzles::SINGLETON_TOP_LAYER_V1_1_HASH,
    types::Mod,
};

/// Decodes a hex string with a trailing newline at compile time.
const fn decode_hex<const N: usize>(hex: &str) -> [u8; N] {
    const fn digit(byte: u8) -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => panic!("invalid hex digit"),
        }
    }

    let hex = hex.as_bytes();
    assert!(
        hex.len() == N * 2 + 1 && hex[N * 2] == b'\n',
        "invalid hex length"
    );

    let mut bytes = [0; N];
    let mut i = 0;
    while i < N {
        bytes[i] = (digit(hex[i * 2]) << 4) | digit(hex[i * 2 + 1]);
        i += 1;
    }
    bytes
}

macro_rules! puzzle {
    ($reveal:ident, $hash:ident, $name:literal) => {
        #[doc = concat!("The bytecode of `puzzles/", $name, "`.")]
        pub const $reveal: [u8; {
            (include_str!(concat!("../puzzles/dist/", $name, ".hex")).len() - 1) / 2
        }] = decode_hex(include_str!(concat!("../puzzles/dist/", $name, ".hex")));

        #[doc = concat!("The tree hash of `puzzles/", $name, "`.")]
        pub const $hash: TreeHash = TreeHash::new(decode_hex(include_str!(concat!(
            "../puzzles/dist/",
            $name,
            ".hash"
        ))));
    };
}

puzzle!(DOG_PUZZLE, DOG_PUZZLE_HASH, "dog.rue");
puzzle!(DOG_EXEC_PUZZLE, DOG_EXEC_PUZZLE_HASH, "dog_exec.rue");
puzzle!(
    GENESIS_BY_COIN_ID_TAIL,
    GENESIS_BY_COIN_ID_TAIL_HASH,
    "genesis_by_coin_id.rue"
);
puzzle!(
    EVERYTHING_WITH_SIGNATURE_TAIL,
    EVERYTHING_WITH_SIGNATURE_TAIL_HASH,
    "everything_with_signature.rue"
);
puzzle!(
    EVERYTHING_WITH_SINGLETON_TAIL,
    EVERYTHING_WITH_SINGLETON_TAIL_HASH,
    "everything_with_singleton.rue"
);
puzzle!(
    CAPPED_SUPPLY_TAIL,
    CAPPED_SUPPLY_TAIL_HASH,
    "capped_supply.rue"
);
puzzle!(
    SUPPLY_COUNTER_PUZZLE,
    SUPPLY_COUNTER_PUZZLE_HASH,
    "supply_counter.rue"
);
puzzle!(BURNABLE_TAIL, BURNABLE_TAIL_HASH, "burnable.rue");
puzzle!(
    ISSUANCE_DEADLINE_TAIL,
    ISSUANCE_DEADLINE_TAIL_HASH,
    "issuance_deadline.rue"
);
puzzle!(DELEGATED_TAIL, DELEGATED_TAIL_HASH, "delegated_tail.rue");
puzzle!(MULTISIG_TAIL, MULTISIG_TAIL_HASH, "multisig.rue");

/// The curried argument that the DOG coins and execution coins of an asset have in common.
///
/// It's a pair rather than a list, which makes it a little smaller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct DogAsset {
    pub exec_mod_hash: Bytes32,
    #[clvm(rest)]
    pub asset_id: Bytes32,
}

impl DogAsset {
    pub fn new(asset_id: Bytes32) -> Self {
        Self {
            exec_mod_hash: DOG_EXEC_PUZZLE_HASH.into(),
            asset_id,
        }
    }
}

/// The curried arguments of a DOG coin, which holds tokens with an amount of 0 mojos.
///
/// The arguments that differ between coins come first, so that the hash of the rest can be
/// calculated once per spend. The DOG coin puzzle doesn't curry its own mod hash, since the
/// execution coin puzzle includes it as a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct DogArgs {
    pub inner_puzzle_hash: Bytes32,
    pub amount: u64,
    pub asset: DogAsset,
}

impl DogArgs {
    pub fn new(asset_id: Bytes32, inner_puzzle_hash: Bytes32, amount: u64) -> Self {
        Self {
            inner_puzzle_hash,
            amount,
            asset: DogAsset::new(asset_id),
        }
    }

    /// The puzzle hash of a DOG coin.
    pub fn puzzle_hash(asset_id: Bytes32, inner_puzzle_hash: Bytes32, amount: u64) -> TreeHash {
        Self::new(asset_id, inner_puzzle_hash, amount).curry_tree_hash()
    }
}

impl Mod for DogArgs {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&DOG_PUZZLE)
    }

    fn mod_hash() -> TreeHash {
        DOG_PUZZLE_HASH
    }
}

/// The solution of a DOG coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct DogSolution<T, S> {
    /// The id of the DOG coin being spent.
    pub my_coin_id: Bytes32,
    /// Proof that the parent is an execution coin of the same asset.
    pub lineage_proof: Option<LineageProof>,
    /// A reveal of the TAIL, to issue or melt tokens or spend a coin without a lineage proof.
    pub tail_spend: Option<TailSpend<T, S>>,
}

/// A reveal of the TAIL in the solution of a DOG coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct TailSpend<T, S> {
    /// The TAIL puzzle.
    pub tail: T,
    /// The tree hash of the asset's [`DogMetadata`](crate::DogMetadata).
    pub metadata_hash: Bytes32,
    /// The amount added to the DOG coin's amount to get the execution coin's amount.
    pub delta: i128,
    /// The solution of the TAIL, which is passed after the supply change.
    #[clvm(rest)]
    pub solution: S,
}

/// The curried arguments of an execution coin, which runs the inner puzzle of a DOG coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct DogExecArgs<I> {
    pub inner_puzzle: I,
    pub asset: DogAsset,
}

impl<I> DogExecArgs<I> {
    pub fn new(asset_id: Bytes32, inner_puzzle: I) -> Self {
        Self {
            inner_puzzle,
            asset: DogAsset::new(asset_id),
        }
    }
}

impl DogExecArgs<TreeHash> {
    /// The puzzle hash of an execution coin, which doesn't depend on its amount.
    pub fn puzzle_hash(asset_id: Bytes32, inner_puzzle_hash: TreeHash) -> TreeHash {
        Self::new(asset_id, inner_puzzle_hash).curry_tree_hash()
    }
}

impl<I> Mod for DogExecArgs<I> {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&DOG_EXEC_PUZZLE)
    }

    fn mod_hash() -> TreeHash {
        DOG_EXEC_PUZZLE_HASH
    }
}

/// The solution of an execution coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct DogExecSolution<I> {
    pub inner_solution: I,
    /// The amount of the execution coin in mojos, which is its token amount.
    pub my_amount: u64,
    /// The token amount curried into the parent DOG coin.
    pub parent_amount: u64,
    /// The running total of the previous coins in the ring, minus the amounts they created.
    pub prev_subtotal: i128,
    /// The inner puzzle hash of the next execution coin in the ring, which the subtotal is sent to.
    pub next_inner_puzzle_hash: Bytes32,
}

/// A TAIL that can only issue tokens from DOG coins whose parent is the genesis coin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct GenesisByCoinIdTailArgs {
    pub genesis_coin_id: Bytes32,
}

impl GenesisByCoinIdTailArgs {
    pub fn new(genesis_coin_id: Bytes32) -> Self {
        Self { genesis_coin_id }
    }
}

impl Mod for GenesisByCoinIdTailArgs {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&GENESIS_BY_COIN_ID_TAIL)
    }

    fn mod_hash() -> TreeHash {
        GENESIS_BY_COIN_ID_TAIL_HASH
    }
}

/// A TAIL that allows any supply change signed by a public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct EverythingWithSignatureTailArgs {
    pub public_key: PublicKey,
}

impl EverythingWithSignatureTailArgs {
    pub fn new(public_key: PublicKey) -> Self {
        Self { public_key }
    }

    /// The message that must be signed with `AGG_SIG_ME` to change the supply by `supply_change`.
    ///
    /// # Panics
    ///
    /// Never, since an integer can always be allocated.
    pub fn message(supply_change: i128) -> Vec<u8> {
        supply_change_message(supply_change)
    }
}

impl Mod for EverythingWithSignatureTailArgs {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&EVERYTHING_WITH_SIGNATURE_TAIL)
    }

    fn mod_hash() -> TreeHash {
        EVERYTHING_WITH_SIGNATURE_TAIL_HASH
    }
}

/// The canonical CLVM encoding of a supply change, which is the message that TAILs sign or receive.
///
/// # Panics
///
/// Never, since an integer can always be allocated.
pub fn supply_change_message(supply_change: i128) -> Vec<u8> {
    let mut allocator = chia_wallet_sdk::clvmr::Allocator::new();
    let ptr = supply_change
        .to_clvm(&mut allocator)
        .expect("an integer can always be allocated");
    allocator.atom(ptr).as_ref().to_vec()
}

/// The tree hash of the singleton struct for a launcher id, as curried into the singleton TAILs.
fn singleton_struct_hash(launcher_id: Bytes32) -> Bytes32 {
    SingletonStruct::new(launcher_id).tree_hash().into()
}

/// The message mode with which a singleton authorizes a supply change, from its puzzle hash to the
/// DOG coin's id.
pub const SINGLETON_AUTHORIZATION_MODE: u8 = 0b010_111;

/// A TAIL that allows any supply change authorized by a singleton.
///
/// The singleton sends a message with the mode [`SINGLETON_AUTHORIZATION_MODE`] to the DOG coin's id,
/// containing [`supply_change_message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct EverythingWithSingletonTailArgs {
    pub singleton_mod_hash: Bytes32,
    pub singleton_struct_hash: Bytes32,
}

impl EverythingWithSingletonTailArgs {
    pub fn new(launcher_id: Bytes32) -> Self {
        Self {
            singleton_mod_hash: SINGLETON_TOP_LAYER_V1_1_HASH.into(),
            singleton_struct_hash: singleton_struct_hash(launcher_id),
        }
    }
}

impl Mod for EverythingWithSingletonTailArgs {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&EVERYTHING_WITH_SINGLETON_TAIL)
    }

    fn mod_hash() -> TreeHash {
        EVERYTHING_WITH_SINGLETON_TAIL_HASH
    }
}

/// The solution of [`EverythingWithSingletonTailArgs`], after the supply change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct EverythingWithSingletonTailSolution {
    /// The singleton's current inner puzzle hash.
    pub singleton_inner_puzzle_hash: Bytes32,
}

/// A TAIL that allows supply changes authorized by a singleton whose inner puzzle is a
/// [`SupplyCounterArgs`] with the cap, so no more than the cap can ever be issued in total.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct CappedSupplyTailArgs {
    pub singleton_mod_hash: Bytes32,
    pub singleton_struct_hash: Bytes32,
    pub counter_mod_hash: Bytes32,
    pub cap: u64,
}

impl CappedSupplyTailArgs {
    pub fn new(launcher_id: Bytes32, cap: u64) -> Self {
        Self {
            singleton_mod_hash: SINGLETON_TOP_LAYER_V1_1_HASH.into(),
            singleton_struct_hash: singleton_struct_hash(launcher_id),
            counter_mod_hash: SUPPLY_COUNTER_PUZZLE_HASH.into(),
            cap,
        }
    }
}

impl Mod for CappedSupplyTailArgs {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&CAPPED_SUPPLY_TAIL)
    }

    fn mod_hash() -> TreeHash {
        CAPPED_SUPPLY_TAIL_HASH
    }
}

/// The solution of [`CappedSupplyTailArgs`], after the supply change. It's the state of the supply
/// counter singleton when it authorizes the supply change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct CappedSupplyTailSolution {
    pub issued: u64,
    pub owner_puzzle_hash: Bytes32,
}

/// The inner puzzle of the singleton for [`CappedSupplyTailArgs`]. It counts the positive supply
/// changes that the owner authorizes, and can't authorize more than the cap in total.
///
/// Its solution is passed to the owner's puzzle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct SupplyCounterArgs<I> {
    pub mod_hash: Bytes32,
    pub cap: u64,
    pub issued: u64,
    pub owner: I,
}

impl<I> SupplyCounterArgs<I> {
    pub fn new(cap: u64, issued: u64, owner: I) -> Self {
        Self {
            mod_hash: SUPPLY_COUNTER_PUZZLE_HASH.into(),
            cap,
            issued,
            owner,
        }
    }
}

impl SupplyCounterArgs<TreeHash> {
    /// The puzzle hash of a supply counter, which is the singleton's inner puzzle hash.
    pub fn puzzle_hash(cap: u64, issued: u64, owner_puzzle_hash: Bytes32) -> TreeHash {
        Self::new(cap, issued, owner_puzzle_hash.into()).curry_tree_hash()
    }
}

impl<I> Mod for SupplyCounterArgs<I> {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&SUPPLY_COUNTER_PUZZLE)
    }

    fn mod_hash() -> TreeHash {
        SUPPLY_COUNTER_PUZZLE_HASH
    }
}

/// A TAIL that allows any negative supply change, and passes the rest to the issuance TAIL.
///
/// Its solution is passed to the issuance TAIL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct BurnableTailArgs<T> {
    pub issuance_tail: T,
}

impl<T> BurnableTailArgs<T> {
    pub fn new(issuance_tail: T) -> Self {
        Self { issuance_tail }
    }
}

impl<T> Mod for BurnableTailArgs<T> {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&BURNABLE_TAIL)
    }

    fn mod_hash() -> TreeHash {
        BURNABLE_TAIL_HASH
    }
}

/// A TAIL that passes every supply change to another TAIL, but only allows issuing tokens before
/// the deadline, which is a Unix timestamp.
///
/// Its solution is passed to the other TAIL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct IssuanceDeadlineTailArgs<T> {
    pub deadline: u64,
    pub tail: T,
}

impl<T> IssuanceDeadlineTailArgs<T> {
    pub fn new(deadline: u64, tail: T) -> Self {
        Self { deadline, tail }
    }
}

impl<T> Mod for IssuanceDeadlineTailArgs<T> {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&ISSUANCE_DEADLINE_TAIL)
    }

    fn mod_hash() -> TreeHash {
        ISSUANCE_DEADLINE_TAIL_HASH
    }
}

/// A TAIL that runs any TAIL whose tree hash is signed by the public key with `AGG_SIG_UNSAFE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct DelegatedTailArgs {
    pub public_key: PublicKey,
}

impl DelegatedTailArgs {
    pub fn new(public_key: PublicKey) -> Self {
        Self { public_key }
    }
}

impl Mod for DelegatedTailArgs {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&DELEGATED_TAIL)
    }

    fn mod_hash() -> TreeHash {
        DELEGATED_TAIL_HASH
    }
}

/// The solution of [`DelegatedTailArgs`], after the supply change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct DelegatedTailSolution<P, S> {
    /// The delegated TAIL, whose tree hash must be signed.
    pub delegated_tail: P,
    /// The solution of the delegated TAIL, which is passed after the supply change.
    #[clvm(rest)]
    pub delegated_solution: S,
}

/// A TAIL that allows any supply change signed by at least `threshold` of the public keys, with
/// `AGG_SIG_ME` and [`supply_change_message`].
#[derive(Debug, Clone, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
pub struct MultisigTailArgs {
    pub threshold: u64,
    pub public_keys: Vec<PublicKey>,
}

impl MultisigTailArgs {
    pub fn new(threshold: u64, public_keys: Vec<PublicKey>) -> Self {
        Self {
            threshold,
            public_keys,
        }
    }
}

impl Mod for MultisigTailArgs {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Borrowed(&MULTISIG_TAIL)
    }

    fn mod_hash() -> TreeHash {
        MULTISIG_TAIL_HASH
    }
}

/// The solution of [`MultisigTailArgs`], after the supply change.
#[derive(Debug, Clone, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct MultisigTailSolution {
    /// Whether each of the public keys signs, in the same order.
    pub signers: Vec<bool>,
}
