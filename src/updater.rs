//! The metadata updater singleton, which lets the owner of an asset's updater change its display
//! metadata, and the resolver that wallets use to find the current metadata.

use chia_wallet_sdk::{
    chia::{
        protocol::{Bytes32, Coin, CoinSpend},
        puzzle_types::{
            EveProof, LineageProof, Proof,
            singleton::{LauncherSolution, SingletonArgs, SingletonSolution, SingletonStruct},
        },
    },
    clvm_traits::{FromClvm, ToClvm},
    clvm_utils::{ToTreeHash, TreeHash, tree_hash},
    clvmr::{Allocator, NodePtr, SExp, serde::node_from_bytes},
    driver::{Puzzle, Spend, SpendContext, SpendWithConditions},
    puzzles::{SINGLETON_LAUNCHER_HASH, SINGLETON_TOP_LAYER_V1_1_HASH},
    types::{Conditions, run_puzzle},
};

use crate::{
    DogError, DogInfo, DogMetadata, DogMetadataState, METADATA_UPDATE_PREFIX,
    METADATA_UPDATER_PUZZLE_HASH, MetadataUpdaterArgs,
};

/// The data in the launcher's solution (its "key value list"), which reveals the eve singleton's
/// state and owner, so that they can be checked against its puzzle hash before it's spent.
#[derive(Debug, Clone, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct MetadataUpdaterLaunch<S> {
    pub state: S,
    pub owner_puzzle_hash: Bytes32,
}

/// A metadata updater singleton: a singleton (top layer v1.1) whose inner puzzle is
/// [`MetadataUpdaterArgs`], holding the current [`DogMetadataState`] of every asset whose
/// [`DogMetadata::updater`] is its launcher id.
///
/// The owner (any puzzle) can update the state, transfer ownership, and lock the metadata by
/// melting the singleton. Every spend is recorded on chain, so the update history can be shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataUpdater {
    pub launcher_id: Bytes32,
    /// The current singleton coin.
    pub coin: Coin,
    /// The lineage proof of the current coin.
    pub proof: Proof,
    pub state: DogMetadataState,
    pub owner_puzzle_hash: Bytes32,
}

/// One spend of the launcher or of a [`MetadataUpdater`], as parsed by
/// [`MetadataUpdater::parse_spend`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataRevision {
    /// The coin that was spent, which is the launcher or a singleton coin.
    pub coin: Coin,
    /// The state after the spend.
    pub state: DogMetadataState,
    /// Whether the spend changed the state (always true for the launcher).
    pub updated: bool,
    /// The singleton coin created by the spend, or `None` if it melted the singleton, which locks
    /// the metadata permanently.
    pub child: Option<MetadataUpdater>,
}

impl MetadataUpdater {
    /// The amount of the singleton. A singleton must have an odd amount, so this is the 1 mojo
    /// that an updatable asset needs.
    pub const AMOUNT: u64 = 1;

    /// The launcher id of the updater launched by [`MetadataUpdater::launch`] from the parent
    /// coin, which can be committed to in the metadata before the launcher exists.
    pub fn launcher_id(parent_coin_id: Bytes32) -> Bytes32 {
        Coin::new(parent_coin_id, SINGLETON_LAUNCHER_HASH.into(), Self::AMOUNT).coin_id()
    }

    /// Launches an updater from the parent coin, with the initial state and owner.
    ///
    /// Returns the conditions that the parent coin must output, which create the launcher and
    /// assert its announcement. The assertion is what stops anyone else from spending the launcher
    /// (which anyone can spend) with another state or owner, so it must not be left out.
    pub fn launch(
        ctx: &mut SpendContext,
        parent_coin_id: Bytes32,
        state: DogMetadataState,
        owner_puzzle_hash: Bytes32,
    ) -> Result<(Conditions, Self), DogError> {
        let launcher = chia_wallet_sdk::driver::Launcher::new(parent_coin_id, Self::AMOUNT);
        let launcher_coin = launcher.coin();
        let launcher_id = launcher_coin.coin_id();

        let inner_puzzle_hash =
            MetadataUpdaterArgs::puzzle_hash(state.tree_hash(), owner_puzzle_hash);
        let (conditions, coin) = launcher.spend(
            ctx,
            inner_puzzle_hash.into(),
            MetadataUpdaterLaunch {
                state: state.clone(),
                owner_puzzle_hash,
            },
        )?;

        Ok((
            conditions,
            Self {
                launcher_id,
                coin,
                proof: Proof::Eve(EveProof {
                    parent_parent_coin_info: launcher_coin.parent_coin_info,
                    parent_amount: launcher_coin.amount,
                }),
                state,
                owner_puzzle_hash,
            },
        ))
    }

    /// The inner puzzle hash of the singleton.
    pub fn inner_puzzle_hash(&self) -> TreeHash {
        MetadataUpdaterArgs::puzzle_hash(self.state.tree_hash(), self.owner_puzzle_hash)
    }

    /// The `REMARK` that the owner outputs to replace the state.
    pub fn update_condition(
        ctx: &mut SpendContext,
        state: &DogMetadataState,
    ) -> Result<Conditions, DogError> {
        let rest = ctx.alloc(&(METADATA_UPDATE_PREFIX, state))?;
        Ok(Conditions::new().remark(rest))
    }

    /// The `CREATE_COIN` that the owner outputs to recreate the singleton with the owner, hinted
    /// with the launcher id so that wallets can find every coin in the singleton's lineage.
    pub fn recreate_condition(
        &self,
        ctx: &mut SpendContext,
        owner_puzzle_hash: Bytes32,
    ) -> Result<Conditions, DogError> {
        let hint = ctx.hint(self.launcher_id)?;
        Ok(Conditions::new().create_coin(owner_puzzle_hash, self.coin.amount, hint))
    }

    /// Adds the spend of the singleton with the owner's spend, whose puzzle must have the hash
    /// [`MetadataUpdater::owner_puzzle_hash`]. Returns the spend, parsed like a wallet would.
    pub fn spend(
        &self,
        ctx: &mut SpendContext,
        owner_spend: Spend,
    ) -> Result<MetadataRevision, DogError> {
        let state = ctx.alloc(&self.state)?;
        let inner_puzzle = ctx.curry(MetadataUpdaterArgs::new(state, owner_spend.puzzle))?;
        let puzzle = ctx.curry(SingletonArgs::new(self.launcher_id, inner_puzzle))?;
        let solution = ctx.alloc(&SingletonSolution {
            lineage_proof: self.proof,
            amount: self.coin.amount,
            inner_solution: owner_spend.solution,
        })?;
        ctx.spend(self.coin, Spend::new(puzzle, solution))?;

        Self::parse_spend(ctx, self.launcher_id, self.coin, puzzle, solution)
    }

    /// Replaces the state, keeping the owner. The owner's spend also outputs `conditions`.
    pub fn update(
        &self,
        ctx: &mut SpendContext,
        owner: &impl SpendWithConditions,
        state: &DogMetadataState,
        conditions: Conditions,
    ) -> Result<Self, DogError> {
        let conditions = conditions
            .extend(Self::update_condition(ctx, state)?)
            .extend(self.recreate_condition(ctx, self.owner_puzzle_hash)?);
        self.spend_child(ctx, owner, conditions)
    }

    /// Transfers ownership to another puzzle hash, keeping the state.
    pub fn transfer(
        &self,
        ctx: &mut SpendContext,
        owner: &impl SpendWithConditions,
        new_owner_puzzle_hash: Bytes32,
        conditions: Conditions,
    ) -> Result<Self, DogError> {
        let conditions = conditions.extend(self.recreate_condition(ctx, new_owner_puzzle_hash)?);
        self.spend_child(ctx, owner, conditions)
    }

    /// Locks the metadata permanently by melting the singleton, so that the current state is
    /// final. Nobody, including the owner, can update it afterward.
    pub fn lock(
        &self,
        ctx: &mut SpendContext,
        owner: &impl SpendWithConditions,
        conditions: Conditions,
    ) -> Result<MetadataRevision, DogError> {
        let owner_spend = owner.spend_with_conditions(ctx, conditions.melt_singleton())?;
        self.spend(ctx, owner_spend)
    }

    fn spend_child(
        &self,
        ctx: &mut SpendContext,
        owner: &impl SpendWithConditions,
        conditions: Conditions,
    ) -> Result<Self, DogError> {
        let owner_spend = owner.spend_with_conditions(ctx, conditions)?;
        self.spend(ctx, owner_spend)?
            .child
            .ok_or(DogError::InvalidUpdaterSpend)
    }

    /// Parses a spend of the launcher, or of a singleton coin of the updater with the launcher id.
    ///
    /// Returns [`DogError::InvalidUpdaterSpend`] if it's the spend of another coin, such as another
    /// singleton with the same puzzle, or of a launcher with a solution that doesn't match the
    /// singleton it creates. It doesn't check that the spend is on chain, so it must come from a
    /// trusted full node.
    pub fn parse_spend(
        allocator: &mut Allocator,
        launcher_id: Bytes32,
        coin: Coin,
        puzzle: NodePtr,
        solution: NodePtr,
    ) -> Result<MetadataRevision, DogError> {
        if coin.puzzle_hash != tree_hash(allocator, puzzle).into() {
            return Err(DogError::InvalidUpdaterSpend);
        }

        if coin.puzzle_hash == SINGLETON_LAUNCHER_HASH.into() {
            return Self::parse_launcher_spend(allocator, launcher_id, coin, solution);
        }

        // The puzzle must be the singleton top layer with the launcher id, around the updater.
        let Some(singleton) = Puzzle::parse(allocator, puzzle).as_curried() else {
            return Err(DogError::InvalidUpdaterSpend);
        };
        if singleton.mod_hash != SINGLETON_TOP_LAYER_V1_1_HASH.into() {
            return Err(DogError::InvalidUpdaterSpend);
        }
        let singleton_args = SingletonArgs::<NodePtr>::from_clvm(allocator, singleton.args)?;
        if singleton_args.singleton_struct != SingletonStruct::new(launcher_id) {
            return Err(DogError::InvalidUpdaterSpend);
        }

        let Some(updater) = Puzzle::parse(allocator, singleton_args.inner_puzzle).as_curried()
        else {
            return Err(DogError::InvalidUpdaterSpend);
        };
        if updater.mod_hash != METADATA_UPDATER_PUZZLE_HASH {
            return Err(DogError::InvalidUpdaterSpend);
        }
        let args = MetadataUpdaterArgs::<NodePtr, NodePtr>::from_clvm(allocator, updater.args)?;
        if args.mod_hash != METADATA_UPDATER_PUZZLE_HASH.into() {
            return Err(DogError::InvalidUpdaterSpend);
        }

        let solution = SingletonSolution::<NodePtr>::from_clvm(allocator, solution)?;
        let conditions = run_puzzle(allocator, args.owner, solution.inner_solution)?;
        let output = OwnerOutput::parse(allocator, conditions)?;

        let state_ptr = output.update.unwrap_or(args.state);
        let state = parse_state(allocator, state_ptr)?;

        let child = match output.child {
            Child::Melted => None,
            Child::Recreated {
                owner_puzzle_hash,
                amount,
            } => {
                let inner_puzzle_hash = MetadataUpdaterArgs::puzzle_hash(
                    tree_hash(allocator, state_ptr),
                    owner_puzzle_hash,
                );
                Some(Self {
                    launcher_id,
                    coin: Coin::new(
                        coin.coin_id(),
                        SingletonArgs::curry_tree_hash(launcher_id, inner_puzzle_hash).into(),
                        amount,
                    ),
                    proof: Proof::Lineage(LineageProof {
                        parent_parent_coin_info: coin.parent_coin_info,
                        parent_inner_puzzle_hash: tree_hash(allocator, singleton_args.inner_puzzle)
                            .into(),
                        parent_amount: coin.amount,
                    }),
                    state: state.clone(),
                    owner_puzzle_hash,
                })
            }
        };

        Ok(MetadataRevision {
            coin,
            state,
            updated: output.update.is_some(),
            child,
        })
    }

    fn parse_launcher_spend(
        allocator: &Allocator,
        launcher_id: Bytes32,
        coin: Coin,
        solution: NodePtr,
    ) -> Result<MetadataRevision, DogError> {
        if coin.coin_id() != launcher_id {
            return Err(DogError::InvalidUpdaterSpend);
        }

        let solution =
            LauncherSolution::<MetadataUpdaterLaunch<NodePtr>>::from_clvm(allocator, solution)?;
        let launch = solution.key_value_list;
        let state = parse_state(allocator, launch.state)?;

        // The launcher only commits to the singleton's puzzle hash, so the state and owner in its
        // solution must be checked against it.
        let inner_puzzle_hash = MetadataUpdaterArgs::puzzle_hash(
            tree_hash(allocator, launch.state),
            launch.owner_puzzle_hash,
        );
        let puzzle_hash: Bytes32 =
            SingletonArgs::curry_tree_hash(launcher_id, inner_puzzle_hash).into();
        if solution.singleton_puzzle_hash != puzzle_hash || solution.amount % 2 != 1 {
            return Err(DogError::InvalidUpdaterSpend);
        }

        Ok(MetadataRevision {
            coin,
            state: state.clone(),
            updated: true,
            child: Some(Self {
                launcher_id,
                coin: Coin::new(launcher_id, puzzle_hash, solution.amount),
                proof: Proof::Eve(EveProof {
                    parent_parent_coin_info: coin.parent_coin_info,
                    parent_amount: coin.amount,
                }),
                state,
                owner_puzzle_hash: launch.owner_puzzle_hash,
            }),
        })
    }
}

/// Parses a state, which must be exactly a [`DogMetadataState`], so that the singleton's puzzle can
/// be built from it again. The puzzle doesn't check the state, so the owner could publish anything.
fn parse_state(allocator: &Allocator, state: NodePtr) -> Result<DogMetadataState, DogError> {
    let parsed = DogMetadataState::from_clvm(allocator, state)
        .map_err(|_| DogError::InvalidMetadataState)?;
    if parsed.tree_hash() != tree_hash(allocator, state) {
        return Err(DogError::InvalidMetadataState);
    }
    Ok(parsed)
}

/// What the singleton's child is, from the owner's odd `CREATE_COIN`.
enum Child {
    Recreated {
        owner_puzzle_hash: Bytes32,
        amount: u64,
    },
    Melted,
}

/// The parts of the owner's output that the updater and the singleton top layer look at.
struct OwnerOutput {
    update: Option<NodePtr>,
    child: Child,
}

impl OwnerOutput {
    /// Parses the owner's conditions the same way as the puzzles: by exact opcode bytes, the
    /// lowest bit of the amount, and the canonical encoding of -113 for a melt.
    fn parse(allocator: &Allocator, mut conditions: NodePtr) -> Result<Self, DogError> {
        let mut update = None;
        let mut child = None;

        while let SExp::Pair(condition, rest) = allocator.sexp(conditions) {
            conditions = rest;

            let SExp::Pair(opcode, args) = allocator.sexp(condition) else {
                continue;
            };
            let SExp::Atom = allocator.sexp(opcode) else {
                continue;
            };

            match allocator.atom(opcode).as_ref() {
                [1] => {
                    let SExp::Pair(prefix, state) = allocator.sexp(args) else {
                        continue;
                    };
                    if let SExp::Atom = allocator.sexp(prefix)
                        && allocator.atom(prefix).as_ref() == METADATA_UPDATE_PREFIX.as_bytes()
                    {
                        if update.is_some() {
                            return Err(DogError::InvalidUpdaterSpend);
                        }
                        update = Some(state);
                    }
                }
                [51] => {
                    let (puzzle_hash, (amount, _)) =
                        <(NodePtr, (NodePtr, NodePtr))>::from_clvm(allocator, args)?;
                    let amount_bytes = allocator.atom(amount);
                    if amount_bytes
                        .as_ref()
                        .last()
                        .is_none_or(|byte| byte & 1 == 0)
                    {
                        continue;
                    }
                    if child.is_some() {
                        return Err(DogError::InvalidUpdaterSpend);
                    }
                    child = Some(if amount_bytes.as_ref() == [0x8f] {
                        Child::Melted
                    } else {
                        Child::Recreated {
                            owner_puzzle_hash: Bytes32::from_clvm(allocator, puzzle_hash)?,
                            amount: u64::from_clvm(allocator, amount)?,
                        }
                    });
                }
                _ => {}
            }
        }

        Ok(Self {
            update,
            child: child.ok_or(DogError::InvalidUpdaterSpend)?,
        })
    }
}

/// Whether an asset's display metadata can change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataMutability {
    /// There's no updater, so the genesis info is permanent.
    Immutable,
    /// There's an updater, but no spend of its launcher was given. Until the launcher is spent,
    /// whoever can create it may still launch the updater, so wallets should show the genesis info
    /// as mutable.
    Unlaunched,
    /// The owner of the updater can change the metadata. This is the updater's latest known coin,
    /// which the wallet should check is unspent.
    Updatable(Box<MetadataUpdater>),
    /// The updater was melted, so the metadata can never change again.
    Locked,
}

/// The current metadata of an asset, as resolved by [`resolve_metadata`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMetadata {
    /// The decimals, which are always the committed ones.
    pub decimals: u8,
    /// The current display metadata.
    pub info: DogInfo,
    /// Links to the current image, as published by the updater's owner. Empty if there's no
    /// updater, in which case the links in the issuance `REMARK` are the only ones.
    pub image_uris: Vec<String>,
    /// Links to the current data, as published by the updater's owner.
    pub data_uris: Vec<String>,
    pub mutability: MetadataMutability,
    /// The parsed spends of the updater, oldest first.
    pub history: Vec<MetadataRevision>,
}

/// Resolves the current metadata of an asset from its committed metadata and the spends of its
/// updater singleton, oldest first.
///
/// The spends must be consecutive in the singleton's lineage, starting from the launcher's spend
/// for the full history, or from any later spend. The current state only needs the latest one: the
/// spend of the parent of the unspent singleton coin, or the spend that melted it. They aren't
/// checked to be on chain, so they must come from a trusted full node, but they're checked to be
/// spends of the committed updater, so a spend of another singleton is rejected.
pub fn resolve_metadata(
    allocator: &mut Allocator,
    metadata: &DogMetadata,
    spends: &[CoinSpend],
) -> Result<ResolvedMetadata, DogError> {
    let genesis = ResolvedMetadata {
        decimals: metadata.decimals,
        info: metadata.genesis.clone(),
        image_uris: Vec::new(),
        data_uris: Vec::new(),
        mutability: MetadataMutability::Immutable,
        history: Vec::new(),
    };

    let Some(launcher_id) = metadata.updater else {
        return Ok(genesis);
    };

    let mut history: Vec<MetadataRevision> = Vec::with_capacity(spends.len());

    for coin_spend in spends {
        if let Some(previous) = history.last()
            && previous.child.as_ref().map(|child| child.coin) != Some(coin_spend.coin)
        {
            return Err(DogError::InvalidUpdaterSpend);
        }

        let puzzle = node_from_bytes(allocator, coin_spend.puzzle_reveal.as_ref())?;
        let solution = node_from_bytes(allocator, coin_spend.solution.as_ref())?;
        history.push(MetadataUpdater::parse_spend(
            allocator,
            launcher_id,
            coin_spend.coin,
            puzzle,
            solution,
        )?);
    }

    let Some(latest) = history.last() else {
        return Ok(ResolvedMetadata {
            mutability: MetadataMutability::Unlaunched,
            ..genesis
        });
    };

    Ok(ResolvedMetadata {
        decimals: metadata.decimals,
        info: latest.state.info.clone(),
        image_uris: latest.state.image_uris.clone(),
        data_uris: latest.state.data_uris.clone(),
        mutability: latest
            .child
            .clone()
            .map_or(MetadataMutability::Locked, |child| {
                MetadataMutability::Updatable(Box::new(child))
            }),
        history,
    })
}
