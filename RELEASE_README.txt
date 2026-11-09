Clarus PACS v0.3.0 — Quick Start
=================================

COMPONENTS IN THIS PACKAGE
--------------------------
clarus-server-{variant}-{os}.exe   — DICOMweb server (Rust)
clarus-bridge.exe / .py            — DIMSE→DICOMweb adapter (Python/EXE)
drivers/                            — C-PRINT printer drivers
sop_classes.yaml                    — SOP class registry (48 classes)
printers.yaml                       — DICOM printer configuration


QUICK START (Windows)
---------------------
1. Start the server:
   clarus-server-base-windows-amd64.exe --db-path clarus.db --storage-dir ./storage --no-auth

2. Start the bridge (choose one):
   a) Without Python: clarus-bridge.exe --host 127.0.0.1 --port 8024 --bridge-port 8104
   b) With Python:    python clarus-bridge.py --host 127.0.0.1 --port 8024 --bridge-port 8104

3. Open viewer: http://127.0.0.1:8024/


QUICK START (Linux)
-------------------
1. Start the server:
   ./clarus-server-base-linux-amd64 --db-path clarus.db --storage-dir ./storage --no-auth

2. Start the bridge:
   python3 clarus-bridge.py --host 127.0.0.1 --port 8024 --bridge-port 8104

3. Open viewer: http://127.0.0.1:8024/


PRINTER SETUP
-------------
Edit printers.yaml with your DICOM printer's host, port, and AE Title.
The generic_dicom driver supports most film printers (AGFA, Carestream, etc.).


BASE vs FULL
------------
base (3.8 MB) — SQLite storage, uncompressed DICOM only.
full (6+ MB)  — + JPEG2000 codec, + CJK charset support (Chinese/Japanese/Korean).


MORE INFO
---------
Full documentation: https://github.com/clarus-pacs/clarus
License: LGPLv3
