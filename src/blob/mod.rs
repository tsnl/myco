#![doc = include_str!("README.md")]

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use sha2::{Digest, Sha256};

//
// Blobs
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobRef(pub [u8; 32]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub media_type: MediaType,
    pub data: Arc<[u8]>,
}

impl Blob {
    fn reference(&self) -> BlobRef {
        let hash = Sha256::new()
            .chain_update(b"myco/blob/v1\0")
            .chain_update(self.media_type.as_str())
            .chain_update(b"\0")
            .chain_update(&self.data);
        BlobRef(hash.finalize().into())
    }
}

//
// Media types
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaType {
    Png,
    Jpeg,
    Gif,
    WebP,
    PlainText,
    OctetStream,
}

impl MediaType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::WebP => "image/webp",
            Self::PlainText => "text/plain",
            Self::OctetStream => "application/octet-stream",
        }
    }
}

//
// BlobStore
//

#[derive(Debug, Clone, Default)]
pub struct BlobStore {
    blobs: Arc<RwLock<HashMap<BlobRef, Blob>>>,
}

impl BlobStore {
    pub fn insert(&self, blob: Blob) -> Result<BlobRef, BlobError> {
        let reference = blob.reference();
        let mut blobs = self.blobs.write().map_err(|_| BlobError::Poisoned)?;
        blobs.entry(reference).or_insert(blob);
        Ok(reference)
    }
    pub fn get(&self, reference: BlobRef) -> Result<Blob, BlobError> {
        self.blobs
            .read()
            .map_err(|_| BlobError::Poisoned)?
            .get(&reference)
            .cloned()
            .ok_or(BlobError::Missing(reference))
    }
}

//
// Errors
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BlobError {
    #[error("missing blob {0:?}")]
    Missing(BlobRef),
    #[error("a panic occurred while holding the blob store's write lock")]
    Poisoned,
}
