# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Inbound association guards: PDU size ceiling and calling-AE allowlist.

Two checks that a DICOM SCP owes its peers and that pynetdicom 3.0.4 does not
perform for it. Both live here rather than inside a service handler because
they apply to the ASSOCIATION, not to one operation: the PDU ceiling is a
property of the negotiated maximum and the allowlist is a property of the
calling AE, so a per-handler copy would have to be repeated six times and
would drift.

DIMSE status codes and their Chinese-wall decoding are in protocol.py; this
module only distinguishes "accept" from "refuse".
"""

import logging

# pynetdicom's own provider and the PDU grammar this module reads. Imported at
# module scope so the bundled file carries the same binding; no new
# dependency - both modules ship with pynetdicom itself.
from pynetdicom.dul import DULServiceProvider

# ---------------------------------------------------------------------------
# T1: negotiated maximum PDU length
# ---------------------------------------------------------------------------
# PS3.8 9.3.2.2 / Figure 9-1: the User Information item of A-ASSOCIATE
# carries Maximum Length Received, "the maximum length (in bytes) of a PDU
# that may be received" - the negotiated value is the one THIS side announced,
# which pynetdicom stores in `assoc.acceptor.maximum_length`.
#
# pynetdicom 3.0.4 does NOT enforce it. `DULServiceProvider._read_pdu_data`
# (dul.py:245-334) unpacks the six-byte header and then calls
# `self.socket.recv(pdu_length)` (dul.py:302) with no comparison, so a peer
# that declares a near-4 GiB PDU makes the bridge READ it before anything can
# object. `EVT_PDU_RECV` fires from `_decode_pdu` (dul.py:140), i.e. after the
# body is already in memory - a guard there bounds DISPATCH, not memory.
#
# There is no event between header and body, so the bound is placed in a
# DULServiceProvider SUBCLASS (`GuardedDUL`) that reads the header, compares
# the declared length, and only then reads the body. That is the only point in
# the receive path the bridge owns.
PDU_HEADER_LEN = 6
PDU_LEN_OFFSET = 2
# PS3.8 9.3.1 Table 9-1: byte 0 is the PDU type. Only P-DATA-TF (0x04) is
# subject to the negotiated Maximum Length Received: PS3.8 9.3.2.2 scopes that
# value to the variable field of a P-DATA-TF, and an A-ASSOCIATE-RQ is
# legitimately larger than the default before anything has been negotiated.
P_DATA_TF_TYPE = 0x04
A_ABORT_TYPE = 0x07

# PS3.8 9.3.1 Table 9-1: the complete set of PDU types, and the exact declared
# length each fixed-size control PDU must carry once no A-ASSOCIATE has been
# negotiated. A peer that names type 0x09 or 0xFF is not speaking DICOM; a peer
# that names A-RELEASE-RQ with a 4 GiB body is not speaking it either.
#
# The ceiling in `pdu_exceeds_maximum` covers only P-DATA-TF (PS3.8 9.3.2.2 is
# explicit that the maximum length received governs the P-DATA-TF variable
# field), so every other type reached `self.socket.recv(declared)` with an
# unbounded allocation - a six-byte header was enough to make the bridge
# allocate and read whatever the peer declared. A-ASSOCIATE-RQ/RJ/AC are the
# variable-size exceptions and keep a generous but finite cap of their own.
_KNOWN_PDU_TYPES = frozenset(
    {
        0x01,  # A-ASSOCIATE-RQ
        0x02,  # A-ASSOCIATE-AC
        0x03,  # A-ASSOCIATE-RJ
        0x04,  # P-DATA-TF
        0x05,  # A-RELEASE-RQ
        0x06,  # A-RELEASE-RP
        0x07,  # A-ABORT
    }
)

# Declared body length of each fixed-size control PDU (PS3.8 9.3.1 Table 9-1
# and the individual section definitions). A mismatch is a framing error, so
# the body is never read.
_FIXED_PDU_BODY_LEN = {
    0x03: 4,  # A-ASSOCIATE-RJ: reserved(1) + reserved/result/source/reason(3)
    0x05: 4,  # A-RELEASE-RQ: reserved(4)
    0x06: 4,  # A-RELEASE-RP: reserved(4)
    0x07: 4,  # A-ABORT: reserved(2) + reserved/source/reason(2)
}

# Upper bound for the three A-ASSOCIATE PDUs, which are variable-size by design
# and legitimately exceed the default P-DATA maximum before anything has been
# negotiated. 1 MiB is far above any real association item set (a maximal
# implementation class UID list plus every declared context is tens of KiB) and
# far below the ~4 GiB a bare length field can announce.
_ASSOCIATE_PDU_MAX_BODY = 1024 * 1024


def declared_pdu_length(header):
    """The 32-bit length field of a PDU header, or None when unreadable.

    PS3.8 9.3.1: bytes 0-1 are the PDU type and reserved field, bytes 2-5 the
    big-endian length of everything that follows. Fewer than six bytes means
    the caller has not received a header yet, which is not this check's
    business - None says "cannot judge", never "fits".
    """
    if header is None or len(header) < PDU_HEADER_LEN:
        return None
    return int.from_bytes(header[PDU_LEN_OFFSET:PDU_HEADER_LEN], "big")


def pdu_type_of(header):
    """The PDU type byte of a header, or None when unreadable."""
    if header is None or len(header) < 1:
        return None
    return header[0]


def header_rejection_reason(header):
    """Why this header must not be followed by a body read, or '' when it may.

    Pynetdicom's own reader rejects an unrecognised PDU type from the HEADER
    (dul.py, before any body read). `GuardedDUL` skipped that check whenever
    the P-DATA ceiling did not apply - which is exactly the case for every type
    that is not P-DATA-TF - so a type it would have refused reached
    `socket.recv(declared)` and the body was accumulated before decode could
    object.

    This restores the early type check AND bounds the types whose length is
    fixed or merely generous, so an unrecognised type and an oversized control
    PDU are both refused from six bytes instead of after the allocation.
    """
    if header is None or len(header) < PDU_HEADER_LEN:
        return ""
    pdu_type = header[0]
    if pdu_type not in _KNOWN_PDU_TYPES:
        return f"unknown PDU type 0x{pdu_type:02X} (PS3.8 9.3.1 Table 9-1)"
    length = declared_pdu_length(header)
    if length is None:
        return ""
    fixed = _FIXED_PDU_BODY_LEN.get(pdu_type)
    if fixed is not None and length != fixed:
        return (
            f"PDU type 0x{pdu_type:02X} declares {length} body bytes, "
            f"the standard fixes {fixed}"
        )
    if pdu_type in (0x01, 0x02) and length > _ASSOCIATE_PDU_MAX_BODY:
        return (
            f"PDU type 0x{pdu_type:02X} declares {length} body bytes, over "
            f"the {_ASSOCIATE_PDU_MAX_BODY}-byte association cap"
        )
    return ""


def ceiling_applies(pdu_type, maximum):
    """True when the negotiated maximum governs this PDU.

    Only P-DATA-TF, and only once a maximum has been negotiated. A PDU seen
    before A-ASSOCIATE-AC has no ceiling to check against (PS3.8 9.3.2.2), and
    the association request itself is never subject to it.
    """
    return pdu_type == P_DATA_TF_TYPE and bool(maximum)


def pdu_exceeds_maximum(assoc, header):
    """True when the declared PDU length is over the negotiated maximum.

    "Over" is strict: a PDU of exactly the announced maximum is conformant
    (PS3.8 9.3.2.2 says "maximum length ... that may be received"), and
    refusing it would break every peer that fills its window exactly.
    """
    length = declared_pdu_length(header)
    if length is None:
        return False
    maximum = getattr(getattr(assoc, "acceptor", None), "maximum_length", None)
    if not ceiling_applies(pdu_type_of(header), maximum):
        return False
    return length > maximum


def pdu_ceiling_message(assoc, length):
    """Operator-facing text for an oversized PDU, or '' when it fits."""
    maximum = getattr(getattr(assoc, "acceptor", None), "maximum_length", 0) or 0
    return f"PDU of {length} bytes exceeds the negotiated maximum of {maximum} bytes"


def pdu_length_of(pdu):
    """The declared length of an already-decoded PDU, or None when unknown.

    `EVT_PDU_RECV` hands over the DECODED PDU, whose `pdu_length` is the same
    32-bit header field `declared_pdu_length` reads off the wire - one fact,
    two carriers. A PDU class without that attribute is not judged (None),
    never assumed to fit.
    """
    length = getattr(pdu, "pdu_length", None)
    if length is None:
        return None
    try:
        return int(length)
    except (TypeError, ValueError):
        return None


def pdu_exceeds(pdu, maximum):
    """True when the decoded PDU is longer than the negotiated maximum.

    The dispatch-level twin of `pdu_exceeds_maximum`: same rule, applied to an
    object that has already been read. Kept because a PDU can also reach the
    reactor through the requestor path, where this module does not install a
    guarded provider.
    """
    length = pdu_length_of(pdu)
    if length is None:
        return False
    return ceiling_applies(getattr(pdu, "pdu_type", None), maximum) and (
        length > maximum
    )


class GuardedDUL(DULServiceProvider):
    """A DUL provider that refuses an oversized P-DATA-TF BEFORE reading it.

    `_read_pdu_data` is the only function that turns wire bytes into a DIMSE
    message, so overriding it is the one place the ceiling can bound receive
    work rather than merely reject the result. Everything else - the state
    machine, the queues, the timers - is pynetdicom's own, inherited.

    On a breach the provider sends an A-ABORT and queues NOTHING: no `Evt10`,
    no PDU on `_recv_pdu`. The DIMSE layer therefore never sees the frame and
    no service handler can run for it, which is what makes the refusal
    effective even for a complete C-STORE-RQ.
    """

    def __init__(self, assoc, source=None):
        """Build a guarded provider carrying `source`'s configured timeouts.

        `DULServiceProvider.__init__` starts from the LIBRARY defaults (`idle`
        60 s, ARTIM 30 s) and expects the owner to overwrite them afterwards.
        Pynetdicom does that on the provider it creates itself, but this
        replacement is constructed later, from `install_guarded_dul` - after
        the association already applied the configured ACSE and network
        timeouts to the ORIGINAL provider. A fresh provider therefore silently
        reverted a configured five-second idle timeout to sixty, and the
        association then checked the new idle timer.

        `source` is that original provider; its timer values are copied so the
        replacement is a drop-in for it in every respect except the read.
        """
        super().__init__(assoc)
        if source is None:
            return
        try:
            self._idle_timer = source._idle_timer
        except AttributeError:
            pass
        try:
            self.artim_timer = source.artim_timer
        except AttributeError:
            pass

    def _read_pdu_data(self):
        bytestream = bytearray()
        try:
            bytestream.extend(self.socket.recv(PDU_HEADER_LEN))
        except (OSError, TimeoutError):
            # Same classification pynetdicom uses: connection closed.
            self.event_queue.put("Evt17")
            return

        if pdu_exceeds_maximum(self.assoc, bytestream):
            length = declared_pdu_length(bytestream)
            logging.error(
                "[DIMSE] Refusing association: %s",
                pdu_ceiling_message(self.assoc, length),
            )
            # Refuse without reading the declared body: this is the whole
            # point of the subclass. The abort must NOT block here: we are
            # running ON the DUL thread, and `Association._abort_blocking`
            # with block=True calls `self.kill()` and waits for the state
            # machine to go idle - which can only happen on the thread that is
            # now waiting, i.e. a deadlock. The A-ABORT itself is already
            # queued by `acse.send_abort` before that point, so the peer is
            # told either way.
            self.assoc.abort(block=False)
            return

        declared = declared_pdu_length(bytestream)
        if declared is None:
            # Fewer than six bytes arrived: the peer closed mid-header.
            self.event_queue.put("Evt17")
            return
        # Refuse an unusable header BEFORE its body is read. The negotiated
        # ceiling above only governs P-DATA-TF, so every other type used to
        # reach `recv(declared)` with an unbounded allocation: six bytes from
        # the peer decided how much the bridge would read into memory.
        rejection = header_rejection_reason(bytestream)
        if rejection:
            logging.error("[DIMSE] Refusing association: %s", rejection)
            self.assoc.abort(block=False)
            return
        try:
            bytestream += self.socket.recv(declared)
        except (OSError, TimeoutError):
            self.event_queue.put("Evt17")
            return
        if len(bytestream) != PDU_HEADER_LEN + declared:
            self.event_queue.put("Evt17")
            return

        try:
            pdu, event = self._decode_pdu(bytestream)
            self.event_queue.put(event)
        except Exception:
            logging.exception("[DIMSE] Unable to decode the received PDU data")
            self.event_queue.put("Evt19")
            return
        self._recv_pdu.put(pdu)


def install_guarded_dul(event):
    """EVT_CONN_OPEN handler: swap in `GuardedDUL` before the DUL thread runs.

    `RequestHandler.handle` (transport.py:718-723) fires EVT_CONN_OPEN and only
    then calls `assoc.start()`, which is what starts the DUL thread, so the
    replacement here is in place before a single byte is read.

    The original provider's queues and socket are SHARED, not copied:
    `AssociationSocket.__init__` has already put the acceptor's `Evt5` (the
    transport-connection indication that drives the state machine from Sta1 to
    Sta2, transport.py:353) on the existing event queue, and the send path uses
    `assoc.dul.to_provider_queue`. A fresh set of queues would strand that
    priming and the association would never establish.
    """
    assoc = event.assoc
    current = getattr(assoc, "dul", None)
    if current is None or isinstance(current, GuardedDUL):
        return
    if getattr(assoc, "_started_dul", False):
        # Already running: swapping the provider under a live thread would
        # lose whatever it is holding. Nothing to do, and nothing unsafe.
        return
    guarded = GuardedDUL(assoc, source=current)
    guarded.socket = current.socket
    guarded.event_queue = current.event_queue
    guarded.to_user_queue = current.to_user_queue
    guarded.to_provider_queue = current.to_provider_queue
    guarded._recv_pdu = current._recv_pdu
    assoc.dul = guarded


def make_pdu_ceiling_handler():
    """Build the EVT_PDU_RECV handler: the dispatch-level second line.

    The receive-side bound is `GuardedDUL`. This handler stays because a PDU
    can also reach the reactor through a path where no guarded provider was
    installed (the requestor side, or a future pynetdicom that starts the DUL
    before EVT_CONN_OPEN). It is a backstop, not the primary bound.
    """

    def on_pdu_recv(event):
        assoc = event.assoc
        maximum = getattr(getattr(assoc, "acceptor", None), "maximum_length", 0)
        if not pdu_exceeds(event.pdu, maximum):
            return
        length = pdu_length_of(event.pdu)
        logging.error(
            "[DIMSE] Refusing association: %s", pdu_ceiling_message(assoc, length)
        )
        assoc.abort(block=False)

    return on_pdu_recv


# ---------------------------------------------------------------------------
# T3: calling-AE allowlist
# ---------------------------------------------------------------------------
def normalise_ae_title(value):
    """An AE title reduced to what the DICOM rules compare.

    PS3.5 6.2: an AE title is leading- and trailing-space padded to 16
    characters, so `" CT_ROOM "` and `"CT_ROOM"` name the same AE. The
    comparison is also case-insensitive (AE titles are not case-sensitive in
    the field, and a modality that announces `ct_room` must not be locked out
    by a config line that spells it in upper case).
    """
    return str(value or "").strip().upper()


def calling_ae_allowed(calling_aet, allowed_titles):
    """True when `calling_aet` is on the allowlist (case/space insensitive).

    An EMPTY allowlist means "no filter", which is what a deployment that
    turned the knob off wants - and is also the pynetdicom default for
    `require_calling_aet`. A CALLING side that announces an empty AE title is
    never on the list: it names nobody, and accepting it would let any peer
    bypass the check by sending blanks.
    """
    allowed = {normalise_ae_title(t) for t in (allowed_titles or ())}
    if not allowed:
        return True
    requested = normalise_ae_title(calling_aet)
    return bool(requested) and requested in allowed


def unknown_calling_ae_message(calling_aet, peer_address):
    """The operator warning for a refused association.

    Mirrors the unknown Move Destination report in move_jobs.lookup_scp: name
    what was seen, name where it came from, and name the fix. The source PORT
    is deliberately absent - it is ephemeral, whereas the AE title is the
    stable thing the operator can write down.
    """
    where = peer_address or "unknown address"
    return (
        f"unknown calling AE '{calling_aet or '(empty)'}' from {where} - "
        "add it to peers.conf"
    )


def log_unknown_calling_ae(calling_aet, peer_address):
    """Emit the allowlist warning. Returns the message for the caller/log."""
    message = unknown_calling_ae_message(calling_aet, peer_address)
    logging.warning("[PEER] %s", message)
    return message


def make_calling_ae_guard(allowed_titles):
    """Build the EVT_REQUESTED handler: one comparison, one decision.

    The problem this solves: pynetdicom's `require_calling_aet` is a good
    REFUSAL (it answers with a real A-ASSOCIATE-RJ through its own state
    machine, fsm.AE_8) but a bad COMPARISON - `acse._negotiate_as_acceptor`
    (acse.py:330-334) strips the configured titles and then does a plain `in`
    against the received one, which is case-SENSITIVE. A peer announcing
    `ct_room` against a config line `CT_ROOM` was therefore refused while a
    case-insensitive warning saw it as known and stayed silent: a refusal with
    no log line.

    The fix keeps pynetdicom's refusal and normalises what it compares.
    EVT_REQUESTED fires (association.py:667) with the A-ASSOCIATE-RQ in hand
    and BEFORE `acse.negotiate_association` runs (:671), so folding the
    received title here means the comparison at acse.py:333 sees the same
    spelling the allowlist uses. The allowlist is folded the same way in
    `_load_calling_ae_allowlist`, so both sides are canonical and one rule
    decides both the warning and the rejection.

    Sending our own RJ from this handler does NOT work: `acse.send_reject`
    only queues a primitive on the DUL (`send_pdu` puts it on
    `to_provider_queue`), and the reactor that would transmit it is torn down
    by the `is_rejected` check at association.py:670 before it can be sent -
    the peer sees a closed connection, not an RJ. The state machine's own
    path is the one that actually puts bytes on the wire.
    """
    allowed = tuple(allowed_titles or ())

    def on_assoc_requested(event):
        if not allowed:
            return
        assoc = event.assoc
        requestor = getattr(assoc, "requestor", None)
        primitive = getattr(requestor, "primitive", None)
        if primitive is None:
            return
        incoming = getattr(primitive, "calling_ae_title", None)
        if not calling_ae_allowed(incoming, allowed):
            # Warn for exactly the peer the negotiation is about to refuse.
            address = getattr(requestor, "address", None) if requestor else None
            log_unknown_calling_ae(incoming, address)
            return
        # Canonicalise in place so pynetdicom's case-sensitive compare at
        # acse.py:333 agrees with `calling_ae_allowed` above.
        folded = normalise_ae_title(incoming)
        if folded and folded != incoming:
            primitive.calling_ae_title = folded

    return on_assoc_requested
