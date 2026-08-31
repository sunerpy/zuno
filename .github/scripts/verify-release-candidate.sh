#!/usr/bin/env bash
set -euo pipefail

: "${CANDIDATE_ROOT:?CANDIDATE_ROOT is required}"
: "${EXPECTED_REPOSITORY:?EXPECTED_REPOSITORY is required}"
: "${EXPECTED_RUN_ID:?EXPECTED_RUN_ID is required}"
: "${EXPECTED_RUN_ATTEMPT:?EXPECTED_RUN_ATTEMPT is required}"
: "${EXPECTED_PR_NUMBER:?EXPECTED_PR_NUMBER is required}"
: "${EXPECTED_HEAD_SHA:?EXPECTED_HEAD_SHA is required}"
: "${EXPECTED_PR_HEAD_SHA:?EXPECTED_PR_HEAD_SHA is required}"
: "${EXPECTED_TREE_SHA:?EXPECTED_TREE_SHA is required}"
: "${EXPECTED_VERSION:?EXPECTED_VERSION is required}"
: "${EXPECTED_TAG:?EXPECTED_TAG is required}"

readonly targets=(
  aarch64-apple-darwin
  aarch64-unknown-linux-musl
  x86_64-apple-darwin
  x86_64-pc-windows-msvc
  x86_64-unknown-linux-musl
)

cd "$CANDIDATE_ROOT"
manifest=candidate-manifest.json
if [ ! -f "$manifest" ] || [ ! -f SHA256SUMS ]; then
  echo "::error title=Candidate verification::candidate-manifest.json or SHA256SUMS is missing"
  exit 1
fi

jq -e \
  --arg repository "$EXPECTED_REPOSITORY" \
  --argjson run_id "$EXPECTED_RUN_ID" \
  --argjson run_attempt "$EXPECTED_RUN_ATTEMPT" \
  --argjson release_pr_number "$EXPECTED_PR_NUMBER" \
  --arg head_sha "$EXPECTED_HEAD_SHA" \
  --arg release_pr_head_sha "$EXPECTED_PR_HEAD_SHA" \
  --arg tree_sha "$EXPECTED_TREE_SHA" \
  --arg version "$EXPECTED_VERSION" \
  --arg tag "$EXPECTED_TAG" \
  '
    .schema_version == 1
    and .repository == $repository
    and (.workflow_ref | startswith($repository + "/.github/workflows/release-candidate.yml@"))
    and .workflow_sha == $head_sha
    and .run_id == $run_id
    and .run_attempt == $run_attempt
    and .release_pr_number == $release_pr_number
    and .head_sha == $head_sha
    and .release_pr_head_sha == $release_pr_head_sha
    and .tree_sha == $tree_sha
    and .version == $version
    and .tag == $tag
    and (.mode == "automatic" or .mode == "backfill")
    and .test_conclusion == "success"
    and (.targets | length == 5)
  ' "$manifest" >/dev/null

mode=$(jq -er '.mode' "$manifest")
if [ "$mode" = automatic ] && [ "$EXPECTED_HEAD_SHA" != "$EXPECTED_PR_HEAD_SHA" ]; then
  echo "::error title=Candidate verification::automatic candidate source differs from the release PR head"
  exit 1
fi

mapfile -t manifest_targets < <(jq -r '.targets[].target' "$manifest" | LC_ALL=C sort)
if ! diff -u \
  <(printf '%s\n' "${targets[@]}") \
  <(printf '%s\n' "${manifest_targets[@]}")
then
  echo "::error title=Candidate verification::manifest target set is incomplete or duplicated"
  exit 1
fi

mapfile -t disk_evidence < <(find evidence -maxdepth 1 -type f -name '*.json' -printf '%f\n' | LC_ALL=C sort)
mapfile -t expected_evidence < <(printf '%s.json\n' "${targets[@]}" | LC_ALL=C sort)
if ! diff -u \
  <(printf '%s\n' "${expected_evidence[@]}") \
  <(printf '%s\n' "${disk_evidence[@]}")
then
  echo "::error title=Candidate verification::bundle evidence set is incomplete or unexpected"
  exit 1
fi

mapfile -t manifest_archives < <(jq -r '.targets[].archive' "$manifest" | LC_ALL=C sort)
mapfile -t disk_archives < <(find . -maxdepth 1 -type f \( -name '*.tar.gz' -o -name '*.zip' \) -printf '%f\n' | LC_ALL=C sort)
if [ "${#disk_archives[@]}" -ne "${#targets[@]}" ] || ! diff -u \
  <(printf '%s\n' "${manifest_archives[@]}") \
  <(printf '%s\n' "${disk_archives[@]}")
then
  echo "::error title=Candidate verification::bundle archive set differs from the manifest"
  exit 1
fi

for target in "${targets[@]}"; do
  entry=$(jq -ce --arg target "$target" '.targets[] | select(.target == $target)' "$manifest")
  archive=$(jq -er '.archive' <<<"$entry")
  recorded_size=$(jq -er '.size' <<<"$entry")
  recorded_sha=$(jq -er '.sha256' <<<"$entry")
  build_conclusion=$(jq -er '.build_conclusion' <<<"$entry")
  smoke_conclusion=$(jq -er '.smoke_conclusion' <<<"$entry")
  attestation_id=$(jq -er '.attestation_id' <<<"$entry")

  if [ "$build_conclusion" != success ] || [ "$smoke_conclusion" != success ]; then
    echo "::error title=Candidate verification::${target} lacks successful build/smoke evidence"
    exit 1
  fi
  if [ -z "$attestation_id" ]; then
    echo "::error title=Candidate verification::${target} lacks an attestation ID"
    exit 1
  fi
  if [ "$target" = x86_64-pc-windows-msvc ]; then
    expected_archive="zuno-${EXPECTED_VERSION}-${target}.zip"
  else
    expected_archive="zuno-${EXPECTED_VERSION}-${target}.tar.gz"
  fi
  if [ "$archive" != "$expected_archive" ]; then
    echo "::error title=Candidate verification::${target} archive is ${archive}, expected ${expected_archive}"
    exit 1
  fi
  if ! diff -u \
    <(jq -S . "evidence/${target}.json") \
    <(jq -S --arg target "$target" '.targets[] | select(.target == $target)' "$manifest")
  then
    echo "::error title=Candidate verification::${target} evidence differs from the sealed manifest"
    exit 1
  fi
  if [ "$(stat -c '%s' "$archive")" != "$recorded_size" ]; then
    echo "::error title=Candidate verification::${archive} size does not match its evidence"
    exit 1
  fi
  if [ "$(sha256sum "$archive" | awk '{print $1}')" != "$recorded_sha" ]; then
    echo "::error title=Candidate verification::${archive} digest does not match its evidence"
    exit 1
  fi
done

mapfile -t top_level_files < <(find . -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort)
mapfile -t expected_top_level < <(
  {
    printf '%s\n' "${disk_archives[@]}"
    printf '%s\n' SHA256SUMS candidate-manifest.json
  } | LC_ALL=C sort
)
if ! diff -u \
  <(printf '%s\n' "${expected_top_level[@]}") \
  <(printf '%s\n' "${top_level_files[@]}")
then
  echo "::error title=Candidate verification::bundle contains unexpected top-level files"
  exit 1
fi

sha256sum --check --strict SHA256SUMS
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
for archive in "${disk_archives[@]}"; do
  sha256sum "$archive" >> "$tmp"
done
if ! cmp -s "$tmp" SHA256SUMS; then
  echo "::error title=Candidate verification::SHA256SUMS is not the canonical digest list for the bundle"
  diff -u SHA256SUMS "$tmp" || true
  exit 1
fi

echo "release candidate verified: ${EXPECTED_TAG}, run ${EXPECTED_RUN_ID}, tree ${EXPECTED_TREE_SHA}"
