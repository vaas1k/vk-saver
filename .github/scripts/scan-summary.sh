#!/usr/bin/env bash
# Usage: scan-summary.sh summary|gate <reports dir>
# summary: markdown for $GITHUB_STEP_SUMMARY; gate: exit 1 on critical/high vulnerabilities
# with a fix, on any leaked secret, on any cargo-audit vulnerability, or on a cargo-deny
# bans/sources violation. Prints no secret values (gitleaks runs with --redact).
set -euo pipefail
mode=$1
dir=$2
rows=50

counts() { jq -r 'group_by(.) | map("\(.[0]): \(length)") | join(", ") | if . == "" then "none" else . end'; }

trivy_fixable() {
  jq '[.Results[]?.Vulnerabilities[]? | select((.Severity == "CRITICAL" or .Severity == "HIGH") and (.FixedVersion // "") != "")] | length' "$dir/trivy.json"
}

secrets() {
  local n=0
  [ -f "$dir/gitleaks.json" ] && n=$(jq 'length' "$dir/gitleaks.json")
  [ -f "$dir/trivy.json" ] && n=$((n + $(jq '[.Results[]?.Secrets[]?] | length' "$dir/trivy.json")))
  echo "$n"
}

audit_vulns() {
  [ -f "$dir/cargo-audit.json" ] && jq '.vulnerabilities.count' "$dir/cargo-audit.json" || echo 0
}

deny_failed() {
  [ -f "$dir/cargo-deny.exit" ] && cat "$dir/cargo-deny.exit" || echo 0
}

if [ "$mode" = gate ]; then
  fixable=$(trivy_fixable)
  leaked=$(secrets)
  audit=$(audit_vulns)
  deny=$(deny_failed)
  echo "critical/high vulns with a fix: $fixable; secrets: $leaked; cargo-audit vulns: $audit; cargo-deny failed: $deny"
  [ "$fixable" -eq 0 ] && [ "$leaked" -eq 0 ] && [ "$audit" -eq 0 ] && [ "$deny" -eq 0 ]
  exit
fi

if [ -f "$dir/trivy.json" ]; then
  echo "### Trivy (package-lock.json, src-tauri/Cargo.lock, secrets, misconfig)"
  echo "Vulnerabilities by severity: $(jq '[.Results[]?.Vulnerabilities[]?.Severity]' "$dir/trivy.json" | counts)"
  echo "Critical/high with a fix: $(trivy_fixable)"
  echo "Secrets: $(jq '[.Results[]?.Secrets[]?] | length' "$dir/trivy.json"), misconfigurations: $(jq '[.Results[]?.Misconfigurations[]?] | length' "$dir/trivy.json")"
  echo
  echo "| Severity | Id | Package | Installed | Fixed | File |"
  echo "|---|---|---|---|---|---|"
  jq -r --argjson n "$rows" '[.Results[]? | .Target as $t | .Vulnerabilities[]? | [.Severity, .VulnerabilityID, .PkgName, .InstalledVersion, (.FixedVersion // ""), $t]]
    | .[:$n][] | "| " + join(" | ") + " |"' "$dir/trivy.json"
  jq -r --argjson n "$rows" '[.Results[]? | .Target as $t | .Misconfigurations[]? | [.Severity, .ID, .Title, "", "", $t]]
    | .[:$n][] | "| " + join(" | ") + " |"' "$dir/trivy.json"
  echo
else
  echo "### Trivy: no report"
fi

if [ -f "$dir/cargo-audit.json" ]; then
  echo "### cargo audit (src-tauri/Cargo.lock, all targets)"
  echo "Linux-only crates (gtk, webkit2gtk and their deps) are in the lock file but not in the Windows build."
  echo "Vulnerabilities: $(jq '.vulnerabilities.count' "$dir/cargo-audit.json"); warnings: $(jq -r '[.warnings | to_entries[] | .value[] | .kind]' "$dir/cargo-audit.json" | counts)"
  echo
  echo "| Kind | Id | Crate | Version | Title |"
  echo "|---|---|---|---|---|"
  jq -r '[(.vulnerabilities.list[] | ["vulnerability", .advisory.id, .package.name, .package.version, .advisory.title]),
          (.warnings | to_entries[] | .value[] | [.kind, (.advisory.id // ""), .package.name, .package.version, (.advisory.title // "")])]
    | .[] | "| " + join(" | ") + " |"' "$dir/cargo-audit.json"
  echo
else
  echo "### cargo audit: no report"
fi

if [ -f "$dir/clippy.sarif" ]; then
  echo "### clippy (vk-engine)"
  echo "Findings by level: $(jq '[.runs[]?.results[]?.level]' "$dir/clippy.sarif" | counts)"
  echo
  echo "| Level | Rule | Message | File | Line |"
  echo "|---|---|---|---|---|"
  jq -r --argjson n "$rows" '[.runs[]? | .results[]? | [.level, .ruleId, .message.text, (.locations[0].physicalLocation.artifactLocation.uri // ""), (.locations[0].physicalLocation.region.startLine // 1)]]
    | .[:$n][] | "| " + join(" | ") + " |"' "$dir/clippy.sarif"
  echo
else
  echo "### clippy: no report"
fi

if [ -f "$dir/cargo-deny.txt" ]; then
  echo "### cargo deny (bans + sources)"
  echo "Exit code: $(cat "$dir/cargo-deny.exit" 2>/dev/null || echo 0)"
  echo
  tail -n 20 "$dir/cargo-deny.txt" | sed 's/^/    /'
  echo
else
  echo "### cargo deny: no report"
fi

if [ -f "$dir/gitleaks.json" ]; then
  echo "### gitleaks (full history, redacted)"
  echo "Leaks: $(jq 'length' "$dir/gitleaks.json")"
  if [ "$(jq 'length' "$dir/gitleaks.json")" -gt 0 ]; then
    echo
    echo "| Rule | File | Line | Commit |"
    echo "|---|---|---|---|"
    jq -r '.[] | "| \(.RuleID) | \(.File) | \(.StartLine) | \(.Commit[:12]) |"' "$dir/gitleaks.json"
  fi
  echo
else
  echo "### gitleaks: no report"
fi

if [ -f "$dir/zizmor.json" ]; then
  echo "### zizmor (.github/workflows)"
  echo "Findings by severity: $(jq '[.[].determinations.severity]' "$dir/zizmor.json" | counts)"
  if [ "$(jq 'length' "$dir/zizmor.json")" -gt 0 ]; then
    echo
    echo "| Severity | Confidence | Audit | File |"
    echo "|---|---|---|---|"
    jq -r '.[] | "| \(.determinations.severity) | \(.determinations.confidence) | \(.ident) | \(.locations[0].symbolic.key.Local.given_path // .locations[0].symbolic.key.Local.verbatim_path // "") |"' "$dir/zizmor.json"
  fi
  echo
else
  echo "### zizmor: no report"
fi

if [ -f "$dir/semgrep.sarif" ]; then
  echo "### semgrep (p/rust + p/javascript)"
  echo "Findings: $(jq '[.runs[]?.results[]?] | length' "$dir/semgrep.sarif")"
  echo
  echo "| Level | Rule | File | Line |"
  echo "|---|---|---|---|"
  jq -r --argjson n "$rows" '[.runs[]? | .results[]? | [(.level // "warning"), .ruleId, (.locations[0].physicalLocation.artifactLocation.uri // ""), (.locations[0].physicalLocation.region.startLine // 1)]]
    | .[:$n][] | "| " + join(" | ") + " |"' "$dir/semgrep.sarif"
  echo
else
  echo "### semgrep: no report"
fi
