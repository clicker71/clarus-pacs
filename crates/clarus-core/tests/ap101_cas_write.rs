// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! ap101: the STOW folded-hash tee (`CasWrite`) must be 0-alloc per chunk.
//!
//! `CasWrite::write_all` = `inner.write_all` (into a pre-allocated sink) +
//! `hasher.update` (the facade, no heap). Constructing the wrapper (which
//! builds the hasher) and `finish` (which finalizes) are stack-only. So a
//! stream that writes N chunks costs a CONSTANT number of allocations - the
//! sink's own growth, which the caller sizes - never a per-chunk heap.
//!
//! This is the property that makes the 0410 fold free on the hot path: the
//! tee adds no allocation on top of the write it is already doing.

use clarus_core::hash::CasWrite;
use ferrite_testkit::heap::TestAllocator;

#[global_allocator]
static ALLOC: TestAllocator = TestAllocator::new();

fn measure<F, R>(f: F) -> ((usize, usize), R)
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
    ((after.0 - before.0, after.1 - before.1), result)
}

/// A `Vec<u8>` sink that cannot reallocate: pre-reserved to the exact size, so
/// every `write_all` in the measured window is pure `extend_from_slice` with
/// capacity already present. This isolates the TEE's cost (hashing) from the
/// sink's growth, which is the caller's budget.
struct FixedSink {
    buf: Vec<u8>,
    cap: usize,
}

impl FixedSink {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
        }
    }
}

impl std::io::Write for FixedSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn cas_write_fold_is_zero_alloc_per_chunk() {
    const CHUNKS: usize = 4096;
    const CHUNK: usize = 8192;
    let chunk = vec![0x5au8; CHUNK];

    // Warm-up (first-touch statics inside the hasher).
    {
        let mut sink = FixedSink::new(CHUNK * CHUNKS);
        let mut w = CasWrite::new(&mut sink);
        for _ in 0..CHUNKS {
            w.write_all(&chunk).expect("warmup write");
        }
        let (_d, _s) = w.finish();
    }

    let mut sink = FixedSink::new(CHUNK * CHUNKS);
    assert_eq!(sink.buf.capacity(), sink.cap, "sink must be pre-reserved");

    let ((count, bytes), _digest) = measure(|| {
        let mut w = CasWrite::new(&mut sink);
        for _ in 0..CHUNKS {
            w.write_all(&chunk).expect("write");
        }
        w.finish().0
    });

    // The measured window holds: N `extend_from_slice` (0 alloc, capacity
    // reserved) + the hasher update/finalize (stack-only). The ONLY allocations
    // allowed are the handful inside hasher construction/handling - a small
    // constant, NOT one-per-chunk.
    assert!(
        count < 64,
        "CasWrite fold must not allocate per chunk: {CHUNKS} chunks produced \
         {count} allocations"
    );
    assert!(
        bytes < 64 * 1024,
        "CasWrite fold allocated {bytes} bytes for {CHUNKS} stack-sized chunks"
    );
    // Sanity: all bytes reached the sink.
    assert_eq!(sink.buf.len(), CHUNK * CHUNKS);
}
