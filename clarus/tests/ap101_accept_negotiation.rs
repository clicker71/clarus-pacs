// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: Accept classification on the negotiated retrieve path is free.
//!
//! `classify_retrieve_accept` runs on EVERY GET of the overloaded study,
//! series and instance resources, so it must not allocate - the media types
//! are compared in place, case-insensitively, over the header's ranges.

use clarus_server::dispatch_streams::{RetrieveWanted, classify_retrieve_accept};
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

fn allocate_env<F, R>(f: F) -> (bool, R)
where
    F: FnOnce() -> R,
{
    ALLOC.set_enabled(false);
    ALLOC.reset();
    ALLOC.set_enabled(true);
    let before = ALLOC.snapshot();
    let result = f();
    let after = ALLOC.snapshot();
    ALLOC.set_enabled(false);
    (before == after, result)
}

/// Every branch of the classifier: JSON, WADO-RS, wildcard default, foreign
/// 406, parameters, mixed case, and an absent header. Zero allocations over
/// the whole matrix.
#[test]
fn accept_classification_alloc_zero() {
    let cases: [(&str, RetrieveWanted); 10] = [
        ("application/dicom+json", RetrieveWanted::Json),
        ("application/dicom+json;q=0.9", RetrieveWanted::Json),
        (
            "application/dicom+json, multipart/related; type=\"application/dicom\"",
            RetrieveWanted::Json,
        ),
        (
            "multipart/related; type=\"application/dicom\"",
            RetrieveWanted::Wado,
        ),
        ("application/dicom", RetrieveWanted::Wado),
        ("*/*", RetrieveWanted::Wado),
        ("application/*", RetrieveWanted::Wado),
        (
            "MULTIPART/RELATED; TYPE=\"APPLICATION/DICOM\"",
            RetrieveWanted::Wado,
        ),
        ("text/html", RetrieveWanted::Unacceptable),
        ("text/html, application/dicom+json", RetrieveWanted::Json),
    ];
    let (zero, ()) = allocate_env(|| {
        for (accept, wanted) in cases {
            assert_eq!(classify_retrieve_accept(Some(accept)), wanted);
        }
        assert_eq!(classify_retrieve_accept(None), RetrieveWanted::Wado);
    });
    assert!(zero, "Accept classification must not allocate");
}
