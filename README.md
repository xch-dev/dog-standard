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

### Rules for inner puzzles

The execution coin changes the inner puzzle's conditions in two ways:

- Each `CREATE_COIN` is wrapped in the DOG coin puzzle with its amount curried, and created with 0 mojos. The amount must be between 0 and `u64::MAX`, and it's converted to canonical form. Memos are kept, but the puzzle hash changes with the amount, so wallets need a hint (usually the inner puzzle hash) to find the coin. The driver doesn't add hints, since the right hint depends on the inner puzzle.
- `SEND_MESSAGE` and `RECEIVE_MESSAGE` conditions with mode `0b111111` (coin id to coin id) whose message starts with `0xd0` are rejected, since they could take the place of ring messages. All other conditions are passed through.

## Extending DOG

Features like transfer fees and credential restrictions are built as an inner puzzle **layer**, like they would be for a CAT. The layer wraps the holder's inner puzzle, and wraps every coin it creates in the same layer, so the rule applies to every future holder. The execution coin then wraps those coins in DOG coins as usual. A layer sees an ordinary coin, so it can use `ASSERT_MY_AMOUNT` to learn the token amount, and messages or `ASSERT_CONCURRENT_SPEND` to coordinate with other coins in the spend. It can also learn the asset id or its own puzzle hash by currying them in or asserting them with `ASSERT_MY_PUZZLE_HASH`, but it doesn't need to: the DOG coin commits to the inner puzzle hash, so a holder can't remove the layer.

There are two examples in [`puzzles/layers`](puzzles/layers), tested in [`tests/extensions.rs`](tests/extensions.rs). They're proofs of concept, and aren't part of the standard:

- [`transfer_fee`](puzzles/layers/transfer_fee.rue) pays the issuer a share of the tokens that leave a holder, in the token itself. The fee is charged on the coin's amount minus its change, not on what it sends, since otherwise two holders could spend together and have the receiver create the tokens as their own change.
- [`credential_restriction`](puzzles/layers/credential_restriction.rue) requires a credential (a signature from an authority over a puzzle hash and an expiry) for the holder and for every recipient. Revoking a credential, by not renewing it, freezes the holder's tokens.

Things to know when writing a layer:

- **Every coin of the asset must be in the layer.** The ring only checks the total, so a coin of the same asset outside the layer (even one worth 0 tokens) could join a spend and take the tokens out. The TAIL, or the eve coin's inner puzzle, must make sure that tokens are only issued into the layer. TAILs can't see the inner puzzle hash or its conditions, unlike CAT TAILs, so a TAIL that issues without a signature has to prove the inner puzzle hash with `ASSERT_MY_PUZZLE_HASH` itself.
- **Relative time conditions don't work in inner puzzles.** The execution coin is ephemeral, and consensus rejects relative time conditions (such as `ASSERT_SECONDS_RELATIVE`) on ephemeral coins. Absolute ones work, so vesting schedules and the standard clawback do too.
- **Coin to coin messages that start with `0xd0` are rejected.** A layer that sends or receives a hash with mode `0b111111` should add a prefix byte, or it will fail 1 time in 256.
- `ASSERT_MY_COIN_ID`, `ASSERT_MY_PARENT_ID` and `AGG_SIG_ME` apply to the execution coin, whose parent is the DOG coin, so a layer that checks its own lineage needs to go back two coins.

## Puzzles

| Puzzle | Curried arguments | Solution |
| --- | --- | --- |
| [`dog.rue`](puzzles/dog.rue) | `DOG_MOD_HASH EXEC_MOD_HASH ASSET_ID INNER_PUZZLE_HASH AMOUNT` | `my_coin_id lineage_proof tail_spend` |
| [`dog_exec.rue`](puzzles/dog_exec.rue) | `DOG_MOD_HASH EXEC_MOD_HASH ASSET_ID INNER_PUZZLE` | `inner_solution my_amount parent_amount prev_coin_id prev_subtotal next_coin_proof` |

- `lineage_proof` is `(parent_parent_coin_info parent_inner_puzzle_hash parent_amount)` or nil.
- `tail_spend` is `(tail metadata_hash delta . tail_solution)` or nil.
- `next_coin_proof` is `(parent_coin_info inner_puzzle_hash amount)` of the next execution coin.

The DOG coin outputs `ASSERT_MY_COIN_ID`, `CREATE_COIN` for the execution coin, a message to it with mode `0b010111` (its puzzle hash to the execution coin's id) containing the amount, `ASSERT_MY_PARENT_ID` if there's a lineage proof, and the TAIL's conditions. The execution coin receives that message, sends and receives a ring message, and outputs the inner puzzle's conditions.

## Cost

Spending a DOG takes two coin spends instead of one, and creates two coins per output. Measured with `cargo test --test costs -- --nocapture`:

| Spend | CAT | DOG | Ratio |
| --- | --- | --- | --- |
| 1 coin to 1 coin | 32,969,564 | 41,929,229 | 1.27x |
| 1 coin to 2 coins | 35,760,177 | 44,730,102 | 1.25x |
| 3 coins to 1 coin | 93,231,549 | 120,042,053 | 1.29x |
| 5 coins to 5 coins | 164,727,971 | 209,478,413 | 1.27x |

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

`cargo test` checks that the compiled puzzles are up to date and have no lint diagnostics, and runs the tests:

| Tests | What they cover |
| --- | --- |
| [`transfers`](tests/transfers.rs) | Sending, splitting, merging, zero amounts, fees, several assets, wallet syncing |
| [`supply`](tests/supply.rs) | Issuing, melting, both standard TAILs, what the TAIL sees, `u64::MAX`, metadata |
| [`offers`](tests/offers.rs) | XCH for DOG, DOG for XCH, DOG for DOG, underpaying, paying with the wrong asset |
| [`attacks`](tests/attacks.rs) | Fake TAILs, negative and oversized outputs, forged DOG and execution coins, joining the ring, ring messages from inner puzzles, tampered solutions, replayed signatures |
| [`properties`](tests/properties.rs) | Random balanced rings conserve supply, and unbalanced rings are rejected |
| [`costs`](tests/costs.rs) | Cost compared to CATs |
| [`puzzles`](tests/puzzles.rs) | The compiled puzzles match the Rue sources, with no lint diagnostics |
| [`extensions`](tests/extensions.rs) | Example transfer fee and credential layers can't be bypassed, and which time conditions work in inner puzzles |
