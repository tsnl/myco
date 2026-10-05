use std::sync::{Arc, Barrier};

use myco::blob::{Blob, BlobRef, BlobStore, MediaType};

//
// Shared registry
//

#[test]
fn cloned_handles_observe_later_insertions_and_share_bytes() {
    let store = BlobStore::default();
    let reader = store.clone();
    let blob = blob(1);
    let reference = store.insert(blob.clone()).unwrap();
    let loaded = reader.get(reference).unwrap();
    assert!(Arc::ptr_eq(&loaded.data, &blob.data));
    drop(store);
    assert_eq!(reader.get(reference), Ok(blob));
    drop(reader);
    assert_eq!(loaded.data.as_ref(), &[1]);
}

#[test]
fn modifying_a_returned_blob_does_not_rebind_the_registry() {
    let store = BlobStore::default();
    let original = blob(1);
    let reference = store.insert(original.clone()).unwrap();
    let mut loaded = store.get(reference).unwrap();
    loaded.media_type = MediaType::Jpeg;
    Arc::make_mut(&mut loaded.data)[0] = 2;
    let changed = store.insert(loaded.clone()).unwrap();
    assert_ne!(changed, reference);
    assert_eq!(store.get(reference), Ok(original));
    assert_eq!(store.get(changed), Ok(loaded));
}

//
// Content addressing
//

#[test]
fn identical_content_deduplicates_without_replacing_existing_bytes() {
    let store = BlobStore::default();
    let original = blob(1);
    let reference = store.insert(original.clone()).unwrap();
    assert_eq!(store.insert(blob(1)), Ok(reference));
    assert!(Arc::ptr_eq(
        &store.get(reference).unwrap().data,
        &original.data
    ));
}

#[test]
fn content_references_have_a_stable_sha256_encoding_across_stores() {
    let expected = BlobRef([
        179, 145, 203, 147, 50, 33, 202, 195, 97, 158, 217, 60, 96, 195, 205, 78, 96, 167, 145, 19,
        24, 98, 36, 38, 90, 26, 89, 240, 249, 74, 28, 135,
    ]);
    for store in [BlobStore::default(), BlobStore::default()] {
        assert_eq!(store.insert(blob(1)), Ok(expected));
        assert_eq!(store.get(expected), Ok(blob(1)));
    }
}

#[test]
fn different_bytes_or_media_types_have_distinct_references() {
    let store = BlobStore::default();
    let png = store.insert(blob(1)).unwrap();
    let other_bytes = store.insert(blob(2)).unwrap();
    let jpeg = store
        .insert(Blob {
            media_type: MediaType::Jpeg,
            ..blob(1)
        })
        .unwrap();
    assert_ne!(png, other_bytes);
    assert_ne!(png, jpeg);
    assert_ne!(jpeg, other_bytes);
}

#[test]
fn concurrent_insertions_of_identical_content_return_the_same_reference() {
    let store = BlobStore::default();
    let barrier = Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let writers = [(), ()].map(|_| {
            let store = store.clone();
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                store.insert(blob(1)).unwrap()
            })
        });
        writers.map(|writer| writer.join().unwrap())
    });
    assert_eq!(results[0], results[1]);
    assert_eq!(store.get(results[0]), Ok(blob(1)));
}

//
// Fixtures
//

fn blob(value: u8) -> Blob {
    Blob {
        media_type: MediaType::Png,
        data: vec![value].into(),
    }
}
