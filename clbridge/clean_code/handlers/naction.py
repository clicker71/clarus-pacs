# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge - N-ACTION handler (Storage Commitment).

handle_n_action: parses Storage Commitment Request, verifies SOPs via WADO-RS,
builds N-ACTION-RSP with success/failed SOP sequences, and registers the batch
in the CommitTracker for the async N-EVENT-REPORT (closes the
RSP->report loop).
"""

import logging
from urllib.parse import quote

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
    # `safe=""` per segment: a UID is one path element, so `/`, `?` and `#`
    # inside it must not change which resource the GET addresses
    # (PS3.18 10.1 resource paths). Unescaped, a crafted UID of `1.2?a=b`
    # pointed the verification at a DIFFERENT resource, and a 200 there
    # confirmed a SOP the server never stored.
    url = (
        f"{base}/studies/{quote(study_uid, safe='')}"
        f"/series/{quote(series_uid, safe='')}/instances/{quote(sop_uid, safe='')}"
        "/metadata"
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

    Returns True when the batch was registered (its N-EVENT-REPORT will be
    pushed); False when there is no resolvable requestor address/port, so the
    caller can refuse the request instead of acknowledging a commitment whose
    report can never go out.
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
        try:
            commit_tracker.register(tx_uid, calling_ae, address, int(port_), sop_list)
        except Exception as e:
            # Persistence failed, so the commitment cannot be honoured later.
            # Answer Failure (0x0110) instead of acknowledging a request whose
            # N-EVENT-REPORT would never be pushed (PS3.4 J.3.3.1).
            logging.error(
                "[N-ACTION] tx=%s could not be registered: %s", tx_uid, e
            )
            return False
        return True

    logging.warning(
        "[N-ACTION] No requestor address/port for tx=%s - report skipped",
        tx_uid,
    )
    return False


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
    # 0x0123 "No Such Action" (PS3.7): this action_type is not one the bridge
    # implements. 0x0122 would claim the SOP CLASS is unsupported - a different
    # refusal - so it must not be used here.
    action_type = getattr(event, "action_type", None)
    if action_type is not None and action_type != 1:
        logging.warning(
            "[N-ACTION] Unsupported action_type=%s (only type 1 supported)",
            action_type,
        )
        return 0x0123, None

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
        # 0x0110 Processing Failure: the request cannot be parsed and will
        # never succeed, so the SCU must not read this as a temporary
        # condition (0x0213 Resource Limitation invites a retry).
        logging.error("[N-ACTION] Unexpected parse error: %s", e)
        logging.debug("[N-ACTION] Full traceback follows:", exc_info=True)
        return 0x0110, None

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

    # Register the batch for the async N-EVENT-REPORT. A batch that cannot be
    # registered (no resolvable requestor address/port) would otherwise be
    # answered 0x0000 while its N-EVENT-REPORT can never go out, leaving the
    # Transaction UID hanging at the modality. Refuse with 0x0110 (Processing
    # Failure) so the SCU knows the request is not being processed.
    #
    # A request that NAMES SOPs has committed the SCP to reporting on them, so
    # it is refused when no tracker exists either: with `commit_tracker=None`
    # nothing was registered and nothing guarantees the N-EVENT-REPORT
    # (PS3.4 J.3.3.1 - the report IS the result), yet the request was
    # acknowledged 0x0000. Answering success for work that was silently
    # dropped is worse than refusing it.
    if sop_list:
        if commit_tracker is None:
            logging.error(
                "[N-ACTION] tx=%s references %d SOP(s) but no commitment "
                "tracker is available - refusing (no N-EVENT-REPORT possible)",
                tx_uid,
                len(sop_list),
            )
            return 0x0110, None
        if not _register_commit(
            commit_tracker,
            tx_uid,
            calling_ae,
            sop_list,
            event,
            resolve_scp=resolve_scp,
        ):
            return 0x0110, None

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
