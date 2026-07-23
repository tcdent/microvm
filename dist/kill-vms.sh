#!/bin/bash
# Kill any running microvm processes. Used between bench runs to ensure
# a clean slate before booting a fresh VM.
#
# Usage: dist/kill-vms.sh

PIDS=$(ps aux | grep "target/release/microvm" | grep -v grep | awk '{print $2}')
if [ -z "$PIDS" ]; then
    echo "No microvm processes running."
    exit 0
fi

echo "Killing microvm processes: $PIDS"
kill $PIDS 2>/dev/null
sleep 1

REMAINING=$(ps aux | grep "target/release/microvm" | grep -v grep | awk '{print $2}')
if [ -n "$REMAINING" ]; then
    echo "Forcing kill on stragglers: $REMAINING"
    kill -9 $REMAINING 2>/dev/null
fi
