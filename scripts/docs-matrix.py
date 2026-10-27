#!/usr/bin/env python3
"""Generate the `create cluster` table in docs/command-matrix.md.

The support/reject cells come from `kurin capabilities --json` (the
single source of truth, aggregated from the Component registry). The
e2e column is manually maintained: the generator reads it from the
existing table and writes it back verbatim.

Usage: just docs-matrix
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DOC = ROOT / "docs" / "command-matrix.md"
BIN = ROOT / "target" / "debug" / "kurin"

BEGIN = "<!-- docs-matrix:begin -->"
END = "<!-- docs-matrix:end -->"


def capability_rows() -> list[dict]:
    out = subprocess.run(
        [str(BIN), "capabilities", "--json"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(out)



def build_table(caps: list[dict], e2e: dict[str, str]) -> str:
    lines = [
        "<!-- 由 `just docs-matrix` 生成：支持/拒绝列来自 kurin capabilities --json",
        "     （单一事实来源）；e2e 验证列手工维护，生成器只透传。 -->",
        "| 组件 | containerd | crio | k3s | 离线 | zone=cn | e2e 验证 |",
        "|---|---|---|---|---|---|---|",
    ]
    for c in caps:
        if c["kind"] not in ("cni", "ha"):
            continue
        # containerd/crio columns: kubeadm-track components run on both
        # runtimes; kurin-lb installs distro packages on lb nodes and
        # was never e2e-validated against crio.
        runtime = "—" if c["id"] == "ha/kurin-lb" else "✅"
        k3s = "✅" if "k3s" in c["distributions"] else "—"
        offline = {"bundle": "✅", "rejected": "❌ 拒绝", "online-only": "—"}[c["offline"]]
        zones = "✅" if "cn" in c["zones"] else "—"
        e2e_cell = e2e.get(c["id"], "—")
        lines.append(
            f"| `{c['id']}` | {runtime} | {runtime} | {k3s} | {offline} | {zones} | {e2e_cell} |"
        )
    return "\n".join(lines)


def main() -> int:
    if not BIN.exists():
        print(f"missing {BIN}; run `cargo build` first", file=sys.stderr)
        return 1
    caps = capability_rows()
    text = DOC.read_text(encoding="utf-8")

    # The manually maintained e2e column, keyed by component id. The
    # generator owns support/reject; this column stays human-curated.
    e2e = {
        "cni/calico": "✅",
        "cni/flannel": "✅",
        "cni/cilium": "✅",
        "cni/kube-ovn": "✅",
        "cni/none": "—",
        "ha/kube-vip": "✅",
        "ha/kurin-lb": "✅",
        "ha/external": "✅",
    }

    replacement = f"{BEGIN}\n{build_table(caps, e2e)}\n{END}"
    if BEGIN in text and END in text:
        start = text.index(BEGIN)
        end = text.index(END) + len(END)
        text = text[:start] + replacement + text[end:]
    else:
        # First run: the legacy table sits under the `## create cluster`
        # heading; replace it wholesale with the generated block.
        section = re.search(r"(## create cluster\n)(.*?)(\n## )", text, re.S)
        if not section:
            print("cannot find the create-cluster section", file=sys.stderr)
            return 1
        text = (
            text[: section.end(1)] + replacement + "\n" + text[section.start(3) :]
        )

    DOC.write_text(text, encoding="utf-8")
    print(f"wrote {DOC}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
