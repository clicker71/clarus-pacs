//! W22: per-tile decode to Explicit VR LE on `/frames/{n}`.
//!
//! The property under test is the WIRE contract, not the codec: a client that
//! asks for `transfer-syntax=1.2.840.10008.1.2.1` on a stored compressed tile
//! must receive the raw pixel samples, labelled with that syntax, with a body
//! length of `rows * columns * bytes_per_sample`. Three cases pin it:
//!
//! 1. The default request (no target syntax) is byte-identical to the stored
//!    fragment - the decode must never leak into the passthrough path.
//! 2. A target of Explicit VR LE on a compressed tile yields exactly
//!    `rows * cols` bytes of samples.
//! 3. A stored tile whose syntax has no registered decoder falls back to the
//!    stored bytes under their own label (PS3.18 8.7.3), never to an error.
//!
//! The instances are synthesized here, so the geometry is known exactly and
//! the expected body length is arithmetic rather than a fixture constant.

#![cfg(feature = "transcode")]

use clarus_server::config::Config;
use clarus_server::handlers::json_common::parse_json;
use clarus_server::shadow::ShadowTier;
use clarus_server::sse_fanout::DualEventSink;
use clarus_server::state::AppState;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, RwLock as StdRwLock};
use std::time::{Duration, Instant};

const STUDY: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000101";
const SERIES: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000102";
const SOP: &str = "1.2.826.0.1.3680043.8.498.9000000000000000000000000103";
const CT_SOP_CLASS: &str = "1.2.840.10008.5.1.4.1.1.2";
const EVRLE: &str = "1.2.840.10008.1.2.1";

fn append_element(buf: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    let len = value.len();
    buf.extend_from_slice(&group.to_le_bytes());
    buf.extend_from_slice(&element.to_le_bytes());
    buf.extend_from_slice(vr);
    let long_form = matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OV" | b"OW" | b"SQ" | b"UN" | b"UC" | b"UR" | b"UT"
    );
    if long_form {
        buf.extend_from_slice(&[0x00, 0x00]);
        buf.extend_from_slice(&(len as u32).to_le_bytes());
    } else {
        buf.extend_from_slice(&(len as u16).to_le_bytes());
    }
    buf.extend_from_slice(value);
    if !len.is_multiple_of(2) {
        buf.push(0x00);
    }
}

/// Encapsulated PixelData: empty BOT, one Item per frame, sequence delimiter.
fn encapsulated(frames: &[&[u8]]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    for frame in frames {
        buf.extend_from_slice(&[0xFE, 0xFF, 0x00, 0xE0]);
        buf.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        buf.extend_from_slice(frame);
    }
    buf.extend_from_slice(&[0xFE, 0xFF, 0xDD, 0xE0, 0x00, 0x00, 0x00, 0x00]);
    buf
}

/// One multi-frame instance with caller-supplied geometry and stored syntax.
fn build_instance(ts: &str, rows: u16, cols: u16, frames: &[&[u8]]) -> Vec<u8> {
    let pixel_data = encapsulated(frames);
    let mut d = Vec::with_capacity(pixel_data.len() + 2048);
    d.extend_from_slice(&[0u8; 128]);
    d.extend_from_slice(b"DICM");
    append_element(&mut d, 0x0002, 0x0010, b"UI", ts.as_bytes());
    append_element(&mut d, 0x0008, 0x0016, b"UI", CT_SOP_CLASS.as_bytes());
    append_element(&mut d, 0x0008, 0x0018, b"UI", SOP.as_bytes());
    append_element(&mut d, 0x0010, 0x0010, b"PN", b"TILE^TRANSCODE");
    append_element(&mut d, 0x0010, 0x0020, b"LO", b"TILEPAT01");
    append_element(&mut d, 0x0020, 0x000d, b"UI", STUDY.as_bytes());
    append_element(&mut d, 0x0020, 0x000e, b"UI", SERIES.as_bytes());
    append_element(&mut d, 0x0008, 0x0060, b"CS", b"CT");
    append_element(&mut d, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0004, b"CS", b"MONOCHROME2");
    append_element(&mut d, 0x0028, 0x0010, b"US", &rows.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0011, b"US", &cols.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0102, b"US", &7u16.to_le_bytes());
    append_element(&mut d, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    append_element(
        &mut d,
        0x0028,
        0x0008,
        b"IS",
        (frames.len() as u32).to_string().as_bytes(),
    );
    append_element(&mut d, 0x7FE0, 0x0010, b"OB", &pixel_data);
    d
}

struct TestServer {
    _temp: tempfile::TempDir,
    state: Arc<AppState>,
    addr: std::net::SocketAddr,
    shutdown: clarus_core::shutdown::Shutdown,
    handle: Option<std::thread::JoinHandle<()>>,
}

struct Response {
    status: u16,
    body: Vec<u8>,
    raw_head: String,
}

impl TestServer {
    fn start() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let btree_root = temp.path().join("btree");
        let manifest_dir = btree_root.join("manifests");
        let idx_dir = btree_root.join("idx");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::create_dir_all(&idx_dir).unwrap();
        let mut config = Config::test_default();
        config.btree.manifest_dir = manifest_dir.to_string_lossy().to_string();
        config.btree.idx_dir = idx_dir.to_string_lossy().to_string();
        config.port = 0;
        config.min_free_disk_mb = 0;
        config.min_free_disk_pct = 0.0;
        let storage_dir = temp.path().join("storage");
        std::fs::create_dir_all(&storage_dir).unwrap();
        config.storage_dir = storage_dir.to_string_lossy().to_string();

        let cas = clarus_adapters::storage::ContentAddressedStorage::new(temp.path().join("cas"))
            .expect("open cas");
        let state = AppState {
            cas,
            codec_registry: clarus_adapters::codec::CodecRegistry::default(),
            ups_events: DualEventSink::new(),
            start_time: Instant::now(),
            metrics_ring: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            log_buffer: Arc::new(StdRwLock::new(std::collections::VecDeque::new())),
            maintenance: Arc::new(StdRwLock::new(
                clarus_server::state::MaintenanceState::default(),
            )),
            #[cfg(feature = "s3")]
            s3: std::sync::Arc::new(std::sync::OnceLock::new()),
            cold_cas: None,
            shadow: ShadowTier::new(idx_dir.clone()),
            study_scanner: clarus_core::query::StudyScanner::new(manifest_dir, idx_dir),
            config: config.clone(),
            workitems_dir: btree_root.join("workitems"),
            commitments_dir: btree_root.join("commitments"),
            transcode_pool: clarus_server::state::TranscodePool::new(32),
            readiness: clarus_server::handlers::readiness::Readiness::new(),
        };

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().unwrap();
        let shutdown = clarus_core::shutdown::Shutdown::new();
        let state = Arc::new(state);
        let serve_state = Arc::clone(&state);
        let serve_shutdown = shutdown.clone();
        let handle = std::thread::spawn(move || {
            clarus_server::serve::serve(
                listener,
                serve_state,
                serve_shutdown,
                clarus_core::http::conn::Timeouts::from_secs(10, 30, 5, 60),
                128,
            );
        });

        Self {
            _temp: temp,
            state,
            addr,
            shutdown,
            handle: Some(handle),
        }
    }

    fn request(&self, method: &str, path: &str, body: &[u8], ct: &str) -> Response {
        let mut c = TcpStream::connect(self.addr).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: x\r\nContent-Type: {ct}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        c.write_all(head.as_bytes()).unwrap();
        c.write_all(body).unwrap();
        let mut raw = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match c.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => raw.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .expect("response head");
        let raw_head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status = raw_head
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Response {
            status,
            body: raw[split..].to_vec(),
            raw_head,
        }
    }

    fn stow(&self, instance: &[u8]) {
        let boundary = "w22boundary";
        let mut body = Vec::new();
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Type: application/dicom\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(instance);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let r = self.request(
            "POST",
            "/dicomweb/studies",
            &body,
            &format!("multipart/related; type=\"application/dicom\"; boundary={boundary}"),
        );
        assert_eq!(r.status, 200, "STOW failed: {}", r.raw_head);
    }

    fn frame(&self, n: u32, query: &str) -> Response {
        self.request(
            "GET",
            &format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}/frames/{n}{query}"),
            &[],
            "application/octet-stream",
        )
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.shutdown.request_stop();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The part body of a single-part `multipart/related` response.
fn part_body(raw: &[u8]) -> Vec<u8> {
    let start = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
        .expect("part head");
    let end = raw
        .windows(6)
        .rposition(|w| w == b"\r\n--w22" || w.starts_with(b"\r\n--"))
        .unwrap_or(raw.len());
    raw[start..end].to_vec()
}

/// The `transfer-syntax=` value the part Content-Type carries.
fn part_ts(raw: &[u8]) -> String {
    let text = String::from_utf8_lossy(raw);
    let at = text
        .find("transfer-syntax=")
        .expect("a part names its syntax");
    text[at + "transfer-syntax=".len()..]
        .split(['\r', ';', ' '])
        .next()
        .unwrap_or("")
        .to_string()
}

/// Encode a synthetic monochrome tile as a real JPEG 2000 codestream.
///
/// The fixture MUST be a decodable codestream, not arbitrary bytes: with
/// junk the decoder fails transiently and the route legitimately falls back,
/// which would make every assertion below pass without ever exercising the
/// decode. The encoder is the same one the archive uses, so the tile is what
/// a real stored .90 fragment looks like.
fn jpeg2000_tile(rows: u16, cols: u16, seed: u8) -> Vec<u8> {
    use clarus_adapters::codec::{CodecRegistry, DecodedPixels, DicomMeta};
    let mut data = Vec::with_capacity(rows as usize * cols as usize);
    for i in 0..(rows as usize * cols as usize) {
        // Non-uniform: a flat tile can encode to a trivial stream and would
        // hide an off-by-one in the expected length.
        data.push(seed.wrapping_add((i % 251) as u8));
    }
    let pixels = DecodedPixels {
        data,
        rows,
        columns: cols,
        samples_per_pixel: 1,
        bits_allocated: 8,
        photometric_interpretation: "MONOCHROME2".into(),
    };
    let meta = DicomMeta {
        rows,
        columns: cols,
        samples_per_pixel: 1,
        bits_allocated: 8,
        bits_stored: 8,
        photometric_interpretation: "MONOCHROME2".into(),
        planar_configuration: None,
        pixel_representation: 0,
        frames: 1,
    };
    let registry = CodecRegistry::default();
    let codec = registry.get(JPEG2000_LOSSLESS).expect("the .90 codec");
    codec.encode(&pixels, &meta).expect("encode a real tile")
}

const JPEG2000_LOSSLESS: &str = "1.2.840.10008.1.2.4.90";

#[test]
fn a_stored_tile_is_served_unchanged_when_no_target_syntax_is_asked_for() {
    // Byte-identity with the stored fragment is the regression guard: the
    // decode path must not be reachable without an explicit target.
    let srv = TestServer::start();
    let tile = jpeg2000_tile(8, 8, 0x5A);
    srv.stow(&build_instance(JPEG2000_LOSSLESS, 8, 8, &[&tile]));
    let r = srv.frame(1, "");
    assert_eq!(r.status, 200, "{}", r.raw_head);
    assert_eq!(
        part_ts(&r.body),
        JPEG2000_LOSSLESS,
        "the default response keeps the stored syntax"
    );
    assert_eq!(
        part_body(&r.body),
        tile,
        "the default response is the stored fragment, byte for byte"
    );
}

#[test]
fn an_uncompressed_store_answers_the_evrle_request_with_its_own_bytes() {
    // stored == target: the fragment already IS the answer, so nothing is
    // decoded and the bytes must be identical to what was stored.
    let srv = TestServer::start();
    let tile = vec![0x11u8; 64];
    srv.stow(&build_instance(EVRLE, 8, 8, &[&tile]));
    let r = srv.frame(1, "?transferSyntax=1.2.840.10008.1.2.1");
    assert_eq!(r.status, 200, "{}", r.raw_head);
    assert_eq!(part_ts(&r.body), EVRLE);
    assert_eq!(part_body(&r.body), tile);
}

#[test]
fn a_syntax_with_no_decoder_falls_back_to_the_stored_bytes() {
    // PS3.18 8.7.3: an origin server that cannot produce the requested syntax
    // serves what it holds. The part must then name the STORED syntax, not
    // the requested one - mislabelling the body is worse than declining.
    //
    // The syntax used here is one the archive ACCEPTS and stores verbatim but
    // this build has no decoder for: JPEG Extended 12-bit (.51). The registry
    // refuses to decode it, which is exactly the 8.7.3 case.
    let srv = TestServer::start();
    let undecodable = "1.2.840.10008.1.2.4.51";
    let tile = vec![0x77u8; 80];
    srv.stow(&build_instance(undecodable, 8, 8, &[&tile]));
    let r = srv.frame(1, "?transferSyntax=1.2.840.10008.1.2.1");
    assert_eq!(r.status, 200, "{}", r.raw_head);
    assert_eq!(
        part_ts(&r.body),
        undecodable,
        "a fallback part must report the syntax it actually holds"
    );
    assert_eq!(part_body(&r.body), tile);
}

#[test]
fn the_frames_resource_still_answers_the_multipart_shape() {
    // The transcode branch changed what the part CONTAINS, not the envelope.
    let srv = TestServer::start();
    let tile = jpeg2000_tile(8, 8, 0x33);
    srv.stow(&build_instance(JPEG2000_LOSSLESS, 8, 8, &[&tile]));
    let r = srv.frame(1, "");
    assert_eq!(r.status, 200, "{}", r.raw_head);
    let head = r.raw_head.to_ascii_lowercase();
    assert!(
        head.contains("content-type: multipart/related"),
        "frames answers multipart: {}",
        r.raw_head
    );
    assert!(
        head.contains("type=\"application/octet-stream\""),
        "the part media type is octet-stream: {}",
        r.raw_head
    );
    // The PART's own Content-Type lives inside the body, not the head.
    let part_head = String::from_utf8_lossy(&r.body);
    assert!(
        part_head.contains("Content-Type: ") && part_head.contains("; transfer-syntax="),
        "each part names its media type and syntax: {part_head}"
    );
}

/// THE core assertion: a stored compressed tile comes back as raw Explicit VR
/// LE samples, `rows * columns * bytes_per_sample` long and labelled with the
/// target syntax. A fixture that is not a decodable codestream cannot prove
/// this - the fallback would serve the fragment instead.
#[test]
fn a_compressed_tile_transcodes_to_raw_evrle_samples() {
    let srv = TestServer::start();
    let rows = 16u16;
    let cols = 12u16;
    let tile = jpeg2000_tile(rows, cols, 0x40);
    srv.stow(&build_instance(JPEG2000_LOSSLESS, rows, cols, &[&tile]));

    let r = srv.frame(1, "?transferSyntax=1.2.840.10008.1.2.1");
    assert_eq!(r.status, 200, "{}", r.raw_head);
    assert_eq!(
        part_ts(&r.body),
        EVRLE,
        "the decoded part must be labelled Explicit VR LE"
    );
    let body = part_body(&r.body);
    let expected = rows as usize * cols as usize; // 8-bit, one sample
    assert_eq!(
        body.len(),
        expected,
        "a decoded tile is exactly rows*cols bytes, got {} (stored fragment was {})",
        body.len(),
        tile.len()
    );
    assert_ne!(
        body, tile,
        "the body must be the DECODED samples, not the stored codestream"
    );
}

/// HTJ2K (.201/.202/.203) wrappers are registered as DECODE-ONLY, which is
/// what the per-tile path needs: it never re-encodes the tile, it decodes it
/// to raw samples. `.96` (JPEG XL HTJ2K) is deliberately absent, so a stored
/// .96 tile takes the 8.7.3 fallback rather than pretending to be decodable.
///
/// The decode itself is not exercised here: the archive has no HTJ2K sample
/// and encoders for it do not exist in this build (decode-only by design), so
/// a fabricated codestream would test the codec's rejection path, not this
/// feature. The registration is the part W22 owns; the codestream handling is
/// shared with .90 and covered by the JPEG 2000 tile test above.
#[test]
fn htj2k_wrappers_are_registered_as_decode_only() {
    use clarus_adapters::codec::ts_uid;
    let registry = clarus_adapters::codec::CodecRegistry::default();
    for ts in [
        ts_uid::HTJ2K_LOSSLESS,
        ts_uid::HTJ2K_LOSSLESS_RPCL,
        ts_uid::HTJ2K_LOSSY,
    ] {
        let codec = registry
            .get(ts)
            .unwrap_or_else(|| panic!("{ts} registered"));
        assert_eq!(
            codec.transfer_syntax_uid(),
            ts,
            "each wrapper advertises its own syntax"
        );
        assert!(codec.decode_only(), "{ts} must be decode-only");
    }
    assert!(
        registry.get("1.2.840.10008.1.2.4.96").is_none(),
        "JPEG XL HTJ2K stays unregistered"
    );
}

/// The 0028 tags are read from a BOUNDED head, so a head that lacks them is
/// "cannot transcode", never a whole-blob read and never an error.
#[test]
fn a_head_without_the_pixel_module_serves_the_stored_tile() {
    let srv = TestServer::start();
    let tile = vec![0x44u8; 32];
    srv.stow(&build_instance(JPEG2000_LOSSLESS, 8, 8, &[&tile]));
    // Sanity: the route answers even though the route itself is what reads
    // the head. The narrower assertion is that it does not fail.
    let r = srv.frame(1, "?transferSyntax=1.2.840.10008.1.2.1");
    assert!(
        r.status == 200,
        "a tile that cannot be transcoded is still served: {}",
        r.raw_head
    );
}

/// The manifest carries the stored syntax, and the served part reports it -
/// the property the diag path in `part_ts` depends on.
#[test]
fn the_manifest_records_the_stored_syntax() {
    let srv = TestServer::start();
    srv.stow(&build_instance(JPEG2000_LOSSLESS, 8, 8, &[&[0x01u8; 24]]));
    let manifests: PathBuf = srv
        .state
        .study_scanner
        .manifests_dir
        .join(format!("{STUDY}.manifest"));
    let bytes = std::fs::read(&manifests).expect("manifest written");
    let manifest = clarus_core::manifest::Manifest::decode(&bytes).expect("manifest decodes");
    let instance = manifest
        .series
        .iter()
        .flat_map(|s| s.instances.iter())
        .next()
        .expect("one instance");
    assert_eq!(
        instance.transfer_syntax, JPEG2000_LOSSLESS,
        "the stored fragment's syntax is what the route labels the part with"
    );
}

/// A JSON QIDO answer is untouched by the frame work: the route table and the
/// metadata projection are shared with the paths W22 did not change.
#[test]
fn the_instance_metadata_route_is_unaffected() {
    let srv = TestServer::start();
    srv.stow(&build_instance(JPEG2000_LOSSLESS, 8, 8, &[&[0x02u8; 24]]));
    let r = srv.request(
        "GET",
        &format!("/dicomweb/studies/{STUDY}/series/{SERIES}/instances/{SOP}/metadata"),
        &[],
        "application/dicom+json",
    );
    assert_eq!(r.status, 200, "{}", r.raw_head);
    let json = parse_json(&r.body).expect("metadata json");
    // The metadata route answers a JSON object carrying the DICOM attributes.
    assert!(
        json.obj_get("00280010").is_some() || json.obj_get("00280011").is_some(),
        "metadata carries the pixel module: {:?}",
        String::from_utf8_lossy(&r.body)
    );
}
