#!/usr/bin/env bash
# Resolve a release-please PR only after GitHub's PR and commit APIs agree that
# its head is a stable child of the main commit that started this controller.
#
# release-please does not always rewrite an existing release PR after a
# non-releasable main commit. When the trusted release head remains a single
# bot-authored child of an older main commit, replay that one commit onto the
# triggering main SHA and update the same branch with an exact force-with-lease.
set -euo pipefail

repository=${GITHUB_REPOSITORY:?GITHUB_REPOSITORY is required}
pr_number=${RELEASE_PR_NUMBER:?RELEASE_PR_NUMBER is required}
expected_base_sha=${EXPECTED_BASE_SHA:?EXPECTED_BASE_SHA is required}
attempts=${RELEASE_PR_RESOLVE_ATTEMPTS:-30}
delay_seconds=${RELEASE_PR_RESOLVE_DELAY_SECONDS:-2}
refresh_enabled=${RELEASE_PR_REFRESH_ENABLED:-1}
refresh_observations=${RELEASE_PR_REFRESH_OBSERVATIONS:-3}
gh_bin=${GH_BIN:-gh}
git_bin=${GIT_BIN:-git}
git_remote=${GIT_REMOTE:-origin}

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
if [[ "$refresh_enabled" != 0 && "$refresh_enabled" != 1 ]]; then
  echo "release PR refresh flag must be 0 or 1: $refresh_enabled" >&2
  exit 2
fi
if ! [[ "$refresh_observations" =~ ^[1-9][0-9]*$ ]]; then
  echo "release PR refresh observations must be positive: $refresh_observations" >&2
  exit 2
fi

refresh_release_head() (
  set -euo pipefail

  old_head=$1
  old_parent=$2
  head_ref=$3
  subject=$4

  if [ -z "${GH_TOKEN:-}" ]; then
    echo "GH_TOKEN is required to refresh a stale release PR" >&2
    exit 1
  fi
  if ! "$git_bin" check-ref-format "refs/heads/${head_ref}" >/dev/null; then
    echo "release PR head ref is not a valid Git branch: $head_ref" >&2
    exit 1
  fi

  repository_root=$("$git_bin" rev-parse --show-toplevel)
  if ! "$git_bin" -C "$repository_root" cat-file -e "${expected_base_sha}^{commit}" 2>/dev/null; then
    echo "triggering main commit is not available locally: $expected_base_sha" >&2
    exit 1
  fi

  temporary_root=$(mktemp -d)
  replay_root="${temporary_root}/replay"
  refresh_ref="refs/zuno-release-refresh/pr-${pr_number}"
  askpass="${temporary_root}/git-askpass"

  cleanup() {
    "$git_bin" -C "$repository_root" worktree remove --force "$replay_root" >/dev/null 2>&1 || true
    "$git_bin" -C "$repository_root" update-ref -d "$refresh_ref" >/dev/null 2>&1 || true
    rm -rf "$temporary_root"
  }
  trap cleanup EXIT

  umask 077
  cat >"$askpass" <<'ASKPASS'
#!/usr/bin/env bash
case "${1:-}" in
  *Username*) printf '%s\n' 'x-access-token' ;;
  *Password*) printf '%s\n' "${GH_TOKEN:?GH_TOKEN is required}" ;;
  *) exit 1 ;;
esac
ASKPASS
  chmod 700 "$askpass"
  export GIT_ASKPASS=$askpass
  export GIT_TERMINAL_PROMPT=0

  "$git_bin" -C "$repository_root" fetch --no-tags "$git_remote" \
    "+refs/heads/${head_ref}:${refresh_ref}"
  fetched_head=$("$git_bin" -C "$repository_root" rev-parse "$refresh_ref")
  if [ "$fetched_head" != "$old_head" ]; then
    echo "release PR head changed before refresh: API=${old_head} remote=${fetched_head}" >&2
    exit 2
  fi

  local_parents=$("$git_bin" -C "$repository_root" show -s --format=%P "$old_head")
  if [[ "$local_parents" == *" "* ]] || [ "$local_parents" != "$old_parent" ]; then
    echo "fetched release head does not match its trusted single parent" >&2
    exit 1
  fi
  local_author_email=$("$git_bin" -C "$repository_root" show -s --format=%ae "$old_head")
  local_subject=$("$git_bin" -C "$repository_root" show -s --format=%s "$old_head")
  if [ "$local_author_email" != '41898282+github-actions[bot]@users.noreply.github.com' ]; then
    echo "fetched release head is not authored by github-actions[bot]" >&2
    exit 1
  fi
  if [ "$local_subject" != "$subject" ] || [[ "$local_subject" != "chore: release"* ]]; then
    echo "fetched release head subject does not match the trusted API response" >&2
    exit 1
  fi
  if ! "$git_bin" -C "$repository_root" merge-base --is-ancestor \
    "$old_parent" "$expected_base_sha"
  then
    echo "stale release parent is not an ancestor of triggering main" >&2
    exit 1
  fi

  "$git_bin" -C "$repository_root" worktree add --quiet --detach \
    "$replay_root" "$expected_base_sha"
  "$git_bin" -C "$replay_root" \
    -c user.name='github-actions[bot]' \
    -c user.email='41898282+github-actions[bot]@users.noreply.github.com' \
    -c commit.gpgSign=false \
    cherry-pick "$old_head" >&2

  new_head=$("$git_bin" -C "$replay_root" rev-parse HEAD)
  new_parent=$("$git_bin" -C "$replay_root" show -s --format=%P HEAD)
  new_author_email=$("$git_bin" -C "$replay_root" show -s --format=%ae HEAD)
  new_subject=$("$git_bin" -C "$replay_root" show -s --format=%s HEAD)
  if [ "$new_parent" != "$expected_base_sha" ] \
    || [ "$new_author_email" != '41898282+github-actions[bot]@users.noreply.github.com' ] \
    || [ "$new_subject" != "$subject" ]
  then
    echo "replayed release head failed its parent, author, or subject invariant" >&2
    exit 1
  fi

  "$git_bin" -C "$replay_root" push \
    --force-with-lease="refs/heads/${head_ref}:${old_head}" \
    "$git_remote" "HEAD:refs/heads/${head_ref}"
  echo "refreshed release PR #${pr_number}: ${old_head} -> ${new_head}" >&2
)

last_observed=unavailable
stale_identity=
stale_observations=0
refreshed=false
previous_base_sha=
previous_head_sha=
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
    if [[ "$head_sha" =~ ^[0-9a-f]{40}$ ]]; then
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

        if [ "$base_sha" = "$expected_base_sha" ] && [ "$parent_sha" = "$expected_base_sha" ]; then
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
                --argjson refreshed "$refreshed" \
                --arg previous_base_sha "$previous_base_sha" \
                --arg previous_head_sha "$previous_head_sha" \
                '{
                  number: $number,
                  base_sha: $base_sha,
                  head_sha: $head_sha,
                  head_ref: $head_ref,
                  refreshed: $refreshed,
                  previous_base_sha: (
                    if $previous_base_sha == "" then null else $previous_base_sha end
                  ),
                  previous_head_sha: (
                    if $previous_head_sha == "" then null else $previous_head_sha end
                  )
                }'
              exit 0
            fi
            last_observed="PR changed during confirmation: base=${confirmed_base:-missing} head=${confirmed_head:-missing}"
          else
            last_observed="confirmation API unavailable"
          fi
        elif [ "$refresh_enabled" = 1 ] \
          && [ "$refreshed" = false ] \
          && [[ "$parent_sha" =~ ^[0-9a-f]{40}$ ]] \
          && [ "$parent_sha" != "$expected_base_sha" ] \
          && { [ "$base_sha" = "$expected_base_sha" ] || [ "$base_sha" = "$parent_sha" ]; }
        then
          observed_identity="${base_sha}:${head_sha}:${parent_sha}:${head_ref}"
          if [ "$observed_identity" = "$stale_identity" ]; then
            stale_observations=$((stale_observations + 1))
          else
            stale_identity=$observed_identity
            stale_observations=1
          fi
          last_observed="stale trusted head=${head_sha} parent=${parent_sha} observation=${stale_observations}/${refresh_observations}"
          if [ "$stale_observations" -ge "$refresh_observations" ]; then
            if refresh_release_head "$head_sha" "$parent_sha" "$head_ref" "$subject"; then
              refreshed=true
              previous_base_sha=$parent_sha
              previous_head_sha=$head_sha
              stale_identity=
              stale_observations=0
              last_observed="refreshed stale head ${head_sha}; waiting for GitHub confirmation"
            else
              refresh_status=$?
              if [ "$refresh_status" -ne 2 ]; then
                exit "$refresh_status"
              fi
              last_observed="release head changed while preparing refresh; retrying API confirmation"
            fi
          fi
        else
          stale_identity=
          stale_observations=0
          last_observed="head=${head_sha} parent=${parent_sha:-missing} base=${base_sha:-missing}"
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
