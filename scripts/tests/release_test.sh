#!/usr/bin/env bash
#
# release_test.sh — fixture tests for the version logic in scripts/release.sh.
#
# Sources the real script (functions only — main is guarded and not run) and
# exercises the version computation against throwaway git repos in a temp
# dir. Runs from any CWD; each case prints PASS/FAIL, and the script exits
# non-zero if any case fails.

set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)
RELEASE_SH="${SCRIPT_DIR}/../release.sh"
if [[ ! -f "$RELEASE_SH" ]]; then
  echo "  [FAIL] release script not found at $RELEASE_SH"
  exit 1
fi
# shellcheck source=../release.sh
source "$RELEASE_SH"

FIXTURE_DIR=$(mktemp -d)
trap 'rm -rf "$FIXTURE_DIR"' EXIT

FAILURES=0

pass() {
  echo "  [PASS] $1"
}

fail() {
  echo "  [FAIL] $1"
  FAILURES=$((FAILURES + 1))
}

# Run a release.sh function inside the fixture repo (subshell, so any
# exit/die inside only affects that subshell).
run_in_fixture() {
  (cd "$FIXTURE_DIR" && "$@")
}

# make_fixture_repo WORKSPACE_VERSION COMMIT_MSG...
# Builds a git repo with a base commit tagged v0.2.0 (Cargo.toml carrying the
# given workspace version) plus the given additional commits (each argument
# is one full commit message; use separate -m paragraphs for bodies via $'\n').
make_fixture_repo() {
  local ws_version="$1"
  shift
  rm -rf "$FIXTURE_DIR"
  mkdir -p "$FIXTURE_DIR"
  git -C "$FIXTURE_DIR" init -q
  git -C "$FIXTURE_DIR" config user.name "Release Test"
  git -C "$FIXTURE_DIR" config user.email "release-test@example.com"
  printf '[workspace.package]\nversion = "%s"\nedition = "2024"\n\n[workspace]\nmembers = []\n' "$ws_version" > "$FIXTURE_DIR/Cargo.toml"
  git -C "$FIXTURE_DIR" add Cargo.toml
  git -C "$FIXTURE_DIR" commit -q -m "chore: initial release v0.2.0"
  git -C "$FIXTURE_DIR" tag -a v0.2.0 -m v0.2.0
  local msg
  for msg in "$@"; do
    git -C "$FIXTURE_DIR" commit -q --allow-empty -F - <<< "$msg"
  done
}

# expect_target DESC EXPECTED — compute the target in the fixture repo and
# compare it against EXPECTED.
expect_target() {
  local desc="$1" expected="$2" out
  if out=$(run_in_fixture compute_target_from_commits); then
    if [[ "$out" == "$expected" ]]; then
      pass "$desc"
    else
      fail "$desc (expected $expected, got $out)"
    fi
  else
    fail "$desc (computation failed, expected $expected)"
  fi
}

echo "release_test.sh: sourcing functions from $RELEASE_SH"

# Case 1: feat only since tag -> minor (0.2.0 -> 0.3.0)
make_fixture_repo "0.2.0" "feat: add shiny feature"
expect_target "case 1: feat only since tag -> minor (0.2.0 -> 0.3.0)" "0.3.0"

# Case 2: fix only -> patch (0.2.0 -> 0.2.1)
make_fixture_repo "0.2.0" "fix: repair the thing"
expect_target "case 2: fix only -> patch (0.2.0 -> 0.2.1)" "0.2.1"

# Case 3: breaking subject feat(x)!: ... -> major (0.2.0 -> 1.0.0)
make_fixture_repo "0.2.0" "feat(x)!: drop the old API"
expect_target "case 3: breaking subject feat(x)! -> major (0.2.0 -> 1.0.0)" "1.0.0"

# Case 4: breaking via BREAKING CHANGE: in the body (plain feat: subject)
# -> major (0.2.0 -> 1.0.0)
make_fixture_repo "0.2.0" $'feat: x\n\nBREAKING CHANGE: the old API is gone'
expect_target "case 4: BREAKING CHANGE in body -> major (0.2.0 -> 1.0.0)" "1.0.0"

# Case 5: no commits since tag -> computation errors (non-zero)
make_fixture_repo "0.2.0"
if out=$(run_in_fixture compute_target_from_commits 2>/dev/null); then
  fail "case 5: no commits since tag should error (got success: $out)"
else
  pass "case 5: no commits since tag -> computation errors"
fi

# Case 6: regression guard — target == current aborts; target lower aborts.
make_fixture_repo "0.2.0" "feat: x"
if run_in_fixture check_regression_guard 0.2.0 0.2.0 >/dev/null 2>&1; then
  fail "case 6a: target == current (0.2.0) should abort"
else
  pass "case 6a: target == current (0.2.0) -> abort"
fi
if run_in_fixture check_regression_guard 0.1.0 0.2.0 >/dev/null 2>&1; then
  fail "case 6b: target lower (0.1.0) should abort"
else
  pass "case 6b: target lower (0.1.0) -> abort"
fi
if run_in_fixture check_regression_guard 0.3.0 0.2.0 >/dev/null 2>&1; then
  pass "case 6c: target greater (0.3.0) -> accepted"
else
  fail "case 6c: target greater (0.3.0) should be accepted"
fi

# Case 7: explicit parse — v0.3.0 accepted, 1.2 rejected.
if out=$(parse_target_version "v0.3.0"); then
  if [[ "$out" == "0.3.0" ]]; then
    pass "case 7a: explicit v0.3.0 -> 0.3.0 accepted"
  else
    fail "case 7a: explicit v0.3.0 (expected 0.3.0, got $out)"
  fi
else
  fail "case 7a: explicit v0.3.0 should be accepted"
fi
if parse_target_version "1.2" >/dev/null 2>&1; then
  fail "case 7b: explicit 1.2 should be rejected"
else
  pass "case 7b: explicit 1.2 -> rejected"
fi

# Case 8: --bump=minor on 0.2.0 -> 0.3.0
if out=$(bump_version "0.2.0" "minor"); then
  if [[ "$out" == "0.3.0" ]]; then
    pass "case 8: --bump=minor on 0.2.0 -> 0.3.0"
  else
    fail "case 8: --bump=minor on 0.2.0 (expected 0.3.0, got $out)"
  fi
else
  fail "case 8: bump_version failed"
fi

echo
if ((FAILURES > 0)); then
  echo "release_test.sh: $FAILURES case(s) FAILED"
  exit 1
fi
echo "release_test.sh: all cases passed"
