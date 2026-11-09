Clarus PACS v0.3.0-beta - Quick Start
=====================================

COMPONENTS IN THIS PACKAGE
--------------------------
clarus(.exe)        - DICOMweb server (single static binary, zero runtime deps)
clinfer(.exe)       - AI sidecar (UPS-RS worklist + site-supplied models)
clbridge(.exe/.py)  - DIMSE->DICOMweb bridge (Python sidecar; .exe on Windows)
clarus.conf         - server configuration (nginx-style)
clbridge.conf       - bridge configuration (nginx-style)
sop_classes.yaml    - SOP class registry (72 classes)
printers.yaml       - DICOM printer configuration (C-PRINT)
peers.conf          - DIMSE peer registry for C-MOVE (template)

QUICK START (Windows)
---------------------
1. Start the server:
   clarus.exe --config clarus.conf

2. Start the bridge (optional, for DIMSE modalities):
   clbridge.exe --config clbridge.conf

3. Open the mini-viewer: http://127.0.0.1:8024/


QUICK START (Linux)
-------------------
1. Start the server:
   ./clarus --config clarus.conf

2. Start the bridge (optional):
   python3 clbridge.py --config clbridge.conf

3. Open the mini-viewer: http://127.0.0.1:8024/


NOTES
-----
The server is a static binary: no SQL database, no JVM, no async runtime.
Storage is the filesystem (data/ next to the config). This package is built
with the transcode feature (full DICOMweb conformance on compressed transfer
syntaxes and viewer thumbnails).


BASE vs FULL
------------
base (3.8 MB) — SQLite storage, uncompressed DICOM only.
full (6+ MB)  — + JPEG2000 codec, + CJK charset support (Chinese/Japanese/Korean).


MORE INFO
---------
Full documentation: https://github.com/clarus-pacs/clarus
License: LGPLv3
