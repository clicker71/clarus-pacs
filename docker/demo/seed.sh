#!/bin/sh
# Seed the demo studies into a fresh Clarus. Idempotent: if the chest study
# is already present, skip. Runs once per fresh volume (compose `restart: "no"`).
set -eu

URL="http://clarus:8024/dicomweb"
# Health probe lives at the root, not under /dicomweb.
HEALTH="http://clarus:8024/health/status"
# Chest study StudyInstanceUID of the demo patient XRV-DEMO.
SEED_MARKER="1.2.826.0.1.3680043.8.498.31790203639536201545828494624411576907"

echo "seed: waiting for clarus"
until curl -fsS "$HEALTH" >/dev/null 2>&1; do
  sleep 1
done

if curl -fsS "$URL/studies?PatientID=XRV-DEMO" 2>/dev/null | grep -q "$SEED_MARKER"; then
  echo "seed: demo studies already present, skipping"
  exit 0
fi

for f in /seed/*.dcm; do
  [ -e "$f" ] || continue
  echo "seed: STOW $(basename "$f")"
  curl -fsS -X POST -H "Content-Type: application/dicom" \
    --data-binary "@$f" "$URL/studies" >/dev/null
done

echo "seed: done"
