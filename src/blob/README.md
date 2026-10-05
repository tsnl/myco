# myco::blob

Shared, content-addressed in-memory blobs for conversation history and inference requests.

```rust
use myco::blob::{Blob, BlobStore, MediaType};

let store = BlobStore::default();
let reader = store.clone();
let reference = store.insert(Blob {
    media_type: MediaType::Png,
    data: vec![0x89, b'P', b'N', b'G'].into(),
})?;
assert_eq!(reader.get(reference)?.media_type, MediaType::Png);
assert_eq!(store.insert(reader.get(reference)?)?, reference);
# Ok::<(), myco::blob::BlobError>(())
```

`BlobRef` wraps a 32-byte SHA-256 digest. `insert(blob)` computes the reference and
deduplicates identical blobs, preserving the existing bytes. References are stable
across stores. `BlobStore::clone` shares the registry, including subsequent inserts.

The digest covers `b"myco/blob/v1\0"`, the canonical MIME string from
`MediaType::as_str()`, a zero byte, then the blob bytes. Changing either media type
or bytes produces a different reference. This encoding is part of the reference
format. `MediaType` covers PNG, JPEG, GIF, WebP, plain text, and opaque binary data.

`get` returns an owned `Blob`, copying its media type and sharing its immutable
bytes through `Arc<[u8]>`. It holds no store lock after returning. The store uses
short synchronous read/write locks and supports access from concurrent clients.
Missing references are explicit errors. `BlobError::Poisoned` reports a panic
while the store's write lock was held; it does not describe the blob's contents.

The store is append-only. Dropping a generation or client does not remove blobs
from other handles. Dropping the last store handle releases the registry; any
previously returned blobs keep their bytes alive.

`thread` records references; `gen_ai` resolves selected references when encoding
requests. Loading, media verification, size limits, persistence, and authorization
belong to the application. The store performs no file or network I/O.

This store owns the canonical private copy. Provider uploads and remote file
handles belong to the `gen_ai` drivers; they do not change a `BlobRef` or enter
thread history. Local persistence and provider uploads are separate features.
