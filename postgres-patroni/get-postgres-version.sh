#!/bin/bash
# get-postgres-version.sh
# Usage: ./get-postgres-version.sh 16
# Returns: 16.10 (latest minor version for PostgreSQL 16)

set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: $0 <major_version>" >&2
    echo "Example: $0 16" >&2
    exit 1
fi

MAJOR_VERSION=$1

# Validate input is a number
if ! [[ "$MAJOR_VERSION" =~ ^[0-9]+$ ]]; then
    echo "Error: Major version must be a number" >&2
    exit 1
fi

echo "Fetching latest PostgreSQL $MAJOR_VERSION version..." >&2

# Resolve via the Docker Registry v2 API (registry-1.docker.io, anonymous
# pull token) rather than the Hub website API (hub.docker.com): the website
# API hard-blocks shared GitHub runner IPs with persistent 403s, while the
# registry endpoint is the same path every `docker pull` takes and is
# expected to serve CI runners.
# --retry-all-errors: plain --retry does not retry 403/429; -f makes HTTP
# errors exit nonzero instead of silently yielding an empty tag list.
TOKEN=$(curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 \
  "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/postgres:pull" \
  | jq -r .token)
if [ -z "$TOKEN" ] || [ "$TOKEN" = "null" ]; then
  echo "Error: Could not obtain an anonymous pull token" >&2
  exit 1
fi

# Unlike the Hub website API, the registry API guarantees no tag ordering,
# so collect the complete tag list first, then pick the newest matching
# minor. RFC-5988 pagination: a Link rel="next" header carries the relative
# URL of the next page; no header means last page.
TAGS_FILE=$(mktemp)
HEADERS_FILE=$(mktemp)
trap 'rm -f "$TAGS_FILE" "$HEADERS_FILE"' EXIT

URL="https://registry-1.docker.io/v2/library/postgres/tags/list?n=1000"
PAGES=0
while [ -n "$URL" ]; do
  if [ "$PAGES" -ge 50 ]; then
    echo "Error: More than 50 pages of registry tags; refusing to loop forever" >&2
    exit 1
  fi
  RESPONSE=$(curl -fsSL --retry 5 --retry-all-errors --retry-delay 5 \
    -H "Authorization: Bearer $TOKEN" -D "$HEADERS_FILE" "$URL")
  echo "$RESPONSE" | jq -r '.tags[]' >> "$TAGS_FILE"
  NEXT=$(sed -nE 's/^[Ll]ink: *<([^>]+)>; *rel="next".*/\1/p' "$HEADERS_FILE" | tr -d '\r')
  if [ -n "$NEXT" ]; then
    URL="https://registry-1.docker.io${NEXT}"
  else
    URL=""
  fi
  PAGES=$((PAGES + 1))
done

# We look for tags that are major.minor format (no alpine, bookworm, etc)
LATEST_VERSION=$(grep -E "^${MAJOR_VERSION}\.[0-9]+$" "$TAGS_FILE" | sort -V | tail -1 || true)

if [ -z "$LATEST_VERSION" ]; then
  echo "Error: Could not find version for PostgreSQL $MAJOR_VERSION" >&2
  echo "Available major versions might be different. Check https://hub.docker.com/_/postgres" >&2
  exit 1
fi

echo "Found latest version: $LATEST_VERSION" >&2
echo "$LATEST_VERSION"
