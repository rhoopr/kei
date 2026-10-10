#!/usr/bin/env bash
# Offline checks of the actual image probe and Docker's build-context filtering.
set -euo pipefail

repo_root=$(git rev-parse --show-toplevel)
image="${KEI_DOCKER_IMAGE:-kei:dev}"
work=$(mktemp -d "${TMPDIR:-/tmp}/kei-docker-packaging-XXXXX")
server_pid=""
cleanup() {
    if [[ -n "$server_pid" ]]; then
        kill "$server_pid" 2>/dev/null || true
        wait "$server_pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT

mkdir -p "$work/context/fuzz/target" "$work/context/fuzz/artifacts" "$work/context/fuzz/corpus" \
    "$work/context/nested/fuzz/target" "$work/context/src" "$work/context/docker"
cp "$repo_root/.dockerignore" "$work/context/.dockerignore"
for path in fuzz/target/output fuzz/artifacts/crash fuzz/corpus/seed nested/fuzz/target/output; do
    touch "$work/context/$path"
done
for path in src/main.rs docker/entrypoint.sh Cargo.toml Cargo.lock build.rs; do
    touch "$work/context/$path"
done
printf 'FROM scratch\nCOPY . /\n' >"$work/context/Dockerfile"
docker buildx build --output "type=local,dest=$work/export" "$work/context"
for path in fuzz/target/output fuzz/artifacts/crash fuzz/corpus/seed nested/fuzz/target/output; do
    test ! -e "$work/export/$path"
done
for path in src/main.rs docker/entrypoint.sh Cargo.toml Cargo.lock build.rs; do
    test -f "$work/export/$path"
done
echo "Docker context exclusions and required inputs passed"

probe=$(docker inspect --format '{{index .Config.Healthcheck.Test 1}}' "$image")
cat >"$work/server.py" <<'PY'
import http.server
import pathlib
import sys

root = pathlib.Path(sys.argv[1])


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        status = int((root / "status").read_text()) if self.path == "/healthz" else 404
        self.send_response(status)
        self.end_headers()

    def log_message(self, *args):
        pass


server = http.server.HTTPServer(("127.0.0.1", int(sys.argv[2])), Handler)
(root / "port").write_text(str(server.server_port))
server.serve_forever()
PY
start_server() {
    rm -f "$work/port"
    printf 200 >"$work/status"
    python3 "$work/server.py" "$work" "$1" >"$work/server.log" 2>&1 &
    server_pid=$!
    for _ in {1..50}; do
        [[ -f "$work/port" ]] && return
        kill -0 "$server_pid" || { cat "$work/server.log"; return 1; }
        sleep 0.1
    done
    return 1
}
start_server 9090
docker run --rm --network host --entrypoint sh "$image" -ec "$probe"
kill "$server_pid"
wait "$server_pid" 2>/dev/null || true
server_pid=""
start_server 0
port=$(cat "$work/port")
docker run --rm --network host -e KEI_HEALTHCHECK_PORT="$port" --entrypoint sh "$image" -ec "$probe"
printf 503 >"$work/status"
if docker run --rm --network host -e KEI_HEALTHCHECK_PORT="$port" --entrypoint sh "$image" -ec "$probe"; then
    echo "healthcheck accepted HTTP 503" >&2
    exit 1
fi
echo "Actual image healthcheck passed default port, custom port and HTTP failure checks"
