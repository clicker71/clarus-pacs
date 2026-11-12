# Clarus C-PRINT Diagnostic Probe (Windows)
# Copyright (C) 2026 Daniil Solgalov. License: LGPLv3.
#
# This script collects DIAGNOSTIC DATA ONLY -- no code contributions.
# All driver code is written by the Clarus team to maintain clean
# copyright for software registration under our jurisdiction.
#
# Usage: .\cprint-probe.ps1 PRINTER_IP [PRINTER_PORT]
# Outputs everything needed to write a print driver.
#
# Send this output as a GitHub issue:
# https://github.com/clicker71/clarus-pacs/issues/new
# Title: [C-PRINT] <Vendor> <Model>

param(
    [Parameter(Mandatory=$true, Position=0)]
    [string]$PrinterIp,
    [Parameter(Position=1)]
    [int]$PrinterPort = 104
)

$ErrorActionPreference = "Stop"
$Now = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")

Write-Host "============================================================"
Write-Host "  Clarus C-PRINT Diagnostic Probe (Windows)"
Write-Host "  Target: ${PrinterIp}:${PrinterPort}"
Write-Host "  Date: $Now"
Write-Host "============================================================"
Write-Host ""

# Check DCMTK
$echoscu = Get-Command echoscu.exe -ErrorAction SilentlyContinue
if (-not $echoscu) {
    Write-Host "ERROR: DCMTK not found."
    Write-Host "Download DCMTK from: https://dicom.offis.de/dcmtk.php.en"
    Write-Host "Add the bin/ folder to your PATH, then re-run this script."
    exit 1
}

# Step 1: C-ECHO
Write-Host "=== Step 1: C-ECHO (connectivity check) ==="
Write-Host ""
$echoResult = & echoscu.exe -d $PrinterIp $PrinterPort 2>&1
$echoExit = $LASTEXITCODE
Write-Host $echoResult
if ($echoExit -ne 0) {
    Write-Host ""
    Write-Host "C-ECHO FAILED. Check:"
    Write-Host "   - Is the printer powered on?"
    Write-Host "   - Is the IP correct? (try: ping $PrinterIp)"
    Write-Host "   - Is port $PrinterPort open? (try: Test-NetConnection $PrinterIp -Port $PrinterPort)"
    Write-Host "   - Does the printer accept DICOM connections?"
    exit 1
}
Write-Host ""

# Step 2: Printer capabilities dump
Write-Host "=== Step 2: Printer Capabilities (dcmprintscu --dump) ==="
Write-Host ""
$dumpResult = & dcmprintscu.exe --dump $PrinterIp $PrinterPort 2>&1
$dumpExit = $LASTEXITCODE
Write-Host $dumpResult
if ($dumpExit -ne 0) {
    Write-Host ""
    Write-Host "dcmprintscu failed. The printer may not support N-GET Printer SOP Class."
    Write-Host "This is still useful -- report the error in the GitHub issue."
}
Write-Host ""

# Step 3: SOP Class negotiation
Write-Host "=== Step 3: SOP Class Negotiation ==="
Write-Host ""
$sopInfo = $echoResult | Select-String -Pattern "sop class|presentation context|abstract syntax" -CaseSensitive:$false
if ($sopInfo) {
    Write-Host $sopInfo
} else {
    Write-Host "(No SOP class info extracted -- see raw C-ECHO output above)"
}
Write-Host ""

Write-Host "============================================================"
Write-Host "  Probe complete!"
Write-Host ""
Write-Host "  To contribute, copy ALL output above and open an issue:"
Write-Host "     https://github.com/clicker71/clarus-pacs/issues/new"
Write-Host ""
Write-Host "  Title: [C-PRINT] <Vendor> <Model>"
Write-Host "  Paste this output in the body."
Write-Host ""
Write-Host "  Thank you for helping build the open-source"
Write-Host "  DICOM printer database."
Write-Host "============================================================"
