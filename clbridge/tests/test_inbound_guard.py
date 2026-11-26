# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Inbound association guards on the wire: PDU ceiling and calling-AE allowlist.

Both checks run against a REAL pynetdicom AE over a raw socket, not against a
mock: the whole point is that pynetdicom 3.0.4 does not enforce either, so a
unit test of the predicate alone would pass while the association still
accepted the frame.

T1 - PS3.8 9.3.2.2: the negotiated Maximum Length Received bounds the PDUs a
peer may send. `DULServiceProvider._read_pdu_data` reads whatever length the
header declares, so an oversized P-DATA-TF reaches the DIMSE layer and the
instance is stored. EVT_PDU_RECV fires after decode and before the DIMSE
message reaches a service handler, so the abort refuses it at the protocol
level and no handler ever runs.

T3 - the calling-AE allowlist is answered with A-ASSOCIATE-RJ during
negotiation (PS3.7 Table 9-21: rejected-transient, DUL service user,
calling-AE-title-not-recognised), so an unknown peer cannot get an accepted
association to send anything on.
"""

import io
import socket
import threading
import time

import pytest
from clean_code.inbound_guard import (
    calling_ae_allowed,
    ceiling_applies,
    declared_pdu_length,
    install_guarded_dul,
    make_calling_ae_guard,
    make_pdu_ceiling_handler,
    normalise_ae_title,
    pdu_ceiling_message,
    pdu_exceeds_maximum,
    pdu_length_of,
    pdu_type_of,
    unknown_calling_ae_message,
)
from pynetdicom import AE, evt
from pynetdicom.dimse_messages import C_STORE_RQ
from pynetdicom.pdu import A_ASSOCIATE_RQ, P_DATA_TF
from pynetdicom.pdu_items import PresentationDataValueItem
from pynetdicom.pdu_primitives import (
    A_ASSOCIATE,
    ImplementationClassUIDNotification,
    MaximumLengthNotification,
)
from pynetdicom.pdu_primitives import PresentationContext as PrimPresentationContext

MAXLEN = 16384
ALLOWED = ["CT_ROOM"]


# ---------------------------------------------------------------------------
# Unit level: the predicates
# ---------------------------------------------------------------------------
class TestDeclaredPduLength:
    """PS3.8 9.3.1 Table 9-1: bytes 2-5 are the big-endian PDU length."""

    def test_reads_the_length_field(self):
        header = bytes([0x04, 0x00, 0x00, 0x00, 0x40, 0x00])
        assert declared_pdu_length(header) == 0x4000

    def test_short_buffer_is_not_judged(self):
        """Fewer than six bytes means no header yet - None, never "fits"."""
        assert declared_pdu_length(b"\x04\x00") is None
        assert declared_pdu_length(b"") is None
        assert declared_pdu_length(None) is None

    def test_decoded_pdu_exposes_the_same_field(self):
        pdu = P_DATA_TF()
        assert pdu_length_of(pdu) == 0
        assert pdu_length_of(object()) is None


class TestCeilingBoundary:
    """Strictly greater: exactly-at-maximum is conformant (PS3.8 9.3.2.2)."""

    class _Assoc:
        def __init__(self, maximum):
            class _Acc:
                maximum_length = maximum

            self.acceptor = _Acc()

    def _fits(self, declared, maximum):
        header = bytes([0x04, 0x00]) + declared.to_bytes(4, "big")
        from clean_code.inbound_guard import pdu_exceeds_maximum

        return not pdu_exceeds_maximum(self._Assoc(maximum), header)

    def test_one_byte_over_is_refused(self):
        assert not self._fits(MAXLEN + 1, MAXLEN)

    def test_exactly_the_maximum_is_accepted(self):
        assert self._fits(MAXLEN, MAXLEN)

    def test_one_byte_under_is_accepted(self):
        assert self._fits(MAXLEN - 1, MAXLEN)

    def test_unnagoitiated_maximum_is_not_judged(self):
        assert self._fits(MAXLEN + 1000, None)

    def test_message_names_both_numbers(self):
        assoc = self._Assoc(MAXLEN)
        assert "17406" in pdu_ceiling_message(assoc, 17406)
        assert str(MAXLEN) in pdu_ceiling_message(assoc, 17406)


class TestCallingAeAllowlist:
    """AE titles are space padded to 16 chars and compared case-insensitively."""

    def test_title_is_normalised(self):
        assert normalise_ae_title("  ct_room  ") == "CT_ROOM"
        assert normalise_ae_title(None) == ""

    @pytest.mark.parametrize("calling", ["CT_ROOM", "ct_room", "  Ct_RoOm  "])
    def test_listed_title_matches_regardless_of_case_and_padding(self, calling):
        assert calling_ae_allowed(calling, ALLOWED)

    def test_unlisted_title_is_refused(self):
        assert not calling_ae_allowed("INTRUDER", ALLOWED)

    def test_empty_allowlist_disables_the_filter(self):
        assert calling_ae_allowed("ANYBODY", [])
        assert calling_ae_allowed("ANYBODY", None)

    def test_an_empty_calling_title_is_never_allowed(self):
        """A blank AE title names nobody; accepting it would let any peer
        through by announcing blanks."""
        assert not calling_ae_allowed("", ALLOWED)
        assert not calling_ae_allowed("   ", ALLOWED)

    def test_warning_matches_the_move_destination_style(self):
        msg = unknown_calling_ae_message("INTRUDER", "10.0.0.5")
        assert "unknown calling AE 'INTRUDER'" in msg
        assert "10.0.0.5" in msg
        assert "peers.conf" in msg


# ---------------------------------------------------------------------------
# Wire level: a real AE and a raw socket
# ---------------------------------------------------------------------------
def _build_request(calling_aet, maximum=MAXLEN, contexts=None):
    prim = A_ASSOCIATE()
    prim.calling_ae_title = calling_aet
    prim.called_ae_title = "SUT"
    prim.application_context_name = "1.2.840.10008.3.1.1.1"
    if contexts is None:
        contexts = [("1.2.840.10008.1.1", 1)]
    ctx_list = []
    for abstract, ctx_id in contexts:
        ctx = PrimPresentationContext()
        ctx.context_id = ctx_id
        ctx.abstract_syntax = abstract
        ctx.transfer_syntax = ["1.2.840.10008.1.2.1"]
        ctx_list.append(ctx)
    prim.presentation_context_definition_list = ctx_list
    length = MaximumLengthNotification()
    length.maximum_length = maximum
    impl = ImplementationClassUIDNotification()
    impl.implementation_class_uid = "1.2.826.0.1.3680043.9.7536"
    prim.user_information = [length, impl]
    rq = A_ASSOCIATE_RQ()
    rq.from_primitive(prim)
    return rq


class _Server:
    """A real pynetdicom AE on a loopback port, serving in a daemon thread.

    The handlers installed here are the PRODUCTION ones (`install_guarded_dul`,
    `make_calling_ae_guard`, `make_pdu_ceiling_handler`), so a test cannot pin
    a rule the bridge does not run.
    """

    def __init__(self, restrict_calling_aet=None):
        self.pdu_lengths = []
        self.bodies_read = []
        self.echo_handled = 0
        self.store_handled = 0
        self.rejected = 0
        self.aborted = 0
        self.ae = AE(ae_title="SUT")
        self.ae.maximum_pdu_size = MAXLEN
        self.ae.add_supported_context("1.2.840.10008.1.1", "1.2.840.10008.1.2.1")
        # A storage context, so the C-STORE falsifier exercises the path
        # DCST4 actually uses.
        self.ae.add_supported_context(
            "1.2.840.10008.5.1.4.1.1.2", "1.2.840.10008.1.2.1"
        )
        # Production arming (see _init_dimse_ae): the allowlist drives
        # pynetdicom's own A-ASSOCIATE-RJ path, and EVT_REQUESTED folds the
        # received title so its case-sensitive compare matches.
        self._allowed = tuple(
            normalise_ae_title(t) for t in (restrict_calling_aet or ())
        )
        if self._allowed:
            self.ae.require_calling_aet = list(self._allowed)
        self._pdu_ceiling = make_pdu_ceiling_handler()
        threading.Thread(target=self._serve, daemon=True).start()
        for _ in range(200):
            if self.ae._servers:
                break
            time.sleep(0.01)
        self.port = self.ae._servers[0].socket.getsockname()[1]

    def _serve(self):
        self.ae.start_server(
            ("127.0.0.1", 0),
            block=True,
            evt_handlers=[
                (evt.EVT_CONN_OPEN, self._install_guarded),
                (evt.EVT_REQUESTED, make_calling_ae_guard(self._allowed)),
                (evt.EVT_PDU_RECV, self._guard),
                (evt.EVT_C_ECHO, self._on_c_echo),
                (evt.EVT_C_STORE, self._on_c_store),
                (evt.EVT_REJECTED, self._on_rejected),
                (evt.EVT_ABORTED, self._on_aborted),
            ],
        )

    def _install_guarded(self, event):
        install_guarded_dul(event)

    def _guard(self, event):
        self.pdu_lengths.append(pdu_length_of(event.pdu))
        return self._pdu_ceiling(event)

    def _on_c_echo(self, event):
        self.echo_handled += 1
        return 0x0000

    def _on_c_store(self, event):
        # Counts INVOCATION, not success: an oversized frame must not reach
        # this at all.
        self.store_handled += 1
        return 0x0000

    def _on_rejected(self, event):
        self.rejected += 1

    def _on_aborted(self, event):
        self.aborted += 1

    def connect(self, calling_aet, contexts=None):
        sock = socket.create_connection(("127.0.0.1", self.port), timeout=10)
        sock.sendall(_build_request(calling_aet, contexts=contexts).encode())
        header = sock.recv(6)
        if not header:
            return sock, None, b""
        body = sock.recv(int.from_bytes(header[2:6], "big"))
        return sock, header[0], body

    def shutdown(self):
        self.ae.shutdown()


@pytest.fixture
def server():
    created = []

    def _make(restrict=None):
        srv = _Server(restrict)
        created.append(srv)
        return srv

    yield _make
    for srv in created:
        srv.shutdown()


class TestOversizedPduOnTheWire:
    """T1: refused over the announced maximum, accepted at it."""

    def _send_pdata(self, sock, declared_length):
        """Send a P-DATA-TF whose PDU header declares `declared_length`.

        The encoder frames each value as an item (type + reserved + 4-byte
        length) plus the context id, and pads the value with its message
        control header: measured overhead is a constant 5 bytes on top of the
        value, so the value length is the declared length minus that.
        """
        value_len = declared_length - 5
        assert value_len > 0, "declared length must leave room for a value"
        item = PresentationDataValueItem()
        item.presentation_context_id = 1
        item.presentation_data_value = b"\x55" * value_len
        pdu = P_DATA_TF()
        pdu.presentation_data_value_items = [item]
        encoded = pdu.encode()
        assert len(encoded) - 6 == declared_length, (
            "probe must send exactly the length it claims: "
            f"{len(encoded) - 6} != {declared_length}"
        )
        sock.sendall(encoded)

    def test_oversized_pdu_aborts_before_any_handler_runs(self, server):
        """The DCST4 case: 1024 bytes over the announced 16384.

        The strong claim is that the body is never READ: an oversized PDU
        therefore never reaches `EVT_PDU_RECV` at all, so the recorded lengths
        contain only the conformant A-ASSOCIATE-RQ.
        """
        srv = server()
        sock, pdu_type, _body = srv.connect("RAWCLIENT")
        assert pdu_type == 0x02, "association must be accepted first"

        self._send_pdata(sock, MAXLEN + 1024)
        time.sleep(1.0)
        sock.settimeout(2.0)
        try:
            after = sock.recv(6)
        except (OSError, socket.timeout):
            after = b""
        sock.close()
        time.sleep(0.3)

        assert MAXLEN + 1024 not in srv.pdu_lengths, (
            "the oversized PDU was DECODED and delivered to EVT_PDU_RECV - "
            "the ceiling was applied after the read, not before it "
            f"(finding :119); lengths seen: {srv.pdu_lengths}"
        )
        assert after[:1] in (b"\x07", b""), (
            "an oversized PDU must be answered with A-ABORT (PDU type 0x07) "
            f"or the connection torn down, got {after[:1]!r}"
        )
        assert (
            srv.echo_handled == 0
        ), "no DIMSE operation may run after the PDU ceiling was broken"

    def test_exactly_the_maximum_is_not_refused(self, server):
        """Fail-then-pass boundary: the SAME rule refuses one byte more, so the
        check is a ceiling and not a blanket rejection.

        `aborted` is deliberately not asserted here: closing the raw socket in
        the test is itself an abort from the peer's side, which would make the
        counter say nothing about the guard.
        """
        srv = server()
        sock, pdu_type, _body = srv.connect("RAWCLIENT")
        assert pdu_type == 0x02

        self._send_pdata(sock, MAXLEN)
        time.sleep(1.0)
        sock.close()
        time.sleep(0.3)

        assert (
            srv.pdu_lengths[-1] == MAXLEN
        ), f"the at-maximum PDU must still be delivered, got {srv.pdu_lengths}"
        assert srv.pdu_lengths.count(MAXLEN) == 1
        # Nothing in the recorded lengths was over the ceiling, i.e. the guard
        # did not fire on a conformant frame.
        assert all(length <= MAXLEN for length in srv.pdu_lengths)


class TestOversizedCStore:
    """T1 :128: an oversized P-DATA-TF must not reach the C-STORE handler.

    This is the DCST4 defect in its own terms: the frame is a COMPLETE
    C-STORE-RQ, so "the association aborted" is not the claim - the claim is
    that the instance is never stored. A dispatch-level guard that only aborted
    would still let this frame through, because the PDU is already decoded and
    queued before the abort is honoured.
    """

    SOP_CLASS = "1.2.840.10008.5.1.4.1.1.2"
    SOP_INSTANCE = "1.2.3.4.5.6.7.8.9"

    def _cstore_frames(self):
        """(command PDV, data PDV) for a complete C-STORE-RQ on context 3."""
        message = C_STORE_RQ()
        message.command_set.CommandField = 0x0001
        message.command_set.MessageID = 1
        message.command_set.AffectedSOPClassUID = self.SOP_CLASS
        message.command_set.AffectedSOPInstanceUID = self.SOP_INSTANCE
        message.command_set.Priority = 0
        message.command_set.CommandDataSetType = 0x0001
        # A small data set; the OVERRUN comes from padding the last P-DATA-TF,
        # not from the object size, so the test isolates the ceiling.
        message.data_set = io.BytesIO(b"\x00" * 64)
        frames = []
        for prim in message.encode_msg(3, 0):
            for ctx_id, value in prim.presentation_data_value_list:
                frames.append((ctx_id, value))
        return frames

    def _send_cstore_pdata(self, sock, declared_length):
        """Send the whole C-STORE-RQ in ONE P-DATA-TF of `declared_length`."""
        frames = self._cstore_frames()
        items = []
        for ctx_id, value in frames:
            item = PresentationDataValueItem()
            item.presentation_context_id = ctx_id
            item.presentation_data_value = value
            items.append(item)
        # Pad the data-set fragment so the PDU lands on the declared length.
        # The frame overhead is measured from the encoder rather than assumed:
        # item framing differs between one and several PDVs.
        pdu = P_DATA_TF()
        pdu.presentation_data_value_items = items
        shortfall = declared_length - (len(pdu.encode()) - 6)
        assert shortfall >= 0, (
            "declared length is shorter than the C-STORE frames themselves: "
            f"{declared_length} < {len(pdu.encode()) - 6}"
        )
        last = items[-1]
        last.presentation_data_value = last.data + b"\x00" * shortfall
        encoded = pdu.encode()
        assert len(encoded) - 6 == declared_length, (
            f"frame must declare exactly {declared_length}, " f"got {len(encoded) - 6}"
        )
        sock.sendall(encoded)

    def test_oversized_cstore_never_reaches_the_handler(self, server):
        srv = server()
        sock, pdu_type, _body = srv.connect(
            "RAWCLIENT", contexts=[(self.SOP_CLASS, 1), ("1.2.840.10008.1.1", 3)]
        )
        assert pdu_type == 0x02, "association must be accepted first"

        self._send_cstore_pdata(sock, MAXLEN + 1024)
        time.sleep(1.2)
        # The server aborts, so the connection goes quiet rather than closing
        # cleanly: bound the read instead of blocking on it.
        sock.settimeout(2.0)
        try:
            after = sock.recv(6)
        except (OSError, socket.timeout):
            after = b""
        sock.close()
        time.sleep(0.4)

        assert srv.store_handled == 0, (
            "the oversized C-STORE-RQ reached the storage handler - the "
            "ceiling bounded dispatch, not the receive work (finding :128)"
        )
        assert after[:1] in (b"\x07", b""), (
            "an oversized P-DATA-TF must be answered with A-ABORT (or the "
            f"connection torn down), got {after[:1]!r}"
        )

    def test_a_cstore_within_the_ceiling_is_stored(self, server):
        """The pass half: the SAME frame under the ceiling must be handled, so
        the test above proves a boundary and not a broken C-STORE path."""
        srv = server()
        sock, pdu_type, _body = srv.connect(
            "RAWCLIENT", contexts=[(self.SOP_CLASS, 1), ("1.2.840.10008.1.1", 3)]
        )
        assert pdu_type == 0x02

        self._send_cstore_pdata(sock, MAXLEN - 64)
        time.sleep(1.2)
        sock.close()
        time.sleep(0.4)

        assert srv.store_handled == 1, (
            "a C-STORE-RQ under the ceiling must reach the handler, "
            f"got {srv.store_handled}"
        )


class TestPreNegotiationIsNeverCapped:
    """T1 :122: the ceiling applies to P-DATA-TF after establishment only.

    PS3.8 9.3.2.2 scopes the negotiated maximum to the variable field of a
    P-DATA-TF. An A-ASSOCIATE-RQ is legitimately larger than the default
    before anything is negotiated, so refusing it would break association
    setup for every peer with a wide context list.
    """

    def test_an_oversized_associate_rq_is_not_refused(self, server):
        srv = server()
        # A request whose User Information alone is larger than MAXLEN: a wide
        # presentation-context list under a small announced maximum.
        contexts = [("1.2.840.10008.1.1", i) for i in range(1, 2)]
        primitive = A_ASSOCIATE()
        primitive.calling_ae_title = "RAWCLIENT"
        primitive.called_ae_title = "SUT"
        primitive.application_context_name = "1.2.840.10008.3.1.1.1"
        ctx = PrimPresentationContext()
        ctx.context_id = 1
        ctx.abstract_syntax = "1.2.840.10008.1.1"
        ctx.transfer_syntax = ["1.2.840.10008.1.2.1"]
        primitive.presentation_context_definition_list = [ctx]
        # Announce a tiny maximum so the ASSOCIATION REQUEST itself is over
        # the limit it carries.
        length = MaximumLengthNotification()
        length.maximum_length = 64
        impl = ImplementationClassUIDNotification()
        impl.implementation_class_uid = "1.2.826.0.1.3680043.9.7536"
        primitive.user_information = [length, impl]
        rq = A_ASSOCIATE_RQ()
        rq.from_primitive(primitive)
        encoded = rq.encode()
        assert len(encoded) > 64, (
            "test setup must actually exceed the announced maximum: "
            f"{len(encoded)} vs 64"
        )

        sock = socket.create_connection(("127.0.0.1", srv.port), timeout=10)
        sock.sendall(encoded)
        header = sock.recv(6)
        sock.close()
        time.sleep(0.3)

        assert header and header[0] == 0x02, (
            "an A-ASSOCIATE-RQ must never be aborted by the PDU ceiling "
            f"(PS3.8 9.3.2.2 applies it to P-DATA-TF); response type was "
            f"{header[0] if header else None!r}"
        )
        assert contexts  # keep the fixture intent visible


class TestCeilingScope:
    """The predicate itself: P-DATA-TF only, and only once established."""

    def test_pdu_type_is_read_from_the_header(self):
        assert pdu_type_of(b"\x04\x00\x00\x00\x00\x10") == 0x04
        assert pdu_type_of(b"") is None

    def test_ceiling_applies_only_to_pdata_with_a_maximum(self):
        assert ceiling_applies(0x04, MAXLEN) is True
        assert ceiling_applies(0x01, MAXLEN) is False, "A-ASSOCIATE-RQ"
        assert ceiling_applies(0x05, MAXLEN) is False, "A-RELEASE-RQ"
        assert ceiling_applies(0x07, MAXLEN) is False, "A-ABORT"
        assert ceiling_applies(0x04, 0) is False, "nothing negotiated yet"

    def test_an_associate_header_over_the_limit_is_not_capped(self):
        class _Assoc:
            class acceptor:
                maximum_length = MAXLEN

        header = bytes([0x01, 0x00]) + (MAXLEN + 5000).to_bytes(4, "big")
        assert pdu_exceeds_maximum(_Assoc(), header) is False

    def test_a_pdata_header_over_the_limit_is_capped(self):
        class _Assoc:
            class acceptor:
                maximum_length = MAXLEN

        header = bytes([0x04, 0x00]) + (MAXLEN + 1).to_bytes(4, "big")
        assert pdu_exceeds_maximum(_Assoc(), header) is True


class TestCallingAeRejectionOnTheWire:
    """T3: an unknown calling AE gets A-ASSOCIATE-RJ, an allowed one gets AC."""

    def test_unknown_calling_ae_is_rejected(self, server):
        srv = server(restrict=ALLOWED)
        sock, pdu_type, body = srv.connect("INTRUDER")
        sock.close()
        time.sleep(0.3)

        assert pdu_type == 0x03, (
            f"expected A-ASSOCIATE-RJ (0x03), got {pdu_type!r}: an unknown "
            "calling AE must not receive an accepted association"
        )
        # PS3.7 Table 9-21. The RJ body leads with a reserved byte, then
        # result, source and diagnostic.
        assert body[0] == 0x00, f"reserved byte={body[0]}"
        assert body[1] == 0x01, f"result={body[1]}"
        assert body[2] == 0x01, f"source={body[2]}"
        assert body[3] == 0x03, f"diagnostic={body[3]}"
        assert srv.rejected == 1

    def test_listed_calling_ae_is_accepted(self, server):
        srv = server(restrict=ALLOWED)
        sock, pdu_type, _body = srv.connect("CT_ROOM")
        sock.close()
        time.sleep(0.3)
        assert pdu_type == 0x02, "a listed AE must be accepted"
        assert srv.rejected == 0

    def test_empty_allowlist_accepts_everyone(self, server):
        srv = server(restrict=None)
        sock, pdu_type, _body = srv.connect("ANYBODY")
        sock.close()
        time.sleep(0.3)
        assert pdu_type == 0x02


class TestOneComparisonOneDecision:
    """T3 :209: refusal and warning must agree.

    pynetdicom's `require_calling_aet` compares the received title
    case-SENSITIVELY (acse.py:330-334), so a peer announcing `ct_room` against
    a config line `CT_ROOM` would be refused while a case-insensitive warning
    reported it as known - a refusal with no log line. The bridge now uses one
    rule for both, so the two halves of this test must move together.
    """

    @pytest.mark.parametrize("calling", ["CT_ROOM", "ct_room", "  Ct_RoOm  "])
    def test_every_spelling_of_a_listed_title_is_accepted(self, server, calling):
        srv = server(restrict=ALLOWED)
        # The title is padded/truncated to 16 characters on the wire; the
        # significant part survives, which is what PS3.5 6.2 defines.
        sock, pdu_type, _body = srv.connect(calling)
        sock.close()
        time.sleep(0.3)
        assert pdu_type == 0x02, (
            f"{calling!r} names the same AE as 'CT_ROOM' (PS3.5 6.2: case is "
            "not significant, padding is stripped) and must be accepted"
        )
        assert (
            srv.rejected == 0
        ), f"{calling!r} was refused although the allowlist names CT_ROOM"

    def test_the_warning_is_emitted_for_exactly_what_is_refused(self, server, caplog):
        """An unlisted title is refused AND logged - the pair, in one run."""
        import logging

        caplog.set_level(logging.WARNING)
        srv = server(restrict=ALLOWED)
        sock, pdu_type, _body = srv.connect("INTRUDER")
        sock.close()
        time.sleep(0.4)

        assert pdu_type == 0x03, "an unlisted title must be refused"
        warned = [r for r in caplog.records if "unknown calling AE" in r.getMessage()]
        assert warned, (
            "a refused association must leave a log line naming the AE; "
            f"records seen: {[r.getMessage() for r in caplog.records][:5]}"
        )
        assert "INTRUDER" in warned[0].getMessage()
        assert "peers.conf" in warned[0].getMessage()

    def test_an_accepted_title_leaves_no_warning(self, server, caplog):
        """The other half of the pair: no silent-refusal inversion."""
        import logging

        caplog.set_level(logging.WARNING)
        srv = server(restrict=ALLOWED)
        sock, pdu_type, _body = srv.connect("ct_room")
        sock.close()
        time.sleep(0.4)

        assert pdu_type == 0x02, "accepted"
        assert not [
            r for r in caplog.records if "unknown calling AE" in r.getMessage()
        ], "an accepted peer must not be reported as unknown"
