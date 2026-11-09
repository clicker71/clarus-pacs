# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge  -  Prometheus metrics (P22d, BR-030).

All metrics are direct singletons  -  no no-op fallback.
prometheus_client is the only hard dependency (pip install prometheus_client).
KISS: every admin needs monitoring. Zero benefit from optional metrics.
"""

from prometheus_client import REGISTRY, Counter, Gauge, Histogram


def _g(name, *args, **kwargs):
    """Create Gauge. Idempotent  -  returns existing if re-imported (test suite)."""
    if name in REGISTRY._names_to_collectors:
        return REGISTRY._names_to_collectors[name]
    return Gauge(name, *args, registry=REGISTRY, **kwargs)


def _c(name, *args, **kwargs):
    """Create Counter. Idempotent."""
    if name in REGISTRY._names_to_collectors:
        return REGISTRY._names_to_collectors[name]
    return Counter(name, *args, registry=REGISTRY, **kwargs)


def _h(name, *args, **kwargs):
    """Create Histogram. Idempotent."""
    if name in REGISTRY._names_to_collectors:
        return REGISTRY._names_to_collectors[name]
    return Histogram(name, *args, registry=REGISTRY, **kwargs)


# Group 1: Health
bridge_up = _g("clarus_bridge_up", "Bridge process is running")

# Group 2: DIMSE Translation
cstore_requests = _c(
    "clarus_bridge_cstore_requests_total",
    "Total C-STORE requests received from modalities",
)

stow_requests = _c(
    "clarus_bridge_stow_requests_total",
    "Total STOW-RS requests to Clarus Server",
    ["status"],
)

translation_duration = _h(
    "clarus_bridge_translation_duration_seconds",
    "DIMSE-to-DICOMweb translation latency",
    buckets=[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0],
)

find_responses = _c(
    "clarus_bridge_find_responses_total", "Total C-FIND responses sent", ["type"]
)

move_jobs_counter = _c("clarus_bridge_move_jobs_total", "Total C-MOVE jobs", ["status"])

# Group 3: Data Quality
charset_conversions = _c(
    "clarus_bridge_charset_conversions_total",
    "Total charset conversions during C-STORE",
    ["from_encoding"],
)

malformed_tags = _c(
    "clarus_bridge_malformed_tags_total", "Total malformed DICOM tags detected"
)

# Group 4: Resources
memory_usage = _g(
    "clarus_bridge_memory_usage_bytes", "Bridge process RSS memory in bytes"
)

stow_retries = _c("clarus_bridge_stow_retries_total", "Total STOW-RS retry attempts")

# Group 5: Clarus Server Health
server_health = _g(
    "clarus_bridge_server_health", "Clarus Server health: 1=alive, 0=dead"
)

# Group 6: Outbox Queue (reliable persistence)
outbox_pending = _g(
    "clarus_bridge_outbox_pending",
    "Number of pending (undelivered) items in outbox queue",
)

# outbox_pending_files: live pending count as seen by the outbox worker.
outbox_pending_files = _g(
    "clarus_bridge_outbox_pending_files",
    "Number of pending files in the outbox (outbox worker view)",
)

# outbox_last_success_timestamp: unix time of the last successful STOW flush.
outbox_last_success_timestamp = _g(
    "clarus_bridge_outbox_last_success_timestamp",
    "Unix timestamp of the last successful outbox STOW flush (0 = never)",
)

outbox_enqueue_latency_seconds = _h(
    "clarus_bridge_outbox_enqueue_latency_seconds",
    "C-STORE → outbox.enqueue() latency (serialize + dcmwrite + os.rename)",
    buckets=[0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0],
)

# undelivered counter
outbox_undelivered_total = _c(
    "clarus_bridge_outbox_undelivered_total",
    "Total SOPs moved to undelivered (max retries exhausted)",
)

# Group 7: Outbox FS internals
outbox_recovery_total = _c(
    "clarus_bridge_outbox_recovery_total",
    "Total items recovered on startup (moved from in_progress/ → pending/)",
)

outbox_disk_bytes = _g(
    "clarus_bridge_outbox_disk_bytes", "Total size of outbox directory in bytes"
)

# ---------------------------------------------------------------------------
# Group: Forward queue (0409) - best-effort DIMSE->DIMSE mirror of accepted
# C-STORE blobs to an optional second (non-authoritative) archive.
# ---------------------------------------------------------------------------

# forward_pending: live count of queued forward items (drain view).
forward_pending = _g(
    "clarus_bridge_forward_pending",
    "Number of pending (undelivered) items in the forward queue",
)

# forward_pending_files: live pending count as seen by the forward worker
# (symmetric with outbox_pending_files).
forward_pending_files = _g(
    "clarus_bridge_forward_pending_files",
    "Number of pending files in the forward queue (forward worker view)",
)

forward_last_success_timestamp = _g(
    "clarus_bridge_forward_last_success_timestamp",
    "Unix timestamp of the last successful forward C-STORE (0 = never)",
)

forward_delivered_total = _c(
    "clarus_bridge_forward_delivered_total",
    "Total SOPs forwarded to the second archive (C-STORE RSP 0x0000)",
)

forward_failed_total = _c(
    "clarus_bridge_forward_failed_total",
    "Total SOPs whose forward C-STORE failed (retried, then undelivered)",
)

forward_undelivered_total = _c(
    "clarus_bridge_forward_undelivered_total",
    "Total SOPs moved to forward undelivered (max retries exhausted)",
)

forward_dropped_total = _c(
    "clarus_bridge_forward_dropped_total",
    "Total SOPs dropped when the forward queue is full (drop-newest)",
)

# ---------------------------------------------------------------------------
# Group: N-ACTION / Storage Commitment (0209 backlog)
# ---------------------------------------------------------------------------

naction_requests = _c(
    "clarus_bridge_naction_requests_total",
    "Total N-ACTION (Storage Commitment) requests handled",
)

naction_total = _c(
    "clarus_bridge_naction_total",
    "Total SOP verdicts produced by N-ACTION",
    ["verdict"],  # verdict="success" | "failed"
)

naction_batches_pending = _g(
    "clarus_bridge_naction_batches_pending",
    "Number of Storage Commitment batches awaiting an N-EVENT-REPORT verdict",
)

# ---------------------------------------------------------------------------
# Group: N-EVENT-REPORT delivery
# ---------------------------------------------------------------------------

nevent_delivered_total = _c(
    "clarus_bridge_nevent_delivered_total",
    "Total N-EVENT-REPORTs whose C-STORE-RSP-shaped status was received",
)

nevent_failed_total = _c(
    "clarus_bridge_nevent_failed_total",
    "Total N-EVENT-REPORTs that could not be delivered",
)

# ---------------------------------------------------------------------------
# Group: associations (per role + peer AE title)
#
# The ae label is a peer-announced AE title and therefore attacker
# controllable; it is capped to the DICOM 16-character AE form so the label
# set stays bounded (PS3.7 D.3 AE title length).
# ---------------------------------------------------------------------------

associations_total = _c(
    "clarus_bridge_associations_total",
    "Total DIMSE associations by role and peer AE title",
    ["role", "ae"],  # role="from" (accepted) | "to" (initiated)
)


def ae_label(ae_title):
    """Bound an AE title for use as a Prometheus label value.

    AE titles are announced by the peer, so the value is not trustworthy;
    capping to 16 characters (the DICOM maximum) keeps the label set bounded
    and matches the truncation already applied by assoc_tracker.
    """
    if not ae_title:
        return "UNKNOWN"
    return str(ae_title)[:16]


# ---------------------------------------------------------------------------
# Group: outbound C-STORE latency (per peer AE title)
# ---------------------------------------------------------------------------

cstore_outbound_duration = _h(
    "clarus_bridge_cstore_outbound_duration_seconds",
    "Outbound C-STORE round-trip latency (request -> C-STORE-RSP)",
    ["peer"],
    buckets=[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0],
)

# ---------------------------------------------------------------------------
# Group: queue depth gauges
# ---------------------------------------------------------------------------

cmove_jobs_pending = _g(
    "clarus_bridge_cmove_jobs_pending",
    "Number of C-MOVE jobs in the pending state (move_jobs table)",
)

commit_batches_pending = _g(
    "clarus_bridge_commit_batches_pending",
    "Number of Storage Commitment batches in the commit tracker",
)

# ---------------------------------------------------------------------------
# Group: per-peer byte counters (rx = received from the wire, tx = sent)
#
# rx is counted on the accepted C-STORE payload BEFORE any persistence, so it
# is the real received byte count. The forward and C-MOVE tx legs count the
# size of the blob actually handed to send_c_store.
# ---------------------------------------------------------------------------

bytes_total = _c(
    "clarus_bridge_bytes_total",
    "Total DICOM payload bytes by direction and peer AE title",
    ["direction", "peer"],  # direction="rx" | "tx"
)
