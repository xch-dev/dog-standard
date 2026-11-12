//! DOG is a fungible token standard for the Chia blockchain.
//!
//! Like a CAT, a DOG asset is defined by its TAIL, which decides when tokens can be issued or
//! melted, and tokens are controlled by an inner puzzle. Unlike a CAT, tokens don't need to be
//! backed by mojos:
//!
//! - A DOG coin always has an amount of 0 mojos, and its token amount is curried into its puzzle,
//!   so the supply can be anything up to `u64::MAX` per coin.
//! - Spending a DOG coin creates an execution coin whose amount in mojos is the token amount, and
//!   which is spent in the same block. Its mojos are created and destroyed within the transaction,
//!   so they never need to exist. The inner puzzle runs in the execution coin, so it sees a normal
//!   coin with a real amount and a puzzle hash that doesn't depend on the amount. This keeps DOGs
//!   compatible with offers and other inner puzzles.
//! - The asset id commits to [`DogMetadata`], such as the name, symbol, and decimals.
//!
//! See the Rue sources in `puzzles/` for the details of how the puzzles work.

mod dog;
mod error;
mod metadata;
mod puzzles;

pub use dog::*;
pub use error::*;
pub use metadata::*;
pub use puzzles::*;
