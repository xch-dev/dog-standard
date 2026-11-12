use chia_wallet_sdk::{
    chia::{
        bls::PublicKey,
        protocol::{Bytes32, Coin},
        puzzle_types::{LineageProof, Memos},
    },
    clvm_traits::FromClvm,
    clvm_utils::{ToTreeHash, TreeHash, tree_hash},
    clvmr::{Allocator, NodePtr, SExp},
    driver::{Layer, Puzzle, RevocationLayer, Spend, SpendContext},
    types::{
        Conditions, Mod,
        conditions::CreateCoin,
        puzzles::{RevocationArgs, RevocationSolution},
        run_puzzle,
    },
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
    /// The hash of the puzzle that controls the tokens, which is the owner's address. For revocable
    /// DOGs, it's wrapped in the [`RevocationLayer`] to get the inner puzzle hash of the DOG coin.
    pub p2_puzzle_hash: Bytes32,
    /// Proof that the parent is an execution coin of the same asset. If this is `None`, the TAIL
    /// must be revealed to spend the coin.
    pub lineage_proof: Option<LineageProof>,
    /// The hash of the hidden puzzle, if this is a revocable DOG. The p2 puzzle is then wrapped in
    /// the Wallet SDK's [`RevocationLayer`] (the same one revocable CATs use), so the hidden puzzle
    /// (usually controlled by the issuer) can spend the coin without the owner's permission.
    pub hidden_puzzle_hash: Option<Bytes32>,
}

/// The inner puzzle hash of a DOG coin, which is the p2 puzzle hash wrapped in the
/// [`RevocationLayer`] if there's a hidden puzzle hash.
///
/// This is what's curried into the DOG and execution coin puzzles. For example, the puzzle hash of
/// the execution coin of a revocable DOG held by the settlement payments puzzle (which an offer
/// asserts a payment from) is `DogExecArgs::puzzle_hash(asset_id, inner_puzzle_hash(hidden,
/// SETTLEMENT_PAYMENT_HASH))`.
pub fn inner_puzzle_hash(hidden_puzzle_hash: Option<Bytes32>, p2_puzzle_hash: Bytes32) -> TreeHash {
    match hidden_puzzle_hash {
        Some(hidden_puzzle_hash) => {
            RevocationArgs::new(hidden_puzzle_hash, p2_puzzle_hash).curry_tree_hash()
        }
        None => p2_puzzle_hash.into(),
    }
}

/// A spend of a [`Dog`], which is combined with others in [`Dog::spend_all`].
#[derive(Debug, Clone, Copy)]
pub struct DogSpend {
    pub dog: Dog,
    /// The spend of the p2 puzzle, which must have the tree hash [`Dog::p2_puzzle_hash`], or of
    /// the hidden puzzle if [`hidden`](Self::hidden) is set. For revocable DOGs, the driver wraps
    /// it in the [`RevocationLayer`].
    pub inner_spend: Spend,
    /// A reveal of the TAIL, to issue or melt tokens or to spend a coin without a lineage proof.
    pub tail_spend: Option<TailSpend<NodePtr, NodePtr>>,
    /// Whether to spend the hidden puzzle of a revocable DOG instead of the p2 puzzle.
    pub hidden: bool,
}

impl DogSpend {
    pub fn new(dog: Dog, inner_spend: Spend) -> Self {
        Self {
            dog,
            inner_spend,
            tail_spend: None,
            hidden: false,
        }
    }

    /// A revocation spend of a revocable DOG, where `hidden_spend` is a spend of the hidden puzzle
    /// whose hash is [`Dog::hidden_puzzle_hash`]. The owner's p2 puzzle isn't run.
    ///
    /// The coins created by the hidden puzzle aren't wrapped in the revocation layer, unless the
    /// hidden puzzle wraps their puzzle hashes itself.
    pub fn revoke(dog: Dog, hidden_spend: Spend) -> Self {
        Self {
            dog,
            inner_spend: hidden_spend,
            tail_spend: None,
            hidden: true,
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
            hidden: false,
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
        Self::new_with_hidden_puzzle_hash(
            parent_coin_id,
            asset_id,
            None,
            p2_puzzle_hash,
            amount,
            lineage_proof,
        )
    }

    /// Creates a DOG, which is revocable if `hidden_puzzle_hash` is set.
    pub fn new_with_hidden_puzzle_hash(
        parent_coin_id: Bytes32,
        asset_id: Bytes32,
        hidden_puzzle_hash: Option<Bytes32>,
        p2_puzzle_hash: Bytes32,
        amount: u64,
        lineage_proof: Option<LineageProof>,
    ) -> Self {
        let inner_puzzle_hash = inner_puzzle_hash(hidden_puzzle_hash, p2_puzzle_hash);
        let puzzle_hash = DogArgs::puzzle_hash(asset_id, inner_puzzle_hash.into(), amount);

        Self {
            coin: Coin::new(parent_coin_id, puzzle_hash.into(), 0),
            amount,
            asset_id,
            p2_puzzle_hash,
            lineage_proof,
            hidden_puzzle_hash,
        }
    }

    /// The hash of the inner puzzle that's curried into the DOG coin and execution coin. It's the
    /// p2 puzzle hash, wrapped in the [`RevocationLayer`] if the DOG is revocable.
    pub fn inner_puzzle_hash(&self) -> TreeHash {
        inner_puzzle_hash(self.hidden_puzzle_hash, self.p2_puzzle_hash)
    }

    /// The puzzle hash of the execution coin that this DOG coin creates when spent.
    pub fn exec_puzzle_hash(&self) -> TreeHash {
        DogExecArgs::puzzle_hash(self.asset_id, self.inner_puzzle_hash())
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
    ///
    /// The child has the same revocation layer (or lack thereof) as this one, which is the case
    /// for every coin created by the p2 puzzle. See [`Dog::unrevocable_child`] for coins created
    /// by the hidden puzzle.
    #[must_use]
    pub fn child(&self, exec_amount: u64, p2_puzzle_hash: Bytes32, amount: u64) -> Self {
        self.child_with(exec_amount, self.hidden_puzzle_hash, p2_puzzle_hash, amount)
    }

    /// A DOG coin without a revocation layer, created by this one's execution coin. This is what
    /// the hidden puzzle of a revocable DOG creates, unless it wraps the puzzle hash itself.
    #[must_use]
    pub fn unrevocable_child(
        &self,
        exec_amount: u64,
        p2_puzzle_hash: Bytes32,
        amount: u64,
    ) -> Self {
        self.child_with(exec_amount, None, p2_puzzle_hash, amount)
    }

    /// A DOG coin created by this one's execution coin, with the given hidden puzzle hash.
    #[must_use]
    pub fn child_with(
        &self,
        exec_amount: u64,
        hidden_puzzle_hash: Option<Bytes32>,
        p2_puzzle_hash: Bytes32,
        amount: u64,
    ) -> Self {
        self.exec_parent(exec_amount)
            .child(hidden_puzzle_hash, p2_puzzle_hash, amount)
    }

    fn exec_parent(&self, exec_amount: u64) -> ExecParent {
        ExecParent {
            dog_coin_id: self.coin.coin_id(),
            asset_id: self.asset_id,
            hidden_puzzle_hash: self.hidden_puzzle_hash,
            inner_puzzle_hash: self.inner_puzzle_hash().into(),
            exec_amount,
        }
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

            let expected_puzzle_hash = if spend.hidden {
                spend.dog.hidden_puzzle_hash.ok_or(DogError::NotRevocable)?
            } else {
                spend.dog.p2_puzzle_hash
            };

            if ctx.tree_hash(spend.inner_spend.puzzle) != expected_puzzle_hash.into() {
                return Err(DogError::InnerPuzzleHashMismatch);
            }

            let exec_amount = spend.exec_amount()?;

            let output = ctx.run(spend.inner_spend.puzzle, spend.inner_spend.solution)?;

            let mut created: i128 = 0;

            for create_coin in created_coins(ctx, output)? {
                created += i128::from(create_coin.amount);
                children.push(child_from_p2_create_coin(
                    ctx,
                    &spend.dog.exec_parent(exec_amount),
                    create_coin,
                    spend.hidden,
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
            let next = &spends[(index + 1) % spends.len()];

            spend.dog.spend(
                ctx,
                *spend,
                exec_amounts[index],
                prev_subtotals[index],
                next.dog.inner_puzzle_hash().into(),
            )?;
        }

        Ok(children)
    }

    /// Spends this DOG coin and its execution coin, given its position in the ring.
    ///
    /// The execution coin receives `prev_subtotal` from the previous execution coin in the ring,
    /// and sends its own subtotal to the next one, which is identified by its inner puzzle hash.
    ///
    /// Most callers should use [`Dog::spend_all`], which calculates the ring for them.
    pub fn spend(
        &self,
        ctx: &mut SpendContext,
        spend: DogSpend,
        exec_amount: u64,
        prev_subtotal: i128,
        next_inner_puzzle_hash: Bytes32,
    ) -> Result<(), DogError> {
        let dog_puzzle = ctx.curry(DogArgs::new(
            self.asset_id,
            self.inner_puzzle_hash().into(),
            self.amount,
        ))?;
        let dog_solution = ctx.alloc(&DogSolution {
            my_coin_id: self.coin.coin_id(),
            lineage_proof: self.lineage_proof,
            tail_spend: spend.tail_spend,
        })?;
        ctx.spend(self.coin, Spend::new(dog_puzzle, dog_solution))?;

        let inner_spend = match self.hidden_puzzle_hash {
            Some(hidden_puzzle_hash) => {
                RevocationLayer::new(hidden_puzzle_hash, self.p2_puzzle_hash).construct_spend(
                    ctx,
                    RevocationSolution::new(
                        spend.hidden,
                        spend.inner_spend.puzzle,
                        spend.inner_spend.solution,
                    ),
                )?
            }
            None if spend.hidden => return Err(DogError::NotRevocable),
            None => spend.inner_spend,
        };

        let exec_puzzle = ctx.curry(DogExecArgs::new(self.asset_id, inner_spend.puzzle))?;
        let exec_solution = ctx.alloc(&DogExecSolution {
            inner_solution: inner_spend.solution,
            my_amount: exec_amount,
            parent_amount: self.amount,
            prev_subtotal,
            next_inner_puzzle_hash,
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
        Self::issue_with_hidden_puzzle_hash(
            ctx,
            parent_coin_id,
            tail,
            None,
            metadata,
            uris,
            amount,
            conditions,
        )
    }

    /// Like [`Dog::issue`], but the eve coin is wrapped in the [`RevocationLayer`] if
    /// `hidden_puzzle_hash` is set, so every coin it creates is a revocable DOG.
    ///
    /// Revocability isn't part of the asset id, which only depends on the TAIL and metadata. Use a
    /// TAIL that only allows issuance by the issuer, so that every coin of the asset is revocable.
    #[allow(clippy::too_many_arguments)]
    pub fn issue_with_hidden_puzzle_hash(
        ctx: &mut SpendContext,
        parent_coin_id: Bytes32,
        tail: Spend,
        hidden_puzzle_hash: Option<Bytes32>,
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

        let eve = Dog::new_with_hidden_puzzle_hash(
            parent_coin_id,
            asset_id,
            hidden_puzzle_hash,
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

    /// Issues revocable tokens with a TAIL that only allows the parent coin to issue them. The
    /// hidden puzzle can revoke any coin of the asset at any time.
    pub fn single_issuance_revocable(
        ctx: &mut SpendContext,
        parent_coin_id: Bytes32,
        hidden_puzzle_hash: Bytes32,
        metadata: &DogMetadata,
        uris: &DogUris,
        amount: u64,
        conditions: Conditions,
    ) -> Result<DogIssuance, DogError> {
        let tail = ctx.curry(GenesisByCoinIdTailArgs::new(parent_coin_id))?;

        Self::issue_with_hidden_puzzle_hash(
            ctx,
            parent_coin_id,
            Spend::new(tail, NodePtr::NIL),
            Some(hidden_puzzle_hash),
            metadata,
            uris,
            amount,
            conditions,
        )
    }

    /// Issues revocable tokens with a TAIL that allows the owner of the public key to issue or
    /// melt tokens at any time. The hidden puzzle can revoke any coin of the asset at any time.
    #[allow(clippy::too_many_arguments)]
    pub fn multi_issuance_revocable(
        ctx: &mut SpendContext,
        parent_coin_id: Bytes32,
        public_key: PublicKey,
        hidden_puzzle_hash: Bytes32,
        metadata: &DogMetadata,
        uris: &DogUris,
        amount: u64,
        conditions: Conditions,
    ) -> Result<DogIssuance, DogError> {
        let tail = ctx.curry(EverythingWithSignatureTailArgs::new(public_key))?;

        Self::issue_with_hidden_puzzle_hash(
            ctx,
            parent_coin_id,
            Spend::new(tail, NodePtr::NIL),
            Some(hidden_puzzle_hash),
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

        if args.asset.exec_mod_hash != DOG_EXEC_PUZZLE_HASH.into() {
            return Err(DogError::InvalidModHash);
        }

        // Only the inner solution is parsed. The ring values aren't signed, so anyone could set them
        // to values that don't fit in the driver's types, such as a huge `prev_subtotal`, which
        // the puzzle would still accept.
        let (inner_solution, _ring) = <(NodePtr, NodePtr)>::from_clvm(allocator, parent_solution)?;

        let mut parent = ExecParent {
            dog_coin_id: parent_coin.parent_coin_info,
            asset_id: args.asset.asset_id,
            hidden_puzzle_hash: None,
            inner_puzzle_hash: tree_hash(allocator, args.inner_puzzle).into(),
            exec_amount: parent_coin.amount,
        };

        // For revocable DOGs, run the p2 or hidden puzzle inside the revocation layer, so that the
        // children's p2 puzzle hashes are known.
        let mut inner_spend = Spend::new(args.inner_puzzle, inner_solution);
        let mut hidden = false;

        if let Some(revocation_layer) =
            RevocationLayer::parse_puzzle(allocator, Puzzle::parse(allocator, args.inner_puzzle))?
        {
            let solution = RevocationLayer::parse_solution(allocator, inner_solution)?;
            parent.hidden_puzzle_hash = Some(revocation_layer.hidden_puzzle_hash);
            inner_spend = Spend::new(solution.puzzle, solution.solution);
            hidden = solution.hidden;
        }

        let output = run_puzzle(allocator, inner_spend.puzzle, inner_spend.solution)?;

        let children = created_coins(allocator, output)?
            .into_iter()
            .map(|create_coin| child_from_p2_create_coin(allocator, &parent, create_coin, hidden))
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

        if args.asset.exec_mod_hash != DOG_EXEC_PUZZLE_HASH.into() {
            return Err(DogError::InvalidModHash);
        }

        Ok(Some(args))
    }
}

/// The execution coin of a DOG coin, which is the parent of the DOG coins it creates.
#[derive(Debug, Clone, Copy)]
struct ExecParent {
    dog_coin_id: Bytes32,
    asset_id: Bytes32,
    hidden_puzzle_hash: Option<Bytes32>,
    /// The inner puzzle hash, which includes the revocation layer if there is one.
    inner_puzzle_hash: Bytes32,
    exec_amount: u64,
}

impl ExecParent {
    fn child(
        &self,
        hidden_puzzle_hash: Option<Bytes32>,
        p2_puzzle_hash: Bytes32,
        amount: u64,
    ) -> Dog {
        let exec_puzzle_hash =
            DogExecArgs::puzzle_hash(self.asset_id, self.inner_puzzle_hash.into());
        let exec_coin = Coin::new(self.dog_coin_id, exec_puzzle_hash.into(), self.exec_amount);

        Dog::new_with_hidden_puzzle_hash(
            exec_coin.coin_id(),
            self.asset_id,
            hidden_puzzle_hash,
            p2_puzzle_hash,
            amount,
            Some(LineageProof {
                parent_parent_coin_info: self.dog_coin_id,
                parent_inner_puzzle_hash: self.inner_puzzle_hash,
                parent_amount: self.exec_amount,
            }),
        )
    }
}

/// The DOG coin created by a `CREATE_COIN` condition from the p2 or hidden puzzle of a DOG.
///
/// Like [`Cat::child_from_p2_create_coin`](chia_wallet_sdk::driver::Cat::child_from_p2_create_coin):
/// - Coins created by the p2 puzzle are wrapped in the parent's revocation layer, if any.
/// - Coins created by the hidden puzzle aren't wrapped, unless the hint wrapped in the parent's
///   revocation layer is the puzzle hash, in which case the hidden puzzle wrapped it itself.
fn child_from_p2_create_coin(
    allocator: &Allocator,
    parent: &ExecParent,
    create_coin: CreateCoin<NodePtr>,
    hidden: bool,
) -> Dog {
    let CreateCoin {
        puzzle_hash,
        amount,
        memos,
    } = create_coin;

    let Some(hidden_puzzle_hash) = parent.hidden_puzzle_hash else {
        return parent.child(None, puzzle_hash, amount);
    };

    if !hidden {
        return parent.child(Some(hidden_puzzle_hash), puzzle_hash, amount);
    }

    if let Memos::Some(memos) = memos
        && let Ok((hint, _)) = <(Bytes32, NodePtr)>::from_clvm(allocator, memos)
        && puzzle_hash == inner_puzzle_hash(Some(hidden_puzzle_hash), hint).into()
    {
        return parent.child(Some(hidden_puzzle_hash), hint, amount);
    }

    parent.child(None, puzzle_hash, amount)
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
