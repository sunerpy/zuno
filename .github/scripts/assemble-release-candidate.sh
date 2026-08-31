#!/usr/bin/env bash
set -euo pipefail

: "${CANDIDATE_ROOT:?CANDIDATE_ROOT is required}"
: "${CANDIDATE_REPOSITORY:?CANDIDATE_REPOSITORY is required}"
: "${CANDIDATE_WORKFLOW_REF:?CANDIDATE_WORKFLOW_REF is required}"
: "${CANDIDATE_WORKFLOW_SHA:?CANDIDATE_WORKFLOW_SHA is required}"
: "${CANDIDATE_RUN_ID:?CANDIDATE_RUN_ID is required}"
: "${CANDIDATE_RUN_ATTEMPT:?CANDIDATE_RUN_ATTEMPT is required}"
: "${CANDIDATE_PR_NUMBER:?CANDIDATE_PR_NUMBER is required}"
: "${CANDIDATE_MODE:?CANDIDATE_MODE is required}"
: "${CANDIDATE_HEAD_SHA:?CANDIDATE_HEAD_SHA is required}"
: "${CANDIDATE_PR_HEAD_SHA:?CANDIDATE_PR_HEAD_SHA is required}"
: "${CANDIDATE_TREE_SHA:?CANDIDATE_TREE_SHA is required}"
: "${CANDIDATE_VERSION:?CANDIDATE_VERSION is required}"

readonly targets=(
  aarch64-apple-darwin
  aarch64-unknown-linux-musl
  x86_64-apple-darwin
  x86_64-pc-windows-msvc
  x86_64-unknown-linux-musl
)

case "$CANDIDATE_MODE" in
  automatic | dry-run | backfill) ;;
  *)
    echo "::error title=Candidate manifest::unsupported mode ${CANDIDATE_MODE}"
    exit 1
    ;;
esac
for value in \
  "$CANDIDATE_WORKFLOW_SHA" \
  "$CANDIDATE_HEAD_SHA" \
  "$CANDIDATE_PR_HEAD_SHA" \
  "$CANDIDATE_TREE_SHA"
do
  if ! [[ "$value" =~ ^[0-9a-f]{40}$ ]]; then
    echo "::error title=Candidate manifest::candidate identity contains a non-SHA value"
    exit 1
  fi
done

cd "$CANDIDATE_ROOT"
mkdir -p evidence

mapfile -t disk_evidence < <(find evidence -maxdepth 1 -type f -name '*.json' -printf '%f\n' | LC_ALL=C sort)
mapfile -t expected_evidence < <(printf '%s.json\n' "${targets[@]}" | LC_ALL=C sort)
if ! diff -u \
  <(printf '%s\n' "${expected_evidence[@]}") \
  <(printf '%s\n' "${disk_evidence[@]}")
then
  echo "::error title=Candidate manifest::candidate bundle contains an unexpected evidence set"
  exit 1
fi

archive_names=()
for target in "${targets[@]}"; do
  evidence_file="evidence/${target}.json"
  actual_target=$(jq -er '.target' "$evidence_file")
  archive=$(jq -er '.archive' "$evidence_file")
  recorded_size=$(jq -er '.size' "$evidence_file")
  recorded_sha=$(jq -er '.sha256' "$evidence_file")
  build_conclusion=$(jq -er '.build_conclusion' "$evidence_file")
  smoke_conclusion=$(jq -er '.smoke_conclusion' "$evidence_file")
  attestation_id=$(jq -er '.attestation_id' "$evidence_file")

  if [ "$actual_target" != "$target" ]; then
    echo "::error title=Candidate manifest::${evidence_file} names ${actual_target}, expected ${target}"
    exit 1
  fi
  if [ "$build_conclusion" != success ] || [ "$smoke_conclusion" != success ]; then
    echo "::error title=Candidate manifest::${target} was not both built and smoked successfully"
    exit 1
  fi
  if [ -z "$attestation_id" ]; then
    echo "::error title=Candidate manifest::${target} has no provenance attestation"
    exit 1
  fi
  if [ "$target" = x86_64-pc-windows-msvc ]; then
    expected_archive="zuno-${CANDIDATE_VERSION}-${target}.zip"
  else
    expected_archive="zuno-${CANDIDATE_VERSION}-${target}.tar.gz"
  fi
  if [ "$archive" != "$expected_archive" ]; then
    echo "::error title=Candidate manifest::${target} archive is ${archive}, expected ${expected_archive}"
    exit 1
  fi
  if [ ! -f "$archive" ]; then
    echo "::error title=Candidate manifest::missing archive ${archive}"
    exit 1
  fi

  actual_size=$(stat -c '%s' "$archive")
  actual_sha=$(sha256sum "$archive" | awk '{print $1}')
  if [ "$actual_size" != "$recorded_size" ]; then
    echo "::error title=Candidate manifest::${archive} size changed (${recorded_size} -> ${actual_size})"
    exit 1
  fi
  if [ "$actual_sha" != "$recorded_sha" ]; then
    echo "::error title=Candidate manifest::${archive} digest changed"
    exit 1
  fi
  archive_names+=("$archive")
done

mapfile -t disk_archives < <(find . -maxdepth 1 -type f \( -name '*.tar.gz' -o -name '*.zip' \) -printf '%f\n' | LC_ALL=C sort)
mapfile -t expected_archives < <(printf '%s\n' "${archive_names[@]}" | LC_ALL=C sort)
if [ "${#disk_archives[@]}" -ne "${#targets[@]}" ] || ! diff -u \
  <(printf '%s\n' "${expected_archives[@]}") \
  <(printf '%s\n' "${disk_archives[@]}")
then
  echo "::error title=Candidate manifest::candidate bundle contains an unexpected archive set"
  exit 1
fi

: > SHA256SUMS
for archive in "${expected_archives[@]}"; do
  sha256sum "$archive" >> SHA256SUMS
done

targets_json=$(jq -s 'sort_by(.target)' evidence/*.json)
jq -n \
  --arg repository "$CANDIDATE_REPOSITORY" \
  --arg workflow_ref "$CANDIDATE_WORKFLOW_REF" \
  --arg workflow_sha "$CANDIDATE_WORKFLOW_SHA" \
  --argjson run_id "$CANDIDATE_RUN_ID" \
  --argjson run_attempt "$CANDIDATE_RUN_ATTEMPT" \
  --argjson release_pr_number "$CANDIDATE_PR_NUMBER" \
  --arg mode "$CANDIDATE_MODE" \
  --arg head_sha "$CANDIDATE_HEAD_SHA" \
  --arg release_pr_head_sha "$CANDIDATE_PR_HEAD_SHA" \
  --arg tree_sha "$CANDIDATE_TREE_SHA" \
  --arg version "$CANDIDATE_VERSION" \
  --arg tag "v${CANDIDATE_VERSION}" \
  --argjson targets "$targets_json" \
  '{
    schema_version: 1,
    repository: $repository,
    workflow_ref: $workflow_ref,
    workflow_sha: $workflow_sha,
    run_id: $run_id,
    run_attempt: $run_attempt,
    release_pr_number: $release_pr_number,
    mode: $mode,
    head_sha: $head_sha,
    release_pr_head_sha: $release_pr_head_sha,
    tree_sha: $tree_sha,
    version: $version,
    tag: $tag,
    test_conclusion: "success",
    targets: $targets
  }' > candidate-manifest.json

jq -e '.targets | length == 5' candidate-manifest.json >/dev/null
sha256sum --check --strict SHA256SUMS
cat candidate-manifest.json
