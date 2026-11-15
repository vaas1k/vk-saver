#!/usr/bin/env bash
# Usage: scan-summary.sh summary|gate <reports dir>
# summary: markdown for $GITHUB_STEP_SUMMARY; gate: exit 1 on critical/high vulnerabilities
# with a fix or on any leaked secret. Prints no secret values (gitleaks runs with --redact).
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

if [ "$mode" = gate ]; then
  fixable=$(trivy_fixable)
  leaked=$(secrets)
  echo "critical/high vulnerabilities with a fix: $fixable; secrets: $leaked"
  [ "$fixable" -eq 0 ] && [ "$leaked" -eq 0 ]
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
