#!/usr/bin/env bash
set -euo pipefail
ROOT="/Users/rachmataditiya/.openclaw/workspace"
cd "$ROOT"
# Run one cycle every launch trigger (LaunchAgent handles cadence)
bash "$ROOT/skills/crypto-trading-lab/scripts/sim_engine.sh" once >> /tmp/sim-engine-daemon.out.log 2>> /tmp/sim-engine-daemon.err.log
# Sync markdown journal into SQLite history store
python3 "$ROOT/skills/crypto-trading-lab/scripts/sync_journal_sqlite.py" >> /tmp/sim-engine-daemon.out.log 2>> /tmp/sim-engine-daemon.err.log
