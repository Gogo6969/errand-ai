#!/usr/bin/env bash
# Pre-push secret and personal-data audit.
#
# This repository is public from the first commit, so the guard has to run
# before the first commit rather than before the first release. Enforcement,
# not vigilance: run from a pre-push hook and from CI.
#
# Exit 0 clean, 1 dirty.

set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

fail=0
note() { printf '  %s\n' "$1"; }
problem() { printf '\n  FAIL  %s\n' "$1"; fail=1; }

# Files git actually tracks or would track. Never scan target/ or node_modules.
tracked() {
  if git rev-parse --git-dir >/dev/null 2>&1; then
    { git ls-files; git ls-files --others --exclude-standard; } | sort -u
  else
    find . -type f \
      -not -path './.git/*' -not -path './target/*' \
      -not -path './node_modules/*' -not -path './frontend/build/*' \
      | sed 's|^\./||'
  fi
}

FILES=$(tracked | grep -v '^scripts/scrub-check.sh$' || true)
[ -z "$FILES" ] && { echo "no files to scan"; exit 0; }

echo "Scrub check over $(echo "$FILES" | wc -l | tr -d ' ') files"

# --- 1. Private network addresses -------------------------------------------
# Wolf's LAN model endpoints must never ship as defaults. They live in a
# gitignored dev overlay and are found by auto-detection like anyone else's.
# A line may opt out with `scrub:allow private-ip <reason>`. Every exemption is
# printed, so they stay reviewable rather than accumulating unnoticed.
if raw=$(echo "$FILES" | xargs grep -nE '\b(192\.168\.|10\.[0-9]+\.|172\.(1[6-9]|2[0-9]|3[01])\.)[0-9]' 2>/dev/null); then
  exempt=$(echo "$raw" | grep 'scrub:allow private-ip' || true)
  hits=$(echo "$raw" | grep -v 'scrub:allow private-ip' || true)
  if [ -n "$exempt" ]; then
    note "private-ip exemptions (review these):"
    echo "$exempt" | sed 's/^/        /'
  fi
  if [ -n "$hits" ]; then
    problem "private LAN addresses found. Endpoints are discovered, never shipped as defaults:"
    echo "$hits" | head -10 | sed 's/^/        /'
  fi
fi

# --- 2. Credential-shaped strings -------------------------------------------
# Real tokens, not the words. sk-ant-, GitHub tokens, Telegram bot tokens,
# Errand tokens, AWS keys, private key blocks.
patterns=(
  'sk-ant-[A-Za-z0-9_-]{20,}'
  'gh[pousr]_[A-Za-z0-9]{30,}'
  '\b[0-9]{8,10}:AA[A-Za-z0-9_-]{30,}'   # Telegram bot token
  'err_v1_[a-f0-9]{64}'                  # a real Errand API token
  'AKIA[0-9A-Z]{16}'
  'BEGIN (RSA|OPENSSH|EC|PGP) PRIVATE KEY'
)
for p in "${patterns[@]}"; do
  if hits=$(echo "$FILES" | xargs grep -nE "$p" 2>/dev/null); then
    problem "credential-shaped string matching /$p/:"
    echo "$hits" | head -5 | sed 's/^/        /'
  fi
done

# --- 3. Personal identifiers ------------------------------------------------
# Docs use reserved example values instead.
if hits=$(echo "$FILES" | xargs grep -nE '@me\.com|@mac\.com|clawdbot|[Cc]laudeClaw|Terminatoer' 2>/dev/null); then
  problem "personal identifier or private project name:"
  echo "$hits" | head -10 | sed 's/^/        /'
fi

# --- 4. Real phone numbers --------------------------------------------------
# Reserved ranges (+1 555 01xx, and the 99xx numbers) are fine in docs.
if hits=$(echo "$FILES" | xargs grep -nE '\+[0-9]{10,15}' 2>/dev/null | grep -vE '\+1\s?555\s?01|\+15550[01]|\+49\s?30\s?99|\+44\s?7700\s?9'); then
  problem "possible real phone number (use +1 555 0100 style reserved numbers in docs):"
  echo "$hits" | head -5 | sed 's/^/        /'
fi

# --- 5. Absolute home paths -------------------------------------------------
if hits=$(echo "$FILES" | grep -vE '\.(md|lock)$' | xargs grep -nE '/Users/[a-z0-9._-]+/' 2>/dev/null | grep -v '/Users/USER'); then
  problem "hardcoded home directory path:"
  echo "$hits" | head -5 | sed 's/^/        /'
fi

# --- 6. gitleaks, when available --------------------------------------------
# Leaks are reported as exit 2 so they are distinguishable from a tool error.
# Collapsing the two is how a bad flag once got reported as "findings", which is
# the same failure as reporting a broken check as a passing one.
if command -v gitleaks >/dev/null 2>&1; then
  gl_out=$(gitleaks detect --no-git --redact --no-banner --exit-code 2 2>&1)
  gl_rc=$?
  case "$gl_rc" in
    0) note "gitleaks: clean" ;;
    2) problem "gitleaks found secrets:"
       echo "$gl_out" | grep -iE 'finding|file|line|rule' | head -20 | sed 's/^/        /' ;;
    *) problem "gitleaks failed to run (exit $gl_rc). A check that cannot run is not a check that passed:"
       echo "$gl_out" | tail -5 | sed 's/^/        /' ;;
  esac
else
  note "gitleaks not installed locally; CI runs it on every push"
fi

echo
if [ "$fail" -eq 0 ]; then
  echo "Scrub check passed."
else
  echo "Scrub check FAILED. Nothing is pushed until this is clean."
fi
exit "$fail"
