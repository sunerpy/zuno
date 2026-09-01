#!/usr/bin/env bash
set -euo pipefail

# Ubuntu 24.04 restricts unprivileged user namespaces through AppArmor. Merely
# installing bubblewrap leaves it in the generic unprivileged_userns profile,
# which cannot create the UID map or network namespace Zuno requires. Load the
# distribution-maintained, path-bound profile documented in docs/faq.md instead
# of weakening the host-wide user-namespace policy.
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  apparmor \
  apparmor-profiles \
  bubblewrap

profile_source=/usr/share/apparmor/extra-profiles/bwrap-userns-restrict
profile_target=/etc/apparmor.d/bwrap-userns-restrict
if [[ ! -r "$profile_source" ]]; then
  echo "::error title=Sandbox setup::Ubuntu did not provide ${profile_source}"
  exit 1
fi

sudo install -o root -g root -m 0644 "$profile_source" "$profile_target"
sudo /usr/sbin/apparmor_parser -r "$profile_target"

/usr/bin/bwrap --version

# Prove both deployment capabilities before spending time compiling tests or a
# release artifact. Zuno intentionally fails closed if either probe is absent.
/usr/bin/bwrap \
  --unshare-user --uid 0 --gid 0 \
  --unshare-pid --unshare-uts --unshare-ipc \
  --ro-bind / / \
  -- /usr/bin/true

/usr/bin/bwrap \
  --unshare-user --uid 0 --gid 0 \
  --unshare-net \
  --ro-bind / / \
  -- /usr/bin/true
