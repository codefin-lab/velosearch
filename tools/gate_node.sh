#!/bin/sh
# The node the gates are run against, with everything the suites expect.
#
# The corpus asks a node for things that are arranged outside it: an
# attribute the cat and settings tests read back, the geoip databases and the
# phonetic rule files (neither vendored -- see docs/geoip.md and
# docs/phonetic.md), where a URL repository may be read from, and which
# clusters a reindex may read from. A node
# started without these fails sections that have nothing wrong with them, so
# this is the one way to start it.
#
# One suite is written against a cluster with no ingest node, which is a
# different cluster rather than a different request -- VELO_ROLES starts one.
#
# The transport port follows the http one (+100) unless VELO_TRANSPORT says
# otherwise: every gate node used to take 9300, so two of them could not run
# at once and the second died with "Address already in use".
#
#   tools/gate_node.sh            starts it on 9213
#   VELO_PORT=9214 tools/gate_node.sh
#   VELO_PORT=9214 VELO_DATA=/tmp/velo-noingest \
#     VELO_ROLES=data,cluster_manager,remote_cluster_client tools/gate_node.sh
set -e
PORT=${VELO_PORT:-9213}
TRANSPORT_PORT=${VELO_TRANSPORT:-$((PORT + 100))}
# A node left over from another session holding this port used to be
# indistinguishable from the one this script starts: `exec` failed with
# "address already in use", the script died where nothing was reading its
# output, and the gate that followed counted a stranger's answers. Whoever is
# there is named, and this stops.
for p in "$PORT" "$TRANSPORT_PORT"; do
  holder=$(lsof -nP -iTCP:"$p" -sTCP:LISTEN -t 2>/dev/null | head -1)
  if [ -n "$holder" ]; then
    echo "gate_node.sh: port $p is already held by pid $holder:" >&2
    ps -o lstart=,command= -p "$holder" >&2 2>/dev/null || true
    echo "gate_node.sh: refusing to start; kill that process or set VELO_PORT" >&2
    exit 2
  fi
done
# The geoip databases, the Beider-Morse rule files and the Ukrainian
# dictionary are somebody else's data and are not in this repository
# (docs/geoip.md, docs/phonetic.md, docs/ukrainian.md). They used
# to be looked for in /tmp, which a restart empties: the suites that read them
# then failed for want of a file rather than for anything the code does. They
# live under the home directory now, and VELO_FIXTURES says where.
FIXTURES=${VELO_FIXTURES:-$HOME/velo-fixtures}
DATA=${VELO_DATA:-/tmp/velo-gate}
REPO=${VELO_URL_REPO:-/tmp/velo-url-repo}
FIXTURE=${VELO_URL_FIXTURE_PORT:-9280}
rm -rf "$DATA" "$REPO"
# the user-agent suite names a regex file its build copies into the node's
# config; the file is in the OpenSearch tree, so it is copied from there
mkdir -p "$DATA/config/ingest-user-agent"
cp study/OpenSearch/modules/ingest-user-agent/src/test/test-regexes.yml \
   "$DATA/config/ingest-user-agent/" 2>/dev/null || true
VELOSEARCH_ADDR=127.0.0.1:$PORT \
VELOSEARCH_TRANSPORT_PORT=$TRANSPORT_PORT \
VELOSEARCH_DATA="$DATA" \
VELOSEARCH_NODE_ATTRS=testattr=test \
VELOSEARCH_GEOIP_PATH=${VELO_GEOIP:-$FIXTURES/geoip-db} \
VELOSEARCH_PHONETIC_RULES=${VELO_PHONETIC:-$FIXTURES/phonetic-rules} \
VELOSEARCH_UKRAINIAN_DICT=${VELO_UKRAINIAN:-$FIXTURES/ukrainian-dict} \
VELOSEARCH_PATH_REPO="$REPO" \
VELOSEARCH_URL_ALLOWED="http://snapshot.test*,http://127.0.0.1:$FIXTURE*" \
VELOSEARCH_REINDEX_ALLOWLIST="127.0.0.1:*" \
VELOSEARCH_NODE_ROLES="${VELO_ROLES:-cluster_manager,data,ingest,remote_cluster_client}" \
exec target/release/velosearch
