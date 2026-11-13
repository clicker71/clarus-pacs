# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false, reportArgumentType=false

"""Clarus Bridge - N-EVENT-REPORT SCU (Storage Commitment push report).

Closes the RSP->report loop: after the
N-ACTION-RSP is returned, one N-EVENT-REPORT per batch is sent back to the
modality over a SEPARATE SCU association (PS3.4 Push Model).

EventTypeID (PS3.7 F.2 / table): 1 = "Storage Commitment Request Successful",
2 = "Storage Commitment Request Complete - Failures Exist". A failed SOP
carries a Failure Reason in its FailedSOPSequence item (PS3.3 C.14.1.1). The
modality keeps its local copy of any SOP it was NOT confirmed for.
"""

import logging
import threading
import time

from pydicom import Dataset
from pydicom.uid import ImplicitVRLittleEndian
from pynetdicom import AE, build_role
from pynetdicom.sop_class import StorageCommitmentPushModel

from clean_code.handlers.cmove import _release_or_abort
from clean_code.metrics import (
    ae_label,
    associations_total,
    nevent_delivered_total,
    nevent_failed_total,
)
from clean_code.protocol import _dimse_status_name

# EventTypeID (see module docstring).
EVENT_SUCCESS = 1
EVENT_FAILURE = 2

# PS3.4 J.3.2/J.3.3.1: an N-EVENT-REPORT on the Push Model addresses the SOP
# Instance this SOP Class defines as a constant, created conceptually at SCP
# startup. The Transaction UID is NOT the Affected SOP Instance UID - it travels
# inside the dataset as (0008,1195) and is how the SCU correlates the report with
# its request. Sending the UID as the instance makes a strict SCU drop the
# report: it does not recognise the affected instance as the commitment one.
WELL_KNOWN_SOP_INSTANCE_UID = "1.2.840.10008.1.20.1.1"

# Expected chatter from pynetdicom while a Storage Commitment association is
# negotiated: "No accepted presentation contexts" is the normal answer from an
# SCP that does not implement the push model, and it must not spam the journal.
#
# It is suppressed with a per-THREAD filter on the library logger, not by
# moving the logger's level: the level is one object shared by the whole
# process, so silencing it also dropped every pynetdicom record of the C-STORE
# and C-MOVE workers for as long as an association took (seconds, times the
# retries), for no reason of theirs. A filter sees only the thread that
# installed it, so the level never has to change and no lock is needed.
_PN_QUIET = threading.local()


def _pn_quiet_filter(record):
    """Drop pynetdicom records from a thread that asked for silence."""
    return not getattr(_PN_QUIET, "on", False)


# PS3.4 J.3.2 / J.3.3: a Storage Commitment transaction needs the requestor to
# receive the N-EVENT-REPORT the SCP pushes back, so the requestor must hold
# BOTH roles. PS3.7 D.3.3.4 spells the requestor's proposal as an SCP/SCU Role
# Selection item, which is a SEPARATE negotiation primitive - it is not a
# property of a presentation context (`PresentationContext` only carries the
# acceptor-side scp_role/scu_role), so it must travel in `ext_neg`.


def send_n_event_report(
    host,
    port,
    calling_ae,
    tx_uid,
    event_type,
    success_sops,
    failed_sops,
    bridge_ae_title,
    max_retries=3,
    retry_delay=2.0,
):
    """Send ONE N-EVENT-REPORT-RQ to the modality (SCU role).

    Push model: a separate SCU association to (host, port). Retries with
    fixed backoff; on final failure only a WARN is logged (no persistence in
    v1 - the modality re-requests commitment, and we only confirm what is
    actually stored, so no data is at risk).

    Args:
        success_sops: list of (sop_class_uid, sop_instance_uid).
        failed_sops:  list of (sop_class_uid, sop_instance_uid, failure_reason).
        bridge_ae_title: Calling AE Title for this push - the configured
            [server] ae_title. A modality that knows the commitment-requestor
            AE rejects the report it did not expect, so this is required
            rather than defaulted here.
    """
    reported = False
    last_err = None

    for attempt in range(max_retries + 1):
        if attempt > 0:
            logging.warning(
                "[N-EVENT] Report attempt %d/%d after %.0fs (tx=%s)",
                attempt + 1,
                max_retries + 1,
                retry_delay,
                tx_uid,
            )
            time.sleep(retry_delay)

        ae = AE(ae_title=bridge_ae_title)
        # `connection_timeout` is the ONLY setting that bounds the raw TCP
        # connect: pynetdicom's transport does
        # `socket.settimeout(self.assoc.connection_timeout)` immediately
        # before `socket.connect()`, and the AE default is None - which means
        # blocks for the OS SYN timeout (minutes) behind a black-hole peer.
        # `network_timeout` and `acse_timeout` cover DIMSE and ACSE only, so
        # setting them alone left the connect unbounded (verified against the
        # installed pynetdicom, transport.AssociationSocket.connect).
        ae.connection_timeout = 5
        ae.network_timeout = 5
        ae.acse_timeout = 3
        ae.dimse_timeout = 30
        # PS3.7 D.3.3.4: without a role item the requestor is SCU and the
        # acceptor is SCP, so the SCP could never send the N-EVENT-REPORT that
        # carries the Storage Commitment Result (PS3.4 J.3.3.1). The bridge
        # proposes BOTH roles; a peer that declines leaves the default roles,
        # which the response check below reports.
        ae.add_requested_context(StorageCommitmentPushModel, ImplicitVRLittleEndian)
        sc_role = build_role(StorageCommitmentPushModel, scu_role=True, scp_role=True)

        # "No accepted presentation contexts" is normal for SCPs that
        # do not implement the push model. Quiet ONLY this thread's pynetdicom
        # records around associate() so that expected ERROR does not spam the
        # journal, without touching the level other threads are logging at.
        _pn_logger = logging.getLogger("pynetdicom")
        if _pn_quiet_filter not in _pn_logger.filters:
            # Added once per process; the filter is inert outside a silenced
            # thread, so it never filters anything another thread logs.
            _pn_logger.addFilter(_pn_quiet_filter)
        try:
            _PN_QUIET.on = True
            try:
                assoc = ae.associate(host, port, ae_title=calling_ae, ext_neg=[sc_role])
                try:
                    associations_total.labels(role="to", ae=ae_label(calling_ae)).inc()
                except Exception:
                    pass
            finally:
                _PN_QUIET.on = False
        except Exception as e:
            # A setup error (AE/role construction, a resolver failure, a
            # socket error escaping transport) is a TRANSIENT failure like any
            # other: this attempt has nothing to deliver, so record it and let
            # the loop's backoff try again. Letting it escape turned a
            # recoverable peer problem into an unreported commitment - the
            # verdict was never sent and nothing retried it (PS3.7 D.3.3.4:
            # the association is the transport, not the outcome).
            assoc = None
            last_err = f"association setup failed: {e}"
            logging.warning("[N-EVENT] %s (tx=%s)", last_err, tx_uid)
            continue

        if assoc is None or not assoc.is_established:
            last_err = (
                f"SCP {calling_ae}@{host}:{port} unavailable (attempt {attempt + 1})"
            )
            logging.warning("[N-EVENT] %s", last_err)
            _release_or_abort(assoc, timeout=2.0)
            continue

        # The context alone does not prove the role: an SCP that accepts the
        # abstract syntax but DECLINES the role item leaves the requestor as
        # plain SCU (PS3.7 D.3.3.4), and the report would then never be
        # accepted.
        #
        # `as_scp` is the ACCEPTOR's answer, not ours (pynetdicom
        # PresentationContext.as_scp: "True if the association Acceptor can
        # act as SCP for the current context"). The bridge is the N-EVENT
        # requestor, i.e. the SCU of that service, so what it needs is a peer
        # that will act as SCP - which is exactly `as_scp`. Testing `as_scu`
        # here would check the wrong side of the association and reject a
        # correct peer.
        sc_accepted = any(
            str(ctx.abstract_syntax) == StorageCommitmentPushModel and ctx.as_scp
            for ctx in assoc.accepted_contexts
        )
        if not sc_accepted:
            last_err = (
                f"SCP {calling_ae} did not grant the SCP role for storage commitment"
            )
            logging.warning("[N-EVENT] %s", last_err)
            _release_or_abort(assoc, timeout=2.0)
            continue

        try:
            # EventTypeID (0000,1002) is a COMMAND-SET element (PS3.7 Table
            # 9.1-1); it travels as the separate `event_type` argument to
            # send_n_event_report below, not inside the Event Information data
            # set. Embedding a group-0000 element in the data set makes a
            # strict SCP reject the report.
            ds = Dataset()
            ds.TransactionUID = tx_uid

            if success_sops:
                seq = []
                for sop_class, sop_uid in success_sops:
                    item = Dataset()
                    item.ReferencedSOPClassUID = sop_class
                    item.ReferencedSOPInstanceUID = sop_uid
                    seq.append(item)
                ds.ReferencedSOPSequence = seq

            if failed_sops:
                fseq = []
                for sop_class, sop_uid, reason in failed_sops:
                    item = Dataset()
                    item.ReferencedSOPClassUID = sop_class
                    item.ReferencedSOPInstanceUID = sop_uid
                    item.FailureReason = reason
                    fseq.append(item)
                ds.FailedSOPSequence = fseq

            # pynetdicom 3.x signature: send_n_event_report(dataset, event_type,
            # class_uid, instance_uid) -> (status, response_dataset).
            # PS3.4 J.3.2: the Affected SOP Instance UID is the well-known
            # constant, not the Transaction UID (which is in the dataset).
            status, _resp = assoc.send_n_event_report(
                ds,
                event_type,
                StorageCommitmentPushModel,
                WELL_KNOWN_SOP_INSTANCE_UID,
            )
            if status is None or not hasattr(status, "Status"):
                # PS3.7 9.3: a response is mandatory. An empty one means the
                # association went away mid-request (timeout, abort), and the
                # modality was NOT told the outcome - treating it as delivered
                # stopped the retries and left the commitment never reported.
                last_err = "no N-EVENT-REPORT response"
                logging.warning("[N-EVENT] %s (tx=%s)", last_err, tx_uid)
                continue
            code = status.Status
            logging.info(
                "[N-EVENT] tx=%s type=%d sent to %s: %s (%d ok / %d failed)",
                tx_uid,
                event_type,
                calling_ae,
                _dimse_status_name(code),
                len(success_sops),
                len(failed_sops),
            )
            # PS3.7 9.1: only 0x0000 means the primitive succeeded. Any other
            # code says the SCP did not receive the report, so the modality was
            # never told the outcome - counting it as delivered stopped the
            # retries and left the commitment unreported for good.
            if code == 0x0000:
                reported = True
            else:
                last_err = f"N-EVENT-REPORT rejected with {_dimse_status_name(code)}"
                logging.warning("[N-EVENT] %s (tx=%s)", last_err, tx_uid)
        except Exception as e:
            last_err = f"send_n_event_report error: {e}"
            logging.warning("[N-EVENT] %s (tx=%s)", last_err, tx_uid)
        finally:
            _release_or_abort(assoc, timeout=2.0)

        if reported:
            try:
                nevent_delivered_total.inc()
            except Exception:
                pass
            return True

    logging.error(
        "[N-EVENT] Report NOT delivered for tx=%s after %d attempts: %s",
        tx_uid,
        max_retries + 1,
        last_err,
    )
    try:
        nevent_failed_total.inc()
    except Exception:
        pass
    return False
