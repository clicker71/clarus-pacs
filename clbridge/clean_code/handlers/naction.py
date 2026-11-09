# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge - N-ACTION handler (Storage Commitment).

handle_n_action: parses Storage Commitment Request, verifies SOPs via WADO-RS,
builds N-ACTION-RSP with success/failed SOP sequences, and registers the batch
in the CommitTracker for the async N-EVENT-REPORT (closes the
RSP->report loop).
"""

import logging

import httpx

from clean_code.http_helpers import _wado_metadata_request, dicomweb_base
from clean_code.metrics import naction_requests
from clean_code.protocol import _parse_n_action_rq
from clean_code.utils import _shutdown_requested


# ---------------------------------------------------------------------------
# Per-SOP Storage Commitment confirmation (shared by sync RSP + async report)
# ---------------------------------------------------------------------------
def _sop_confirm(sop_uid, sop_index, clarus_url, http_timeout, tls=None):
    """Confirm one SOP is actually stored in Clarus. Returns DIMSE reason.

    0      - confirmed (in index AND WADO-RS metadata 200), also marks verified.
    0xC201 - not in index / not on server.
    0xC202 - server unavailable (timeout / connection refused).
    0xC210 - processing failure.
    """
    entry = sop_index.lookup_sop(sop_uid)
    if entry is None:
        return 0xC201

    study_uid = entry["study_uid"]
    series_uid = entry["series_uid"]
    base = dicomweb_base(clarus_url)
    url = (
        f"{base}/studies/{study_uid}"
        f"/series/{series_uid}/instances/{sop_uid}/metadata"
    )
    try:
        resp = _wado_metadata_request(url, http_timeout, tls=tls)
    except (httpx.TimeoutException, httpx.ConnectError):
        return 0xC202
    except Exception as e:
        logging.warning("  [N-ACTION] SOP %s: WADO error (%s)", sop_uid, e)
        return 0xC210

    if resp.status_code == 200:
        try:
            sop_index.mark_verified(sop_uid)
        except Exception:
            pass
        return 0
    logging.warning(
        "  [N-ACTION] SOP %s: WADO-RS HTTP %d -> not confirmed",
        sop_uid,
        resp.status_code,
    )
    return 0xC201


# ---------------------------------------------------------------------------
# N-ACTION handler (Storage Commitment)
# ---------------------------------------------------------------------------
def _register_commit(
    commit_tracker,
    tx_uid,
    calling_ae,
    sop_list,
    event,
    resolve_scp=None,
):
    """Register the batch so the async N-EVENT-REPORT can be pushed later.

    PS3.8: A-ASSOCIATE carries no listening port, and `requestor.port` is the
    EPHEMERAL SOURCE port of this very connection - pushing an N-EVENT-REPORT
    there always failed, because that socket is gone once the association
    closes. The destination port therefore comes from the peer config (the same
    lookup C-MOVE uses) and nowhere else. A peer that cannot be resolved is
    reported as skipped rather than guessed at.
    """
    requestor = getattr(getattr(event, "assoc", None), "requestor", None)
    address = getattr(requestor, "address", None)
    port_ = None
    if resolve_scp is not None and calling_ae and address:
        try:
            target = resolve_scp(calling_ae, address)
            if target:
                address, port_ = target[0], target[1]
        except Exception as e:
            logging.warning("[N-ACTION] resolve_scp for %s failed: %s", calling_ae, e)

    if address and port_:
        commit_tracker.register(tx_uid, calling_ae, address, int(port_), sop_list)
    else:
        logging.warning(
            "[N-ACTION] No requestor address/port for tx=%s - report skipped",
            tx_uid,
        )


def handle_n_action(
    event,
    clarus_url,
    verbose,
    sop_index,
    http_timeout=30,
    commit_tracker=None,
    resolve_scp=None,
    tls=None,
    stc_presence=False,
):
    """Handle N-ACTION-RQ (Storage Commitment Push Model, PS3.4 K.2).

    Two modes, selected once at startup by ``[dicomweb]
    storage_commitment_presence``:

    - ``stc_presence=False`` (default, today's behaviour): confirm each SOP
      against the local index + WADO-RS, build the RSP with provisional
      Success/Failed sequences, and register the batch for the async
      N-EVENT-REPORT.
    - ``stc_presence=True``: the ORIGIN owns commitment. The RSP is
      status-only 0x0000 (the standard's shape, J.3.2.1.3) - the verdict is
      not known yet, because the bridge's own STOW may still be queued - and
      the batch is registered so the tracker asks the origin ONCE when the
      bridge has delivered every SOP.

    Registers only when a commit_tracker is provided (fire-and-forget); the
    N-ACTION-RSP itself stays synchronous.
    """
    # Bridge: reject new requests during shutdown
    if _shutdown_requested.is_set():
        return 0xC000, None

    # Validate action_type (1 = Storage Commitment Request).
    # 0x0122 "No such action" is part of the PS3.7 general-refusal set - a
    # clean refusal, NOT an implementation 0xC2xx code a modality is not
    # required to understand.
    action_type = getattr(event, "action_type", None)
    if action_type is not None and action_type != 1:
        logging.warning(
            "[N-ACTION] Unsupported action_type=%s (only type 1 supported)",
            action_type,
        )
        return 0x0122, None

    calling_ae = event.assoc.requestor.ae_title
    logging.debug("[DIMSE] N-ACTION from %s", calling_ae)

    try:
        naction_requests.inc()
    except Exception:
        pass

    try:
        tx_uid, sop_list = _parse_n_action_rq(event)
    except ValueError as e:
        # Unparsable request (e.g. missing TransactionUID) -> 0x0110
        # Processing Failure (the 0107/0110 class).
        logging.warning("[N-ACTION] Parse error: %s", e)
        return 0x0110, None
    except Exception as e:
        # 0x0213 Processing Failure (general-refusal set).
        logging.error("[N-ACTION] Unexpected parse error: %s", e)
        logging.debug("[N-ACTION] Full traceback follows:", exc_info=True)
        return 0x0213, None

    logging.info(
        "[N-ACTION] TransactionUID: %s, SOPs to verify: %d", tx_uid, len(sop_list)
    )

    # J.3.2: a successful N-ACTION-RSP says only that the SCP RECEIVED the
    # request; the per-SOP verdict travels in the N-EVENT-REPORT (J.3.3.1).
    # The RSP is therefore ALWAYS status-only 0x0000 here, and the verdict is
    # left entirely to the tracker, which probes asynchronously.
    #
    # Confirming every referenced SOP before answering meant one WADO
    # round-trip per instance inside the DIMSE reactor: a modality that sends
    # a 500-instance batch held its association open for tens of minutes
    # waiting for an acknowledgement that, by the standard, carries nothing.
    # A caller-supplied `sop_index` is no longer consulted here; the tracker's
    # `verify` callable is the one place that knows how a SOP is confirmed.
    rsp = None
    if stc_presence:
        # Origin-owned commitment. The verdict is not knowable here: the
        # bridge may still be holding some of these SOPs in its own outbox,
        # so any sequence content would be a guess. The tracker resolves the
        # batch against the origin once every SOP has been delivered (or the
        # deadline passes).
        logging.info(
            "[N-ACTION] tx=%s: origin STC mode - status-only RSP, "
            "%d SOP(s) registered for the origin's verdict",
            tx_uid,
            len(sop_list),
        )
    else:
        # Local heuristic. The tracker's `verify` (the SOP index + WADO-RS
        # path) decides per SOP on its own tick; this handler acknowledges
        # receipt and gets out of the way.
        #
        # The SOPs the RSP used to report as failures are NOT reported as
        # failures here: at this instant nothing has been checked, so a
        # 0x0110 for each of them would be a claim the bridge has no evidence
        # for. The N-EVENT-REPORT the tracker sends is where that verdict
        # legitimately appears.
        logging.info(
            "[N-ACTION] tx=%s: %d SOP(s) accepted for asynchronous "
            "verification (status-only RSP)",
            tx_uid,
            len(sop_list),
        )

    # Register the batch for the async N-EVENT-REPORT.
    if commit_tracker is not None and sop_list:
        _register_commit(
            commit_tracker,
            tx_uid,
            calling_ae,
            sop_list,
            event,
            resolve_scp=resolve_scp,
        )

    # A correctly-parsed request is ALWAYS acknowledged 0x0000 (J.3.2.1.4: no
    # service-class specific values; RSP only says "received / not handling").
    # In false-mode success, partial and total failure differ ONLY by the
    # content of the Success / Failed SOP sequences built above; in true-mode
    # `rsp` is None (status-only) and every per-SOP outcome travels in the
    # async N-EVENT-REPORT, which is untouched here.
    return 0x0000, rsp


def _display_reason(reason, sop_uid):
    """Log helper: emits per-SOP WADO failure line, returns the reason code."""
    if reason == 0xC202:
        logging.warning(
            "  [N-ACTION] SOP %s: WADO-RS UNAVAILABLE -> FAILED (0xC202)", sop_uid
        )
    elif reason == 0xC210:
        logging.warning(
            "  [N-ACTION] SOP %s: WADO-RS ERROR -> FAILED (0xC210)", sop_uid
        )
    else:
        logging.warning(
            "  [N-ACTION] SOP %s: NOT CONFIRMED -> FAILED (0xC201)", sop_uid
        )
    return reason
