#!/usr/bin/env bash
# Prepare a signed UI bundle with the existing approved publisher key.
set -euo pipefail
if (( $# != 5 )); then
  echo 'usage: package-ui.sh WEB_EXTENSION_TOOL OUTPUT SEQUENCE EXPIRES_UNIX SECRET_HEX_FILE' >&2
  exit 2
fi
repository=$(cd -- "$(dirname -- "$0")/.." && pwd)
if [[ ! -f "$repository/ui/dist/remote-session.js" || ! -f "$repository/ui/ui.json" ]]; then
  echo 'Build the Remote Monitor and Input UI before packaging.' >&2
  exit 2
fi
"$1" pack "$repository/ui/dist" "$2" remote-session 'Remote Monitor and Input' remote-session.js "$3" "$4" "$5" "$repository/ui/ui.json"
