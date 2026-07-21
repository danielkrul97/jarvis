#!/usr/bin/env python3
"""Kill-gate 1: the doctrine registry item is a valid shadcn registry item AND
carries a well-formed doctrine descriptor. Fails loudly on any violation."""
import json
import sys
from pathlib import Path

import jsonschema

HERE = Path(__file__).parent
schema = json.loads((HERE / "shadcn-registry-item.schema.json").read_text())
item = json.loads((HERE.parent / "registry" / "favorites.json").read_text())

errors = []

# 1a. shadcn schema conformance (name/type required, type enum, files rules).
try:
    jsonschema.validate(item, schema)
except jsonschema.ValidationError as e:
    errors.append(f"shadcn schema: {e.message} at {list(e.absolute_path)}")

# 1b. doctrine descriptor invariants (the extension that makes it a doctrine).
d = item.get("meta", {}).get("doctrine")
if d is None:
    errors.append("meta.doctrine missing")
else:
    p = d.get("priority")
    if not (isinstance(p, (int, float)) and 0 <= p <= 1):
        errors.append(f"priority must be in [0,1], got {p!r}")
    if d.get("dataLayer", {}).get("valueRange") != [0, 1]:
        errors.append("dataLayer.valueRange must be [0,1]")
    sinks = {s.get("sink") for s in d.get("emittedSignals", [])}
    if not {"meta-capi", "ga4", "intel"} <= sinks:
        errors.append(f"emittedSignals must cover meta-capi/ga4/intel, got {sinks}")
    for s in d.get("emittedSignals", []):
        if "gatedOn" not in s:
            errors.append(f"emittedSignal {s.get('sink')} missing 'gatedOn' (use null if ungated)")
    # every marketing sink must be consent-gated; intel must not be.
    for s in d.get("emittedSignals", []):
        if s.get("sink") in ("meta-capi", "ga4") and s.get("gatedOn") != "marketing_consent":
            errors.append(f"marketing sink {s.get('sink')} must be gatedOn marketing_consent")
        if s.get("sink") == "intel" and s.get("gatedOn") is not None:
            errors.append("intel sink must be ungated (gatedOn=null)")
    if not any(r.get("key") == "listing.id" and r.get("required") for r in d.get("requiresData", [])):
        errors.append("requiresData must mark listing.id as required")

# files reference real, existing paths relative to the doctrine root.
for f in item.get("files", []):
    if not (HERE.parent / f["path"]).exists():
        errors.append(f"file path does not exist: {f['path']}")

if errors:
    print("KILL-GATE 1 FAILED:")
    for e in errors:
        print("  -", e)
    sys.exit(1)
print("KILL-GATE 1 PASS: registry item valid + doctrine descriptor well-formed + files present")
