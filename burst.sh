#!/usr/bin/env bash
# Reproduce the on-sale stampede against a running service and check it
# stayed correct. Exits non-zero if any check fails.
#
#   ./burst.sh http://localhost:8080
#   ./burst.sh https://<live-url> --phases 2000:30 --seed 42
#   ./burst.sh http://localhost:8080 --help
#
# The service needs AUTH_TOKEN_ROUTE_ENABLED=true, and nothing else should
# be using it during the run.
set -euo pipefail
cd "$(dirname "$0")"
exec cargo run --release --quiet --bin burst -- "$@"
