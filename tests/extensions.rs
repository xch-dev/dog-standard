//! Proofs of concept for token features built as inner puzzle layers on top of DOG, without any
//! change to the DOG puzzles. See "Extending DOG" in the README.
//!
//! - [`transfer_fee.rue`](../puzzles/layers/transfer_fee.rue) charges a fee to the issuer, paid
//!   in the token itself, whenever tokens leave their holder.
//! - [`credential_restriction.rue`](../puzzles/layers/credential_restriction.rue) only lets
//!   holders with a credential from an authority send or receive tokens.
//!
//! It also shows a limitation of the execution coin: relative time conditions in the inner puzzle
//! apply to the ephemeral execution coin, not to the DOG coin.

mod common;

use std::{borrow::Cow, collections::HashMap, path::Path};

use anyhow::Result;
use chia_wallet_sdk::{
    chia::{
        bls::{PublicKey, SecretKey, Signature, sign},
        consensus::validation_error::ErrorCode,
        protocol::{Bytes32, CoinSpend, SpendBundle},
        sha2::Sha256,
    },
    clvm_traits::{FromClvm, ToClvm},
    clvm_utils::{TreeHash, tree_hash},
    clvmr::{Allocator, serde::node_from_bytes},
    driver::{Spend, SpendContext},
    signer::{AggSigConstants, RequiredSignature},
    test::{BlsPairWithCoin, Simulator, SimulatorError},
    types::{Conditions, Mod, TESTNET11_CONSTANTS, compile_rue},
};
use common::*;
use dog::{Dog, DogError, DogIssuance, DogSpend};
use rstest::rstest;

/// Decodes the committed bytecode or hash of a puzzle in `puzzles/dist/`.
fn decode(hex: &str) -> Vec<u8> {
    hex::decode(hex.trim()).expect("the committed hex should be valid")
}

const TRANSFER_FEE_HEX: &str = include_str!("../puzzles/dist/transfer_fee.rue.hex");
const TRANSFER_FEE_HASH_HEX: &str = include_str!("../puzzles/dist/transfer_fee.rue.hash");
const CREDENTIAL_HEX: &str = include_str!("../puzzles/dist/credential_restriction.rue.hex");
const CREDENTIAL_HASH_HEX: &str = include_str!("../puzzles/dist/credential_restriction.rue.hash");

fn mod_hash(hash_hex: &str) -> TreeHash {
    TreeHash::new(
        decode(hash_hex)
            .try_into()
            .expect("the committed hash should be 32 bytes"),
    )
}

#[rstest]
#[case::transfer_fee(
    "puzzles/layers/transfer_fee.rue",
    TRANSFER_FEE_HEX,
    TRANSFER_FEE_HASH_HEX
)]
#[case::credential_restriction(
    "puzzles/layers/credential_restriction.rue",
    CREDENTIAL_HEX,
    CREDENTIAL_HASH_HEX
)]
fn compiled_layers_are_up_to_date(
    #[case] path: &str,
    #[case] hex: &str,
    #[case] hash_hex: &str,
) -> Result<()> {
    let compilation = compile_rue(Path::new(path), false, None)?;
    assert_eq!(hex::encode(&compilation.reveal), hex.trim());
    assert_eq!(compilation.hash, mod_hash(hash_hex));

    let mut allocator = Allocator::new();
    let ptr = node_from_bytes(&mut allocator, &decode(hex))?;
    assert_eq!(tree_hash(&allocator, ptr), mod_hash(hash_hex));

    Ok(())
}

// Transfer fees

/// The curried arguments of the transfer fee layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
struct TransferFeeArgs<I> {
    mod_hash: Bytes32,
    issuer_puzzle_hash: Bytes32,
    fee_basis_points: u64,
    inner_puzzle: I,
}

impl<I> TransferFeeArgs<I> {
    fn new(issuer_puzzle_hash: Bytes32, fee_basis_points: u64, inner_puzzle: I) -> Self {
        Self {
            mod_hash: mod_hash(TRANSFER_FEE_HASH_HEX).into(),
            issuer_puzzle_hash,
            fee_basis_points,
            inner_puzzle,
        }
    }
}

impl<I> Mod for TransferFeeArgs<I> {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Owned(decode(TRANSFER_FEE_HEX))
    }

    fn mod_hash() -> TreeHash {
        mod_hash(TRANSFER_FEE_HASH_HEX)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
struct TransferFeeSolution<S> {
    my_amount: u64,
    inner_solution: S,
}

/// 1% of each transfer goes to the issuer.
const FEE_BASIS_POINTS: u64 = 100;

/// An asset whose coins are all wrapped in the transfer fee layer.
struct FeeToken {
    issuer: BlsPairWithCoin,
}

impl FeeToken {
    /// The inner puzzle hash of a holder's DOG coins.
    fn puzzle_hash(&self, owner: Bytes32) -> Bytes32 {
        TransferFeeArgs::new(
            self.issuer.puzzle_hash,
            FEE_BASIS_POINTS,
            TreeHash::from(owner),
        )
        .curry_tree_hash()
        .into()
    }

    /// Issues tokens into the layer, so that every coin of the asset pays the fee.
    fn issue(
        &self,
        sim: &mut Simulator,
        ctx: &mut SpendContext,
        owner: Bytes32,
        amount: u64,
    ) -> Result<Dog> {
        let conditions =
            Conditions::new().create_coin(self.puzzle_hash(owner), amount, ctx.hint(owner)?);
        let DogIssuance {
            parent_conditions,
            children,
            ..
        } = Dog::single_issuance(
            ctx,
            self.issuer.coin.coin_id(),
            &metadata(),
            &DogUris::default(),
            amount,
            conditions,
        )?;
        chia_wallet_sdk::driver::StandardLayer::new(self.issuer.pk).spend(
            ctx,
            self.issuer.coin,
            parent_conditions,
        )?;
        submit(sim, ctx, &[self.issuer.sk.clone()])?;
        Ok(children[0])
    }

    /// The inner spend of a holder's DOG coin, whose standard puzzle outputs the conditions.
    fn spend(
        &self,
        ctx: &mut SpendContext,
        owner: &BlsPairWithCoin,
        dog: Dog,
        conditions: Conditions,
    ) -> Result<Spend> {
        let p2 = p2(ctx, owner, conditions)?;
        let puzzle = ctx.curry(TransferFeeArgs::new(
            self.issuer.puzzle_hash,
            FEE_BASIS_POINTS,
            p2.puzzle,
        ))?;
        let solution = ctx.alloc(&TransferFeeSolution {
            my_amount: dog.amount,
            inner_solution: p2.solution,
        })?;
        Ok(Spend::new(puzzle, solution))
    }
}

/// The DOG coins in `children` with the given inner puzzle hash and amount.
fn find(children: &[Dog], p2_puzzle_hash: Bytes32, amount: u64) -> Option<Dog> {
    children
        .iter()
        .copied()
        .find(|dog| dog.p2_puzzle_hash == p2_puzzle_hash && dog.amount == amount)
}

#[test]
fn transfer_fee_is_paid_to_issuer() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let token = FeeToken { issuer: sim.bls(0) };
    let alice = sim.bls(0);
    let bob = sim.bls(0);

    let dog = token.issue(&mut sim, ctx, alice.puzzle_hash, 10_000)?;
    assert_eq!(dog.p2_puzzle_hash, token.puzzle_hash(alice.puzzle_hash));

    // Alice sends 5,000 tokens away and keeps 5,000 as change. The fee is 1% of the 5,000 that
    // leave her, and comes out of what she sends.
    let conditions = send_many(ctx, &[(bob.puzzle_hash, 4950), (alice.puzzle_hash, 5000)])?;
    let inner_spend = token.spend(ctx, &alice, dog, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;

    assert_eq!(children.len(), 3);
    let fee = find(&children, token.puzzle_hash(token.issuer.puzzle_hash), 50).expect("fee");
    let bobs = find(&children, token.puzzle_hash(bob.puzzle_hash), 4950).expect("bob");
    find(&children, token.puzzle_hash(alice.puzzle_hash), 5000).expect("change");
    for child in &children {
        assert_unspent(&sim, child);
    }

    // The issuer is exempt, so it can spend the fees it collects without paying a fee.
    let conditions = send(ctx, bob.puzzle_hash, 50)?;
    let inner_spend = token.spend(ctx, &token.issuer, fee, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(fee, inner_spend)])?;
    submit(&mut sim, ctx, &[token.issuer.sk.clone()])?;
    assert_eq!(children.len(), 1);
    assert_eq!(
        children[0].p2_puzzle_hash,
        token.puzzle_hash(bob.puzzle_hash)
    );

    // Bob's coins are still in the layer, so he pays the fee when he sends them on.
    let conditions = send(ctx, alice.puzzle_hash, 4900)?;
    let inner_spend = token.spend(ctx, &bob, bobs, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(bobs, inner_spend)])?;
    submit(&mut sim, ctx, &[bob.sk.clone()])?;
    find(&children, token.puzzle_hash(token.issuer.puzzle_hash), 50).expect("fee");

    Ok(())
}

#[test]
fn transfer_fee_cant_be_skipped() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let token = FeeToken { issuer: sim.bls(0) };
    let alice = sim.bls(0);
    let bob = sim.bls(0);

    let dog = token.issue(&mut sim, ctx, alice.puzzle_hash, 10_000)?;

    // Sending everything without leaving room for the fee doesn't balance. The driver notices,
    // and if the ring is built by hand, the puzzle rejects it.
    let conditions = send(ctx, bob.puzzle_hash, 10_000)?;
    let inner_spend = token.spend(ctx, &alice, dog, conditions.clone())?;
    assert!(matches!(
        Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)]),
        Err(DogError::UnbalancedSpend(-100))
    ));
    ctx.take();

    let inner_spend = token.spend(ctx, &alice, dog, conditions)?;
    spend_ring(ctx, &[RawRingItem::new(dog, inner_spend, 10_000)])?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        ErrorCode::MessageNotSentOrReceived
    );

    // Alice can't strip the layer by spending her coin with her standard puzzle directly, since
    // the DOG coin's puzzle commits to the layer. That coin doesn't exist.
    let unwrapped = Dog::new(
        dog.coin.parent_coin_info,
        dog.asset_id,
        alice.puzzle_hash,
        dog.amount,
        dog.lineage_proof,
    );
    let conditions = send(ctx, bob.puzzle_hash, 10_000)?;
    let inner_spend = p2(ctx, &alice, conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(unwrapped, inner_spend)])?;
    assert!(matches!(
        sim.spend_coins(ctx.take(), &[alice.sk.clone()]),
        Err(SimulatorError::Validation(ErrorCode::UnknownUnspent))
    ));

    // Bob receives 2,000 tokens, so that he has a coin to collude with.
    let conditions = send_many(ctx, &[(bob.puzzle_hash, 1980), (alice.puzzle_hash, 8000)])?;
    let inner_spend = token.spend(ctx, &alice, dog, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    submit(&mut sim, ctx, &[alice.sk.clone()])?;
    let alices = find(&children, token.puzzle_hash(alice.puzzle_hash), 8000).expect("alice");
    let bobs = find(&children, token.puzzle_hash(bob.puzzle_hash), 1980).expect("bob");

    // Alice and Bob try to move Alice's tokens to Bob without a transfer: Alice's coin creates
    // nothing, and Bob's coin creates the tokens as change back to himself. The fee is charged on
    // what leaves Alice's coin, so this doesn't balance either.
    let alice_spend = token.spend(ctx, &alice, alices, Conditions::new())?;
    let bob_conditions = send(ctx, bob.puzzle_hash, 9980)?;
    let bob_spend = token.spend(ctx, &bob, bobs, bob_conditions)?;
    spend_ring(
        ctx,
        &[
            RawRingItem::new(alices, alice_spend, 0),
            RawRingItem::new(bobs, bob_spend, 9980),
        ],
    )?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone(), bob.sk.clone()]),
        ErrorCode::MessageNotSentOrReceived
    );

    // If they leave room for the fee, it's paid, so it wasn't skipped.
    let alice_spend = token.spend(ctx, &alice, alices, Conditions::new())?;
    let bob_conditions = send(ctx, bob.puzzle_hash, 9900)?;
    let bob_spend = token.spend(ctx, &bob, bobs, bob_conditions)?;
    let children = Dog::spend_all(
        ctx,
        &[
            DogSpend::new(alices, alice_spend),
            DogSpend::new(bobs, bob_spend),
        ],
    )?;
    submit(&mut sim, ctx, &[alice.sk.clone(), bob.sk.clone()])?;
    find(&children, token.puzzle_hash(token.issuer.puzzle_hash), 80).expect("fee");
    find(&children, token.puzzle_hash(bob.puzzle_hash), 9900).expect("bob");

    Ok(())
}

#[test]
fn transfer_fee_must_use_real_amount() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let token = FeeToken { issuer: sim.bls(0) };
    let alice = sim.bls(0);
    let bob = sim.bls(0);

    let dog = token.issue(&mut sim, ctx, alice.puzzle_hash, 10_000)?;

    // Alice claims that her coin is smaller than it is, to pay a smaller fee. The fee is checked
    // against the execution coin's amount, which is the token amount.
    let conditions = send(ctx, bob.puzzle_hash, 9999)?;
    let p2 = p2(ctx, &alice, conditions)?;
    let puzzle = ctx.curry(TransferFeeArgs::new(
        token.issuer.puzzle_hash,
        FEE_BASIS_POINTS,
        p2.puzzle,
    ))?;
    let solution = ctx.alloc(&TransferFeeSolution {
        my_amount: 100,
        inner_solution: p2.solution,
    })?;
    spend_ring(
        ctx,
        &[RawRingItem::new(dog, Spend::new(puzzle, solution), 10_000)],
    )?;
    assert_eq!(
        submit_err(&mut sim, ctx, &[alice.sk.clone()]),
        ErrorCode::AssertMyAmountFailed
    );

    Ok(())
}

// Credential restriction

/// The curried arguments of the credential restriction layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(curry)]
struct CredentialArgs<I> {
    mod_hash: Bytes32,
    authority_public_key: PublicKey,
    inner_puzzle: I,
}

impl<I> CredentialArgs<I> {
    fn new(authority_public_key: PublicKey, inner_puzzle: I) -> Self {
        Self {
            mod_hash: mod_hash(CREDENTIAL_HASH_HEX).into(),
            authority_public_key,
            inner_puzzle,
        }
    }
}

impl<I> Mod for CredentialArgs<I> {
    fn mod_reveal() -> Cow<'static, [u8]> {
        Cow::Owned(decode(CREDENTIAL_HEX))
    }

    fn mod_hash() -> TreeHash {
        mod_hash(CREDENTIAL_HASH_HEX)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
struct CredentialSolution<S> {
    my_expiry: u64,
    recipient_expiries: Vec<u64>,
    inner_solution: S,
}

/// A credential that the authority issues to a holder, which they can reuse until it expires.
#[derive(Debug, Clone)]
struct Credential {
    expiry: u64,
    signature: Signature,
}

/// The canonical CLVM encoding of an integer.
fn int_bytes(value: u64) -> Vec<u8> {
    let mut allocator = Allocator::new();
    let ptr = value
        .to_clvm(&mut allocator)
        .expect("an integer can be allocated");
    allocator.atom(ptr).as_ref().to_vec()
}

fn credential_message(puzzle_hash: Bytes32, expiry: u64) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(b"dog credential");
    hasher.update(puzzle_hash);
    hasher.update(int_bytes(expiry));
    hasher.finalize().to_vec()
}

/// An asset whose coins are all wrapped in the credential restriction layer.
struct RestrictedToken {
    issuer: BlsPairWithCoin,
    authority: SecretKey,
}

impl RestrictedToken {
    fn new(sim: &mut Simulator) -> Self {
        Self {
            issuer: sim.bls(0),
            authority: sim.bls(0).sk,
        }
    }

    /// The authority issues a credential to a puzzle hash, offline.
    fn credential(&self, puzzle_hash: Bytes32, expiry: u64) -> Credential {
        Credential {
            expiry,
            signature: sign(&self.authority, credential_message(puzzle_hash, expiry)),
        }
    }

    fn puzzle_hash(&self, owner: Bytes32) -> Bytes32 {
        CredentialArgs::new(self.authority.public_key(), TreeHash::from(owner))
            .curry_tree_hash()
            .into()
    }

    /// Issues tokens into the layer. The issuance doesn't need a credential, since the eve coin's
    /// inner puzzle creates the layered coin directly.
    fn issue(
        &self,
        sim: &mut Simulator,
        ctx: &mut SpendContext,
        owner: Bytes32,
        amount: u64,
    ) -> Result<Dog> {
        let conditions =
            Conditions::new().create_coin(self.puzzle_hash(owner), amount, ctx.hint(owner)?);
        let DogIssuance {
            parent_conditions,
            children,
            ..
        } = Dog::single_issuance(
            ctx,
            self.issuer.coin.coin_id(),
            &metadata(),
            &DogUris::default(),
            amount,
            conditions,
        )?;
        chia_wallet_sdk::driver::StandardLayer::new(self.issuer.pk).spend(
            ctx,
            self.issuer.coin,
            parent_conditions,
        )?;
        submit(sim, ctx, &[self.issuer.sk.clone()])?;
        Ok(children[0])
    }

    /// The inner spend of a holder's DOG coin, given the holder's credential and the expiry of
    /// each recipient's credential.
    fn spend(
        &self,
        ctx: &mut SpendContext,
        owner: &BlsPairWithCoin,
        my_expiry: u64,
        recipient_expiries: Vec<u64>,
        conditions: Conditions,
    ) -> Result<Spend> {
        let p2 = p2(ctx, owner, conditions)?;
        let puzzle = ctx.curry(CredentialArgs::new(self.authority.public_key(), p2.puzzle))?;
        let solution = ctx.alloc(&CredentialSolution {
            my_expiry,
            recipient_expiries,
            inner_solution: p2.solution,
        })?;
        Ok(Spend::new(puzzle, solution))
    }

    /// Signs the coin spends with the holders' keys, and adds the credentials they present. The
    /// authority's key isn't used, so a missing credential can't be made up.
    fn sign(
        &self,
        coin_spends: &[CoinSpend],
        keys: &[SecretKey],
        credentials: &[(Bytes32, &Credential)],
    ) -> Result<Signature> {
        let credentials: HashMap<Vec<u8>, Signature> = credentials
            .iter()
            .map(|(puzzle_hash, credential)| {
                (
                    credential_message(*puzzle_hash, credential.expiry),
                    credential.signature.clone(),
                )
            })
            .collect();
        let keys: HashMap<PublicKey, &SecretKey> =
            keys.iter().map(|sk| (sk.public_key(), sk)).collect();

        let mut signature = Signature::default();

        for required in RequiredSignature::from_coin_spends(
            &mut Allocator::new(),
            coin_spends,
            &AggSigConstants::new(TESTNET11_CONSTANTS.agg_sig_me_additional_data),
        )? {
            let RequiredSignature::Bls(required) = required else {
                continue;
            };

            if required.public_key == self.authority.public_key() {
                if let Some(credential) = credentials.get(&required.message()) {
                    signature += credential;
                }
            } else if let Some(sk) = keys.get(&required.public_key) {
                signature += &sign(sk, required.message());
            }
        }

        Ok(signature)
    }

    fn submit(
        &self,
        sim: &mut Simulator,
        ctx: &mut SpendContext,
        keys: &[SecretKey],
        credentials: &[(Bytes32, &Credential)],
    ) -> Result<(), ErrorCode> {
        let coin_spends = ctx.take();
        // If a puzzle raises, the signatures can't be found, so the error comes from consensus.
        let signature = self
            .sign(&coin_spends, keys, credentials)
            .unwrap_or_default();

        match sim.new_transaction(SpendBundle::new(coin_spends, signature)) {
            Ok(_) => Ok(()),
            Err(SimulatorError::Validation(code)) => Err(code),
            Err(error) => panic!("unexpected error: {error}"),
        }
    }
}

#[test]
fn credentials_are_required_to_send_and_receive() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let token = RestrictedToken::new(&mut sim);
    let alice = sim.bls(0);
    let bob = sim.bls(0);
    let carol = sim.bls(0);

    let expiry = sim.next_timestamp() + 1000;
    let alice_credential = token.credential(alice.puzzle_hash, expiry);
    let bob_credential = token.credential(bob.puzzle_hash, expiry);

    let dog = token.issue(&mut sim, ctx, alice.puzzle_hash, 1000)?;
    assert_eq!(dog.p2_puzzle_hash, token.puzzle_hash(alice.puzzle_hash));

    // Carol has no credential, so Alice can't send her tokens.
    let conditions = send_many(ctx, &[(carol.puzzle_hash, 400), (alice.puzzle_hash, 600)])?;
    let inner_spend = token.spend(ctx, &alice, expiry, vec![expiry], conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    assert_eq!(
        token.submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[(alice.puzzle_hash, &alice_credential)],
        ),
        Err(ErrorCode::BadAggregateSignature)
    );

    // Alice can't leave out a recipient's credential either.
    let conditions = send_many(ctx, &[(carol.puzzle_hash, 400), (alice.puzzle_hash, 600)])?;
    let inner_spend = token.spend(ctx, &alice, expiry, vec![], conditions)?;
    spend_ring(ctx, &[RawRingItem::new(dog, inner_spend, 1000)])?;
    assert_eq!(
        token.submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[(alice.puzzle_hash, &alice_credential)],
        ),
        Err(PUZZLE_FAILED)
    );

    // Bob has a credential, so Alice can send him tokens.
    let conditions = send_many(ctx, &[(bob.puzzle_hash, 400), (alice.puzzle_hash, 600)])?;
    let inner_spend = token.spend(ctx, &alice, expiry, vec![expiry], conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    token
        .submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[
                (alice.puzzle_hash, &alice_credential),
                (bob.puzzle_hash, &bob_credential),
            ],
        )
        .expect("the transfer should succeed");

    // Every coin stays in the layer.
    let bobs = find(&children, token.puzzle_hash(bob.puzzle_hash), 400).expect("bob");
    find(&children, token.puzzle_hash(alice.puzzle_hash), 600).expect("change");
    assert_unspent(&sim, &bobs);

    // Carol can't receive tokens from Bob either, even with Bob's own credential.
    let conditions = send(ctx, carol.puzzle_hash, 400)?;
    let inner_spend = token.spend(ctx, &bob, expiry, vec![expiry], conditions)?;
    Dog::spend_all(ctx, &[DogSpend::new(bobs, inner_spend)])?;
    assert_eq!(
        token.submit(
            &mut sim,
            ctx,
            &[bob.sk.clone()],
            &[(bob.puzzle_hash, &bob_credential)],
        ),
        Err(ErrorCode::BadAggregateSignature)
    );

    Ok(())
}

#[test]
fn expired_credentials_freeze_tokens() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let token = RestrictedToken::new(&mut sim);
    let alice = sim.bls(0);
    let bob = sim.bls(0);

    let expiry = sim.next_timestamp() + 1000;
    let alice_credential = token.credential(alice.puzzle_hash, expiry);
    let bob_credential = token.credential(bob.puzzle_hash, expiry);

    let dog = token.issue(&mut sim, ctx, alice.puzzle_hash, 1000)?;

    sim.pass_time(2000);

    // Both credentials have expired, so Alice can't spend her tokens.
    let transfer = |ctx: &mut SpendContext, alice_expiry: u64, bob_expiry: u64| -> Result<()> {
        let conditions = send(ctx, bob.puzzle_hash, 1000)?;
        let inner_spend = token.spend(ctx, &alice, alice_expiry, vec![bob_expiry], conditions)?;
        Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
        Ok(())
    };

    transfer(ctx, expiry, expiry)?;
    assert_eq!(
        token.submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[
                (alice.puzzle_hash, &alice_credential),
                (bob.puzzle_hash, &bob_credential),
            ],
        ),
        Err(ErrorCode::AssertBeforeSecondsAbsoluteFailed)
    );

    // Alice can't claim a later expiry than the authority signed.
    let later = sim.next_timestamp() + 1000;
    transfer(ctx, later, later)?;
    assert_eq!(
        token.submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[
                (alice.puzzle_hash, &alice_credential),
                (bob.puzzle_hash, &bob_credential),
            ],
        ),
        Err(ErrorCode::BadAggregateSignature)
    );

    // Once the authority renews Alice's credential, she still can't send to Bob until his is
    // renewed too.
    let alice_renewed = token.credential(alice.puzzle_hash, later);
    transfer(ctx, later, expiry)?;
    assert_eq!(
        token.submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[
                (alice.puzzle_hash, &alice_renewed),
                (bob.puzzle_hash, &bob_credential),
            ],
        ),
        Err(ErrorCode::AssertBeforeSecondsAbsoluteFailed)
    );

    let bob_renewed = token.credential(bob.puzzle_hash, later);
    transfer(ctx, later, later)?;
    token
        .submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[
                (alice.puzzle_hash, &alice_renewed),
                (bob.puzzle_hash, &bob_renewed),
            ],
        )
        .expect("the transfer should succeed");

    Ok(())
}

/// A layer only restricts the coins inside it. If the TAIL lets any coin of the asset exist outside
/// the layer, even one worth 0 tokens, a holder can join it to the ring and move their tokens into
/// it, since the ring only checks the total. So assets that use a layer must only issue into it.
#[test]
fn unrestricted_coins_can_drain_restricted_ones() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let token = RestrictedToken::new(&mut sim);
    let alice = sim.bls(0);
    let carol = sim.bls(0);

    let expiry = sim.next_timestamp() + 1000;
    let alice_credential = token.credential(alice.puzzle_hash, expiry);

    // The issuance creates a restricted coin, and an unrestricted coin worth 0 tokens.
    let conditions = Conditions::new()
        .create_coin(
            token.puzzle_hash(alice.puzzle_hash),
            1000,
            ctx.hint(alice.puzzle_hash)?,
        )
        .create_coin(alice.puzzle_hash, 0, ctx.hint(alice.puzzle_hash)?);
    let DogIssuance {
        parent_conditions,
        children,
        ..
    } = Dog::single_issuance(
        ctx,
        token.issuer.coin.coin_id(),
        &metadata(),
        &DogUris::default(),
        1000,
        conditions,
    )?;
    chia_wallet_sdk::driver::StandardLayer::new(token.issuer.pk).spend(
        ctx,
        token.issuer.coin,
        parent_conditions,
    )?;
    submit(&mut sim, ctx, &[token.issuer.sk.clone()])?;
    let restricted = find(&children, token.puzzle_hash(alice.puzzle_hash), 1000).expect("coin");
    let unrestricted = find(&children, alice.puzzle_hash, 0).expect("coin");

    // The restricted coin creates nothing, and the unrestricted coin sends the tokens to Carol,
    // who has no credential.
    let restricted_spend = token.spend(ctx, &alice, expiry, vec![], Conditions::new())?;
    let conditions = send(ctx, carol.puzzle_hash, 1000)?;
    let unrestricted_spend = p2(ctx, &alice, conditions)?;
    let children = Dog::spend_all(
        ctx,
        &[
            DogSpend::new(restricted, restricted_spend),
            DogSpend::new(unrestricted, unrestricted_spend),
        ],
    )?;
    token
        .submit(
            &mut sim,
            ctx,
            &[alice.sk.clone()],
            &[(alice.puzzle_hash, &alice_credential)],
        )
        .expect("the tokens leave the layer");
    find(&children, carol.puzzle_hash, 1000).expect("carol");

    Ok(())
}

// Time locks

/// Spends a DOG coin whose standard puzzle outputs the time condition, and returns the error if
/// it's rejected.
fn spend_with_time_condition(
    sim: &mut Simulator,
    ctx: &mut SpendContext,
    owner: &BlsPairWithCoin,
    dog: Dog,
    time_condition: Conditions,
) -> Result<Result<Vec<Dog>, ErrorCode>> {
    let conditions = time_condition.extend(send(ctx, owner.puzzle_hash, dog.amount)?);
    let inner_spend = p2(ctx, owner, conditions)?;
    let children = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;
    let coin_spends = ctx.take();
    let signature = chia_wallet_sdk::test::sign_transaction(&coin_spends, &[owner.sk.clone()])?;
    Ok(
        match sim.new_transaction(SpendBundle::new(coin_spends, signature)) {
            Ok(_) => Ok(children),
            Err(SimulatorError::Validation(code)) => Err(code),
            Err(error) => panic!("unexpected error: {error}"),
        },
    )
}

/// Absolute time locks work in inner puzzles as they do for CATs, so vesting schedules and the
/// standard clawback (which uses absolute timestamps) work on DOGs.
#[test]
fn absolute_time_locks_work() -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;
    let unlock = sim.next_timestamp() + 1000;

    let locked = Conditions::new().assert_seconds_absolute(unlock);
    assert_eq!(
        spend_with_time_condition(&mut sim, ctx, &alice, dog, locked.clone())?.err(),
        Some(ErrorCode::AssertSecondsAbsoluteFailed)
    );

    sim.pass_time(1000);
    spend_with_time_condition(&mut sim, ctx, &alice, dog, locked)?
        .expect("the time lock should have passed");

    Ok(())
}

/// Relative time conditions in an inner puzzle apply to the execution coin, which is ephemeral,
/// and consensus rejects relative time conditions on ephemeral coins. So inner puzzles can't use
/// relative time locks or deadlines, however old the DOG coin is. They fail closed, rather than
/// being checked against the wrong coin.
#[rstest]
#[case::time_lock(Conditions::new().assert_seconds_relative(10))]
#[case::deadline(Conditions::new().assert_before_seconds_relative(10_000))]
#[case::height_lock(Conditions::new().assert_height_relative(1))]
fn relative_time_conditions_are_rejected(#[case] time_condition: Conditions) -> Result<()> {
    let mut sim = Simulator::new();
    let ctx = &mut SpendContext::new();

    let alice = sim.bls(0);
    let dog = issue_single(&mut sim, ctx, &alice, 1000)?;

    for _ in 0..3 {
        sim.pass_time(1000);
        sim.create_block();
    }

    assert_eq!(
        spend_with_time_condition(&mut sim, ctx, &alice, dog, time_condition)?.err(),
        Some(ErrorCode::EphemeralRelativeCondition)
    );

    // The same spend without the time condition works.
    spend_with_time_condition(&mut sim, ctx, &alice, dog, Conditions::new())?
        .expect("the spend should succeed");

    Ok(())
}
