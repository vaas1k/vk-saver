#!/usr/bin/env python3
"""Convert `cargo clippy --message-format=json` output to SARIF 2.1.0 for GitHub Code Scanning."""
import json
import sys


def clippy_results(path):
    results = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            message = entry.get("message")
            if not isinstance(message, dict):
                continue
            spans = message.get("spans") or []
            location = None
            for span in spans:
                if span.get("is_primary"):
                    location = {
                        "physicalLocation": {
                            "artifactLocation": {"uri": span.get("file_name", "unknown")},
                            "region": {"startLine": span.get("line_start", 1)},
                        }
                    }
                    break
            if location is None and spans:
                span = spans[0]
                location = {
                    "physicalLocation": {
                        "artifactLocation": {"uri": span.get("file_name", "unknown")},
                        "region": {"startLine": span.get("line_start", 1)},
                    }
                }
            code = message.get("code")
            if isinstance(code, dict):
                code = code.get("code")
            results.append(
                {
                    "ruleId": (code or "clippy").lower(),
                    "message": {"text": message.get("message") or message.get("rendered") or "clippy issue"},
                    "level": "error" if message.get("level") == "error" else "warning",
                    "locations": [location] if location else [],
                }
            )
    return results


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: rust_sarif.py <clippy.json> <output.sarif>")
    src, dst = sys.argv[1:]
    results = clippy_results(src)
    rules = []
    for rid in dict.fromkeys(r["ruleId"] for r in results):
        level = next((r["level"] for r in results if r["ruleId"] == rid), "warning")
        rules.append(
            {
                "id": rid,
                "shortDescription": {"text": rid},
                "defaultConfiguration": {"level": level},
            }
        )
    report = {
        "version": "2.1.0",
        "$schema": "https://docs.oasis-open.org/sarif/sarif/v2.1.0/cs01/schemas/sarif-schema-2.1.0.json",
        "runs": [{"tool": {"driver": {"name": "RustClippy", "rules": rules}}, "results": results}],
    }
    with open(dst, "w", encoding="utf-8") as fh:
        json.dump(report, fh)


if __name__ == "__main__":
    main()
