# DOG

DOG is a fungible token standard for the Chia blockchain. Like a CAT, a DOG asset is defined by a TAIL that decides when tokens can be issued or melted, and tokens are controlled by an inner puzzle. Unlike a CAT:

- **Tokens don't need mojos.** Issuing and holding tokens costs no XCH beyond transaction fees, and a single coin can hold up to `u64::MAX` tokens, so the token amount doesn't need to be a multiple of mojos (CATs use 1000 mojos per token).
- **Metadata is part of the asset.** The asset id commits to the name, symbol, decimals, and content hashes of the asset, so they can be verified without a registry.
- **The TAIL is simpler.** It's given the change in supply, and returns the conditions that authorize it.

DOGs work with the same inner puzzles as CATs, including the settlement payments puzzle used by offers.

The puzzles are written in [Rue](https://github.com/xch-dev/rue), and the driver uses the [Chia Wallet SDK](https://github.com/xch-dev/chia-wallet-sdk).

> [!WARNING]
> This hasn't been audited. Don't use it on mainnet yet.

## How it works

Tokens are held in a **DOG coin**, which is always worth 0 mojos. Its token amount is curried into its puzzle instead.

Spending a DOG coin creates an **execution coin**, which is worth the token amount in mojos, and is spent in the same transaction. The execution coin runs the inner puzzle and wraps every coin it creates in a new DOG coin worth 0 mojos. Its mojos are created by one spend and destroyed by another in the same transaction, so they cancel out and never need to exist.

```
           creates (amount in mojos)            creates (0 mojos each)
DOG coin ─────────────────────────▶ execution coin ──────────────────▶ DOG coins
0 mojos     sends a message that      runs the inner puzzle,          0 mojos, with
AMOUNT      proves the parent         and joins a ring with the       their amounts
curried     is a DOG coin             other execution coins           curried
```

The inner puzzle sees an ordinary coin: its amount in mojos is the token amount, and its puzzle hash doesn't depend on the amount. That's what makes offers work, since the maker has to know the puzzle hash of the settlement coin that pays them before any coins are chosen.

### Supply

The execution coins spent together form a ring of messages. Each one sends the next one the running total of the coin amounts minus the amounts of the coins they create. Each execution coin must send and receive exactly one ring message, so the ring only closes if the total created equals the total spent. This is the same idea as the CAT ring, but with messages instead of announcements, so ring messages can't be reused or shared.

A ring message is sent to the puzzle hash of the next execution coin (which only depends on its inner puzzle hash), and received from anyone. That's enough, since any coin with the puzzle hash of an execution coin can only be spent by receiving a message from its parent DOG coin, and consensus requires every message to be received exactly as many times as it's sent. The ring messages sent to execution coins are exactly the ones they receive, so a message from any other coin would leave one of theirs unreceived, and adding up the subtotals shows that the total amount of the execution coins of an asset equals the total they create.

The amount of an execution coin is the DOG coin's amount, unless the TAIL is run. The **supply change** is the amount of the execution coin minus the amount that's proven by the lineage proof:

| Lineage proof | TAIL reveal | Execution coin amount | Supply change   |
| ------------- | ----------- | --------------------- | --------------- |
| valid         | none        | `AMOUNT`              | none            |
| valid         | `delta`     | `AMOUNT + delta`      | `delta`         |
| none          | `delta`     | `AMOUNT + delta`      | `AMOUNT + delta` |
| none          | none        | rejected              |                 |

A lineage proof shows that the DOG coin's parent is an execution coin of the same asset. A coin without one (such as the eve coin of an issuance) is only valid if the TAIL authorizes all of its tokens.

### TAIL

A TAIL is called with the supply change (positive to issue, negative to melt) followed by its solution, and returns conditions:

```rs
type Tail = fn(supply_change: Int, ...solution: Any) -> List<Condition>;
```

The TAIL runs in the DOG coin's spend, so `AGG_SIG_ME` and `ASSERT_MY_*` conditions apply to the DOG coin. There are two standard TAILs:

- [`genesis_by_coin_id`](puzzles/tails/genesis_by_coin_id.rue) asserts that the DOG coin's parent is the genesis coin, so tokens can only be issued once, and never melted. It doesn't require a signature, so the eve coin's inner puzzle must fix its own outputs, as the driver's quoted conditions do.
- [`everything_with_signature`](puzzles/tails/everything_with_signature.rue) requires an `AGG_SIG_ME` signature of the supply change by a public key, so the key's owner can issue or melt at any time.

There are also TAILs for other policies, each with its rules and caveats in its header comment:

- [`everything_with_singleton`](puzzles/tails/everything_with_singleton.rue) requires a message from a singleton to the DOG coin containing the supply change, so whoever controls the singleton can issue or melt, and each authorization is for one coin and one amount.
- [`capped_supply`](puzzles/tails/capped_supply.rue) is like `everything_with_singleton`, but the singleton's inner puzzle must be the [supply counter](puzzles/tails/supply_counter.rue), which adds up what it authorizes and can never authorize more than a cap in total. Melting doesn't make room under the cap.
- [`burnable`](puzzles/tails/burnable.rue) wraps another TAIL, and lets holders melt their own tokens without it. Wrapping `genesis_by_coin_id` gives a fixed supply that can only go down.
- [`issuance_deadline`](puzzles/tails/issuance_deadline.rue) wraps another TAIL, and only allows issuing before a timestamp. Melting is allowed at any time, if the other TAIL allows it.
- [`delegated_tail`](puzzles/tails/delegated_tail.rue) runs any TAIL whose tree hash is signed by a public key. A signed TAIL can be used by anyone, forever, so it should only be one that's safe for anyone to use.
- [`multisig`](puzzles/tails/multisig.rue) requires `AGG_SIG_ME` signatures of the supply change by at least `m` of `n` public keys.

A TAIL only sees the supply change, not the coin's amount before or after it. A coin with a lineage proof can also be spent without one, in which case the same supply change gives a different amount, so an authorization of a supply change for a coin doesn't fix the amount of its execution coin. The owner's signature does, for inner puzzles that sign the execution coin's id.

The TAIL can't change a coin's amount without its owner's consent. The owner's inner puzzle usually signs with `AGG_SIG_ME`, which commits to the execution coin's id, and the id includes the amount after the supply change. Inner puzzles that don't sign anything (such as the settlement payments puzzle) can be issued into or melted by anyone the TAIL allows, while they're in the mempool.

### Asset id and metadata

The asset id is the tree hash of the pair `(TAIL . METADATA)`, where the metadata is:

```
(name symbol decimals image_hash data_hash)
```

`image_hash` and `data_hash` are optional SHA-256 hashes of the icon and of a document with extra information (such as a description and links). Since the metadata is part of the asset id, two assets with the same TAIL but different metadata are different assets, and the metadata can never change.

Links to the image and data aren't part of the metadata. Content is checked against its hash, so links are only hints for where to download it, and anyone can publish or mirror them at any time without being trusted. This avoids the problem of links that break or need updating after issuance. To make the metadata easy to find, the driver adds a `REMARK` to issuance spends:

```
(1 "dog_metadata" TAIL_HASH METADATA IMAGE_URIS DATA_URIS)
```

An indexer can verify the metadata in it against the asset id, but the links in it are just as untrusted as any others.

### Revocation

A DOG can be made revocable, so that its issuer can claw back tokens from any holder. This doesn't need any changes to the DOG puzzles: the owner's p2 puzzle is wrapped in the same [revocation layer](https://github.com/Chia-Network/chia_puzzles/blob/main/puzzles/vc_puzzles/revocation_layer.clsp) that revocable CATs use, and the revocation layer is the DOG's inner puzzle.

```
                                                    ┌─▶ p2 puzzle (owner): outputs are wrapped in the revocation layer again
DOG coin ──▶ execution coin ──▶ revocation layer ──┤
                                                    └─▶ hidden puzzle (issuer): outputs aren't wrapped
```

The revocation layer's solution chooses whether to run the owner's p2 puzzle or the hidden puzzle (usually the issuer's standard puzzle). When the p2 puzzle runs, every coin it creates is wrapped in the revocation layer again, so the owner can't make tokens unrevocable. When the hidden puzzle runs, its outputs aren't wrapped, unless the hidden puzzle wraps them itself.

Since the revocation layer is just an inner puzzle:

- **Supply is still conserved.** The hidden puzzle's outputs go through the execution coin like any other inner puzzle's, so they're wrapped in DOG coins and counted in the ring. It can't create or destroy tokens, take part in another coin's ring, or skip the ring. Only the TAIL can change the supply, and revoking and melting are separate permissions (an issuer with both can do both in the same spend).
- **Revocation can't be tampered with.** If the hidden puzzle signs with `AGG_SIG_ME` (as the standard puzzle does), the signature commits to the execution coin and the hidden puzzle's output. Anyone can see the revocation layer's solution, but switching it to the p2 puzzle needs the owner's signature. Like any spend, a holder who sees a revocation in the mempool can try to spend their coin first, so a revocation is only final once it's confirmed.
- **Several coins can be revoked together.** Revocation spends are ordinary DOG spends, so they can share a ring with each other and with normal owner spends.
- **Offers work as they do for revocable CATs.** The settlement payments puzzle is wrapped in the revocation layer, so a requested payment of a revocable DOG is asserted from the execution coin with the inner puzzle `revocation_layer(HIDDEN_PUZZLE_HASH, SETTLEMENT_PAYMENT_HASH)`. A payment of unrevocable tokens of the same asset id, or of tokens with a different hidden puzzle, doesn't satisfy it. The issuer can revoke a coin while it's held by the settlement puzzle, which cancels the trade.

Revocability isn't part of the asset id, which only depends on the TAIL and metadata. The same asset can have revocable and unrevocable coins (for example, the issuer revokes tokens into an unrevocable coin), so like revocable CATs, wallets should treat the asset id and the hidden puzzle hash together as the asset. To make sure that every coin of an asset is revocable, issue it with a TAIL that only the issuer can run, and only into the revocation layer.

The DOG coin only commits to the hash of its inner puzzle, so wallets recognise revocable DOGs from the spend of the parent execution coin, which reveals the revocation layer. `Dog::parse_children` follows the same rules as `Cat::parse_children`: coins created by the p2 puzzle are revocable, and coins created by the hidden puzzle aren't, unless their puzzle hash is the revocation layer around the hint.

### Rules for inner puzzles

The execution coin changes the inner puzzle's conditions in two ways:

- Each `CREATE_COIN` is wrapped in the DOG coin puzzle with its amount curried, and created with 0 mojos. The amount must be between 0 and `u64::MAX`, and it's converted to canonical form. Memos are kept, but the puzzle hash changes with the amount, so wallets need a hint (usually the inner puzzle hash) to find the coin. The driver doesn't add hints, since the right hint depends on the inner puzzle.
- `SEND_MESSAGE` and `RECEIVE_MESSAGE` conditions with mode `0b000010` (from anyone to a puzzle hash) whose message starts with `0xd0` are rejected, since they could take the place of ring messages. All other conditions are passed through.

## Extending DOG

Features like transfer fees and credential restrictions are built as an inner puzzle **layer**, like they would be for a CAT. The layer wraps the holder's inner puzzle, and wraps every coin it creates in the same layer, so the rule applies to every future holder. The execution coin then wraps those coins in DOG coins as usual. A layer sees an ordinary coin, so it can use `ASSERT_MY_AMOUNT` to learn the token amount, and messages or `ASSERT_CONCURRENT_SPEND` to coordinate with other coins in the spend. It can also learn the asset id or its own puzzle hash by currying them in or asserting them with `ASSERT_MY_PUZZLE_HASH`, but it doesn't need to: the DOG coin commits to the inner puzzle hash, so a holder can't remove the layer.

There are two examples in [`puzzles/layers`](puzzles/layers), tested in [`tests/extensions.rs`](tests/extensions.rs). They're proofs of concept, and aren't part of the standard:

- [`transfer_fee`](puzzles/layers/transfer_fee.rue) pays the issuer a share of the tokens that leave a holder, in the token itself. The fee is charged on the coin's amount minus its change, not on what it sends, since otherwise two holders could spend together and have the receiver create the tokens as their own change.
- [`credential_restriction`](puzzles/layers/credential_restriction.rue) requires a credential (a signature from an authority over a puzzle hash and an expiry) for the holder and for every recipient. Revoking a credential, by not renewing it, freezes the holder's tokens.

Things to know when writing a layer:

- **Every coin of the asset must be in the layer.** The ring only checks the total, so a coin of the same asset outside the layer (even one worth 0 tokens) could join a spend and take the tokens out. The TAIL, or the eve coin's inner puzzle, must make sure that tokens are only issued into the layer. TAILs can't see the inner puzzle hash or its conditions, unlike CAT TAILs, so a TAIL that issues without a signature has to prove the inner puzzle hash with `ASSERT_MY_PUZZLE_HASH` itself.
- **Relative time conditions don't work in inner puzzles.** The execution coin is ephemeral, and consensus rejects relative time conditions (such as `ASSERT_SECONDS_RELATIVE`) on ephemeral coins. Absolute ones work, so vesting schedules and the standard clawback do too.
- **Messages to a puzzle hash from anyone that start with `0xd0` are rejected.** A layer that sends or receives a hash with mode `0b000010` should add a prefix byte, or it will fail 1 time in 256. Every other mode is unrestricted.
- `ASSERT_MY_COIN_ID`, `ASSERT_MY_PARENT_ID` and `AGG_SIG_ME` apply to the execution coin, whose parent is the DOG coin, so a layer that checks its own lineage needs to go back two coins.

## Puzzles

| Puzzle | Curried arguments | Solution |
| --- | --- | --- |
| [`dog.rue`](puzzles/dog.rue) | `INNER_PUZZLE_HASH AMOUNT ASSET` | `my_coin_id lineage_proof tail_spend` |
| [`dog_exec.rue`](puzzles/dog_exec.rue) | `INNER_PUZZLE ASSET` | `inner_solution my_amount parent_amount prev_subtotal next_inner_puzzle_hash` |

- `ASSET` is the pair `(EXEC_MOD_HASH . ASSET_ID)`. The arguments that differ between coins come first, so the hash of the rest is calculated once per spend.
- The DOG coin puzzle doesn't curry its own mod hash. The execution coin puzzle includes it as a constant (`DOG_MOD_HASH` in [`shared.rue`](puzzles/shared.rue)), and the DOG coin puzzle doesn't need it.
- `lineage_proof` is `(parent_parent_coin_info parent_inner_puzzle_hash parent_amount)` or nil.
- `tail_spend` is `(tail metadata_hash delta . tail_solution)` or nil.
- `next_inner_puzzle_hash` is the inner puzzle hash of the next execution coin in the ring.

The DOG coin outputs `ASSERT_MY_COIN_ID`, `CREATE_COIN` for the execution coin, a message to it with mode `0b010111` (its puzzle hash to the execution coin's id) containing the amount, `ASSERT_MY_PARENT_ID` if there's a lineage proof, and the TAIL's conditions. The execution coin receives that message, sends and receives a ring message, and outputs the inner puzzle's conditions.

## Cost

Spending a DOG takes two coin spends instead of one, and creates two coins per output. Measured with `cargo test --test costs -- --nocapture`:

| Spend | CAT | DOG | Ratio |
| --- | --- | --- | --- |
| 1 coin to 1 coin | 32,969,564 | 34,545,504 | 1.05x |
| 1 coin to 2 coins | 35,760,177 | 37,324,945 | 1.04x |
| 3 coins to 1 coin | 93,231,549 | 97,957,742 | 1.05x |
| 5 coins to 5 coins | 164,727,971 | 172,583,788 | 1.05x |
| Revocable, owner spend (1 coin to 1 coin) | 43,274,669 | 44,813,341 | 1.04x |
| Revocable, revocation spend (1 coin to 1 coin) | 43,251,953 | 44,790,625 | 1.04x |

Most of the cost is the 12,000 per byte of the puzzle reveals and solutions, which is what the mempool charges for. The DOG coin puzzle (650 bytes) and execution coin puzzle (947 bytes) are revealed once per coin, like the CAT puzzle. The rest of the difference is the `CREATE_COIN` for each execution coin (1,800,000) and the extra coin spend.

The revocation layer costs about the same for both (about 10.3 million), since it's the same puzzle.

The execution coins are ephemeral, but they still add coin records to the blockchain.

## Using the driver

```rust
use chia_wallet_sdk::prelude::*;
use dog::{Dog, DogMetadata, DogSpend, DogUris};

// Issue 1,000,000 tokens that can never be issued again.
let metadata = DogMetadata::new("Dogecoin", "DOGE", 8);
let conditions = Conditions::new().create_coin(owner, 1_000_000, ctx.hint(owner)?);
let issuance = Dog::single_issuance(ctx, parent_coin_id, &metadata, &DogUris::default(), 1_000_000, conditions)?;
// The parent coin must output `issuance.parent_conditions`.

// Spend DOG coins together, like CATs.
let children = Dog::spend_all(ctx, &[DogSpend::new(dog, inner_spend)])?;

// Issue revocable tokens, which the hidden puzzle (here, the issuer's standard puzzle) can claw back.
let issuance = Dog::single_issuance_revocable(ctx, parent_coin_id, issuer_puzzle_hash, &metadata, &DogUris::default(), 1_000_000, conditions)?;

// The owner spends a revocable DOG like any other, and the issuer revokes it with the hidden puzzle.
let children = Dog::spend_all(ctx, &[DogSpend::new(dog, owner_spend)])?;
let children = Dog::spend_all(ctx, &[DogSpend::revoke(dog, issuer_spend)])?;

// Find the DOG coins created by an execution coin's spend, to sync a wallet.
let children = Dog::parse_children(allocator, parent_coin, parent_puzzle, parent_solution)?;
```

## Development

The compiled puzzles in [`puzzles/dist`](puzzles/dist) are built from the main branch of Rue, which every lint is denied for in [`Rue.toml`](Rue.toml):

```bash
cargo install --git https://github.com/xch-dev/rue rue-cli
rue build --all --hash
rue test
rue fmt --check puzzles
```

After changing [`dog.rue`](puzzles/dog.rue), copy the new hash from `puzzles/dist/dog.rue.hash` into `DOG_MOD_HASH` in [`shared.rue`](puzzles/shared.rue), and build again.

`cargo test` checks that the compiled puzzles are up to date and have no lint diagnostics, and runs the tests:

| Tests | What they cover |
| --- | --- |
| [`transfers`](tests/transfers.rs) | Sending, splitting, merging, zero amounts, fees, several assets, wallet syncing |
| [`supply`](tests/supply.rs) | Issuing, melting, both standard TAILs, what the TAIL sees, `u64::MAX`, metadata |
| [`tails`](tests/tails.rs) | Issuing and melting with the other TAILs, and the supply changes they reject |
| [`offers`](tests/offers.rs) | XCH for DOG, DOG for XCH, DOG for DOG, underpaying, paying with the wrong asset |
| [`revocation`](tests/revocation.rs) | Issuing revocable DOGs, owner transfers, revoking without the owner, revoking several coins in a ring, revoking coins held by the settlement puzzle, offers, non-issuers can't revoke, revocation can't change supply, parsing children |
| [`attacks`](tests/attacks.rs) | Fake TAILs, negative and oversized outputs, forged DOG and execution coins, joining the ring, sending ring messages to the wrong coin, ring messages from inner puzzles, tampered solutions, replayed signatures |
| [`properties`](tests/properties.rs) | Random balanced rings conserve supply, and unbalanced rings are rejected |
| [`costs`](tests/costs.rs) | Cost compared to CATs, including revocable ones |
| [`puzzles`](tests/puzzles.rs) | The compiled puzzles match the Rue sources, with no lint diagnostics, and `DOG_MOD_HASH` is up to date |
| [`extensions`](tests/extensions.rs) | Example transfer fee and credential layers can't be bypassed, and which time conditions work in inner puzzles |
