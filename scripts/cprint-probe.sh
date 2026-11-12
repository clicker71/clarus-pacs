#!/bin/bash
# Clarus C-PRINT Diagnostic Probe
# Copyright (C) 2026 Daniil Solgalov. License: LGPLv3.
#
# This script collects DIAGNOSTIC DATA ONLY — no code contributions.
# All driver code is written by the Clarus team to maintain clean
# copyright for software registration under our jurisdiction.
#
# Usage: bash cprint-probe.sh PRINTER_IP [PRINTER_PORT]
# Outputs everything needed to write a print driver.
#
# Send this output as a GitHub issue:
# https://github.com/clicker71/clarus-pacs/issues/new
# Title: [C-PRINT] <Vendor> <Model>

set -euo pipefail

PRINTER_IP="${1:-}"
PRINTER_PORT="${2:-104}"

if [ -z "$PRINTER_IP" ]; then
    echo "Usage: bash cprint-probe.sh PRINTER_IP [PRINTER_PORT]"
    echo "Example: bash cprint-probe.sh 192.168.1.50 104"
    exit 1
fi

echo "============================================================"
echo "  Clarus C-PRINT Diagnostic Probe"
echo "  Target: ${PRINTER_IP}:${PRINTER_PORT}"
echo "  Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
echo "============================================================"
echo ""

# Check DCMTK
if ! command -v echoscu &> /dev/null; then
    echo "ERROR: DCMTK not found. Install: sudo apt install dcmtk"
    echo "Or: brew install dcmtk  (macOS)"
    exit 1
fi

# Step 1: C-ECHO
echo "=== Step 1: C-ECHO (connectivity check) ==="
echo ""
echoscu -d "${PRINTER_IP}" "${PRINTER_PORT}" 2>&1 || {
    echo ""
    echo "C-ECHO FAILED. Check:"
    echo "   - Is the printer powered on?"
    echo "   - Is the IP correct? (try: ping ${PRINTER_IP})"
    echo "   - Is port ${PRINTER_PORT} open? (try: nc -zv ${PRINTER_IP} ${PRINTER_PORT})"
    echo "   - Does the printer accept DICOM connections?"
    exit 1
}
echo ""

# Step 2: Printer capabilities dump
echo "=== Step 2: Printer Capabilities (dcmprintscu --dump) ==="
echo ""
dcmprintscu --dump "${PRINTER_IP}" "${PRINTER_PORT}" 2>&1 || {
    echo ""
    echo "dcmprintscu failed. The printer may not support N-GET Printer SOP Class."
    echo "This is still useful — report the error in the GitHub issue."
}
echo ""

# Step 3: SOP Class negotiation
echo "=== Step 3: SOP Class Negotiation ==="
echo ""
echoscu -d "${PRINTER_IP}" "${PRINTER_PORT}" 2>&1 | grep -i "sop class\|presentation context\|abstract syntax" || echo "(No SOP class info extracted — see raw C-ECHO output above)"
echo ""

echo "============================================================"
echo "  Probe complete!"
echo ""
echo "  To contribute, copy ALL output above and open an issue:"
echo "     https://github.com/clicker71/clarus-pacs/issues/new"
echo ""
echo "  Title: [C-PRINT] <Vendor> <Model>"
echo "  Paste this output in the body."
echo ""
echo "  Thank you for helping build the open-source"
echo "  DICOM printer database."
echo "============================================================"
