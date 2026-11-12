use chia_wallet_sdk::{
    chia::protocol::Bytes32,
    clvm_traits::{FromClvm, ToClvm},
    clvm_utils::{ToTreeHash, TreeHash, tree_hash_pair},
};

/// The metadata of a DOG asset, which is committed to by its asset id.
///
/// The asset id is the tree hash of the pair `(TAIL . METADATA)`, so anyone can check that
/// metadata belongs to an asset, and it can never change. Links to the image and the extra data
/// are left out on purpose: they're only hints for where to download content whose hash is
/// committed to here, so they can be published or mirrored by anyone, at any time.
#[derive(Debug, Clone, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct DogMetadata {
    /// The display name, such as `"Spacebucks"`.
    pub name: String,
    /// The ticker symbol, such as `"SBX"`.
    pub symbol: String,
    /// The number of decimal places that the token amount is displayed with.
    pub decimals: u8,
    /// The SHA-256 hash of the icon image.
    pub image_hash: Option<Bytes32>,
    /// The SHA-256 hash of a document with extra information, such as a description and links.
    pub data_hash: Option<Bytes32>,
}

impl DogMetadata {
    pub fn new(name: impl Into<String>, symbol: impl Into<String>, decimals: u8) -> Self {
        Self {
            name: name.into(),
            symbol: symbol.into(),
            decimals,
            image_hash: None,
            data_hash: None,
        }
    }

    #[must_use]
    pub fn with_image_hash(mut self, image_hash: Bytes32) -> Self {
        self.image_hash = Some(image_hash);
        self
    }

    #[must_use]
    pub fn with_data_hash(mut self, data_hash: Bytes32) -> Self {
        self.data_hash = Some(data_hash);
        self
    }
}

/// Calculates the asset id from the tree hashes of the TAIL and the metadata.
pub fn asset_id(tail_hash: TreeHash, metadata_hash: TreeHash) -> Bytes32 {
    tree_hash_pair(tail_hash, metadata_hash).into()
}

/// The first value of a [`DogMetadataRemark`], which identifies it.
pub const DOG_METADATA_REMARK_KIND: &str = "dog_metadata";

/// The contents of a `REMARK` condition that publishes an asset's metadata on chain, along with
/// links to its content, in the form `(1 "dog_metadata" TAIL_HASH METADATA IMAGE_URIS DATA_URIS)`.
///
/// It's added to issuance spends by [`Dog::issue`](crate::Dog::issue) so that indexers can find the
/// metadata of an asset without a separate registry. The metadata can be verified against the
/// asset id, and the content against its hashes, but the links themselves aren't authenticated.
#[derive(Debug, Clone, PartialEq, Eq, ToClvm, FromClvm)]
#[clvm(list)]
pub struct DogMetadataRemark {
    /// Always [`DOG_METADATA_REMARK_KIND`].
    pub kind: String,
    pub tail_hash: Bytes32,
    pub metadata: DogMetadata,
    pub image_uris: Vec<String>,
    pub data_uris: Vec<String>,
}

impl DogMetadataRemark {
    pub fn new(
        tail_hash: Bytes32,
        metadata: DogMetadata,
        image_uris: Vec<String>,
        data_uris: Vec<String>,
    ) -> Self {
        Self {
            kind: DOG_METADATA_REMARK_KIND.to_string(),
            tail_hash,
            metadata,
            image_uris,
            data_uris,
        }
    }

    /// Whether this is a metadata remark, rather than another remark with the same shape.
    pub fn is_valid(&self) -> bool {
        self.kind == DOG_METADATA_REMARK_KIND
    }

    /// The asset id that the published metadata belongs to.
    pub fn asset_id(&self) -> Bytes32 {
        asset_id(self.tail_hash.into(), self.metadata.tree_hash())
    }
}
