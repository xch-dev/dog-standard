use chia_wallet_sdk::{
    clvm_traits::{FromClvmError, ToClvmError},
    clvmr::error::EvalErr,
    driver::DriverError,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DogError {
    #[error("driver error: {0}")]
    Driver(#[from] DriverError),

    #[error("failed to serialize clvm value: {0}")]
    ToClvm(#[from] ToClvmError),

    #[error("failed to deserialize clvm value: {0}")]
    FromClvm(#[from] FromClvmError),

    #[error("clvm eval error: {0}")]
    Eval(#[from] EvalErr),

    #[error("the amount of an execution coin must be between 0 and u64::MAX")]
    AmountOutOfRange,

    #[error("every DOG coin in a spend must have the same asset id")]
    AssetIdMismatch,

    #[error("the inner spend's puzzle doesn't match the DOG coin's inner puzzle hash")]
    InnerPuzzleHashMismatch,

    #[error("the created coins don't add up to the amount of the execution coins (off by {0})")]
    UnbalancedSpend(i128),

    #[error("an inner puzzle created a coin that isn't a valid CREATE_COIN condition")]
    UnsupportedCreateCoin,

    #[error("only revocable DOGs can be spent with the hidden puzzle")]
    NotRevocable,

    #[error("the curried mod hashes don't match the DOG puzzles")]
    InvalidModHash,

    #[error("the coin spend isn't a valid spend of the asset's metadata updater")]
    InvalidUpdaterSpend,

    #[error("the updater's state isn't exactly a DogMetadataState")]
    InvalidMetadataState,
}
