#!/usr/bin/env bash
# Measures an editor's total memory: the app plus every process it started (language servers,
# agents, helpers), sampled once a second. Prints median and peak, for quotable comparisons.
#
# Usage:  bloo/bench/memory.sh "<app name>" [seconds=60]
#   bloo/bench/memory.sh "Bloo Dev" 120
#   bloo/bench/memory.sh "Code" 120          # VS Code
#   bloo/bench/memory.sh "Cursor" 120
#
# Run the same scenario in each editor while it samples (for example: two projects open,
# four agents working), so the numbers are comparable.

set -euo pipefail

app_name="${1:?usage: memory.sh \"<app name>\" [seconds]}"
seconds="${2:-60}"

root_pids() {
    # The app's main process: the executable inside "<name>.app/Contents/MacOS/".
    pgrep -f "/${app_name}\.app/Contents/MacOS/" || true
}

descendants() {
    local pid
    for pid in "$@"; do
        echo "$pid"
        descendants $(pgrep -P "$pid" || true)
    done
}

total_rss_mb() {
    local pids
    pids=$(descendants $(root_pids) | sort -u | tr '\n' ',' | sed 's/,$//')
    if [[ -z "$pids" ]]; then
        echo 0
        return
    fi
    # ps reports RSS in KB.
    ps -o rss= -p "$pids" | awk '{ total += $1 } END { printf "%.0f\n", total / 1024 }'
}

if [[ -z "$(root_pids)" ]]; then
    echo "No running app matched \"${app_name}.app\". Open it first." >&2
    exit 1
fi

echo "Sampling ${app_name} for ${seconds}s..."
samples=()
for ((i = 0; i < seconds; i++)); do
    samples+=("$(total_rss_mb)")
    sleep 1
done

sorted=$(printf '%s\n' "${samples[@]}" | sort -n)
count=${#samples[@]}
median=$(echo "$sorted" | sed -n "$(((count + 1) / 2))p")
peak=$(echo "$sorted" | tail -1)
processes=$(descendants $(root_pids) | sort -u | wc -l | tr -d ' ')

printf '%s: median %s MB, peak %s MB, %s processes (%s samples)\n' \
    "$app_name" "$median" "$peak" "$processes" "$count"
