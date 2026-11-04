#!/usr/bin/env bash
# Fail when the router and api/openapi.yaml disagree.
#
# The panel is a separate repository now. Its whole safety net is that this file
# is true. A route that exists in the router but not in the spec is an endpoint
# no client can be written against; one that exists in the spec but not in the
# router is a promise the hub does not keep. Both are build failures.
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import re, sys, yaml

ROUTER = "crates/hub/src/main.rs"
SPEC = "api/openapi.yaml"

# Not part of the panel contract: the agent rendezvous endpoint speaks the
# satellite protocol (crates/proto AgentMsg/HubMsg), not JSON over HTTP.
NOT_IN_SPEC = {"/agent"}

src = open(ROUTER, encoding="utf-8").read()
# .route("/x", ...) and the multi-line .route(\n "/x", ...) form.
router = set(re.findall(r'\.route\(\s*"([^"]+)"', src)) - NOT_IN_SPEC

spec = yaml.safe_load(open(SPEC, encoding="utf-8"))
paths = set(spec.get("paths", {}))

missing = sorted(router - paths)
extra = sorted(paths - router)

if missing:
    print("routes served but missing from api/openapi.yaml:")
    for p in missing:
        print("   ", p)
if extra:
    print("api/openapi.yaml documents paths the router does not serve:")
    for p in extra:
        print("   ", p)

# The version gate the panel depends on must be documented as required.
meta = spec["components"]["schemas"]["Meta"]
if "api_version" not in meta.get("required", []):
    print("Meta.api_version is not marked required; panels cannot rely on it")
    extra.append("Meta.api_version")

if missing or extra:
    sys.exit(1)

print(f"api contract ok: {len(paths)} paths, api_version documented")
PY
