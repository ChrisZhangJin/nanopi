#!/usr/bin/env python3
"""Regenerate the `BUILTIN_MODELS` table in `src/models.rs` from models.dev.

    curl -o /tmp/models.json https://models.dev/api.json
    python3 scripts/gen-models.py /tmp/models.json

Committed so the next refresh is one command rather than archaeology --
the previous table was generated once, by hand, and the method was lost.

Two rules this script exists to enforce:

1. UNION, never replace. Upstream filtered to tool-calling chat models
   does not contain every id nanopi already ships (mimo-v2-flash,
   qwq-32b, gpt-5.2-chat-latest and ~20 others are absent or lack a
   `tool_call` flag). A straight replacement silently drops live models
   and breaks the tests that pin specific ids. Existing entries are
   always kept; upstream only adds, and corrects windows.

2. Longest-id-first ordering. `lookup` is a linear scan using
   `starts_with`, so first-match-wins equals longest-prefix-wins ONLY
   while the slice stays sorted by descending id length. With `gpt-5`
   ahead of `gpt-5.4`, every `gpt-5.4` session would silently report
   `gpt-5`'s window. `longer_ids_win_over_shorter_prefixes` asserts this
   over the whole slice.
"""

import json
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODELS_RS = ROOT / "src" / "models.rs"

# nanopi vendor id -> models.dev provider ids. nanopi's `qwen` is served
# by upstream's `alibaba*` providers; `fallback` deliberately has no
# catalogue (asserted by `every_shipped_vendor_has_models`).
VENDORS = {
    "anthropic": ["anthropic"],
    "openai": ["openai"],
    "deepseek": ["deepseek"],
    "minimax": ["minimax", "minimax-cn"],
    "qwen": ["alibaba", "alibaba-cn"],
    "xiaomi": ["xiaomi"],
    "zai": ["zai"],
}

# Some providers re-list OTHER vendors' models (Alibaba's DashScope
# serves deepseek-*, kimi-*, and aggregator-namespaced ids such as
# `siliconflow/deepseek-v3.2`). Importing those would attribute them to
# the wrong vendor, and `model_vendor` must agree with
# `vendor::pick_vendor`'s own prefix sniff or `validate_model` starts
# reporting bogus cross-vendor errors. Worse, a longer foreign id sorts
# AHEAD of a shorter native one -- `deepseek-v4-flash-0731` would shadow
# `deepseek-v4-flash` and hand a DeepSeek user vendor `qwen`.
#
# So each vendor may only contribute ids from its own families, matching
# the prefixes `pick_vendor` already recognises.
OWN_PREFIXES = {
    "anthropic": ("claude-",),
    "openai": ("gpt-", "o1", "o3", "o4", "chatgpt-", "codex-"),
    "deepseek": ("deepseek",),
    "minimax": ("minimax",),
    "qwen": ("qwen", "qwq-"),
    "xiaomi": ("mimo",),
    "zai": ("glm-",),
}


def is_own_model(vendor, model_id):
    if "/" in model_id:
        # Aggregator-namespaced (`siliconflow/deepseek-r1-0528`): not an
        # id anyone passes as --model to the vendor's own endpoint.
        return False
    low = model_id.lower()
    return any(low.startswith(p) for p in OWN_PREFIXES[vendor])

BEGIN = "// BEGIN GENERATED MODELS"
END = "// END GENERATED MODELS"


def is_chat_model(m):
    """Usable to serve a coding-agent turn.

    Excludes image/audio/embedding endpoints (gpt-image-2,
    qwen3-asr-flash, ...): they cannot serve a turn, so listing them
    only pollutes the /model picker.
    """
    out = (m.get("modalities") or {}).get("output") or []
    return "text" in out and bool(m.get("tool_call"))


def parse_existing(src):
    """Every entry currently in the table, as {id: (vendor, window)}."""
    pat = re.compile(
        r'ModelInfo\s*\{\s*id:\s*"([^"]+)",\s*vendor:\s*"([^"]+)",'
        r'\s*context_window:\s*(\d+)',
    )
    return {m[0]: (m[1], int(m[2])) for m in pat.findall(src)}


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} <path to models.dev api.json>")
    upstream = json.loads(Path(sys.argv[1]).read_text())
    src = MODELS_RS.read_text()

    existing = parse_existing(src)
    if not existing:
        sys.exit("refusing to run: parsed 0 existing entries (regex drifted?)")

    # Upstream, keyed case-insensitively so a casing difference between
    # the catalogue and models.dev is a match, not a duplicate.
    up = {}
    for vendor, providers in VENDORS.items():
        for pid in providers:
            for mid, m in (upstream.get(pid, {}).get("models") or {}).items():
                ctx = (m.get("limit") or {}).get("context")
                if not ctx or not is_chat_model(m):
                    continue
                if not is_own_model(vendor, mid):
                    continue
                up.setdefault(mid.lower(), (mid, vendor, int(ctx)))

    merged = {}
    for mid, (vendor, window) in existing.items():
        merged[mid] = (vendor, window)

    added, corrected = [], []
    for low, (mid, vendor, ctx) in up.items():
        hit = next((k for k in merged if k.lower() == low), None)
        if hit is None:
            merged[mid] = (vendor, ctx)
            added.append((mid, vendor, ctx))
        elif merged[hit][1] != ctx:
            # Upstream is authoritative for a model it knows. The one
            # that matters: claude-sonnet-4-5 was recorded as 1M but is
            # 200k, so auto-compaction fired far too late.
            corrected.append((hit, merged[hit][1], ctx))
            merged[hit] = (merged[hit][0], ctx)

    # Longest id first; ties broken by id so the output is stable
    # across runs and the diff stays reviewable.
    rows = sorted(merged.items(), key=lambda kv: (-len(kv[0]), kv[0]))

    body = [BEGIN]
    for mid, (vendor, window) in rows:
        body.append("    ModelInfo {")
        body.append(f'        id: "{mid}",')
        body.append(f'        vendor: "{vendor}",')
        body.append(f"        context_window: {window},")
        body.append("    },")
    body.append(f"    {END}")

    start, stop = src.index(BEGIN), src.index(END) + len(END)
    MODELS_RS.write_text(src[:start] + "\n".join(body).lstrip() + src[stop:])

    print(f"total {len(rows)} (was {len(existing)})")
    print(f"added {len(added)}:")
    for mid, vendor, ctx in sorted(added, key=lambda r: r[0]):
        print(f"  + {mid:42} {vendor:10} {ctx:,}")
    print(f"corrected {len(corrected)}:")
    for mid, old, new in sorted(corrected):
        print(f"  ~ {mid:42} {old:,} -> {new:,}")


if __name__ == "__main__":
    main()
