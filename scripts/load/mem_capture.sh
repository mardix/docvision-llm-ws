#!/bin/sh
# Sample RSS of a process every 200 ms; prints idle/peak RSS in MiB when stopped (Ctrl-C) or after N seconds.
# Usage: mem_capture.sh <pid> [seconds]
set -eu
pid=$1; secs=${2:-60}; peak=0; first=""
end=$(( $(date +%s) + secs ))
while kill -0 "$pid" 2>/dev/null && [ "$(date +%s)" -lt "$end" ]; do
  if [ -r "/proc/$pid/status" ]; then kb=$(awk '/VmRSS/{print $2}' "/proc/$pid/status"); else kb=$(ps -o rss= -p "$pid" | tr -d ' '); fi
  [ -z "$first" ] && first=$kb
  [ "$kb" -gt "$peak" ] && peak=$kb
  sleep 0.2
done
echo "rss_first_mib=$((first/1024)) rss_peak_mib=$((peak/1024))"
