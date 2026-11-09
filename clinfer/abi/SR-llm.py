#!/usr/bin/env python3
"""Structured report: call an OpenAI-compatible endpoint and return `sr.text`.

Shows the third result kind, and the one clinfer is NOT part of: the script
talks to its endpoint itself. The URL, the key and the HTTP client are the
script's own configuration, because a processor is an ordinary program and
its network policy belongs where the script lives.

Run by hand from the work directory, which is also how clinfer runs it:

    python SR-llm.py /path/to/workdir

The endpoint must be reachable from the machine running clinfer. Whether it
is local or remote, and whether it speaks TLS, is this script's business.

THE PROMPT may come from a file: set SR_LLM_PROMPT to its path and the file
is read instead of the template below. A file is how a site keeps wording
it owns - a disclaimer, a house style, a language - next to the deployed
script without editing it, and it can be reviewed and versioned on its own.
A path that is set but unreadable is a FAILURE, not a fallback: silently
using the template when the operator pointed at their own wording would run
the wrong prompt and say nothing.

See ../abi/README.md for the contract.
"""

import base64
import json
import os
import pathlib
import sys
import urllib.request

import params

# Site-managed, exactly like clinfer's own configuration is site-managed.
# Nothing here is read from clinfer.conf.
ENDPOINT = os.environ.get("SR_LLM_URL", "http://127.0.0.1:8080/v1/chat/completions")
API_KEY = os.environ.get("SR_LLM_KEY", "")
MODEL = os.environ.get("SR_LLM_MODEL", "local-vlm")
# The prompt file, when the site keeps its wording outside the script.
PROMPT_ENV = "SR_LLM_PROMPT"

# The template used when no prompt file is configured. It is deliberately
# generic: a site that wants specific wording points SR_LLM_PROMPT at a file
# it owns rather than editing this one.
DEFAULT_PROMPT = (
    "You are a radiology assistant. Describe the image in one or two "
    "sentences. State only what you see. Do not speculate about a "
    "diagnosis."
)


def prompt():
    """The prompt to send: the site's file, or the embedded template.

    A path that is set but cannot be read is fatal. Falling back to the
    template there would run wording the operator did not choose and
    report success, which is the one outcome a configured prompt file
    exists to prevent.
    """
    path = os.environ.get(PROMPT_ENV)
    if not path:
        return DEFAULT_PROMPT
    try:
        text = pathlib.Path(path).read_text(encoding="utf-8").strip()
    except OSError as error:
        raise SystemExit(f"{PROMPT_ENV}={path} could not be read: {error}")
    if not text:
        raise SystemExit(f"{PROMPT_ENV}={path} is empty")
    return text

# The frames sent to the endpoint. A study of hundreds of slices is not a
# chat message, so a site picks the frames it wants described.
MAX_IMAGES = 4


def process_batch(workdir):
    workdir = pathlib.Path(workdir)
    manifest = json.loads((workdir / "manifest.json").read_text())

    # The requester tunes how many frames are described; the constant above
    # is the default either way. A slice length of zero would send no image
    # at all, so an unusable request keeps the default.
    max_images = params.count(manifest, "MAX_IMAGES", MAX_IMAGES)
    if max_images < 1:
        max_images = MAX_IMAGES

    content = [{"type": "text", "text": prompt()}]
    for frame in manifest["frames"][:max_images]:
        raw = (workdir / "frames" / frame["file"]).read_bytes()
        # PNG/JPEG is the endpoint's vocabulary, so the raw frame has to be
        # encoded into something it can read; the raw bytes are not sent.
        encoded = base64.b64encode(_to_png(raw, frame)).decode("ascii")
        content.append(
            {
                "type": "image_url",
                "image_url": {"url": f"data:image/png;base64,{encoded}"},
            }
        )

    body = json.dumps(
        {"model": MODEL, "messages": [{"role": "user", "content": content}]}
    ).encode("utf-8")

    request = urllib.request.Request(ENDPOINT, data=body, method="POST")
    request.add_header("Content-Type", "application/json")
    if API_KEY:
        request.add_header("Authorization", f"Bearer {API_KEY}")

    with urllib.request.urlopen(request, timeout=120) as response:
        answer = json.loads(response.read())

    text = answer["choices"][0]["message"]["content"].strip()

    return {
        "protocol": {"name": "clinfer-abi", "version": 1},
        "producer": {
            "script": "SR-llm.py",
            "script_version": "1.0.0",
            "model": MODEL,
        },
        "sr": {"text": text},
    }


def _to_png(raw, frame):
    """The raw frame as PNG bytes, in the frame's own bit depth."""
    import cv2
    import numpy as np

    dtype = np.uint8 if frame["bits"] == 8 else np.uint16
    image = np.frombuffer(raw, dtype=dtype).reshape(frame["height"], frame["width"])
    # A 16 bit image is scaled to 8 bit for the endpoint only: the frames in
    # the work directory are never changed, so this cannot alter the study.
    if dtype == np.uint16:
        image = cv2.convertScaleAbs(image, alpha=255.0 / image.max())
    ok, buffer = cv2.imencode(".png", image)
    if not ok:
        raise RuntimeError(f"could not encode {frame['file']} as PNG")
    return buffer.tobytes()


def main(workdir):
    result = process_batch(workdir)
    workdir = pathlib.Path(workdir)
    tmp = workdir / "result.json.tmp"
    tmp.write_text(json.dumps(result, indent=2))
    tmp.rename(workdir / "result.json")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        print(f"usage: {sys.argv[0]} <workdir>", file=sys.stderr)
        raise SystemExit(2)
    main(sys.argv[1])
