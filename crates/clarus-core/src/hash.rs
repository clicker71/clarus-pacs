// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Content-address hashing facade.
//!
//! All content-derived keys (CAS object addresses, manifest instance hashes,
//! shadow-tier rows, deterministic empty-field synthesis) flow through
//! [`CasHasher`] instead of a concrete hash crate. Today the only backend is
//! BLAKE3 ([`Blake3CasHasher`]); a different backend (e.g. SM3) can be
//! introduced later by swapping the constructor, without touching call sites.
//!
//! The facade is byte-identical to a direct BLAKE3 use: same incremental
//! `update` semantics, same 32-byte `finalize`, zero new dependencies.

/// A stateful content hash. `update` feeds bytes; `finalize` consumes the
/// hasher and returns the 32-byte digest.
pub trait CasHasher {
    /// Absorb more bytes into the running hash.
    fn update(&mut self, data: &[u8]);

    /// Finalize and return the 32-byte content digest.
    fn finalize(self) -> [u8; 32];
}

/// The production BLAKE3 backend for [`CasHasher`].
pub struct Blake3CasHasher(blake3::Hasher);

impl Blake3CasHasher {
    /// Construct a fresh BLAKE3 hasher.
    pub fn new() -> Self {
        Self(blake3::Hasher::new())
    }
}

impl Default for Blake3CasHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl CasHasher for Blake3CasHasher {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

/// One-shot convenience: hash a byte slice into a 32-byte digest.
pub fn cas_hash_one(data: &[u8]) -> [u8; 32] {
    let mut h = Blake3CasHasher::new();
    h.update(data);
    h.finalize()
}

/// The ONE backend-neutral constructor for a fresh content hasher.
///
/// Every content key MUST route through here (never `blake3::Hasher::new()`):
/// `cas_hash_one`, `blake3_of_file`, and the STOW folded stream all call it, so
/// a BLAKE3 -> SM3 backend swap changes this ONE body plus the return type and
/// no call site imports the concrete crate.
pub fn cas_hasher() -> Blake3CasHasher {
    Blake3CasHasher::new()
}

/// A zero-heap "tee" writer: every byte written to the inner writer is also
/// folded into the running content hash, so a stream that is written once is
/// hashed once (no second read).
///
/// Holds the FACADE type ([`Blake3CasHasher`]), never `blake3::*` directly -
/// the whole point of the facade (a backend swap must not touch this file's
/// callers). `write_all` is `inner.write_all` + `hasher.update`, both
/// allocation-free on a stack buffer, so a per-chunk write costs ZERO heap
/// (ap101).
pub struct CasWrite<W: std::io::Write> {
    inner: W,
    hasher: Blake3CasHasher,
}

impl<W: std::io::Write> CasWrite<W> {
    /// Wrap `inner`; the hasher is a fresh backend-neutral [`cas_hasher`].
    pub fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: cas_hasher(),
        }
    }

    /// Write `buf` to the inner writer and fold the SAME bytes into the hash.
    ///
    /// Hashing follows what actually reached the destination, not what the
    /// caller intended to send. `Write::write_all` may write a prefix and then
    /// fail; returning the error without folding that prefix in left the digest
    /// describing fewer bytes than the file on disk held, so a recovered writer
    /// could store a CAS address that does not name its own contents. The
    /// chunks are therefore hashed one successful write at a time.
    ///
    /// A caller that sees `Err` must treat the wrapper as unusable: the digest
    /// is not finished and the destination is a partial body.
    pub fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        let mut rest = buf;
        while !rest.is_empty() {
            match self.inner.write(rest) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "writing to the CAS sink returned 0 bytes",
                    ));
                }
                Ok(n) => {
                    // Only the bytes the destination accepted are hashed.
                    self.hasher.update(&rest[..n]);
                    rest = &rest[n..];
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Consume the wrapper, returning the final 32-byte digest and the inner
    /// writer (e.g. to drop/close the file).
    pub fn finish(self) -> ([u8; 32], W) {
        (self.hasher.finalize(), self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer that accepts a prefix of the buffer and then fails, the way a
    /// full disk or a short device write does.
    struct PartialThenFail {
        accept: usize,
        written: Vec<u8>,
        failed: bool,
    }

    impl std::io::Write for PartialThenFail {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.failed {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "no space left on device",
                ));
            }
            let n = self.accept.min(buf.len());
            self.written.extend_from_slice(&buf[..n]);
            self.failed = n < buf.len();
            if self.failed && n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::StorageFull,
                    "no space left on device",
                ));
            }
            Ok(n)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// C4: the digest must describe the bytes that reached the destination.
    ///
    /// `write_all` can accept a prefix and then fail. Returning the error
    /// without folding that prefix in left the digest shorter than the file,
    /// so a recovered writer could publish a CAS address that names bytes the
    /// object does not contain.
    #[test]
    fn a_partially_written_buffer_hashes_exactly_what_reached_the_sink() {
        let sink = PartialThenFail {
            accept: 5,
            written: Vec::new(),
            failed: false,
        };
        let mut w = CasWrite::new(sink);
        let body = b"0123456789";
        let err = w.write_all(body).expect_err("the sink runs out of space");
        assert_eq!(err.kind(), std::io::ErrorKind::StorageFull);

        let (digest, sink) = w.finish();
        assert_eq!(&sink.written, b"01234", "a 5-byte prefix landed");

        // The digest over the ACCEPTED prefix, not over the whole buffer.
        let expected = cas_hash_one(b"01234");
        assert_eq!(
            hex(&digest),
            hex(&expected),
            "the digest must cover exactly the bytes on disk"
        );
        let wrong = cas_hash_one(body);
        assert_ne!(
            hex(&digest),
            hex(&wrong),
            "hashing the whole buffer would name bytes the object lacks"
        );
    }

    /// The happy path is unchanged: one write, one digest over everything.
    #[test]
    fn a_complete_write_hashes_the_whole_buffer() {
        let mut w = CasWrite::new(Vec::new());
        w.write_all(b"hello").expect("write");
        let (digest, sink) = w.finish();
        assert_eq!(sink, b"hello");
        assert_eq!(hex(&digest), hex(&cas_hash_one(b"hello")));
    }

    fn hex(digest: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in digest {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    /// Known-answer vectors from the BLAKE3 reference test suite
    /// (https://github.com/BLAKE3-team/BLAKE3/blob/master/test_vectors/).
    /// Any dependency bump that changes these digests breaks every CAS
    /// key in the archive - this test pins the contract.
    #[test]
    fn cas_hash_matches_blake3_reference_vectors() {
        assert_eq!(
            hex(&cas_hash_one(b"")),
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        assert_eq!(
            hex(&cas_hash_one(b"abc")),
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85"
        );
    }
}
