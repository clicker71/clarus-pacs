# totalseg_demo_pack — run guide

A self-contained demo of an AI pipeline over DICOMweb: **clarus** (a tiny
DICOMweb PACS), **clinfer** (an inference sidecar), and **totalseg.py**
(a TotalSegmentator producer) wired together.

NOT FOR DIAGNOSTIC USE. This is transport + a research model; no clinical
claim is made or implied.

## What you get

    clarus(.exe)          one-file DICOMweb server (QIDO/WADO/STOW/UPS)
    clinfer(.exe)         inference sidecar (claims workitems, runs models)
    totalseg.py           the example producer (TotalSegmentator
                          craniofacial_structures -> DICOM RTSTRUCT)
    clarus_demo.conf      server config (port 8024, ./data)
    clinfer_demo.conf     sidecar config (totalseg rule, rtstruct result)
    ABI-README.md         the producer contract: write your own model
    ../images/totalseg_demo.png  what the result looks like in Weasis 4.7.2

## Requirements

- Windows x64 or Linux x64.
- Python 3.10+ with TotalSegmentator installed (e.g. `pip install
  TotalSegmentator`). Weights are NOT shipped: the first run downloads
  ~2 GB into ~/.totalsegmentator. Pre-download them once, outside a
  timed run (see totalseg.py header).
- Any DICOMweb client to push a study: Weasis export, curl multipart,
  OHIF, or the STOW snippet below.
- A viewer for the result: Weasis 4.7.2+ (the RT tool shows the contours).

## Python side (one-time setup)

`totalseg.py` runs under whatever `python3` is on PATH. Set the Python
side up once:

1. Create a venv and install TotalSegmentator. This pulls every
   dependency automatically (nibabel, nnunetv2, PyTorch, ...) - there
   is NO separate package list to install and nothing else to pin.
   TotalSegmentator does not use niftyreg.

       python -m venv ts-venv
       ts-venv\Scripts\pip install TotalSegmentator   # Windows
       ts-venv/bin/pip install TotalSegmentator       # Linux/macOS

2. Verify the install (nibabel comes with it):

       ts-venv\Scripts\python -c "import totalsegmentator, nibabel; print('ok')"

3. Pre-download the weights once, outside a timed run (~2 GB into
   ~/.totalsegmentator):

       ts-venv\Scripts\python -m totalsegmentator.bin.totalseg_download_weights

4. Start clinfer with the venv first on PATH (step 2 under Run).

## Run (3 terminals)

1. Server:

       ./clarus --config clarus_demo.conf          # clarus.exe on Windows

2. Sidecar (python with TotalSegmentator on PATH):

       ./clinfer --config clinfer_demo.conf        # clinfer.exe on Windows

3. Push a CT study (STOW-RS), e.g. with curl:

       curl -X POST http://127.0.0.1:8024/dicomweb/studies \
            -H "Content-Type: application/dicom" \
            --data-binary @your_slice.dcm

   The server auto-mints a workitem per study; clinfer claims CT studies
   (craniofacial task) and stores the RTSTRUCT back. Open the study in
   Weasis: the contours appear under the RT tool.

## Which organs? Is there an accuracy setting?

- The demo declares three organs: MANDIBLE, TEETH_LOWER, TEETH_UPPER.
  Open `totalseg.py`: right below the ORGANS table there is a
  "HOW TO ADD AN ORGAN" block with ready-to-paste entries (SKULL, HEAD,
  SINUS_MAXILLARY, ...). The task predicts seven classes in one pass, so
  adding a row costs nothing at inference.
- There is NO accuracy knob. The task runs on its trained 0.5 mm weights
  and refuses --fast, so every run is already at the tool's best quality.
  The only tunables are which organs land in the structure set (ORGANS)
  and where the model runs (TOTALSEG_DEVICE=cpu|cuda).

## Test it in Weasis (recommended: 4.7.3+)

1. Open Weasis and add an Internet node: Description "Clarus Server",
   Type "DICOMweb (all RESTful services)",
   URL `http://127.0.0.1:8024/dicomweb`, no authorization.
2. Import any CT image from the local device into Weasis.
3. Export that image to the Clarus node (Send > DICOMweb node).
   Use Weasis 4.7.3+: earlier 4.7.x builds abort long STOW exports
   with their default 15 s UrlReadTimeout.
4. Wait a few minutes (CPU-dependent; the inference runs once per
   study).
5. Import from the DICOMweb node: search, download the same patient.
6. Open the study: the CT series shows the contours under the RT tool
   (MANDIBLE / TEETH_LOWER / TEETH_UPPER) - see ../images/totalseg_demo.png.

## Your own model

clinfer is not TotalSegmentator-specific. Read `ABI-README.md`: a
producer is any program that reads the workdir and writes a result.json.
Add a `models:` entry and a rule in `clinfer_demo.conf`, done.

## Notes

- CT only: the craniofacial task reads Hounsfield units; the producer
  refuses non-CT studies and clinfer cancels their workitems.
- TotalSegmentator is Apache-2.0 (wasserth/TotalSegmentator). This demo
  ships no weights and no model code beyond the wrapper.
