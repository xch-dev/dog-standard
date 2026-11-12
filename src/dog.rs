use chia_wallet_sdk::{
    chia::{
        bls::PublicKey,
        protocol::{Bytes32, Coin},
        puzzle_types::{CoinProof, LineageProof, Memos},
    },
    clvm_traits::FromClvm,
    clvm_utils::{ToTreeHash, TreeHash, tree_hash},
    clvmr::{Allocator, NodePtr, SExp},
    driver::{Puzzle, Spend, SpendContext},
    types::{Conditions, conditions::CreateCoin, run_puzzle},
};

use crate::{
    DogArgs, DogError, DogExecArgs, DogExecSolution, DogMetadata, DogMetadataRemark, DogSolution,
    EverythingWithSignatureTailArgs, GenesisByCoinIdTailArgs, TailSpend, asset_id,
    puzzles::{DOG_EXEC_PUZZLE_HASH, DOG_PUZZLE_HASH},
};

/// A DOG coin, which holds an amount of tokens with an amount of 0 mojos.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dog {
    /// The DOG coin itself, which always has an amount of 0.
    pub coin: Coin,
    /// The token amount, which is curried into the puzzle.
    pub amount: u64,
    pub asset_id: Bytes32,
    /// The hash of the inner puzzle, which controls the tokens.
    pub p2_puzzle_hash: Bytes32,
    /// Proof that the parent is an execution coin of the same asset. If this is `None`, the TAIL
    /// must be revealed to spend the coin.
    pub lineage_proof: Option<LineageProof>,
}

/// A spend of a [`Dog`], which is combined with others in [`Dog::spend_all`].
#[derive(Debug, Clone, Copy)]
pub struct DogSpend {
    pub dog: Dog,
    /// The spend of the inner puzzle, which must have the tree hash [`Dog::p2_puzzle_hash`].
    pub inner_spend: Spend,
    /// A reveal of the TAIL, to issue or melt tokens or to spend a coin without a lineage proof.
    pub tail_spend: Option<TailSpend<NodePtr, NodePtr>>,
}

impl DogSpend {
    pub fn new(dog: Dog, inner_spend: Spend) -> Self {
        Self {
            dog,
            inner_spend,
            tail_spend: None,
        }
    }

    /// Runs the TAIL, and changes the amount of the execution coin by `delta`.
    pub fn with_tail(
        dog: Dog,
        inner_spend: Spend,
        tail: Spend,
        metadata_hash: Bytes32,
        delta: i128,
    ) -> Self {
        Self {
            dog,
            inner_spend,
            tail_spend: Some(TailSpend {
                tail: tail.puzzle,
                metadata_hash,
                delta,
                solution: tail.solution,
            }),
        }
    }

    /// The amount of the execution coin, after adding the delta from the TAIL.
    pub fn exec_amount(&self) -> Result<u64, DogError> {
        let delta = self.tail_spend.map_or(0, |tail_spend| tail_spend.delta);
        u64::try_from(i128::from(self.dog.amount) + delta).map_err(|_| DogError::AmountOutOfRange)
    }
}

/// Links to an asset's image and extra data, which are published when issuing it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct DogUris {
    pub image_uris: Vec<String>,
    pub data_uris: Vec<String>,
}

/// The result of issuing tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DogIssuance {
    /// The conditions the parent coin must output to create the eve DOG coin.
    pub parent_conditions: Conditions,
    /// The eve DOG coin, which is created and spent in the same transaction.
    pub eve: Dog,
    /// The DOG coins that the eve coin creates.
    pub children: Vec<Dog>,
}

impl Dog {
    pub fn new(
        parent_coin_id: Bytes32,
        asset_id: Bytes32,
        p2_puzzle_hash: Bytes32,
        amount: u64,
        lineage_proof: Option<LineageProof>,
    ) -> Self {
        let puzzle_hash = DogArgs::puzzle_hash(asset_id, p2_puzzle_hash, amount);

        Self {
            coin: Coin::new(parent_coin_id, puzzle_hash.into(), 0),
            amount,
            asset_id,
            p2_puzzle_hash,
            lineage_proof,
        }
    }

    /// The puzzle hash of the execution coin that this DOG coin creates when spent.
    pub fn exec_puzzle_hash(&self) -> TreeHash {
        DogExecArgs::puzzle_hash(self.asset_id, self.p2_puzzle_hash.into())
    }

    /// The execution coin that this DOG coin creates when spent, given its amount. It's the token
    /// amount, unless the TAIL is run to change it.
    pub fn exec_coin(&self, exec_amount: u64) -> Coin {
        Coin::new(
            self.coin.coin_id(),
            self.exec_puzzle_hash().into(),
            exec_amount,
        )
    }

    /// A DOG coin created by this one's execution coin, given the amount of the execution coin.
    #[must_use]
    pub fn child(&self, exec_amount: u64, p2_puzzle_hash: Bytes32, amount: u64) -> Self {
        Self::new(
            self.exec_coin(exec_amount).coin_id(),
            self.asset_id,
            p2_puzzle_hash,
            amount,
            Some(LineageProof {
                parent_parent_coin_info: self.coin.coin_id(),
                parent_inner_puzzle_hash: self.p2_puzzle_hash,
                parent_amount: exec_amount,
            }),
        )
    }

    /// Spends DOG coins of the same asset together, linking their execution coins in a ring.
    ///
    /// The total amount of the coins created by the inner puzzles must equal the total amount of
    /// the execution coins, which is the total token amount plus any deltas from the TAILs.
    ///
    /// Returns the DOG coins created by the inner puzzles.
    pub fn spend_all(ctx: &mut SpendContext, spends: &[DogSpend]) -> Result<Vec<Dog>, DogError> {
        let Some(first) = spends.first() else {
            return Ok(Vec::new());
        };

        let mut exec_amounts = Vec::with_capacity(spends.len());
        let mut prev_subtotals = Vec::with_capacity(spends.len());
        let mut subtotal: i128 = 0;
        let mut children = Vec::new();

        for spend in spends {
            if spend.dog.asset_id != first.dog.asset_id {
                return Err(DogError::AssetIdMismatch);
            }

            if ctx.tree_hash(spend.inner_spend.puzzle) != spend.dog.p2_puzzle_hash.into() {
                return Err(DogError::InnerPuzzleHashMismatch);
            }

            let exec_amount = spend.exec_amount()?;

            let output = ctx.run(spend.inner_spend.puzzle, spend.inner_spend.solution)?;

            let mut created: i128 = 0;

            for create_coin in created_coins(ctx, output)? {
                created += i128::from(create_coin.amount);
                children.push(spend.dog.child(
                    exec_amount,
                    create_coin.puzzle_hash,
                    create_coin.amount,
                ));
            }

            exec_amounts.push(exec_amount);
            prev_subtotals.push(subtotal);
            subtotal += i128::from(exec_amount) - created;
        }

        if subtotal != 0 {
            return Err(DogError::UnbalancedSpend(subtotal));
        }

        for (index, spend) in spends.iter().enumerate() {
            let prev_index = if index == 0 {
                spends.len() - 1
            } else {
                index - 1
            };
            let next_index = if index == spends.len() - 1 {
                0
            } else {
                index + 1
            };

            let prev = &spends[prev_index];
            let next = &spends[next_index];

            spend.dog.spend(
                ctx,
                *spend,
                exec_amounts[index],
                prev.dog.exec_coin(exec_amounts[prev_index]).coin_id(),
                prev_subtotals[index],
                CoinProof {
                    parent_coin_info: next.dog.coin.coin_id(),
                    inner_puzzle_hash: next.dog.p2_puzzle_hash,
                    amount: exec_amounts[next_index],
                },
            )?;
        }

        Ok(children)
    }

    /// Spends this DOG coin and its execution coin, given its position in the ring.
    ///
    /// Most callers should use [`Dog::spend_all`], which calculates the ring for them.
    pub fn spend(
        &self,
        ctx: &mut SpendContext,
        spend: DogSpend,
        exec_amount: u64,
        prev_coin_id: Bytes32,
        prev_subtotal: i128,
        next_coin_proof: CoinProof,
    ) -> Result<(), DogError> {
        let dog_puzzle = ctx.curry(DogArgs::new(
            self.asset_id,
            self.p2_puzzle_hash,
            self.amount,
        ))?;
        let dog_solution = ctx.alloc(&DogSolution {
            my_coin_id: self.coin.coin_id(),
            lineage_proof: self.lineage_proof,
            tail_spend: spend.tail_spend,
        })?;
        ctx.spend(self.coin, Spend::new(dog_puzzle, dog_solution))?;

        let exec_puzzle = ctx.curry(DogExecArgs::new(self.asset_id, spend.inner_spend.puzzle))?;
        let exec_solution = ctx.alloc(&DogExecSolution {
            inner_solution: spend.inner_spend.solution,
            my_amount: exec_amount,
            parent_amount: self.amount,
            prev_coin_id,
            prev_subtotal,
            next_coin_proof,
        })?;
        ctx.spend(
            self.exec_coin(exec_amount),
            Spend::new(exec_puzzle, exec_solution),
        )?;

        Ok(())
    }

    /// Issues new tokens of the asset defined by the TAIL and metadata.
    ///
    /// The parent coin creates an eve DOG coin with an amount of 0 mojos. It's spent in the same
    /// transaction, with an inner puzzle that outputs the given conditions and a `REMARK` that
    /// publishes the metadata. Those conditions should create coins worth `amount` in total.
    ///
    /// The TAIL is run with a supply change of `amount`, so it must authorize the issuance.
    pub fn issue(
        ctx: &mut SpendContext,
        parent_coin_id: Bytes32,
        tail: Spend,
        metadata: &DogMetadata,
        uris: &DogUris,
        amount: u64,
        conditions: Conditions,
    ) -> Result<DogIssuance, DogError> {
        let tail_hash = ctx.tree_hash(tail.puzzle);
        let metadata_hash = metadata.tree_hash();
        let asset_id = asset_id(tail_hash, metadata_hash);

        let remark = ctx.alloc(&DogMetadataRemark::new(
            tail_hash.into(),
            metadata.clone(),
            uris.image_uris.clone(),
            uris.data_uris.clone(),
        ))?;
        let inner_spend = ctx.delegated_spend(conditions.remark(remark))?;

        let eve = Dog::new(
            parent_coin_id,
            asset_id,
            ctx.tree_hash(inner_spend.puzzle).into(),
            amount,
            None,
        );

        let children = Self::spend_all(
            ctx,
            &[DogSpend::with_tail(
                eve,
                inner_spend,
                tail,
                metadata_hash.into(),
                0,
            )],
        )?;

        Ok(DogIssuance {
            parent_conditions: Conditions::new().create_coin(eve.coin.puzzle_hash, 0, Memos::None),
            eve,
            children,
        })
    }

    /// Issues tokens with a TAIL that only allows the parent coin to issue them, so that the
    /// supply can never change afterward.
    pub fn single_issuance(
        ctx: &mut SpendContext,
        parent_coin_id: Bytes32,
        metadata: &DogMetadata,
        uris: &DogUris,
        amount: u64,
        conditions: Conditions,
    ) -> Result<DogIssuance, DogError> {
        let tail = ctx.curry(GenesisByCoinIdTailArgs::new(parent_coin_id))?;

        Self::issue(
            ctx,
            parent_coin_id,
            Spend::new(tail, NodePtr::NIL),
            metadata,
            uris,
            amount,
            conditions,
        )
    }

    /// Issues tokens with a TAIL that allows the owner of the public key to issue or melt tokens
    /// at any time. Each supply change must be signed by the public key.
    pub fn multi_issuance(
        ctx: &mut SpendContext,
        parent_coin_id: Bytes32,
        public_key: PublicKey,
        metadata: &DogMetadata,
        uris: &DogUris,
        amount: u64,
        conditions: Conditions,
    ) -> Result<DogIssuance, DogError> {
        let tail = ctx.curry(EverythingWithSignatureTailArgs::new(public_key))?;

        Self::issue(
            ctx,
            parent_coin_id,
            Spend::new(tail, NodePtr::NIL),
            metadata,
            uris,
            amount,
            conditions,
        )
    }

    /// Parses the DOG coins created by the spend of an execution coin, which is the parent of
    /// every DOG coin that has a lineage proof.
    ///
    /// Returns `None` if the puzzle isn't an execution coin. Wallets can use this to find the
    /// amount, inner puzzle hash, and lineage proof of a DOG coin from its parent's spend.
    pub fn parse_children(
        allocator: &mut Allocator,
        parent_coin: Coin,
        parent_puzzle: NodePtr,
        parent_solution: NodePtr,
    ) -> Result<Option<Vec<Self>>, DogError> {
        let Some(curried) = Puzzle::parse(allocator, parent_puzzle).as_curried() else {
            return Ok(None);
        };

        if curried.mod_hash != DOG_EXEC_PUZZLE_HASH {
            return Ok(None);
        }

        let args = DogExecArgs::<NodePtr>::from_clvm(allocator, curried.args)?;

        if args.dog_mod_hash != DOG_PUZZLE_HASH.into()
            || args.exec_mod_hash != DOG_EXEC_PUZZLE_HASH.into()
        {
            return Err(DogError::InvalidModHash);
        }

        // Only the inner solution is parsed. The ring values aren't signed, so anyone could set them
        // to values that don't fit in the driver's types, such as a huge `prev_subtotal`, which
        // the puzzle would still accept.
        let (inner_solution, _ring) = <(NodePtr, NodePtr)>::from_clvm(allocator, parent_solution)?;
        let output = run_puzzle(allocator, args.inner_puzzle, inner_solution)?;

        let lineage_proof = LineageProof {
            parent_parent_coin_info: parent_coin.parent_coin_info,
            parent_inner_puzzle_hash: tree_hash(allocator, args.inner_puzzle).into(),
            parent_amount: parent_coin.amount,
        };

        let children = created_coins(allocator, output)?
            .into_iter()
            .map(
                |CreateCoin {
                     puzzle_hash,
                     amount,
                     ..
                 }| {
                    Self::new(
                        parent_coin.coin_id(),
                        args.asset_id,
                        puzzle_hash,
                        amount,
                        Some(lineage_proof),
                    )
                },
            )
            .collect();

        Ok(Some(children))
    }

    /// Parses the curried arguments of a DOG coin puzzle, or returns `None` if it isn't one.
    pub fn parse_puzzle(
        allocator: &Allocator,
        puzzle: NodePtr,
    ) -> Result<Option<DogArgs>, DogError> {
        let Some(curried) = Puzzle::parse(allocator, puzzle).as_curried() else {
            return Ok(None);
        };

        if curried.mod_hash != DOG_PUZZLE_HASH {
            return Ok(None);
        }

        let args = DogArgs::from_clvm(allocator, curried.args)?;

        if args.dog_mod_hash != DOG_PUZZLE_HASH.into()
            || args.exec_mod_hash != DOG_EXEC_PUZZLE_HASH.into()
        {
            return Err(DogError::InvalidModHash);
        }

        Ok(Some(args))
    }
}

/// The `CREATE_COIN` conditions output by an inner puzzle, which the execution coin wraps.
///
/// The execution coin wraps every condition with the `CREATE_COIN` opcode, even if consensus
/// wouldn't accept it as one (such as one with a puzzle hash that isn't 32 bytes). Those can't be
/// represented here, so they're rejected rather than skipped, which would get the total wrong.
fn created_coins(
    allocator: &Allocator,
    output: NodePtr,
) -> Result<Vec<CreateCoin<NodePtr>>, DogError> {
    let mut created_coins = Vec::new();

    for condition in Vec::<NodePtr>::from_clvm(allocator, output)? {
        if let Ok(create_coin) = CreateCoin::<NodePtr>::from_clvm(allocator, condition) {
            created_coins.push(create_coin);
            continue;
        }

        let is_create_coin =
            <(NodePtr, NodePtr)>::from_clvm(allocator, condition).is_ok_and(|(opcode, _)| {
                matches!(allocator.sexp(opcode), SExp::Atom)
                    && allocator.atom(opcode).as_ref() == [51]
            });

        if is_create_coin {
            return Err(DogError::UnsupportedCreateCoin);
        }
    }

    Ok(created_coins)
}
