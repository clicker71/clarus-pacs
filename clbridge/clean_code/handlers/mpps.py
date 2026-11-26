# Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.
# pyright: reportAttributeAccessIssue=false, reportCallIssue=false

"""Clarus Bridge - MPPS translator (DIMSE N-CREATE/N-SET -> DICOMweb MPPS).

PS3.18 section 15 serves Modality Performed Procedure Step over DICOMweb and
maps it onto the DIMSE operations one-to-one: Create (15.4) is
`POST /modality-performed-procedure-steps/{mppsUID}`, Update (15.5) is the
SAME URI with the mandatory `update` query parameter - the only thing that
distinguishes the two transactions - and Retrieve (15.6, N-GET) is optional
for the origin server and is not translated here.

The bridge holds no MPPS state: the origin server owns the record and its
state machine (PS3.4 F.7). Every parse and transport path answers with a DIMSE
status, never raises into the association reactor.
"""

import json
import logging
from urllib.parse import quote

import httpx
from pydicom.dataset import Dataset
from pydicom.multival import MultiValue

from clean_code.http_helpers import dicomweb_base
from clean_code.utils import _shutdown_requested

# PS3.4 Table F.7.2-1 (standard/ps3.4/mpps-f.7.2.1-attribute-table.json): every
# attribute the translator reads, with the VR the standard fixes for it. The
# VR is never guessed from a value's shape - a UID- or date-shaped string
# assigned the wrong VR makes the origin reject the whole body.
MPPS_ATTR_TAGS = {
    # SOP Common (PS3.3 C.3.3.1) - written by the translator, not forwarded.
    "00080016": (0x0008, 0x0016, "UI"),
    "00080018": (0x0008, 0x0018, "UI"),
    # Patient / study keys.
    "00080005": (0x0008, 0x0005, "CS"),
    "00100010": (0x0010, 0x0010, "PN"),
    "00100020": (0x0010, 0x0020, "LO"),
    "00100030": (0x0010, 0x0030, "DA"),
    "00100040": (0x0010, 0x0040, "CS"),
    "00080050": (0x0008, 0x0050, "SH"),
    # Procedure keys.
    "00080060": (0x0008, 0x0060, "CS"),
    "00400241": (0x0040, 0x0241, "AE"),
    "00400242": (0x0040, 0x0242, "SH"),
    "00400243": (0x0040, 0x0243, "SH"),
    "00400244": (0x0040, 0x0244, "DA"),
    "00400245": (0x0040, 0x0245, "TM"),
    "00400250": (0x0040, 0x0250, "DA"),
    "00400251": (0x0040, 0x0251, "TM"),
    "00400252": (0x0040, 0x0252, "CS"),
    "00400253": (0x0040, 0x0253, "SH"),
    "00400254": (0x0040, 0x0254, "LO"),
}

MPPS_SOP_CLASS_UID = "1.2.840.10008.3.1.2.3.3"

_STATUS = "00400252"
STATUS_IN_PROGRESS = "IN PROGRESS"
STATUS_COMPLETED = "COMPLETED"
STATUS_DISCONTINUED = "DISCONTINUED"

# PS3.4 F.7.2.1: "the SCU shall only send the N-CREATE request with Performed
# Procedure Step Status (0040,0252) set to IN PROGRESS. The SCP shall accept
# N-CREATE only if the value is IN PROGRESS; otherwise it shall return Failure
# Status 0106H 'Invalid Attribute Value' with an Attribute List." The same code
# is what PS3.7 lists for an out-of-range enumerated value on N-SET.
_INVALID_ATTRIBUTE_VALUE = 0x0106

# PS3.18 Table F.2.3-1: the VRs whose JSON type is a Number. The binary
# integer VRs are packed as integers on the wire; IS is decimal text that also
# maps to Number. FD/FL are the real-valued pair.
_INTEGER_VRS = frozenset({"US", "SS", "UL", "SL", "UV", "SV", "IS"})
_FLOAT_VRS = frozenset({"FD", "FL"})

# PS3.7 (standard/ps3.7/status-codes.json, service "mpps"):
#   0x0110 Processing Failure       - the request will never succeed
#   0x0111 Duplication SOP Instance - the UID is already in use
#   0x0112 No Such SOP Instance     - the instance N-SET addresses does not exist
#   0x0120 Missing Attribute        - a Type 1 attribute is absent
#   0x0121 Missing Attribute Value  - present but empty
#   0x0213 Resource Limitation      - the origin cannot take the work right now
_PROCESSING_FAILURE = 0x0110
_DUPLICATE_INSTANCE = 0x0111
_NO_SUCH_INSTANCE = 0x0112
_MISSING_ATTRIBUTE = 0x0120
_MISSING_ATTRIBUTE_VALUE = 0x0121
_RESOURCE_LIMITATION = 0x0213


class _Refusal(Exception):
    """A DIMSE refusal decided while parsing, carrying the status to answer."""

    def __init__(self, status, reason):
        super().__init__(reason)
        self.status = status
        self.reason = reason


# ---------------------------------------------------------------------------
# DIMSE data set -> DICOMweb JSON
# ---------------------------------------------------------------------------
def _json_value(vr, value):
    """One element value as a DICOMweb JSON `Value` entry (PS3.18 F.2.3).

    Table F.2.3-1 fixes the JSON type per VR: PN is a component-group object,
    and a numeric VR written as a JSON Number must stay a Number. A sequence
    is not handled here - its items are Data Sets, recursed into by
    `_element_json`.
    """
    if vr == "PN":
        return _pn_json(value)
    if vr in _INTEGER_VRS:
        # Integer binary VRs plus IS: the JSON type is Number. `UL` and the
        # other binary integer VRs were falling through to `str()`, so a
        # forwarded `UL` of 7 became "7" - a different JSON type from what the
        # VR allows, and one an origin validating against Table F.2.3-1
        # rejects. A value that will not parse stays as text rather than
        # raising: it is legal on the wire for IS, and for a binary VR it can
        # only come from a nonconforming source.
        try:
            return int(value)
        except (TypeError, ValueError):
            return str(value)
    if vr in _FLOAT_VRS:
        try:
            return float(value)
        except (TypeError, ValueError):
            return str(value)
    return str(value)


def _pn_json(value):
    """A PN value as its component-GROUP object (PS3.18 F.2.2).

    A Person Name is three component groups separated by `=`: Alphabetic,
    Ideographic and Phonetic. Annex F.2.2 gives each populated group its OWN
    property, so folding the whole value into `Alphabetic` (which is what a
    plain `str(value)` of `Yamada^Tarou=...==...` produced) made the origin
    read the two extra groups as part of the alphabetic name - and a name that
    has ONLY an ideographic group was published as an alphabetic one.

    Leading groups are kept even when empty, as `str()` preserves the
    separators, so the origin can still identify which group a value belongs
    to. A value with no separator is a single alphabetic group.
    """
    text = str(value)
    groups = text.split("=")
    names = ("Alphabetic", "Ideographic", "Phonetic")
    out = {}
    for name, group in zip(names, groups):
        if group:
            out[name] = group
    return out if out else {"Alphabetic": ""}


def _element_json(elem):
    """One DICOM element as a DICOMweb JSON attribute object (PS3.18 F.2.2).

    A zero-length attribute is written as an object with NO `Value`: F.2.5
    says an attribute that is present in DICOM but empty "shall be preserved in
    the DICOM JSON attribute object containing no Value, BulkDataURI or
    InlineBinary". Writing `{"Value": [""]}` would instead assert that ONE
    value exists and that it is the empty string - a different statement.
    """
    vr = elem.VR
    value = elem.value
    if vr == "SQ":
        items = [_dataset_json(item) for item in (value or [])]
        return {"vr": vr, "Value": items} if items else {"vr": vr}
    if value is None:
        return {"vr": vr}
    values = value if isinstance(value, (list, tuple, MultiValue)) else [value]
    # Value Length 0 is the same statement as "no value", and F.2.5 writes it
    # as an object with no `Value`. `{"Value": [""]}` would assert that ONE
    # value exists and that it is the empty string - pydicom hands over both
    # forms (an empty list for a sequence, an empty string for a text VR), so
    # both are folded here.
    rendered = [
        None if item in (None, "") else _json_value(vr, item) for item in values
    ]
    if not any(r is not None for r in rendered):
        # Every position is empty: F.2.5 says the attribute is written with no
        # `Value` at all, which is what the caller checks for.
        return {"vr": vr}
    return {"vr": vr, "Value": rendered}


def _dataset_json(ds):
    """A full Data Set as a DICOMweb JSON object, tags in ascending order."""
    return {
        f"{elem.tag.group:04X}{elem.tag.element:04X}": _element_json(elem)
        for elem in sorted(ds, key=lambda e: e.tag)
    }


# ---------------------------------------------------------------------------
# Validation of the untrusted request
# ---------------------------------------------------------------------------
def _read_dataset(event, attr):
    """Read the event's request data set, or refuse.

    pydicom decodes a Data Set DEFERRED, so a body with broken element encoding
    only raises when an element is first touched. Walking the set here turns
    that into a documented 0x0110 Processing Failure instead of an exception
    escaping the handler into the association reactor.
    """
    try:
        ds = getattr(event, attr)
    except AttributeError:
        raise _Refusal(_MISSING_ATTRIBUTE, f"request carries no {attr}") from None
    if ds is None:
        raise _Refusal(_MISSING_ATTRIBUTE, f"request carries no {attr}")
    try:
        for _elem in ds:
            pass
    except Exception as e:
        raise _Refusal(_PROCESSING_FAILURE, f"{attr} could not be decoded: {e}") from e
    return ds


def _text(ds, tag):
    """The stripped string value of one element; None when absent or empty."""
    elem = ds.get(MPPS_ATTR_TAGS[tag][:2])
    if elem is None or elem.value is None:
        return None
    text = str(elem.value).strip()
    return text or None


def _item_text(item, tag, name, series_number, ref_number=None):
    """A Type 1 value inside a sequence item, or a refusal naming the item."""
    elem = item.get(tag)
    text = (
        str(elem.value).strip() if elem is not None and elem.value is not None else ""
    )
    if text:
        return text
    where = f"Performed Series Sequence item {series_number}"
    if ref_number is not None:
        where += f", Referenced Image Sequence item {ref_number}"
    raise _Refusal(_MISSING_ATTRIBUTE, f"{where} has no {name}")


def _check_performed_series(ds, final=False):
    """Validate Performed Series Sequence (0040,0340) item shape.

    PS3.4 Table F.7.2-1 makes Series Instance UID (0020,000E) Type 1 inside
    every item, and puts the (0008,1150)/(0008,1155) reference pair in the
    NESTED Referenced Image Sequence (0008,1140), not on the item itself. The
    origin server stores whatever it is sent, so an item without its series UID
    would create a record no consumer can attribute to a series - refused here,
    while the offending SCU is still on the wire to be told.

    `final` says the update moves the step to COMPLETED/DISCONTINUED, where
    PS3.4 F.7.2.2 stops allowing "no series yet": an empty item is then a
    defect rather than the wire encoding of an absent sequence.
    """
    seq = ds.get((0x0040, 0x0340))
    if seq is None or seq.value is None:
        return
    # PS3.4 F.7.2.2 requires a NON-EMPTY Performed Series Sequence once the
    # status becomes COMPLETED/DISCONTINUED. An explicitly empty sequence is
    # not "the attribute was omitted": it states that no series was performed,
    # which contradicts the final status. Letting it through posted an empty
    # sequence to an origin that stores whatever it is sent, so the study
    # ended up with a completed MPPS and no performed series at all.
    #
    # Omission stays legal on a PARTIAL update (a valid series may already be
    # stored by the origin), so only the explicit-empty case is refused here.
    if final and len(seq.value) == 0:
        raise _Refusal(
            _MISSING_ATTRIBUTE_VALUE,
            "Performed Series Sequence (0040,0340) is empty; a final status "
            "requires at least one performed series (PS3.4 F.7.2.2)",
        )
    for number, item in enumerate(seq.value, start=1):
        if not isinstance(item, Dataset):
            raise _Refusal(
                _PROCESSING_FAILURE,
                f"Performed Series Sequence item {number} is not a Data Set",
            )
        # PS3.4 F.7.2-1 makes Performed Series Sequence (0040,0340) Type 2 at
        # N-CREATE: it may be empty because the series has not been performed
        # yet. A zero-length item is the wire encoding of "no series", and it
        # carries no Type 1 Series Instance UID to enforce - skip it instead of
        # refusing a conformant N-CREATE that has not acquired anything yet.
        #
        # The same skip is NOT correct on a FINAL N-SET: PS3.4 F.7.2.2 requires
        # a non-empty Performed Series Sequence once the status becomes
        # COMPLETED/DISCONTINUED, so an item present but empty there is a
        # malformed update, not "no series". `final` selects the rule.
        if len(item) == 0:
            if final:
                raise _Refusal(
                    _MISSING_ATTRIBUTE,
                    f"Performed Series Sequence item {number} is empty; a final "
                    "status requires a Series Instance UID (0020,000E)",
                )
            continue
        _item_text(item, (0x0020, 0x000E), "Series Instance UID (0020,000E)", number)
        ref_seq = item.get((0x0008, 0x1140))
        if ref_seq is None or ref_seq.value is None:
            continue
        for ref_number, ref in enumerate(ref_seq.value, start=1):
            if not isinstance(ref, Dataset):
                raise _Refusal(
                    _PROCESSING_FAILURE,
                    f"Referenced Image Sequence item {ref_number} of performed "
                    f"series {number} is not a Data Set",
                )
            _item_text(
                ref,
                (0x0008, 0x1150),
                "Referenced SOP Class UID (0008,1150)",
                number,
                ref_number,
            )
            _item_text(
                ref,
                (0x0008, 0x1155),
                "Referenced SOP Instance UID (0008,1155)",
                number,
                ref_number,
            )


def _check_status_for_create(ds):
    """An N-CREATE body must carry IN PROGRESS (PS3.4 F.7.2.1)."""
    status = _text(ds, _STATUS)
    if status is None:
        raise _Refusal(
            _MISSING_ATTRIBUTE,
            "Performed Procedure Step Status (0040,0252) is missing or empty",
        )
    # The comparison is EXACT, not case-folded. PS3.4 F.7.2.1 defines the
    # value as the enumerator `IN PROGRESS`, and the DICOM CS VR is
    # case-sensitive for defined terms; accepting `in progress` and then
    # posting it unchanged sent the origin a value it may reject or store as
    # an unknown term. The request is refused at the DIMSE boundary instead.
    if status != STATUS_IN_PROGRESS:
        raise _Refusal(
            _INVALID_ATTRIBUTE_VALUE,
            "Performed Procedure Step Status must be IN PROGRESS at N-CREATE "
            f"(PS3.4 F.7.2.1), got '{status}'",
        )


def _check_status_for_update(ds):
    """An N-SET body may carry a status, but only one of the three legal ones.

    The value that goes to the origin is the NORMALISED one (`_normalise_status`
    is called by the caller): a set that compared IN PROGRESS only after
    upper-casing used to forward the lower-case spelling unchanged, so the
    origin received a status it may not accept while the bridge had already
    validated it as legal.
    """
    status = _text(ds, _STATUS)
    if status is None:
        # N-SET is a partial update (PS3.4 F.7.2.2): omitting the attribute
        # keeps the stored status, so this is not an error.
        return None
    upper = status.upper()
    if upper not in (
        STATUS_IN_PROGRESS,
        STATUS_COMPLETED,
        STATUS_DISCONTINUED,
    ):
        raise _Refusal(
            _INVALID_ATTRIBUTE_VALUE,
            f"Invalid Performed Procedure Step Status '{status}' (0040,0252)",
        )
    return upper


def _normalise_status(ds, normalised):
    """Write the validated status back so the origin sees the legal spelling.

    Only a value the validator already accepted is rewritten, and only when the
    element is present: an absent status stays absent, keeping the N-SET
    partial-update semantics intact. A present-but-EMPTY status is REMOVED - it
    is the wire encoding of "no value", and forwarding it as `[""]` asserted a
    status the SCU never supplied.
    """
    tag = MPPS_ATTR_TAGS[_STATUS][:2]
    elem = ds.get(tag)
    if elem is None:
        return
    if normalised is None:
        del ds[tag]
        return
    elem.value = normalised


def _invalid_status_attribute_list(ds):
    """The Attribute List returned with an Invalid Attribute Value refusal.

    PS3.4 F.7.2.1: the SCP shall return status 0106H "with an Attribute List"
    for an N-CREATE whose status is not IN PROGRESS. The offending attribute is
    Performed Procedure Step Status (0040,0252) itself, so the list names that
    attribute rather than echoing the request - the SCU already knows the rest
    of what it sent, and the point of the list is to identify the offender.

    A single-element Data Set in DIMSE encoding terms; pynetdicom encodes it
    into the response's Attribute List identifier region. Built here rather
    than reused from the request because the refused value must NOT travel
    back as if it were accepted.
    """
    out = Dataset()
    out.PerformedProcedureStepStatus = STATUS_IN_PROGRESS
    return out


def _body(ds, mpps_uid):
    """The DICOMweb request body for a validated DIMSE data set.

    The SOP Common pair comes from the translator, not from the client's data
    set: DIMSE carries the instance UID as a command field (PS3.7 10.1.5.1 /
    10.3.5), so a data set that also names a DIFFERENT UID is describing an
    object it is not addressing. The command field wins.
    """
    body = _dataset_json(ds)
    body["00080016"] = {"vr": "UI", "Value": [MPPS_SOP_CLASS_UID]}
    body["00080018"] = {"vr": "UI", "Value": [mpps_uid]}
    return body


# ---------------------------------------------------------------------------
# HTTP -> DIMSE status
# ---------------------------------------------------------------------------
def _http_dimse_status(status_code, on_conflict, on_missing):
    """Map the origin's HTTP answer onto a DIMSE status.

    PS3.18 Table 15.4.3-1 enumerates the Create answers: 201 Created, 400 Bad
    Request, 409 Conflict ("the provided MPPS UID is already in use"), 503
    Service Unavailable. Update answers 200 on success and 404 when the
    instance is unknown.
    """
    if 200 <= status_code < 300:
        return 0x0000
    if status_code == 409:
        return on_conflict
    if status_code == 404:
        return on_missing
    if status_code in (429, 503):
        # The origin is asking to be retried. Both listed values are the
        # enumerated "come back later" failure; 0xA700 is a C-STORE/C-FIND
        # warning and is not a legal MPPS status.
        return _RESOURCE_LIMITATION
    return _PROCESSING_FAILURE


def _mpps_url(clarus_url, mpps_uid, update):
    """`{base}/modality-performed-procedure-steps/{mppsUID}` (+ `?update`).

    `safe=""` per segment: a UID is one path element, so `/`, `?` and `#`
    inside it must not change which resource the POST addresses (PS3.18 10.1
    resource paths). `update` is the query parameter PS3.18 15.5 makes
    mandatory for the Update transaction - without it the same URI addresses
    the Create transaction instead.
    """
    base = dicomweb_base(clarus_url)
    url = f"{base}/modality-performed-procedure-steps/{quote(mpps_uid, safe='')}"
    return url + "?update" if update else url


def _push(label, url, body, mpps_uid, http_timeout, tls):
    """POST one transaction and answer ``(http_status, None)`` or ``(None, why)``.

    No retry here on purpose: an N-CREATE/N-SET runs inside the DIMSE reactor
    while the modality waits on one request, and a repeated Create after a
    timeout the origin actually processed would come back 409 on the second
    attempt - a duplicate reported for an instance that exists BECAUSE of the
    first. A failure is answered as a failure and the modality decides.

    The response body is not read: neither transaction returns an attribute the
    bridge forwards (Create/Update answer a render of the stored record), and
    the context-managed client releases the connection either way.
    """
    try:
        client = httpx.Client(timeout=httpx.Timeout(http_timeout + 10.0), cert=tls)
        try:
            resp = client.post(
                url,
                content=json.dumps(body).encode("utf-8"),
                headers={
                    "Accept": "application/dicom+json",
                    "Content-Type": "application/dicom+json",
                },
            )
        finally:
            client.close()
    except httpx.TimeoutException as e:
        return None, f"origin timeout ({e})"
    except httpx.HTTPError as e:
        return None, f"transport error {type(e).__name__}: {e}"
    return resp.status_code, None


def _calling_ae(event):
    """The calling AE Title for the log line; '' when the event carries none."""
    try:
        return str(event.assoc.requestor.ae_title)
    except Exception:
        return ""


def _request_uid(event, attr, label):
    """The instance UID from the DIMSE command field, or a refusal.

    N-CREATE carries the Affected SOP Instance UID (0000,1000) and N-SET the
    Requested SOP Instance UID (0000,1001) (PS3.7 10.1.5.1 / 10.3.5). Neither
    is a data set attribute, so a data set cannot substitute for one; both live
    on the request primitive, which `event.request` is.
    """
    try:
        uid = getattr(event.request, attr)
    except AttributeError:
        uid = None
    text = str(uid).strip() if uid is not None else ""
    if not text:
        raise _Refusal(_MISSING_ATTRIBUTE, f"{label} carries no {attr}")
    return text


def _handle(event, clarus_url, label, uid_attr, ds_attr, update, http_timeout, tls):
    """Shared body of N-CREATE and N-SET: validate, POST, map the answer.

    `uid_attr` is the request primitive's own field: N-CREATE reads the
    Affected SOP Instance UID and N-SET the Requested SOP Instance UID (PS3.7
    10.1.5.1 / 10.3.5). `update` selects the transaction, because PS3.18 15.5
    distinguishes Create from Update ONLY by the `update` query parameter on an
    otherwise identical URI - so the choice is a parameter of one code path,
    never two paths that could drift apart.
    """
    if _shutdown_requested.is_set():
        return 0xC000, None
    ds = None
    try:
        mpps_uid = _request_uid(event, uid_attr, label)
        ds = _read_dataset(event, ds_attr)
        if update:
            normalised = _check_status_for_update(ds)
            # The validator upper-cases for its comparison; the origin must
            # receive the same legal spelling, not the raw one.
            _normalise_status(ds, normalised)
            _check_performed_series(
                ds,
                final=normalised in (STATUS_COMPLETED, STATUS_DISCONTINUED),
            )
        else:
            _check_status_for_create(ds)
            _check_performed_series(ds)
        body = _body(ds, mpps_uid)
    except _Refusal as refusal:
        logging.warning(
            "[%s] MPPS refused 0x%04X: %s", label, refusal.status, refusal.reason
        )
        # PS3.4 F.7.2.1 requires the Invalid Attribute Value refusal on
        # N-CREATE to be accompanied by an Attribute List identifying the
        # offending attribute. Answering the bare status leaves a conformant
        # SCU with no way to learn WHICH attribute was wrong.
        #
        # Only 0x0106 qualifies: the other refusals this handler raises are
        # missing/undecodable data, where no attribute list is defined.
        if refusal.status == _INVALID_ATTRIBUTE_VALUE and not update and ds is not None:
            return refusal.status, _invalid_status_attribute_list(ds)
        return refusal.status, None
    except Exception as e:
        # A refusal with an evidence-free traceback would escape into the
        # association reactor, so every other parse surprise is answered as
        # the documented Processing Failure.
        logging.error("[%s] unexpected parse error: %s", label, e)
        logging.debug("[%s] Full traceback follows:", label, exc_info=True)
        return _PROCESSING_FAILURE, None

    status_code, error = _push(
        label,
        _mpps_url(clarus_url, mpps_uid, update),
        body,
        mpps_uid,
        http_timeout,
        tls,
    )
    if status_code is None:
        logging.warning(
            "[%s] MPPS %s: %s -> 0x%04X", label, mpps_uid, error, _PROCESSING_FAILURE
        )
        return _PROCESSING_FAILURE, None

    # A 409 on Create is the "UID already in use" case PS3.18 Table 15.4.3-1
    # names; on Update it cannot mean a duplicate instance (the MPPS already
    # exists), so it falls through to the general refusal.
    status = _http_dimse_status(
        status_code,
        on_conflict=_DUPLICATE_INSTANCE if not update else _PROCESSING_FAILURE,
        on_missing=_PROCESSING_FAILURE if not update else _NO_SUCH_INSTANCE,
    )
    log = logging.info if status == 0x0000 else logging.warning
    # N-CREATE carries the Affected SOP Instance UID, N-SET the Requested one,
    # so the same log line names the field this request actually used.
    log(
        "[%s] MPPS %s=%s from %s: HTTP %d -> 0x%04X",
        label,
        uid_attr,
        mpps_uid,
        _calling_ae(event),
        status_code,
        status,
    )
    return status, None


def handle_n_create(event, clarus_url, http_timeout=30, tls=None):
    """N-CREATE-RQ -> PS3.18 15.4 Create. Returns (status, None).

    A created instance is acknowledged with no Attribute List: the record the
    origin stored IS the result, and echoing the request back would let an
    attribute the origin rejected travel back as if it had been accepted.
    """
    return _handle(
        event,
        clarus_url,
        "N-CREATE",
        "AffectedSOPInstanceUID",
        "attribute_list",
        update=False,
        http_timeout=http_timeout,
        tls=tls,
    )


def handle_n_set(event, clarus_url, http_timeout=30, tls=None):
    """N-SET-RQ -> PS3.18 15.5 Update. Returns (status, None).

    The Modification List is a PARTIAL data set: PS3.4 F.7.2.2 merges it onto
    the stored instance, so nothing here demands a Type 1 attribute that the
    creation already supplied. Only the status value and the shape of a
    sequence that IS present are checked.
    """
    return _handle(
        event,
        clarus_url,
        "N-SET",
        "RequestedSOPInstanceUID",
        "modification_list",
        update=True,
        http_timeout=http_timeout,
        tls=tls,
    )
