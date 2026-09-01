#!/usr/bin/env bash
# Resolve a release-please PR only after GitHub's PR and commit APIs agree that
# its head is a stable child of the main commit that started this controller.
set -euo pipefail

repository=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}
pr_number=${RELEASE_PR_NUMBER:?RELEASE_PR_NUMBER is required}
expected_base_sha=${EXPECTED_BASE_SHA:?EXPECTED_BASE_SHA is required}
attempts=${RELEASE_PR_RESOLVE_ATTEMPTS:-30}
delay_seconds=${RELEASE_PR_RESOLVE_DELAY_SECONDS:-2}
gh_bin=${GH_BIN:-gh}

if ! [[ "$pr_number" =~ ^[1-9][0-9]*$ ]]; then
  echo "release PR number is invalid: $pr_number" >&2
  exit 2
fi
if ! [[ "$expected_base_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "expected base SHA is invalid: $expected_base_sha" >&2
  exit 2
fi
if ! [[ "$attempts" =~ ^[1-9][0-9]*$ ]]; then
  echo "release PR resolve attempts must be positive: $attempts" >&2
  exit 2
fi
if ! [[ "$delay_seconds" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo "release PR resolve delay is invalid: $delay_seconds" >&2
  exit 2
fi

last_observed=unavailable
for ((attempt = 1; attempt <= attempts; attempt++)); do
  if ! pr=$("$gh_bin" api "repos/${repository}/pulls/${pr_number}" 2>/dev/null); then
    last_observed="pull request API unavailable"
  else
    state=$(jq -r '.state // empty' <<<"$pr")
    user=$(jq -r '.user.login // empty' <<<"$pr")
    base_ref=$(jq -r '.base.ref // empty' <<<"$pr")
    base_sha=$(jq -r '.base.sha // empty' <<<"$pr")
    head_repo=$(jq -r '.head.repo.full_name // empty' <<<"$pr")
    head_ref=$(jq -r '.head.ref // empty' <<<"$pr")
    head_sha=$(jq -r '.head.sha // empty' <<<"$pr")

    if [ "$state" != open ] \
      || [ "$user" != 'github-actions[bot]' ] \
      || [ "$base_ref" != main ] \
      || [ "$head_repo" != "$repository" ] \
      || [[ "$head_ref" != release-please--branches--main--* ]] \
      || ! jq -e '[.labels[].name] | index("autorelease: pending") != null' <<<"$pr" >/dev/null
    then
      echo "release PR identity does not match the trusted release-please shape" >&2
      exit 1
    fi

    last_observed="base=${base_sha:-missing} head=${head_sha:-missing}"
    if [ "$base_sha" = "$expected_base_sha" ] && [[ "$head_sha" =~ ^[0-9a-f]{40}$ ]]; then
      if commit=$("$gh_bin" api "repos/${repository}/commits/${head_sha}" 2>/dev/null); then
        parent_count=$(jq '.parents | length' <<<"$commit")
        parent_sha=$(jq -r '.parents[0].sha // empty' <<<"$commit")
        author=$(jq -r '.author.login // empty' <<<"$commit")
        author_email=$(jq -r '.commit.author.email // empty' <<<"$commit")
        subject=$(jq -r '.commit.message // empty | split("\n")[0]' <<<"$commit")

        if [ "$parent_count" -ne 1 ]; then
          echo "release-please head must have exactly one parent" >&2
          exit 1
        fi
        if [ "$author" != 'github-actions[bot]' ] \
          && [ "$author_email" != '41898282+github-actions[bot]@users.noreply.github.com' ]
        then
          echo "release-please head is not authored by github-actions[bot]" >&2
          exit 1
        fi
        if [[ "$subject" != "chore: release"* ]]; then
          echo "release-please head has an unexpected subject: $subject" >&2
          exit 1
        fi

        if [ "$parent_sha" = "$expected_base_sha" ]; then
          if confirmed=$("$gh_bin" api "repos/${repository}/pulls/${pr_number}" 2>/dev/null); then
            confirmed_base=$(jq -r '.base.sha // empty' <<<"$confirmed")
            confirmed_head=$(jq -r '.head.sha // empty' <<<"$confirmed")
            if jq -e \
              --arg repository "$repository" \
              --arg base_sha "$expected_base_sha" \
              --arg head_sha "$head_sha" \
              --arg head_ref "$head_ref" \
              '
                .state == "open"
                and .user.login == "github-actions[bot]"
                and .base.ref == "main"
                and .base.sha == $base_sha
                and .head.repo.full_name == $repository
                and .head.ref == $head_ref
                and .head.sha == $head_sha
                and ([.labels[].name] | index("autorelease: pending") != null)
              ' <<<"$confirmed" >/dev/null
            then
              jq -n \
                --argjson number "$pr_number" \
                --arg base_sha "$expected_base_sha" \
                --arg head_sha "$head_sha" \
                --arg head_ref "$head_ref" \
                '{number: $number, base_sha: $base_sha, head_sha: $head_sha, head_ref: $head_ref}'
              exit 0
            fi
            last_observed="PR changed during confirmation: base=${confirmed_base:-missing} head=${confirmed_head:-missing}"
          else
            last_observed="confirmation API unavailable"
          fi
        else
          last_observed="head=${head_sha} parent=${parent_sha:-missing}"
        fi
      else
        last_observed="commit ${head_sha} is not readable yet"
      fi
    fi
  fi

  if [ "$attempt" -lt "$attempts" ]; then
    sleep "$delay_seconds"
  fi
done

echo "release PR #${pr_number} did not stabilize on main ${expected_base_sha} after ${attempts} attempts; last observed ${last_observed}" >&2
exit 1
