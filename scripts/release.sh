#!/usr/bin/env bash
#
# release.sh — release automation for devmix/synopsis-rs.
#
# Runs preflight checks, bumps the [workspace.package] version in the root
# Cargo.toml, commits it, creates an annotated tag vX.Y.Z (message exactly
# the tag name), and pushes to the release remote `github` (a v* tag push
# triggers .github/workflows/release.yml) and the mirror remote `origin`.
#
# Usage:
#   scripts/release.sh [VERSION | --bump=major|minor|patch] [--dry-run]
#                      [--no-mirror] [--skip-ci-check]
#
# Safety: never uses push flags that rewrite remote history, never deletes
# tags, never rolls back automatically. On a mid-sequence failure it prints
# the current state and manual next steps.

set -euo pipefail

readonly RELEASE_REMOTE="github"
readonly MIRROR_REMOTE="origin"
readonly REPO_SLUG="devmix/synopsis-rs"

# Progress state for the mid-sequence failure report.
TARGET_VERSION=""
RELEASE_COMMIT_MADE=0
TAG_MADE=0
PUSHED_RELEASE_MAIN=0
PUSHED_RELEASE_TAG=0
PUSHED_MIRROR_MAIN=0
PUSHED_MIRROR_TAG=0
IN_ACTIONS=0

die() {
  echo "release.sh: error: $*" >&2
  exit 1
}

usage() {
  cat <<'EOF'
Usage: scripts/release.sh [VERSION | --bump=major|minor|patch] [options]

Creates a release: bumps the [workspace.package] version in the root
Cargo.toml, commits it, tags it as an annotated tag vX.Y.Z (message exactly
the tag name), and pushes to the release remote (github) and the mirror
(origin). Pushing the v* tag to github triggers the release pipeline.

Arguments:
  VERSION                   Explicit target version, e.g. 0.3.0 or v0.3.0.
  --bump=major|minor|patch  Bump the current workspace version.
  (neither)                 Compute from conventional commits since the last
                            v* tag: breaking change -> major, any feat ->
                            minor, otherwise patch.

Options:
  --dry-run         Print the full plan without making any changes.
  --no-mirror       Do not push to the `origin` mirror remote.
  --skip-ci-check   Skip the "CI green on HEAD" preflight check.
  -h, --help        Print this help and exit.

Preflight checks (all must pass): on branch main in a clean worktree,
local main in sync with github/main, CI green on HEAD (unless skipped),
and the target tag absent locally and on github.
EOF
}

# --- Pure version helpers (unit-testable, no side effects) -----------------

current_workspace_version() {
  # Print the value of the `version = "X.Y.Z"` line inside the
  # [workspace.package] section of the given Cargo.toml. Prints nothing
  # (and returns 1) if the file or the section/line is not found.
  local cargo_toml="$1"
  [[ -f "$cargo_toml" ]] || return 1
  local value
  value=$(awk '
    /^\[workspace\.package\]$/ { in_section = 1; next }
    /^\[/                      { in_section = 0 }
    in_section && /^version[[:space:]]*=/ {
      line = $0
      sub(/^[^"]*"/, "", line)
      sub(/".*$/, "", line)
      print line
      exit
    }
  ' "$cargo_toml")
  [[ -n "$value" ]] || return 1
  printf '%s\n' "$value"
}

parse_target_version() {
  # Validate an explicit VERSION argument against ^v?[0-9]+(\.[0-9]+){2}$
  # and print it without the leading v. Returns 1 if invalid.
  local raw="$1"
  if [[ ! "$raw" =~ ^v?[0-9]+(\.[0-9]+){2}$ ]]; then
    return 1
  fi
  printf '%s\n' "${raw#v}"
}

bump_version() {
  # bump_version X.Y.Z major|minor|patch — print the bumped version.
  local version="$1" part="$2"
  local major minor patch
  IFS=. read -r major minor patch <<< "$version"
  case "$part" in
    major) printf '%d.0.0\n' "$((major + 1))" ;;
    minor) printf '%d.%d.0\n' "$major" "$((minor + 1))" ;;
    patch) printf '%d.%d.%d\n' "$major" "$minor" "$((patch + 1))" ;;
    *)
      echo "release.sh: error: unknown bump part: $part" >&2
      return 1
      ;;
  esac
}

version_greater() {
  # Return 0 iff $1 is strictly greater than $2 (numeric, per component).
  local a1 a2 a3 b1 b2 b3
  IFS=. read -r a1 a2 a3 <<< "$1"
  IFS=. read -r b1 b2 b3 <<< "$2"
  if ((a1 > b1)); then return 0; fi
  if ((a1 < b1)); then return 1; fi
  if ((a2 > b2)); then return 0; fi
  if ((a2 < b2)); then return 1; fi
  if ((a3 > b3)); then return 0; fi
  return 1
}

check_regression_guard() {
  # check_regression_guard TARGET CURRENT — abort (exit 1) if TARGET is not
  # strictly greater than CURRENT.
  local target="$1" current="$2"
  if ! version_greater "$target" "$current"; then
    echo "release.sh: error: target version $target is not greater than the current workspace version $current; refusing to release" >&2
    return 1
  fi
  return 0
}

last_release_tag() {
  # Print the most recent v* tag reachable from HEAD. Fails if there is none.
  git describe --match 'v*' --abbrev=0 HEAD
}

compute_bump() {
  # compute_bump LAST_TAG — print major|minor|patch derived from the
  # conventional commits in LAST_TAG..HEAD:
  #   breaking (subject `type(scope)!: ...` or `BREAKING CHANGE:` in the
  #   body) -> major; else any `feat:`/`feat(...)` subject -> minor;
  #   otherwise patch. Returns 1 if there are no commits in the range.
  local last_tag="$1"
  local range="${last_tag}..HEAD"
  local subjects subject
  subjects=$(git log --no-merges --pretty=%s "$range")
  if [[ -z "$subjects" ]]; then
    return 1
  fi
  local breaking_re='^[A-Za-z]+(\([^)]*\))?!:'
  local feat_re='^feat(\(|:)'
  local breaking=0 has_feat=0
  while IFS= read -r subject; do
    [[ -n "$subject" ]] || continue
    if [[ "$subject" =~ $breaking_re ]]; then
      breaking=1
    fi
    if [[ "$subject" =~ $feat_re ]]; then
      has_feat=1
    fi
  done <<< "$subjects"
  if ((breaking == 0)); then
    local bodies
    bodies=$(git log --no-merges --pretty=%b "$range")
    if [[ "$bodies" == *'BREAKING CHANGE:'* ]]; then
      breaking=1
    fi
  fi
  if ((breaking == 1)); then
    printf 'major\n'
  elif ((has_feat == 1)); then
    printf 'minor\n'
  else
    printf 'patch\n'
  fi
}

compute_target_from_commits() {
  # Print the target version computed from conventional commits since the
  # last v* tag (bump applied to the tag's version). Prints an error and
  # returns 1 if there is nothing to release.
  local last_tag part tag_version
  if ! last_tag=$(last_release_tag); then
    die "no v* tag found in history; tag a release first or pass an explicit VERSION"
  fi
  if ! part=$(compute_bump "$last_tag"); then
    echo "release.sh: error: nothing to release: no commits since $last_tag" >&2
    return 1
  fi
  tag_version="${last_tag#v}"
  bump_version "$tag_version" "$part"
}

# --- Preflight --------------------------------------------------------------

check_ci_green() {
  # Preflight 4: CI green on HEAD via the GitHub check-runs API.
  local head api token resp state
  head=$(git rev-parse HEAD)
  api="https://api.github.com/repos/${REPO_SLUG}/commits/${head}/check-runs"
  token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
  local -a auth=()
  if [[ -n "$token" ]]; then
    auth=(-H "Authorization: Bearer $token")
  fi
  if ! resp=$(curl -s "${auth[@]}" "$api"); then
    die "could not query GitHub check-runs for HEAD ($head); is the network reachable? (use --skip-ci-check to bypass)"
  fi
  if ! state=$(jq -r '
    if (.total_count // 0) == 0 then "none"
    elif ([.check_runs[] | select(.status != "completed" or .conclusion != "success")] | length) > 0 then "red"
    else "green"
    end' <<< "$resp"); then
    die "could not parse the GitHub check-runs response for HEAD ($head)"
  fi
  case "$state" in
    green) echo "  preflight: CI green on ${head:0:12}" ;;
    none) die "CI check failed: no check-runs recorded for HEAD ($head)" ;;
    red) die "CI check failed: HEAD ($head) is not green; see https://github.com/${REPO_SLUG}/actions" ;;
    *) die "CI check failed: unexpected response for HEAD ($head)" ;;
  esac
}

preflight_basic() {
  # Preflight 1-4 (step 5 needs the target version and runs separately).
  local branch dirty local_head remote_head
  # 1. Inside a git worktree and on branch main.
  if ! git rev-parse --git-dir >/dev/null 2>&1; then
    die "not inside a git worktree"
  fi
  branch=$(git branch --show-current)
  if [[ "$branch" != "main" ]]; then
    die "not on branch 'main' (current: ${branch:-<detached HEAD>})"
  fi
  # 2. Clean working tree.
  dirty=$(git status --porcelain)
  if [[ -n "$dirty" ]]; then
    echo "release.sh: error: working tree is not clean; commit or stash the changes below first:" >&2
    printf '%s\n' "$dirty" >&2
    exit 1
  fi
  # 3. In sync with the release remote.
  if ! git fetch "$RELEASE_REMOTE" --tags --quiet; then
    die "git fetch $RELEASE_REMOTE failed (network?)"
  fi
  local_head=$(git rev-parse main)
  if ! remote_head=$(git rev-parse --verify "${RELEASE_REMOTE}/main" 2>/dev/null); then
    die "remote $RELEASE_REMOTE has no 'main' branch"
  fi
  if [[ "$local_head" != "$remote_head" ]]; then
    die "local main (${local_head:0:12}) is not in sync with ${RELEASE_REMOTE}/main (${remote_head:0:12}); pull first"
  fi
  # 4. CI green on HEAD (skippable).
  if ((SKIP_CI_CHECK == 0)); then
    check_ci_green
  fi
  echo "  preflight: worktree clean, main in sync with ${RELEASE_REMOTE}/main"
}

preflight_target() {
  # Preflight 5: the target tag must be absent locally and on github.
  local target_tag="v$1"
  if git rev-parse -q --verify "refs/tags/$target_tag" >/dev/null 2>&1; then
    die "tag $target_tag already exists locally"
  fi
  if [[ -n "$(git ls-remote --tags "$RELEASE_REMOTE" "refs/tags/$target_tag")" ]]; then
    die "tag $target_tag already exists on $RELEASE_REMOTE"
  fi
  echo "  preflight: tag $target_tag absent locally and on $RELEASE_REMOTE"
}

# --- Actions ----------------------------------------------------------------

bump_workspace_version() {
  # Rewrite the [workspace.package] version line of the given Cargo.toml
  # (section-scoped awk edit via temp file + mv).
  local cargo_toml="$1" new_version="$2"
  local tmp
  tmp=$(mktemp "$(dirname "$cargo_toml")/.Cargo.toml.tmp.XXXXXX") || die "mktemp failed"
  if ! awk -v ver="$new_version" '
    /^\[workspace\.package\]$/ { in_section = 1; next }
    /^\[/                      { in_section = 0 }
    in_section && /^version[[:space:]]*=/ {
      sub(/"[^"]*"/, "\"" ver "\"")
      printed = 1
    }
    { print }
    END { if (!printed) exit 1 }
  ' "$cargo_toml" > "$tmp"; then
    rm -f "$tmp"
    die "could not find the version line in [workspace.package] of $cargo_toml"
  fi
  mv "$tmp" "$cargo_toml"
}

yn() {
  if [[ "$1" == "1" ]]; then echo "yes"; else echo "no"; fi
}

release_failed_state() {
  # Printed on a mid-sequence failure: where the release stands and what a
  # human should do next.
  {
    echo "release.sh: the release sequence FAILED partway through. Current state:"
    echo "  version bump commit made : $(yn "$RELEASE_COMMIT_MADE")"
    echo "  tag v$TARGET_VERSION made      : $(yn "$TAG_MADE")"
    echo "  pushed $RELEASE_REMOTE main       : $(yn "$PUSHED_RELEASE_MAIN")"
    echo "  pushed $RELEASE_REMOTE tag         : $(yn "$PUSHED_RELEASE_TAG")"
    echo "  pushed $MIRROR_REMOTE main       : $(yn "$PUSHED_MIRROR_MAIN")"
    echo "  pushed $MIRROR_REMOTE tag         : $(yn "$PUSHED_MIRROR_TAG")"
    echo "Manual next steps (pick what matches the state above):"
    echo "  - commit made, tag missing : git tag -a v$TARGET_VERSION -m v$TARGET_VERSION"
    echo "  - tag made, push missing   : git push $RELEASE_REMOTE main && git push $RELEASE_REMOTE v$TARGET_VERSION"
    if ((NO_MIRROR == 0)); then
      echo "                               git push $MIRROR_REMOTE main && git push $MIRROR_REMOTE v$TARGET_VERSION"
    fi
    echo "  - nothing pushed, undo     : git reset --hard HEAD~1   (removes the bump commit)"
    echo "  - pushed to $RELEASE_REMOTE already : the release pipeline is running; check"
    echo "                               https://github.com/${REPO_SLUG}/actions"
  } >&2
}

on_exit() {
  local rc=$?
  trap - EXIT
  if ((rc != 0 && IN_ACTIONS == 1)); then
    release_failed_state
  fi
}

print_plan() {
  # The full dry-run plan.
  local target="$1" current="$2"
  echo "release.sh: DRY RUN — no changes will be made"
  echo
  echo "  current version : $current"
  echo "  target version  : $target"
  echo "  Cargo.toml      : version = \"$current\"  ->  version = \"$target\"   ([workspace.package])"
  echo "  commit          : chore(release): bump version to v$target"
  echo "  tag             : v$target (annotated, message exactly: v$target)"
  echo "  push            : $RELEASE_REMOTE main, $RELEASE_REMOTE v$target   (tag push triggers the release pipeline)"
  if ((NO_MIRROR == 0)); then
    echo "  push mirror     : $MIRROR_REMOTE main, $MIRROR_REMOTE v$target"
  else
    echo "  push mirror     : skipped (--no-mirror)"
  fi
  echo
  echo "Re-run without --dry-run to execute."
}

run_actions() {
  local target="$1" repo_root="$2"
  local cargo_toml="$repo_root/Cargo.toml"
  IN_ACTIONS=1
  trap on_exit EXIT
  # 1. Bump the [workspace.package] version line.
  bump_workspace_version "$cargo_toml" "$target"
  # 2. Verify the new line is present (section-scoped).
  if [[ "$(current_workspace_version "$cargo_toml")" != "$target" ]]; then
    die "verification failed: [workspace.package] version in $cargo_toml is not $target"
  fi
  # 3. Commit the bump.
  git add -- "$cargo_toml"
  git commit -m "chore(release): bump version to v$target"
  RELEASE_COMMIT_MADE=1
  # 4. Annotated tag, message exactly the tag name.
  git tag -a "v$target" -m "v$target"
  TAG_MADE=1
  # 5. Push to the release remote (the tag push triggers the pipeline).
  git push "$RELEASE_REMOTE" main
  PUSHED_RELEASE_MAIN=1
  git push "$RELEASE_REMOTE" "v$target"
  PUSHED_RELEASE_TAG=1
  # 6. Mirror (unless --no-mirror).
  if ((NO_MIRROR == 0)); then
    git push "$MIRROR_REMOTE" main
    PUSHED_MIRROR_MAIN=1
    git push "$MIRROR_REMOTE" "v$target"
    PUSHED_MIRROR_TAG=1
  fi
  # 7. Summary.
  echo
  echo "Release v$target is done."
  echo "  commit : $(git rev-parse --short HEAD)"
  echo "  tag    : v$target"
  echo "  CI     : https://github.com/${REPO_SLUG}/actions"
  # Best effort: surface the newest Actions run URL; tolerate any failure.
  local token="${GITHUB_TOKEN:-${GH_TOKEN:-}}"
  local -a auth=()
  if [[ -n "$token" ]]; then
    auth=(-H "Authorization: Bearer $token")
  fi
  sleep 5
  local resp run_url
  if resp=$(curl -s "${auth[@]}" "https://api.github.com/repos/${REPO_SLUG}/actions/runs?per_page=1" 2>/dev/null); then
    if run_url=$(jq -r '.workflow_runs[0].html_url // empty' <<< "$resp" 2>/dev/null) && [[ -n "$run_url" ]]; then
      echo "  run    : $run_url"
    fi
  fi
}

main() {
  local target_arg="" bump_arg=""
  local dry_run=0 arg
  for arg in "$@"; do
    case "$arg" in
      -h|--help)
        usage
        exit 0
        ;;
      --dry-run) dry_run=1 ;;
      --no-mirror) NO_MIRROR=1 ;;
      --skip-ci-check) SKIP_CI_CHECK=1 ;;
      --bump=*) bump_arg="${arg#--bump=}" ;;
      --bump)
        echo "release.sh: error: --bump requires a value (major|minor|patch)" >&2
        usage >&2
        exit 2
        ;;
      -*)
        echo "release.sh: error: unrecognized flag: $arg" >&2
        usage >&2
        exit 2
        ;;
      *)
        if [[ -n "$target_arg" || -n "$bump_arg" ]]; then
          echo "release.sh: error: give at most one of VERSION and --bump" >&2
          usage >&2
          exit 2
        fi
        target_arg="$arg"
        ;;
    esac
  done
  NO_MIRROR=${NO_MIRROR:-0}
  SKIP_CI_CHECK=${SKIP_CI_CHECK:-0}

  if [[ -n "$bump_arg" ]]; then
    case "$bump_arg" in
      major|minor|patch) ;;
      *) die "invalid --bump value '$bump_arg' (expected major, minor, or patch)" ;;
    esac
  fi

  # Preflight 1-4 (before touching anything).
  preflight_basic

  local repo_root cargo_toml current target
  repo_root=$(git rev-parse --show-toplevel)
  cargo_toml="$repo_root/Cargo.toml"

  if ! current=$(current_workspace_version "$cargo_toml"); then
    die "could not parse the [workspace.package] version from $cargo_toml"
  fi
  if [[ ! "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    die "unexpected workspace version format: '$current'"
  fi

  if [[ -n "$target_arg" ]]; then
    if ! target=$(parse_target_version "$target_arg"); then
      die "invalid VERSION '$target_arg' (expected X.Y.Z or vX.Y.Z)"
    fi
  elif [[ -n "$bump_arg" ]]; then
    target=$(bump_version "$current" "$bump_arg")
  else
    if ! target=$(compute_target_from_commits); then
      exit 1
    fi
  fi

  check_regression_guard "$target" "$current"
  TARGET_VERSION="$target"

  # Preflight 5 (needs the target).
  preflight_target "$target"

  if ((dry_run == 1)); then
    print_plan "$target" "$current"
    exit 0
  fi

  run_actions "$target" "$repo_root"
}

if [[ "${BASH_SOURCE[0]}" == "${0}" ]]; then
  main "$@"
fi
