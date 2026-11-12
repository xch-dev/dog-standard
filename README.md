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
- `SEND_MESSAGE` and `RECEIVE_MESSAGE` conditions with mode `0b000010` (from anyone to a puzzle hash) whose message starts with `0xd0` are rejected, since they could take the place of ring messages. All other conditions are passed through.

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

Most of the cost is the 12,000 per byte of the puzzle reveals and solutions, which is what the mempool charges for. The DOG coin puzzle (650 bytes) and execution coin puzzle (947 bytes) are revealed once per coin, like the CAT puzzle. The rest of the difference is the `CREATE_COIN` for each execution coin (1,800,000) and the extra coin spend.

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

After changing [`dog.rue`](puzzles/dog.rue), copy the new hash from `puzzles/dist/dog.rue.hash` into `DOG_MOD_HASH` in [`shared.rue`](puzzles/shared.rue), and build again.

`cargo test` checks that the compiled puzzles are up to date and have no lint diagnostics, and runs the tests:

| Tests | What they cover |
| --- | --- |
| [`transfers`](tests/transfers.rs) | Sending, splitting, merging, zero amounts, fees, several assets, wallet syncing |
| [`supply`](tests/supply.rs) | Issuing, melting, both standard TAILs, what the TAIL sees, `u64::MAX`, metadata |
| [`offers`](tests/offers.rs) | XCH for DOG, DOG for XCH, DOG for DOG, underpaying, paying with the wrong asset |
| [`attacks`](tests/attacks.rs) | Fake TAILs, negative and oversized outputs, forged DOG and execution coins, joining the ring, sending ring messages to the wrong coin, ring messages from inner puzzles, tampered solutions, replayed signatures |
| [`properties`](tests/properties.rs) | Random balanced rings conserve supply, and unbalanced rings are rejected |
| [`costs`](tests/costs.rs) | Cost compared to CATs |
| [`puzzles`](tests/puzzles.rs) | The compiled puzzles match the Rue sources, with no lint diagnostics, and `DOG_MOD_HASH` is up to date |
