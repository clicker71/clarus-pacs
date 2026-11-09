#!/usr/bin/env python3
"""Phase 0 conformance smoke: multi-context association probe.

Proposes ONE SOP class with MULTIPLE single-transfer-syntax presentation
contexts (the standard negotiation shape of PS3.8 7.1.1.13 / 9.3.2.2) and
records what the target SCP accepts on each run. This is a smoke of OUR
proposal shape against the field pair (bridge host -> viewer SCP), not a
design gate: a misbehaving viewer is a viewer bug (see
analysis/0109_microdicom_acse_wedge_report.md).

Usage:
    probe_multicontext.py <host> <port> <ae_title> [runs] [class]
    class: CT | DX | US (default CT)
    runs:  default 5, 2 s pause between runs
"""

import inspect
import sys
import time

from pydicom.uid import ExplicitVRLittleEndian, ImplicitVRLittleEndian
from pynetdicom import AE
from pynetdicom.sop_class import (
    CTImageStorage,
    DigitalXRayImageStorageForPresentation,
    UltrasoundImageStorage,
)

# Canonical order (writer spec R2): uncompressed family first, then
# compressed in canonical priority. Covers every stored TS in our corpus.
TS_ORDER = [
    ExplicitVRLittleEndian,   # 1.2.840.10008.1.2.1
    ImplicitVRLittleEndian,   # 1.2.840.10008.1.2
    "1.2.840.10008.1.2.4.80",  # JPEG-LS Lossless (T87)
    "1.2.840.10008.1.2.4.70",  # JPEG Lossless SV1 (P14.1)
]

CLASSES = {
    "CT": CTImageStorage,
    "DX": DigitalXRayImageStorageForPresentation,
    "US": UltrasoundImageStorage,
}


def ts_name(uid):
    """Short human-readable name for a transfer syntax UID.

    pynetdicom may hand back the TS as a single UID or a list of one.
    """
    if isinstance(uid, (list, tuple)):
        uid = uid[0] if uid else ""
    names = {
        ExplicitVRLittleEndian: "EVRLE",
        ImplicitVRLittleEndian: "IVRLE",
        "1.2.840.10008.1.2.4.80": "JPEG-LS-4.80",
        "1.2.840.10008.1.2.4.70": "JPEG-LS-4.70",
    }
    return names.get(uid, str(uid))


def main():
    if len(sys.argv) < 4:
        print(__doc__)
        sys.exit(2)
    host = sys.argv[1]
    port = int(sys.argv[2])
    ae_title = sys.argv[3]
    runs = int(sys.argv[4]) if len(sys.argv) > 4 else 5
    cls_key = (sys.argv[5] if len(sys.argv) > 5 else "CT").upper()
    sop_class = CLASSES.get(cls_key, CTImageStorage)

    print(f"probe: {host}:{port} AE={ae_title} class={cls_key} "
          f"contexts={len(TS_ORDER)} runs={runs}")
    accepted_all = []
    for run in range(1, runs + 1):
        ae = AE()
        ae.ae_title = "PROBE_MCTX"
        for ts in TS_ORDER:
            ae.add_requested_context(sop_class, ts)
        t0 = time.time()
        try:
            # pynetdicom timeouts: AE.acse_timeout attribute (verified 3.0.4:
            # associate() has NO acse_timeout kwarg). Older builds may take
            # the kwarg instead - probe both shapes.
            kwargs = {}
            if "acse_timeout" in inspect.signature(ae.associate).parameters:
                kwargs["acse_timeout"] = 10
            elif hasattr(ae, "acse_timeout"):
                ae.acse_timeout = 10
            assoc = ae.associate(host, port, ae_title=ae_title, **kwargs)
            if assoc.is_established:
                acc = [ts_name(cx.transfer_syntax)
                       for cx in assoc.accepted_contexts]
                accepted_all.append(acc)
                print(f"run {run}: OK accepted={acc} ({time.time() - t0:.1f}s)")
                assoc.release()
            else:
                # Rejection has a result; a silent abort/timeout is the
                # MicroDicom ACSE wedge (no answer at all).
                result = getattr(assoc, "acceptor_result", None)
                print(f"run {run}: NOT ESTABLISHED "
                      f"rejected={assoc.is_rejected} aborted={assoc.is_aborted} "
                      f"result={result} ({time.time() - t0:.1f}s)")
        except Exception as exc:  # ACSE timeout wedge lives here
            print(f"run {run}: ERROR {type(exc).__name__}: {exc} "
                  f"({time.time() - t0:.1f}s)")
        if run < runs:
            time.sleep(2)

    print("--- summary ---")
    ok = len(accepted_all)
    print(f"established {ok}/{runs}, wedge/no-answer {runs - ok}")
    if accepted_all:
        for i, acc in enumerate(accepted_all, 1):
            print(f"ok-run {i}: {len(acc)}/{len(TS_ORDER)} contexts accepted")
        flat = [ts for acc in accepted_all for ts in acc]
        uniq = sorted(set(flat))
        print("accepted TS union:", uniq)


if __name__ == "__main__":
    main()
