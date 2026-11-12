//! The additional TAILs in `puzzles/tails/`: issuing and melting with each of them, and the
//! supply changes they must reject.

mod common;

use std::borrow::Cow;

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{
        bls::SecretKey,
        consensus::validation_error::ErrorCode,
        protocol::{Bytes32, Coin, SpendBundle},
        puzzle_types::{
            EveProof, LineageProof, Memos, Proof,
            singleton::{SingletonArgs, SingletonSolution},
        },
    },
    clvm_traits::{FromClvm, ToClvm},
    clvm_utils::{ToTreeHash, TreeHash},
    clvmr::NodePtr,
    driver::{Launcher, Spend, SpendContext, StandardLayer},
    test::{BlsPairWithCoin, Simulator, SimulatorError, sign_transaction},
    types::{Conditions, Mod},
};
use common::*;
use dog::{
    BurnableTailArgs, CappedSupplyTailArgs, CappedSupplyTailSolution, DelegatedTailArgs,
    DelegatedTailSolution, Dog, DogSpend, EverythingWithSignatureTailArgs,
    EverythingWithSingletonTailArgs, EverythingWithSingletonTailSolution, GenesisByCoinIdTailArgs,
    IssuanceDeadlineTailArgs, MultisigTailArgs, MultisigTailSolution, SINGLETON_AUTHORIZATION_MODE,
    SUPPLY_COUNTER_PUZZLE, SUPPLY_COUNTER_PUZZLE_HASH, SupplyCounterArgs, authorization_message,
};

fn metadata_hash() -> Bytes32 {
    metadata().tree_hash().into()
}

/// The eve DOG coin of an issuance, which the TAIL must authorize in the same transaction, and the
/// coin it creates.
#[derive(Debug, Clone, Copy)]
struct Issued {
    eve: Dog,
    dog: Dog,
}

/// Adds the spends to issue `amount` tokens to `owner` with the TAIL, from the parent coin, without
/// submitting them, so that a test can add the spends that authorize it.
fn issue_with(
    ctx: &mut SpendContext,
    parent: &BlsPairWithCoin,
    tail: Spend,
    owner: Bytes32,
    amount: u64,
) -> Result<Issued> {
    let conditions = send(ctx, owner, amount)?;
    let issuance = Dog::issue(
        ctx,
        parent.coin.coin_id(),
        tail,
        &metadata(),
        &DogUris::default(),
        amount,
        conditions,
    )?;
    StandardLayer::new(parent.pk).spend(ctx, parent.coin, issuance.parent_conditions)?;
    Ok(Issued {
        eve: issuance.eve,
        dog: issuance.children[0],
    })
}

/// Adds the spend of a DOG coin owned by `owner` that changes its amount by `delta` with the TAIL,
/// and sends the result back to the owner. Returns the new coin, if there is one.
fn change_supply(
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    dog: Dog,
    tail: Spend,
    delta: i128,
) -> Result<Option<Dog>> {
    let new_amount = u64::try_from(i128::from(dog.amount) + delta)?;
    let conditions = if new_amount == 0 {
        Conditions::new()
    } else {
        send(ctx, owner.puzzle_hash, new_amount)?
    };
    let inner_spend = p2(ctx, owner, conditions)?;
    let spend = DogSpend::with_tail(dog, inner_spend, tail, metadata_hash(), delta);
    Ok(Dog::spend_all(ctx, &[spend])?.first().copied())
}

/// A TAIL with no solution.
fn tail_without_solution(
    ctx: &mut SpendContext,
    args: impl Mod + ToClvm<chia_wallet_sdk::clvmr::Allocator>,
) -> Result<Spend> {
    Ok(Spend::new(ctx.curry(args)?, NodePtr::NIL))
}

/// A singleton whose inner puzzle is controlled by the test, spent with the singleton top layer
/// v1.1.
#[derive(Debug, Clone, Copy)]
struct TestSingleton {
    launcher_id: Bytes32,
    coin: Coin,
    proof: Proof,
    inner_puzzle_hash: Bytes32,
}

impl TestSingleton {
    /// Launches a singleton with the inner puzzle hash from a new coin.
    fn launch(
        sim: &mut Simulator,
        ctx: &mut SpendContext,
        inner_puzzle_hash: TreeHash,
    ) -> Result<Self> {
        let parent = sim.bls(1);
        let launcher = Launcher::new(parent.coin.coin_id(), 1);
        let launcher_id = launcher.coin().coin_id();
        let (conditions, coin) = launcher.spend(ctx, inner_puzzle_hash.into(), ())?;
        StandardLayer::new(parent.pk).spend(ctx, parent.coin, conditions)?;
        submit(sim, ctx, &[parent.sk])?;

        Ok(Self {
            launcher_id,
            coin,
            proof: Proof::Eve(EveProof {
                parent_parent_coin_info: parent.coin.coin_id(),
                parent_amount: 1,
            }),
            inner_puzzle_hash: inner_puzzle_hash.into(),
        })
    }

    /// Adds the spend of the singleton with the inner spend, which must recreate it with the next
    /// inner puzzle hash, and returns the child.
    fn spend(
        self,
        ctx: &mut SpendContext,
        inner_spend: Spend,
        next_inner_puzzle_hash: TreeHash,
    ) -> Result<Self> {
        assert_eq!(
            ctx.tree_hash(inner_spend.puzzle),
            self.inner_puzzle_hash.into()
        );

        let puzzle = ctx.curry(SingletonArgs::new(self.launcher_id, inner_spend.puzzle))?;
        let solution = ctx.alloc(&SingletonSolution {
            lineage_proof: self.proof,
            amount: self.coin.amount,
            inner_solution: inner_spend.solution,
        })?;
        ctx.spend(self.coin, Spend::new(puzzle, solution))?;

        let puzzle_hash = SingletonArgs::curry_tree_hash(self.launcher_id, next_inner_puzzle_hash);

        Ok(Self {
            launcher_id: self.launcher_id,
            coin: Coin::new(self.coin.coin_id(), puzzle_hash.into(), self.coin.amount),
            proof: Proof::Lineage(LineageProof {
                parent_parent_coin_info: self.coin.parent_coin_info,
                parent_inner_puzzle_hash: self.inner_puzzle_hash,
                parent_amount: self.coin.amount,
            }),
            inner_puzzle_hash: next_inner_puzzle_hash.into(),
        })
    }
}

/// Adds the condition with which a singleton authorizes a supply change for a DOG coin.
fn authorize(
    ctx: &mut SpendContext,
    conditions: Conditions,
    dog: &Dog,
    supply_change: i128,
) -> Result<Conditions> {
    let receiver = ctx.alloc(&dog.coin.coin_id())?;

    // Without a lineage proof, none of the coin's amount is proven, so the new amount is the
    // whole supply change.
    let new_amount = if dog.lineage_proof.is_some() {
        i128::from(dog.amount) + supply_change
    } else {
        supply_change
    };

    Ok(conditions.send_message(
        SINGLETON_AUTHORIZATION_MODE,
        authorization_message(supply_change, u64::try_from(new_amount)?).into(),
        vec![receiver],
    ))
}

mod everything_with_singleton {
    use super::*;

    /// A singleton owned by the issuer's standard puzzle, and the TAIL it controls.
    struct Setup {
        sim: Simulator,
        ctx: SpendContext,
        issuer: BlsPairWithCoin,
        alice: BlsPairWithCoin,
        singleton: TestSingleton,
        tail: NodePtr,
    }

    impl Setup {
        fn new() -> Result<Self> {
            let mut sim = Simulator::new();
            let mut ctx = SpendContext::new();
            let issuer = sim.bls(0);
            let alice = sim.bls(0);
            let singleton = TestSingleton::launch(&mut sim, &mut ctx, issuer.puzzle_hash.into())?;
            let tail = ctx.curry(EverythingWithSingletonTailArgs::new(singleton.launcher_id))?;
            Ok(Self {
                sim,
                ctx,
                issuer,
                alice,
                singleton,
                tail,
            })
        }

        /// The TAIL spend, with the singleton's current inner puzzle hash.
        fn tail_spend(&mut self) -> Result<Spend> {
            let solution = self.ctx.alloc(&EverythingWithSingletonTailSolution {
                singleton_inner_puzzle_hash: self.singleton.inner_puzzle_hash,
            })?;
            Ok(Spend::new(self.tail, solution))
        }

        /// Adds a spend of the singleton that authorizes the supply changes.
        fn authorize(&mut self, authorizations: &[(&Dog, i128)]) -> Result<()> {
            let mut conditions =
                Conditions::new().create_coin(self.issuer.puzzle_hash, 1, Memos::None);
            for &(dog, supply_change) in authorizations {
                conditions = authorize(&mut self.ctx, conditions, dog, supply_change)?;
            }
            let inner_spend = p2(&mut self.ctx, &self.issuer, conditions)?;
            self.singleton =
                self.singleton
                    .spend(&mut self.ctx, inner_spend, self.issuer.puzzle_hash.into())?;
            Ok(())
        }

        /// Adds the spends to issue tokens to Alice, without authorizing it.
        fn issue_unauthorized(&mut self, amount: u64) -> Result<(Issued, SecretKey)> {
            let parent = self.sim.bls(0);
            let tail = self.tail_spend()?;
            let issuance =
                issue_with(&mut self.ctx, &parent, tail, self.alice.puzzle_hash, amount)?;
            Ok((issuance, parent.sk))
        }

        fn submit(&mut self, extra: &[SecretKey]) -> Result<u64> {
            let keys = self.keys(extra);
            submit(&mut self.sim, &mut self.ctx, &keys)
        }

        fn submit_err(&mut self, extra: &[SecretKey]) -> ErrorCode {
            let keys = self.keys(extra);
            submit_err(&mut self.sim, &mut self.ctx, &keys)
        }

        fn keys(&self, extra: &[SecretKey]) -> Vec<SecretKey> {
            let mut keys = vec![self.issuer.sk.clone(), self.alice.sk.clone()];
            keys.extend_from_slice(extra);
            keys
        }
    }

    #[test]
    fn issue_and_melt() -> Result<()> {
        let mut s = Setup::new()?;

        let (issuance, parent_sk) = s.issue_unauthorized(1000)?;
        s.authorize(&[(&issuance.eve, 1000)])?;
        s.submit(&[parent_sk])?;
        let dog = issuance.dog;
        assert_eq!(dog.amount, 1000);
        assert_unspent(&s.sim, &dog);

        // Issue 500 more into Alice's coin.
        s.authorize(&[(&dog, 500)])?;
        let tail = s.tail_spend()?;
        let dog = change_supply(&mut s.ctx, &s.alice, dog, tail, 500)?.expect("a new coin");
        s.submit(&[])?;
        assert_eq!(dog.amount, 1500);
        assert_unspent(&s.sim, &dog);

        // Melt 1000, then the rest. The singleton's inner puzzle hash is the same each time, but
        // its coin isn't.
        s.authorize(&[(&dog, -1000)])?;
        let tail = s.tail_spend()?;
        let dog = change_supply(&mut s.ctx, &s.alice, dog, tail, -1000)?.expect("a new coin");
        s.submit(&[])?;
        assert_eq!(dog.amount, 500);

        s.authorize(&[(&dog, -500)])?;
        let tail = s.tail_spend()?;
        assert!(change_supply(&mut s.ctx, &s.alice, dog, tail, -500)?.is_none());
        s.submit(&[])?;

        Ok(())
    }

    #[test]
    fn one_spend_can_authorize_several_coins() -> Result<()> {
        let mut s = Setup::new()?;

        let (first, first_sk) = s.issue_unauthorized(1000)?;
        let (second, second_sk) = s.issue_unauthorized(2000)?;
        s.authorize(&[(&first.eve, 1000), (&second.eve, 2000)])?;
        s.submit(&[first_sk, second_sk])?;
        assert_unspent(&s.sim, &first.dog);
        assert_unspent(&s.sim, &second.dog);

        Ok(())
    }

    #[test]
    fn requires_the_singleton() -> Result<()> {
        let mut s = Setup::new()?;

        let (_, parent_sk) = s.issue_unauthorized(1000)?;
        assert_eq!(
            s.submit_err(&[parent_sk]),
            ErrorCode::MessageNotSentOrReceived
        );

        Ok(())
    }

    #[test]
    fn authorization_is_for_one_amount() -> Result<()> {
        let mut s = Setup::new()?;
        let singleton = s.singleton;

        // The singleton authorizes 1000, but the eve coin issues 1001.
        let (issuance, parent_sk) = s.issue_unauthorized(1001)?;
        s.authorize(&[(&issuance.eve, 1000)])?;
        assert_eq!(
            s.submit_err(&[parent_sk]),
            ErrorCode::MessageNotSentOrReceived
        );

        // An issuance can't be used to melt the same amount, or the other way around.
        s.singleton = singleton;
        let (issuance, parent_sk) = s.issue_unauthorized(1000)?;
        s.authorize(&[(&issuance.eve, 1000)])?;
        s.submit(&[parent_sk])?;

        s.authorize(&[(&issuance.dog, 100)])?;
        let tail = s.tail_spend()?;
        change_supply(&mut s.ctx, &s.alice, issuance.dog, tail, -100)?;
        assert_eq!(s.submit_err(&[]), ErrorCode::MessageNotSentOrReceived);

        Ok(())
    }

    #[test]
    fn authorization_is_for_one_coin() -> Result<()> {
        let mut s = Setup::new()?;
        let singleton = s.singleton;

        // The singleton authorizes the first eve coin, but the second one tries to use it too.
        // Each message is received once, so the bundle is rejected.
        let (first, first_sk) = s.issue_unauthorized(1000)?;
        let (_, second_sk) = s.issue_unauthorized(1000)?;
        s.authorize(&[(&first.eve, 1000)])?;
        assert_eq!(
            s.submit_err(&[first_sk, second_sk]),
            ErrorCode::MessageNotSentOrReceived
        );

        // An authorization for another coin can't be used either.
        s.singleton = singleton;
        let (_, parent_sk) = s.issue_unauthorized(1000)?;
        s.authorize(&[(&first.eve, 1000)])?;
        assert_eq!(
            s.submit_err(&[parent_sk]),
            ErrorCode::MessageNotSentOrReceived
        );

        Ok(())
    }

    #[test]
    fn other_singletons_cant_authorize() -> Result<()> {
        let mut s = Setup::new()?;

        // Another singleton with the same inner puzzle sends the same message.
        let other = TestSingleton::launch(&mut s.sim, &mut s.ctx, s.issuer.puzzle_hash.into())?;
        let (issuance, parent_sk) = s.issue_unauthorized(1000)?;
        s.singleton = other;
        s.authorize(&[(&issuance.eve, 1000)])?;
        assert_eq!(
            s.submit_err(&[parent_sk]),
            ErrorCode::MessageNotSentOrReceived
        );

        Ok(())
    }
}

mod capped_supply {
    use super::*;

    const CAP: u64 = 1000;

    /// A supply counter singleton owned by the issuer's standard puzzle, and the TAIL it controls.
    struct Setup {
        sim: Simulator,
        ctx: SpendContext,
        issuer: BlsPairWithCoin,
        alice: BlsPairWithCoin,
        singleton: TestSingleton,
        issued: u64,
        owner_puzzle_hash: Bytes32,
        tail: NodePtr,
    }

    impl Setup {
        fn new() -> Result<Self> {
            let mut sim = Simulator::new();
            let mut ctx = SpendContext::new();
            let issuer = sim.bls(0);
            let alice = sim.bls(0);
            let singleton = TestSingleton::launch(
                &mut sim,
                &mut ctx,
                SupplyCounterArgs::puzzle_hash(CAP, 0, issuer.puzzle_hash),
            )?;
            let tail = ctx.curry(CappedSupplyTailArgs::new(singleton.launcher_id, CAP))?;
            Ok(Self {
                sim,
                ctx,
                owner_puzzle_hash: issuer.puzzle_hash,
                issuer,
                alice,
                singleton,
                issued: 0,
                tail,
            })
        }

        /// The TAIL spend, with the counter's current state.
        fn tail_spend(&mut self) -> Result<Spend> {
            let solution = self.ctx.alloc(&CappedSupplyTailSolution {
                issued: self.issued,
                owner_puzzle_hash: self.owner_puzzle_hash,
            })?;
            Ok(Spend::new(self.tail, solution))
        }

        /// Adds a spend of the counter singleton that authorizes the supply changes, and passes
        /// it to the new owner.
        fn authorize_to(
            &mut self,
            owner: &BlsPairWithCoin,
            new_owner: Bytes32,
            authorizations: &[(&Dog, i128)],
        ) -> Result<()> {
            let mut conditions = Conditions::new().create_coin(new_owner, 1, Memos::None);
            let mut issued = self.issued;
            for &(dog, supply_change) in authorizations {
                conditions = authorize(&mut self.ctx, conditions, dog, supply_change)?;
                if supply_change > 0 {
                    // Saturate, so that tests can try to go over the cap.
                    issued = issued.saturating_add(u64::try_from(supply_change)?);
                }
            }

            let owner_spend = p2(&mut self.ctx, owner, conditions)?;
            let counter =
                self.ctx
                    .curry(SupplyCounterArgs::new(CAP, self.issued, owner_spend.puzzle))?;
            self.singleton = self.singleton.spend(
                &mut self.ctx,
                Spend::new(counter, owner_spend.solution),
                SupplyCounterArgs::puzzle_hash(CAP, issued, new_owner),
            )?;
            self.issued = issued;
            self.owner_puzzle_hash = new_owner;
            Ok(())
        }

        fn authorize(&mut self, authorizations: &[(&Dog, i128)]) -> Result<()> {
            let issuer = self.issuer.clone();
            self.authorize_to(&issuer, issuer.puzzle_hash, authorizations)
        }

        /// Adds the spends to issue tokens to Alice, without authorizing it.
        fn issue_unauthorized(&mut self, amount: u64) -> Result<(Issued, SecretKey)> {
            let parent = self.sim.bls(0);
            let tail = self.tail_spend()?;
            let issuance =
                issue_with(&mut self.ctx, &parent, tail, self.alice.puzzle_hash, amount)?;
            Ok((issuance, parent.sk))
        }

        /// Issues tokens to Alice.
        fn issue(&mut self, amount: u64) -> Result<Dog> {
            let (issuance, parent_sk) = self.issue_unauthorized(amount)?;
            self.authorize(&[(&issuance.eve, amount.into())])?;
            self.submit(&[parent_sk])?;
            Ok(issuance.dog)
        }

        /// Adds the spends to change the supply of one of Alice's coins, authorized by the counter.
        fn change(&mut self, dog: Dog, delta: i128) -> Result<Option<Dog>> {
            let tail = self.tail_spend()?;
            self.authorize(&[(&dog, delta)])?;
            change_supply(&mut self.ctx, &self.alice, dog, tail, delta)
        }

        fn submit(&mut self, extra: &[SecretKey]) -> Result<u64> {
            let keys = self.keys(extra);
            submit(&mut self.sim, &mut self.ctx, &keys)
        }

        fn submit_err(&mut self, extra: &[SecretKey]) -> ErrorCode {
            let keys = self.keys(extra);
            submit_err(&mut self.sim, &mut self.ctx, &keys)
        }

        fn keys(&self, extra: &[SecretKey]) -> Vec<SecretKey> {
            let mut keys = vec![self.issuer.sk.clone(), self.alice.sk.clone()];
            keys.extend_from_slice(extra);
            keys
        }
    }

    #[test]
    fn issue_up_to_the_cap() -> Result<()> {
        let mut s = Setup::new()?;

        let dog = s.issue(600)?;
        assert_eq!(dog.amount, 600);

        // Issue the rest of the cap into the same coin.
        let dog = s.change(dog, 400)?.expect("a new coin");
        s.submit(&[])?;
        assert_eq!(dog.amount, 1000);
        assert_eq!(s.issued, CAP);
        assert_unspent(&s.sim, &dog);

        // Nothing more can be issued.
        let snapshot = (s.singleton, s.issued);
        s.change(dog, 1)?;
        assert_eq!(s.submit_err(&[]), PUZZLE_FAILED);
        (s.singleton, s.issued) = snapshot;

        // Melting is authorized, but doesn't make room for more.
        let dog = s.change(dog, -500)?.expect("a new coin");
        s.submit(&[])?;
        assert_eq!(dog.amount, 500);
        assert_eq!(s.issued, CAP);

        s.change(dog, 1)?;
        assert_eq!(s.submit_err(&[]), PUZZLE_FAILED);

        Ok(())
    }

    #[test]
    fn cant_issue_more_than_the_cap_at_once() -> Result<()> {
        let mut s = Setup::new()?;
        let singleton = s.singleton;

        let (issuance, parent_sk) = s.issue_unauthorized(CAP + 1)?;
        s.authorize(&[(&issuance.eve, (CAP + 1).into())])?;
        assert_eq!(s.submit_err(&[parent_sk]), PUZZLE_FAILED);

        // Splitting it into two authorizations in one spend doesn't help.
        s.singleton = singleton;
        s.issued = 0;
        let (first, first_sk) = s.issue_unauthorized(CAP)?;
        let (second, second_sk) = s.issue_unauthorized(1)?;
        s.authorize(&[(&first.eve, CAP.into()), (&second.eve, 1)])?;
        assert_eq!(s.submit_err(&[first_sk, second_sk]), PUZZLE_FAILED);

        Ok(())
    }

    #[test]
    fn requires_the_counter() -> Result<()> {
        let mut s = Setup::new()?;

        // Without the counter's spend, nothing is issued.
        let (_, parent_sk) = s.issue_unauthorized(100)?;
        assert_eq!(
            s.submit_err(&[parent_sk]),
            ErrorCode::MessageNotSentOrReceived
        );

        Ok(())
    }

    #[test]
    fn tail_checks_the_counter_state() -> Result<()> {
        let mut s = Setup::new()?;
        s.issue(600)?;

        // The TAIL is given a state in which nothing has been issued yet, so the sender's puzzle
        // hash doesn't match the counter's.
        let actual = s.issued;
        s.issued = 0;
        let (issuance, parent_sk) = s.issue_unauthorized(100)?;
        s.issued = actual;
        s.authorize(&[(&issuance.eve, 100)])?;
        assert_eq!(
            s.submit_err(&[parent_sk]),
            ErrorCode::MessageNotSentOrReceived
        );

        Ok(())
    }

    #[test]
    fn owner_can_change() -> Result<()> {
        let mut s = Setup::new()?;
        let bob = s.sim.bls(0);

        // The issuer passes the counter to Bob while issuing.
        let (issuance, parent_sk) = s.issue_unauthorized(100)?;
        let issuer = s.issuer.clone();
        s.authorize_to(&issuer, bob.puzzle_hash, &[(&issuance.eve, 100)])?;
        s.submit(&[parent_sk])?;

        // Bob can issue up to the rest of the cap.
        let (issuance, parent_sk) = s.issue_unauthorized(900)?;
        s.authorize_to(&bob, bob.puzzle_hash, &[(&issuance.eve, 900)])?;
        submit(&mut s.sim, &mut s.ctx, &[bob.sk.clone(), parent_sk])?;
        assert_eq!(s.issued, CAP);

        Ok(())
    }

    /// The curried arguments of a supply counter, with a signed amount issued.
    #[derive(Debug, Clone, Copy, ToClvm, FromClvm)]
    #[clvm(curry)]
    struct RawCounterArgs<I> {
        mod_hash: Bytes32,
        cap: u64,
        issued: i64,
        owner: I,
    }

    impl<I> Mod for RawCounterArgs<I> {
        fn mod_reveal() -> Cow<'static, [u8]> {
            Cow::Borrowed(&SUPPLY_COUNTER_PUZZLE)
        }

        fn mod_hash() -> TreeHash {
            SUPPLY_COUNTER_PUZZLE_HASH
        }
    }

    /// A supply counter launched as if 1000 had been melted, which would allow 2000 to be issued.
    fn negative_counter<I>(owner: I) -> RawCounterArgs<I> {
        RawCounterArgs {
            mod_hash: SUPPLY_COUNTER_PUZZLE_HASH.into(),
            cap: CAP,
            issued: -1000,
            owner,
        }
    }

    #[test]
    fn negative_starting_amount_is_rejected() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);

        let singleton = TestSingleton::launch(
            &mut sim,
            ctx,
            negative_counter(TreeHash::from(issuer.puzzle_hash)).curry_tree_hash(),
        )?;

        let conditions = Conditions::new().create_coin(issuer.puzzle_hash, 1, Memos::None);
        let owner_spend = p2(ctx, &issuer, conditions)?;
        let counter = ctx.curry(negative_counter(owner_spend.puzzle))?;
        singleton.spend(
            ctx,
            Spend::new(counter, owner_spend.solution),
            issuer.puzzle_hash.into(),
        )?;
        assert_eq!(submit_err(&mut sim, ctx, &[issuer.sk]), PUZZLE_FAILED);

        Ok(())
    }
}

mod burnable {
    use super::*;

    #[test]
    fn holders_can_melt_without_the_issuer() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);
        let alice = sim.bls(0);
        let parent = sim.bls(0);

        let issuance_tail = ctx.curry(EverythingWithSignatureTailArgs::new(issuer.pk))?;
        let tail = tail_without_solution(ctx, BurnableTailArgs::new(issuance_tail))?;

        // Issuing needs the issuer's signature.
        let issuance = issue_with(ctx, &parent, tail, alice.puzzle_hash, 1000)?;
        submit(&mut sim, ctx, &[parent.sk, issuer.sk.clone()])?;

        // Alice melts some of her tokens on her own.
        let dog = change_supply(ctx, &alice, issuance.dog, tail, -300)?.expect("a new coin");
        submit(&mut sim, ctx, &[alice.sk.clone()])?;
        assert_eq!(dog.amount, 700);

        // But she can't issue more.
        change_supply(ctx, &alice, dog, tail, 1)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[alice.sk.clone()]),
            ErrorCode::BadAggregateSignature
        );

        // Unless the issuer signs.
        let dog = change_supply(ctx, &alice, dog, tail, 1)?.expect("a new coin");
        submit(&mut sim, ctx, &[alice.sk.clone(), issuer.sk])?;
        assert_eq!(dog.amount, 701);

        // She can melt all of it.
        assert!(change_supply(ctx, &alice, dog, tail, -701)?.is_none());
        submit(&mut sim, ctx, &[alice.sk])?;

        Ok(())
    }

    #[test]
    fn fixed_supply_that_can_only_go_down() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let alice = sim.bls(0);
        let parent = sim.bls(0);

        let issuance_tail = ctx.curry(GenesisByCoinIdTailArgs::new(parent.coin.coin_id()))?;
        let tail = tail_without_solution(ctx, BurnableTailArgs::new(issuance_tail))?;

        let issuance = issue_with(ctx, &parent, tail, alice.puzzle_hash, 1000)?;
        submit(&mut sim, ctx, &[parent.sk])?;

        let dog = change_supply(ctx, &alice, issuance.dog, tail, -1)?.expect("a new coin");
        submit(&mut sim, ctx, &[alice.sk.clone()])?;
        assert_eq!(dog.amount, 999);

        change_supply(ctx, &alice, dog, tail, 1)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[alice.sk]),
            ErrorCode::AssertMyParentIdFailed
        );

        Ok(())
    }

    #[test]
    fn melting_needs_the_owner() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let alice = sim.bls(0);
        let parent = sim.bls(0);

        let issuance_tail = ctx.curry(GenesisByCoinIdTailArgs::new(parent.coin.coin_id()))?;
        let tail = tail_without_solution(ctx, BurnableTailArgs::new(issuance_tail))?;
        let issuance = issue_with(ctx, &parent, tail, alice.puzzle_hash, 1000)?;
        submit(&mut sim, ctx, &[parent.sk])?;

        // Alice signs a spend that creates 500 from her 1000 tokens, which doesn't balance.
        let conditions = send(ctx, alice.puzzle_hash, 500)?;
        let inner_spend = p2(ctx, &alice, conditions)?;
        spend_ring(ctx, &[RawRingItem::new(issuance.dog, inner_spend, 500)])?;
        let signature = sign_transaction(&ctx.take(), &[alice.sk])?;

        // Mallory reveals the TAIL to melt the other 500, which balances the ring. Alice's
        // signature is over the execution coin's id, which includes its amount, so it no longer
        // matches.
        Dog::spend_all(
            ctx,
            &[DogSpend::with_tail(
                issuance.dog,
                inner_spend,
                tail,
                metadata_hash(),
                -500,
            )],
        )?;
        let result = sim.new_transaction(SpendBundle::new(ctx.take(), signature));
        assert!(
            matches!(
                result,
                Err(SimulatorError::Validation(ErrorCode::BadAggregateSignature))
            ),
            "{result:?}"
        );

        Ok(())
    }
}

mod issuance_deadline {
    use super::*;

    #[test]
    fn issue_before_the_deadline_and_melt_after() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);
        let alice = sim.bls(0);
        let parent = sim.bls(0);

        sim.set_next_timestamp(10_000)?;
        let deadline = 11_000;
        let inner_tail = ctx.curry(EverythingWithSignatureTailArgs::new(issuer.pk))?;
        let tail = tail_without_solution(ctx, IssuanceDeadlineTailArgs::new(deadline, inner_tail))?;

        let issuance = issue_with(ctx, &parent, tail, alice.puzzle_hash, 1000)?;
        submit(&mut sim, ctx, &[parent.sk, issuer.sk.clone()])?;

        sim.pass_time(1000);
        let keys = [alice.sk.clone(), issuer.sk.clone()];

        // The deadline has passed, so nothing more can be issued, even with the issuer's signature.
        change_supply(ctx, &alice, issuance.dog, tail, 1)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &keys),
            ErrorCode::AssertBeforeSecondsAbsoluteFailed
        );

        // Melting still works, and still needs the issuer's signature.
        change_supply(ctx, &alice, issuance.dog, tail, -100)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[alice.sk.clone()]),
            ErrorCode::BadAggregateSignature
        );

        let dog = change_supply(ctx, &alice, issuance.dog, tail, -100)?.expect("a new coin");
        submit(&mut sim, ctx, &keys)?;
        assert_eq!(dog.amount, 900);

        Ok(())
    }

    #[test]
    fn issue_more_before_the_deadline() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);
        let alice = sim.bls(0);
        let parent = sim.bls(0);

        sim.set_next_timestamp(10_000)?;
        let deadline = 11_000;
        let inner_tail = ctx.curry(EverythingWithSignatureTailArgs::new(issuer.pk))?;
        let tail = tail_without_solution(ctx, IssuanceDeadlineTailArgs::new(deadline, inner_tail))?;

        let issuance = issue_with(ctx, &parent, tail, alice.puzzle_hash, 1000)?;
        submit(&mut sim, ctx, &[parent.sk, issuer.sk.clone()])?;

        // Each block moves the time forward by a second.
        sim.pass_time(990);
        let dog = change_supply(ctx, &alice, issuance.dog, tail, 500)?.expect("a new coin");
        submit(&mut sim, ctx, &[alice.sk, issuer.sk])?;
        assert_eq!(dog.amount, 1500);

        Ok(())
    }

    #[test]
    fn issuance_after_the_deadline_fails() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);
        let alice = sim.bls(0);
        let parent = sim.bls(0);

        sim.set_next_timestamp(10_000)?;
        let deadline = 10_000;
        let inner_tail = ctx.curry(EverythingWithSignatureTailArgs::new(issuer.pk))?;
        let tail = tail_without_solution(ctx, IssuanceDeadlineTailArgs::new(deadline, inner_tail))?;

        issue_with(ctx, &parent, tail, alice.puzzle_hash, 1000)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[parent.sk, issuer.sk]),
            ErrorCode::AssertBeforeSecondsAbsoluteFailed
        );

        Ok(())
    }
}

mod delegated_tail {
    use super::*;

    /// The delegated TAIL spend, and the delegated TAIL's tree hash, which the issuer signs.
    fn delegated(ctx: &mut SpendContext, tail: NodePtr, delegated_tail: NodePtr) -> Result<Spend> {
        let solution = ctx.alloc(&DelegatedTailSolution {
            delegated_tail,
            delegated_solution: NodePtr::NIL,
        })?;
        Ok(Spend::new(tail, solution))
    }

    #[test]
    fn issue_and_melt_with_a_delegated_tail() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);
        let minter = sim.bls(0);
        let alice = sim.bls(0);
        let parent = sim.bls(0);

        let tail = ctx.curry(DelegatedTailArgs::new(issuer.pk))?;

        // The issuer delegates to a TAIL that lets the minter's key change the supply.
        let minter_tail = ctx.curry(EverythingWithSignatureTailArgs::new(minter.pk))?;
        let spend = delegated(ctx, tail, minter_tail)?;

        let issuance = issue_with(ctx, &parent, spend, alice.puzzle_hash, 1000)?;
        submit(
            &mut sim,
            ctx,
            &[parent.sk, issuer.sk.clone(), minter.sk.clone()],
        )?;

        // Both signatures are needed: the issuer's of the delegated TAIL, and the minter's of the
        // supply change.
        let spend = delegated(ctx, tail, minter_tail)?;
        change_supply(ctx, &alice, issuance.dog, spend, -100)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[alice.sk.clone(), issuer.sk.clone()]),
            ErrorCode::BadAggregateSignature
        );

        let spend = delegated(ctx, tail, minter_tail)?;
        let dog = change_supply(ctx, &alice, issuance.dog, spend, -100)?.expect("a new coin");
        submit(&mut sim, ctx, &[alice.sk, issuer.sk, minter.sk])?;
        assert_eq!(dog.amount, 900);

        Ok(())
    }

    #[test]
    fn unsigned_delegated_tail_is_rejected() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);
        let mallory = sim.bls(0);
        let parent = sim.bls(0);

        let tail = ctx.curry(DelegatedTailArgs::new(issuer.pk))?;

        // Mallory delegates to a TAIL with her own key, but the issuer hasn't signed it.
        let mallory_tail = ctx.curry(EverythingWithSignatureTailArgs::new(mallory.pk))?;
        let spend = delegated(ctx, tail, mallory_tail)?;
        issue_with(ctx, &parent, spend, mallory.puzzle_hash, 1000)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[parent.sk, mallory.sk]),
            ErrorCode::BadAggregateSignature
        );

        Ok(())
    }

    #[test]
    fn one_time_delegation() -> Result<()> {
        let mut sim = Simulator::new();
        let ctx = &mut SpendContext::new();
        let issuer = sim.bls(0);
        let alice = sim.bls(0);
        let parent = sim.bls(0);
        let other = sim.bls(0);

        let tail = ctx.curry(DelegatedTailArgs::new(issuer.pk))?;

        // The issuer delegates to `genesis_by_coin_id` for one coin, which needs no other
        // signature, so it can only be used to issue from that coin.
        let genesis_tail = ctx.curry(GenesisByCoinIdTailArgs::new(parent.coin.coin_id()))?;
        let spend = delegated(ctx, tail, genesis_tail)?;
        issue_with(ctx, &parent, spend, alice.puzzle_hash, 1000)?;
        submit(&mut sim, ctx, &[parent.sk, issuer.sk.clone()])?;

        let spend = delegated(ctx, tail, genesis_tail)?;
        issue_with(ctx, &other, spend, alice.puzzle_hash, 1000)?;
        assert_eq!(
            submit_err(&mut sim, ctx, &[other.sk, issuer.sk]),
            ErrorCode::AssertMyParentIdFailed
        );

        Ok(())
    }
    /// The issuer signs the asset id along with the delegated TAIL, so a delegation for one asset
    /// can't be used for another asset with the same key.
    #[test]
    fn delegation_is_tied_to_the_asset() -> Result<()> {
        use chia_wallet_sdk::{
            clvm_traits::{FromClvm, clvm_list},
            types::{Condition, run_puzzle},
        };

        let ctx = &mut SpendContext::new();
        let issuer = chia_wallet_sdk::test::BlsPair::new(0);
        let tail = ctx.curry(DelegatedTailArgs::new(issuer.pk))?;
        let delegated_tail = ctx.alloc(&chia_wallet_sdk::clvm_traits::clvm_quote!(()))?;

        for asset_id in [Bytes32::new([1; 32]), Bytes32::new([2; 32])] {
            let args = ctx.alloc(&clvm_list!(
                100,
                100,
                Bytes32::default(),
                asset_id,
                delegated_tail
            ))?;
            let output = run_puzzle(ctx, tail, args)?;
            let conditions = Vec::<Condition>::from_clvm(ctx, output)?;
            let agg_sig = conditions
                .into_iter()
                .find_map(Condition::into_agg_sig_unsafe)
                .expect("the issuer must sign");

            let mut message = asset_id.to_vec();
            message.extend_from_slice(&ctx.tree_hash(delegated_tail).to_bytes());
            assert_eq!(agg_sig.public_key, issuer.pk);
            assert_eq!(agg_sig.message.as_ref(), message.as_slice());
        }

        Ok(())
    }
}

mod multisig {
    use super::*;

    struct Setup {
        sim: Simulator,
        ctx: SpendContext,
        keys: [BlsPairWithCoin; 3],
        alice: BlsPairWithCoin,
        tail: NodePtr,
    }

    impl Setup {
        /// A 2 of 3 multisig TAIL.
        fn new() -> Result<Self> {
            let mut sim = Simulator::new();
            let mut ctx = SpendContext::new();
            let keys = [sim.bls(0), sim.bls(0), sim.bls(0)];
            let alice = sim.bls(0);
            let tail = ctx.curry(MultisigTailArgs::new(
                2,
                keys.iter().map(|key| key.pk).collect(),
            ))?;
            Ok(Self {
                sim,
                ctx,
                keys,
                alice,
                tail,
            })
        }

        fn tail_spend(&mut self, signers: &[bool]) -> Result<Spend> {
            let solution = self.ctx.alloc(&MultisigTailSolution {
                signers: signers.to_vec(),
            })?;
            Ok(Spend::new(self.tail, solution))
        }

        fn sks(&self, signers: &[usize]) -> Vec<SecretKey> {
            signers.iter().map(|&i| self.keys[i].sk.clone()).collect()
        }
    }

    #[test]
    fn issue_and_melt_with_two_of_three() -> Result<()> {
        let mut s = Setup::new()?;
        let parent = s.sim.bls(0);

        let tail = s.tail_spend(&[true, false, true])?;
        let issuance = issue_with(&mut s.ctx, &parent, tail, s.alice.puzzle_hash, 1000)?;
        let mut keys = s.sks(&[0, 2]);
        keys.push(parent.sk);
        submit(&mut s.sim, &mut s.ctx, &keys)?;

        // Any two of the keys can melt. Missing flags count as not signing.
        let tail = s.tail_spend(&[false, true])?;
        change_supply(&mut s.ctx, &s.alice, issuance.dog, tail, -100)?;
        let mut keys = s.sks(&[1]);
        keys.push(s.alice.sk.clone());
        assert_eq!(submit_err(&mut s.sim, &mut s.ctx, &keys), PUZZLE_FAILED);

        let tail = s.tail_spend(&[false, true, true])?;
        let dog =
            change_supply(&mut s.ctx, &s.alice, issuance.dog, tail, -100)?.expect("a new coin");
        let mut keys = s.sks(&[1, 2]);
        keys.push(s.alice.sk.clone());
        submit(&mut s.sim, &mut s.ctx, &keys)?;
        assert_eq!(dog.amount, 900);

        Ok(())
    }

    #[test]
    fn one_signer_is_not_enough() -> Result<()> {
        let mut s = Setup::new()?;
        let parent = s.sim.bls(0);

        let tail = s.tail_spend(&[true, false, false])?;
        issue_with(&mut s.ctx, &parent, tail, s.alice.puzzle_hash, 1000)?;
        let mut keys = s.sks(&[0, 1, 2]);
        keys.push(parent.sk);
        assert_eq!(submit_err(&mut s.sim, &mut s.ctx, &keys), PUZZLE_FAILED);

        Ok(())
    }

    #[test]
    fn flagged_keys_must_sign() -> Result<()> {
        let mut s = Setup::new()?;
        let parent = s.sim.bls(0);

        // Two keys are flagged, but only one of them signs.
        let tail = s.tail_spend(&[true, true, false])?;
        issue_with(&mut s.ctx, &parent, tail, s.alice.puzzle_hash, 1000)?;
        let mut keys = s.sks(&[0]);
        keys.push(parent.sk);
        assert_eq!(
            submit_err(&mut s.sim, &mut s.ctx, &keys),
            ErrorCode::BadAggregateSignature
        );

        Ok(())
    }
}
