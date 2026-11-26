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

    // Construct INSIDE its own budget, then measure ONLY the write loop.
    //
    // The previous shape wrapped construction + the whole loop + `finish` in
    // one window and allowed `< 64` allocations. With 4096 chunks, one
    // allocation every 128 chunks adds 32 - inside that budget - so the gate
    // could not see an allocation count that GROWS WITH THE STREAM, which is
    // exactly the regression it exists to catch.
    let mut w = CasWrite::new(&mut sink);
    let ((count, bytes), ()) = measure(|| {
        for _ in 0..CHUNKS {
            w.write_all(&chunk).expect("write");
        }
    });
    // `finish` has its own budget: it is not part of the per-chunk fold.
    let _ = measure(|| w.finish());

    // The fold itself: `extend_from_slice` into a pre-reserved buffer plus a
    // stack-only hasher update. ZERO, not "fewer than 64".
    assert_eq!(
        count, 0,
        "CasWrite::write_all must not allocate per chunk: {CHUNKS} chunks \
         produced {count} allocations"
    );
    assert_eq!(
        bytes, 0,
        "CasWrite::write_all allocated {bytes} bytes across {CHUNKS} chunks"
    );
    // Sanity: all bytes reached the sink.
    assert_eq!(sink.buf.len(), CHUNK * CHUNKS);
}
