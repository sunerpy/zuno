#!/bin/sh
#
# Reproduce or update Zuno's audited Agent Client Protocol snapshot.
#
# Modes:
#   --verify          Offline verification of checked-in files (default).
#   --check-upstream  Re-download the pinned upstream inputs and compare them.
#   --refresh         Rebuild the snapshot after all upstream checks succeed.
#
# Optional --refresh pin overrides:
#   ACP_STABLE_TAG    Stable schema release tag.
#   ACP_CRATE_TAG     Rust schema crate release tag.
#   ACP_PREVIEW_TAG   Preview schema release tag.
#   ZED_COMMIT        Exact Zed commit used for the integration observation.
#
# GITHUB_TOKEN may be set to raise GitHub API rate limits. It is never printed.
set -eu

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
snapshot_dir="$repo_root/docs/upstream/acp"
manifest="$snapshot_dir/manifest.json"

acp_repo="agentclientprotocol/agent-client-protocol"
acp_repo_url="https://github.com/$acp_repo"
acp_docs_url="https://agentclientprotocol.com/protocol/v1/overview"
zed_repo="zed-industries/zed"
zed_repo_url="https://github.com/$zed_repo"
zed_docs_url="https://zed.dev/docs/ai/external-agents"
zed_source_path="crates/agent_servers/src/acp.rs"

initial_stable_tag="schema-v1.21.0"
initial_crate_tag="v1.7.0"
initial_preview_tag="schema-v2.0.0-alpha.3"
initial_zed_commit="ac099b4a809a564f06907125e7a536c33cb60084"

expected_snapshot_files='
LICENSE
README.md
SHA256SUMS
assets/stable/meta.json
assets/stable/meta.unstable.json
assets/stable/schema.json
assets/stable/schema.unstable.json
assets/v2-preview/meta.json
assets/v2-preview/meta.unstable.json
assets/v2-preview/schema.json
assets/v2-preview/schema.unstable.json
manifest.json
'

checksum_paths='
LICENSE
assets/stable/meta.json
assets/stable/meta.unstable.json
assets/stable/schema.json
assets/stable/schema.unstable.json
assets/v2-preview/meta.json
assets/v2-preview/meta.unstable.json
assets/v2-preview/schema.json
assets/v2-preview/schema.unstable.json
'

fail() {
  printf 'error: %s\n' "$1" >&2
  exit 1
}

say() {
  printf '%s\n' "$1" >&2
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "$1 is required"
}

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    fail "sha256sum or shasum is required"
  fi
}

validate_tag() {
  case "$1" in
    '' | *[!A-Za-z0-9._-]*) fail "invalid release tag: $1" ;;
  esac
}

validate_commit() {
  value=$1
  [ "${#value}" -eq 40 ] || fail "commit must contain exactly 40 hexadecimal characters: $value"
  case "$value" in
    *[!0-9a-f]*) fail "commit must be lowercase hexadecimal: $value" ;;
  esac
}

api_get() {
  path=$1
  output=$2
  url="https://api.github.com/$path"
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl --proto '=https' --tlsv1.2 -fsSL \
      -H "Accept: application/vnd.github+json" \
      -H "Authorization: Bearer $GITHUB_TOKEN" \
      -H "X-GitHub-Api-Version: 2022-11-28" \
      -H "User-Agent: zuno-acp-spec-updater" \
      "$url" -o "$output"
  else
    curl --proto '=https' --tlsv1.2 -fsSL \
      -H "Accept: application/vnd.github+json" \
      -H "X-GitHub-Api-Version: 2022-11-28" \
      -H "User-Agent: zuno-acp-spec-updater" \
      "$url" -o "$output"
  fi
}

download() {
  url=$1
  output=$2
  case "$url" in
    https://github.com/* | https://raw.githubusercontent.com/*) ;;
    *) fail "refusing unexpected download host: $url" ;;
  esac
  if [ -n "${GITHUB_TOKEN:-}" ]; then
    curl --proto '=https' --tlsv1.2 -fsSL \
      -H "Authorization: Bearer $GITHUB_TOKEN" \
      -H "User-Agent: zuno-acp-spec-updater" \
      "$url" -o "$output"
  else
    curl --proto '=https' --tlsv1.2 -fsSL \
      -H "User-Agent: zuno-acp-spec-updater" \
      "$url" -o "$output"
  fi
}

manifest_value() {
  jq -er "$1" "$manifest"
}

resolve_tag() {
  tag=$1
  key=$2
  ref_json="$work_dir/$key.ref.json"
  tag_json="$work_dir/$key.tag.json"

  api_get "repos/$acp_repo/git/ref/tags/$tag" "$ref_json"
  jq -e --arg tag "$tag" '
    .ref == ("refs/tags/" + $tag)
    and .object.type == "tag"
    and (.object.sha | test("^[0-9a-f]{40}$"))
  ' "$ref_json" >/dev/null || fail "$tag is not an annotated tag with a valid tag object"

  tag_object=$(jq -er '.object.sha' "$ref_json")
  api_get "repos/$acp_repo/git/tags/$tag_object" "$tag_json"
  jq -e --arg tag "$tag" --arg object "$tag_object" '
    .tag == $tag
    and .sha == $object
    and .object.type == "commit"
    and (.object.sha | test("^[0-9a-f]{40}$"))
  ' "$tag_json" >/dev/null || fail "$tag did not peel to one immutable commit"

  printf '%s\t%s\n' "$tag_object" "$(jq -er '.object.sha' "$tag_json")"
}

write_release_assets() {
  tag=$1
  channel=$2
  release_json="$work_dir/$channel.release.json"
  tsv="$work_dir/$channel.assets.tsv"
  : >"$tsv"

  api_get "repos/$acp_repo/releases/tags/$tag" "$release_json"
  jq -e --arg tag "$tag" '
    .tag_name == $tag and .draft == false and (.assets | type == "array")
  ' "$release_json" >/dev/null || fail "release metadata did not match $tag"

  for name in meta.json meta.unstable.json schema.json schema.unstable.json; do
    count=$(jq --arg name "$name" '[.assets[] | select(.name == $name)] | length' "$release_json")
    [ "$count" -eq 1 ] || fail "$tag must publish exactly one $name asset"

    digest=$(jq -er --arg name "$name" '.assets[] | select(.name == $name) | .digest' "$release_json")
    case "$digest" in
      sha256:[0-9a-f][0-9a-f]*) ;;
      *) fail "$tag/$name has no SHA-256 release digest" ;;
    esac
    expected=${digest#sha256:}
    [ "${#expected}" -eq 64 ] || fail "$tag/$name has a malformed SHA-256 digest"
    case "$expected" in
      *[!0-9a-f]*) fail "$tag/$name has a malformed SHA-256 digest" ;;
    esac

    url=$(jq -er --arg name "$name" '.assets[] | select(.name == $name) | .browser_download_url' "$release_json")
    expected_url="https://github.com/$acp_repo/releases/download/$tag/$name"
    [ "$url" = "$expected_url" ] || fail "unexpected release URL for $tag/$name: $url"

    destination="$stage_dir/assets/$channel/$name"
    mkdir -p "$(dirname "$destination")"
    download "$url" "$destination"
    actual=$(sha256_file "$destination")
    [ "$actual" = "$expected" ] \
      || fail "checksum mismatch for $tag/$name: expected $expected, got $actual"

    size=$(wc -c <"$destination" | tr -d '[:space:]')
    api_size=$(jq -er --arg name "$name" '.assets[] | select(.name == $name) | .size' "$release_json")
    [ "$size" = "$api_size" ] || fail "size mismatch for $tag/$name: expected $api_size, got $size"
    asset_id=$(jq -er --arg name "$name" '.assets[] | select(.name == $name) | .id' "$release_json")
    snapshot_path="assets/$channel/$name"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$name" "$snapshot_path" "$actual" "$size" "$asset_id" "$url" >>"$tsv"
  done
}

assets_json() {
  jq -Rn '
    [
      inputs
      | select(length > 0)
      | split("\t")
      | {
          name: .[0],
          snapshotPath: .[1],
          sha256: .[2],
          size: (.[3] | tonumber),
          releaseAssetId: (.[4] | tonumber),
          url: .[5]
        }
    ]
  ' <"$1" >"$2"
}

prepare_stage() {
  stable_tag=$1
  crate_tag=$2
  preview_tag=$3
  zed_commit=$4

  validate_tag "$stable_tag"
  validate_tag "$crate_tag"
  validate_tag "$preview_tag"
  validate_commit "$zed_commit"

  stage_dir="$work_dir/stage"
  mkdir -p "$stage_dir/assets/stable" "$stage_dir/assets/v2-preview"

  IFS='	' read -r stable_tag_object stable_commit <<EOF
$(resolve_tag "$stable_tag" stable)
EOF
  IFS='	' read -r crate_tag_object crate_commit <<EOF
$(resolve_tag "$crate_tag" crate)
EOF
  IFS='	' read -r preview_tag_object preview_commit <<EOF
$(resolve_tag "$preview_tag" preview)
EOF

  crate_release_json="$work_dir/crate.release.json"
  api_get "repos/$acp_repo/releases/tags/$crate_tag" "$crate_release_json"
  jq -e --arg tag "$crate_tag" '
    .tag_name == $tag and .draft == false and .prerelease == false
  ' "$crate_release_json" >/dev/null || fail "crate release metadata did not match $crate_tag"

  write_release_assets "$stable_tag" stable
  write_release_assets "$preview_tag" v2-preview

  license_url="https://raw.githubusercontent.com/$acp_repo/$stable_commit/LICENSE"
  download "$license_url" "$stage_dir/LICENSE"
  grep -Fq "Apache License" "$stage_dir/LICENSE" \
    || fail "upstream LICENSE does not identify the Apache License"
  grep -Fq "Version 2.0, January 2004" "$stage_dir/LICENSE" \
    || fail "upstream LICENSE is not Apache License 2.0"
  license_sha=$(sha256_file "$stage_dir/LICENSE")

  acp_main_json="$work_dir/acp.main.json"
  api_get "repos/$acp_repo/commits/main" "$acp_main_json"
  acp_main_commit=$(jq -er '.sha' "$acp_main_json")
  validate_commit "$acp_main_commit"

  zed_commit_json="$work_dir/zed.commit.json"
  api_get "repos/$zed_repo/commits/$zed_commit" "$zed_commit_json"
  jq -e --arg commit "$zed_commit" '.sha == $commit' "$zed_commit_json" >/dev/null \
    || fail "Zed commit endpoint did not return $zed_commit"

  zed_main_json="$work_dir/zed.main.json"
  api_get "repos/$zed_repo/commits/main" "$zed_main_json"
  zed_main_commit=$(jq -er '.sha' "$zed_main_json")
  validate_commit "$zed_main_commit"

  zed_raw="$work_dir/zed-acp.rs"
  zed_raw_url="https://raw.githubusercontent.com/$zed_repo/$zed_commit/$zed_source_path"
  download "$zed_raw_url" "$zed_raw"
  grep -Fq "const MINIMUM_SUPPORTED_VERSION: ProtocolVersion = ProtocolVersion::V1;" "$zed_raw" \
    || fail "Zed no longer declares ProtocolVersion::V1 as its minimum supported ACP version"
  zed_protocol_line=$(
    awk 'index($0, "acp::InitializeRequest::new(ProtocolVersion::V1)") { print NR; exit }' "$zed_raw"
  )
  [ -n "$zed_protocol_line" ] \
    || fail "Zed no longer initializes the ACP connection with ProtocolVersion::V1"

  assets_json "$work_dir/stable.assets.tsv" "$work_dir/stable.assets.json"
  assets_json "$work_dir/v2-preview.assets.tsv" "$work_dir/preview.assets.json"

  fetched_at=$(date -u '+%Y-%m-%dT%H:%M:%SZ')
  jq -n \
    --arg fetchedAt "$fetched_at" \
    --arg acpRepo "$acp_repo_url" \
    --arg acpDocs "$acp_docs_url" \
    --arg zedDocs "$zed_docs_url" \
    --arg acpMain "$acp_main_commit" \
    --arg licenseUrl "$license_url" \
    --arg licenseSha "$license_sha" \
    --arg crateTag "$crate_tag" \
    --arg crateTagObject "$crate_tag_object" \
    --arg crateCommit "$crate_commit" \
    --arg crateReleaseUrl "$acp_repo_url/releases/tag/$crate_tag" \
    --arg stableTag "$stable_tag" \
    --arg stableTagObject "$stable_tag_object" \
    --arg stableCommit "$stable_commit" \
    --arg stableReleaseUrl "$acp_repo_url/releases/tag/$stable_tag" \
    --arg previewTag "$preview_tag" \
    --arg previewTagObject "$preview_tag_object" \
    --arg previewCommit "$preview_commit" \
    --arg previewReleaseUrl "$acp_repo_url/releases/tag/$preview_tag" \
    --arg zedRepo "$zed_repo_url" \
    --arg zedCommit "$zed_commit" \
    --arg zedMain "$zed_main_commit" \
    --arg zedSourcePath "$zed_source_path" \
    --argjson zedSourceLine "$zed_protocol_line" \
    --arg zedSourceUrl "$zed_repo_url/blob/$zed_commit/$zed_source_path#L$zed_protocol_line" \
    --slurpfile stableAssets "$work_dir/stable.assets.json" \
    --slurpfile previewAssets "$work_dir/preview.assets.json" \
    '{
      formatVersion: 1,
      fetchedAt: $fetchedAt,
      sources: {
        agentClientProtocolRepository: $acpRepo,
        protocolV1Documentation: $acpDocs,
        zedExternalAgentsDocumentation: $zedDocs
      },
      license: {
        spdx: "Apache-2.0",
        snapshotPath: "LICENSE",
        sourceUrl: $licenseUrl,
        sha256: $licenseSha
      },
      acp: {
        observedMainCommit: $acpMain,
        crate: {
          tag: $crateTag,
          tagObject: $crateTagObject,
          commit: $crateCommit,
          releaseUrl: $crateReleaseUrl
        },
        stableSchema: {
          tag: $stableTag,
          tagObject: $stableTagObject,
          commit: $stableCommit,
          releaseUrl: $stableReleaseUrl,
          assets: $stableAssets[0]
        },
        v2PreviewSchema: {
          tag: $previewTag,
          tagObject: $previewTagObject,
          commit: $previewCommit,
          releaseUrl: $previewReleaseUrl,
          assets: $previewAssets[0]
        }
      },
      zed: {
        repository: $zedRepo,
        commit: $zedCommit,
        observedMainCommit: $zedMain,
        sourcePath: $zedSourcePath,
        requestedProtocolVersion: "V1",
        sourceLine: $zedSourceLine,
        sourceUrl: $zedSourceUrl,
        licenseBoundary: "Reference metadata only. No Zed GPL source is copied into this snapshot."
      }
    }' >"$stage_dir/manifest.json"

  : >"$stage_dir/SHA256SUMS"
  printf '%s\n' "$checksum_paths" | while IFS= read -r relative; do
    [ -n "$relative" ] || continue
    printf '%s  %s\n' "$(sha256_file "$stage_dir/$relative")" "$relative"
  done >>"$stage_dir/SHA256SUMS"
}

expected_paths_sorted() {
  printf '%s\n' "$1" | sed '/^$/d' | LC_ALL=C sort
}

verify_snapshot_at() {
  root=$1
  [ -f "$root/manifest.json" ] || fail "missing $root/manifest.json"
  [ -f "$root/SHA256SUMS" ] || fail "missing $root/SHA256SUMS"

  actual_files=$(
    cd "$root"
    find . -type f -print | sed 's#^\./##' | LC_ALL=C sort
  )
  expected_files=$(expected_paths_sorted "$expected_snapshot_files")
  [ "$actual_files" = "$expected_files" ] || {
    printf 'expected files:\n%s\nactual files:\n%s\n' "$expected_files" "$actual_files" >&2
    fail "snapshot contains missing or unexpected files"
  }

  jq -e '
    .formatVersion == 1
    and .license.spdx == "Apache-2.0"
    and (.license.sha256 | test("^[0-9a-f]{64}$"))
    and .zed.requestedProtocolVersion == "V1"
    and (.zed.licenseBoundary | contains("No Zed GPL source is copied"))
    and (.acp.crate.tagObject | test("^[0-9a-f]{40}$"))
    and (.acp.crate.commit | test("^[0-9a-f]{40}$"))
    and (.acp.stableSchema.assets | length == 4)
    and (.acp.v2PreviewSchema.assets | length == 4)
  ' "$root/manifest.json" >/dev/null || fail "manifest.json failed structural validation"

  awk '
    NF != 2 { exit 1 }
    length($1) != 64 || $1 !~ /^[0-9a-f]+$/ { exit 1 }
    $2 !~ /^(LICENSE|assets\/(stable|v2-preview)\/(meta|meta\.unstable|schema|schema\.unstable)\.json)$/ { exit 1 }
    seen[$2]++ { exit 1 }
    END { if (NR != 9) exit 1 }
  ' "$root/SHA256SUMS" || fail "SHA256SUMS is malformed or incomplete"

  actual_checksum_paths=$(awk '{print $2}' "$root/SHA256SUMS" | LC_ALL=C sort)
  expected_checksum_paths=$(expected_paths_sorted "$checksum_paths")
  [ "$actual_checksum_paths" = "$expected_checksum_paths" ] \
    || fail "SHA256SUMS does not cover the exact audited file set"

  printf '%s\n' "$checksum_paths" | while IFS= read -r relative; do
    [ -n "$relative" ] || continue
    expected=$(awk -v path="$relative" '$2 == path { print $1 }' "$root/SHA256SUMS")
    actual=$(sha256_file "$root/$relative")
    [ "$actual" = "$expected" ] \
      || fail "checksum mismatch for $relative: expected $expected, got $actual"

    if [ "$relative" = "LICENSE" ]; then
      manifest_sha=$(jq -er '.license.sha256' "$root/manifest.json")
    else
      manifest_sha=$(
        jq -er --arg path "$relative" '
          [
            .acp.stableSchema.assets[],
            .acp.v2PreviewSchema.assets[]
          ]
          | map(select(.snapshotPath == $path))
          | if length == 1 then .[0].sha256 else error("missing or duplicate asset") end
        ' "$root/manifest.json"
      )
    fi
    [ "$manifest_sha" = "$actual" ] \
      || fail "manifest checksum mismatch for $relative: expected $manifest_sha, got $actual"
  done

  grep -Fq "Apache License" "$root/LICENSE" || fail "LICENSE is not Apache-2.0 text"
  grep -Fq "Version 2.0, January 2004" "$root/LICENSE" || fail "LICENSE is not Apache-2.0 text"
}

canonical_manifest() {
  jq -S '
    del(.fetchedAt)
    | del(.acp.observedMainCommit)
    | del(.zed.observedMainCommit)
  ' "$1"
}

load_refresh_pins() {
  if [ -f "$manifest" ]; then
    base_stable=$(manifest_value '.acp.stableSchema.tag')
    base_crate=$(manifest_value '.acp.crate.tag')
    base_preview=$(manifest_value '.acp.v2PreviewSchema.tag')
    base_zed=$(manifest_value '.zed.commit')
  else
    base_stable=$initial_stable_tag
    base_crate=$initial_crate_tag
    base_preview=$initial_preview_tag
    base_zed=$initial_zed_commit
  fi

  refresh_stable=${ACP_STABLE_TAG:-$base_stable}
  refresh_crate=${ACP_CRATE_TAG:-$base_crate}
  refresh_preview=${ACP_PREVIEW_TAG:-$base_preview}
  refresh_zed=${ZED_COMMIT:-$base_zed}
}

copy_stage_into_snapshot() {
  mkdir -p "$snapshot_dir/assets/stable" "$snapshot_dir/assets/v2-preview"
  cp "$stage_dir/LICENSE" "$snapshot_dir/LICENSE"
  cp "$stage_dir/SHA256SUMS" "$snapshot_dir/SHA256SUMS"
  for name in meta.json meta.unstable.json schema.json schema.unstable.json; do
    cp "$stage_dir/assets/stable/$name" "$snapshot_dir/assets/stable/$name"
    cp "$stage_dir/assets/v2-preview/$name" "$snapshot_dir/assets/v2-preview/$name"
  done
  cp "$stage_dir/manifest.json" "$snapshot_dir/manifest.json"
}

mode=${1:---verify}
[ "$#" -le 1 ] || fail "usage: $0 [--verify|--check-upstream|--refresh]"

need jq
need curl
need awk
need sed
need sort

case "$mode" in
  --verify)
    verify_snapshot_at "$snapshot_dir"
    say "ACP snapshot verification passed."
    ;;
  --check-upstream)
    [ -z "${ACP_STABLE_TAG:-}${ACP_CRATE_TAG:-}${ACP_PREVIEW_TAG:-}${ZED_COMMIT:-}" ] \
      || fail "pin overrides are only accepted with --refresh"
    verify_snapshot_at "$snapshot_dir"
    work_dir=$(mktemp -d 2>/dev/null || mktemp -d -t zuno-acp-spec)
    trap 'rm -rf "$work_dir"' EXIT INT TERM
    prepare_stage \
      "$(manifest_value '.acp.stableSchema.tag')" \
      "$(manifest_value '.acp.crate.tag')" \
      "$(manifest_value '.acp.v2PreviewSchema.tag')" \
      "$(manifest_value '.zed.commit')"
    for relative in $(printf '%s\n' "$checksum_paths" | sed '/^$/d'); do
      cmp -s "$snapshot_dir/$relative" "$stage_dir/$relative" \
        || fail "checked-in $relative differs from the pinned upstream content"
    done
    current_canonical="$work_dir/current.canonical.json"
    stage_canonical="$work_dir/stage.canonical.json"
    canonical_manifest "$manifest" >"$current_canonical"
    canonical_manifest "$stage_dir/manifest.json" >"$stage_canonical"
    cmp -s "$current_canonical" "$stage_canonical" \
      || fail "checked-in manifest metadata differs from the pinned upstream state"
    say "ACP upstream comparison passed."
    ;;
  --refresh)
    work_dir=$(mktemp -d 2>/dev/null || mktemp -d -t zuno-acp-spec)
    trap 'rm -rf "$work_dir"' EXIT INT TERM
    load_refresh_pins
    prepare_stage "$refresh_stable" "$refresh_crate" "$refresh_preview" "$refresh_zed"
    copy_stage_into_snapshot
    verify_snapshot_at "$snapshot_dir"
    say "ACP snapshot refreshed and verified."
    ;;
  *)
    fail "usage: $0 [--verify|--check-upstream|--refresh]"
    ;;
esac
