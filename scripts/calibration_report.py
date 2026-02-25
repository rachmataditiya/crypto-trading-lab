#!/usr/bin/env python3
import os
import sqlite3
from pathlib import Path
from datetime import datetime, timedelta

ROOT = Path('/Users/rachmataditiya/.openclaw/workspace')
DB = ROOT / 'data' / 'trading-journal' / 'sim_history.db'


def pct(n, d):
    return 0.0 if d == 0 else (n / d) * 100.0


def main():
    if not DB.exists():
        print(f'no db: {DB}')
        return

    conn = sqlite3.connect(DB)
    cur = conn.cursor()
    fail_bucket_min = max(1, int(os.getenv('FAIL_BUCKET_MIN', '2')))

    today = datetime.now().date()
    week_ago = today - timedelta(days=6)

    for label, start_date in [('DAILY', today), ('WEEKLY', week_ago)]:
        rows = cur.execute(
            '''SELECT trade_date, trade_time, setup_idea, decision, p_up, p_down, confidence, expected_value_r,
                      quality_gate, regime
               FROM simulation_events
               WHERE trade_date >= ?
               ORDER BY trade_date, trade_time''',
            (start_date.isoformat(),)
        ).fetchall()

        total = len(rows)
        no_trade = sum(1 for r in rows if (r[3] or '').startswith('NO_TRADE'))
        trade_rows = [r for r in rows if (r[3] or '').startswith('PAPER_TRADE')]

        # proxy outcomes from setup text (since no fills/outcomes table yet)
        # hit if predicted direction aligns with higher probability side by >0.1 and positive EV
        hit = 0
        brier_sum = 0.0
        brier_n = 0
        preset_perf = {}
        for r in rows:
            setup = (r[2] or '').lower()
            p_up = r[4] if r[4] is not None else 0.5
            p_down = r[5] if r[5] is not None else 0.5
            ev = r[7] if r[7] is not None else 0.0
            pred_up = p_up >= p_down
            inferred_up = 'long' in setup or 'up' in setup
            inferred_down = 'short' in setup or 'down' in setup
            if inferred_up or inferred_down:
                actual = 1.0 if inferred_up else 0.0
                brier_sum += (p_up - actual) ** 2
                brier_n += 1
                if (pred_up and inferred_up and ev > 0) or ((not pred_up) and inferred_down and ev > 0):
                    hit += 1

            qg = (r[8] or '')
            preset = 'unknown'
            if 'grade=A' in qg:
                preset = 'A'
            elif 'grade=B' in qg:
                preset = 'B'
            elif 'grade=C' in qg:
                preset = 'C'
            elif 'grade=D' in qg:
                preset = 'D'
            bucket = preset_perf.setdefault(preset, {'n': 0, 'avg_ev': 0.0, 'avg_conf': 0.0})
            bucket['n'] += 1
            bucket['avg_ev'] += ev
            bucket['avg_conf'] += (r[6] if r[6] is not None else 0.5)

        fail_reasons = cur.execute(
            '''SELECT COALESCE(NULLIF(TRIM(reason), ''), 'unknown') AS reason_bucket, COUNT(*) AS cnt
               FROM quality_gate_fail_reasons
               WHERE trade_date >= ?
               GROUP BY reason_bucket
               HAVING cnt >= ?
               ORDER BY cnt DESC, reason_bucket ASC''',
            (start_date.isoformat(), fail_bucket_min)
        ).fetchall()

        print(f'=== {label} CALIBRATION REPORT ({start_date} .. {today}) ===')
        print(f'total_cycles={total}')
        print(f'paper_trades={len(trade_rows)}')
        print(f'no_trade={no_trade} ({pct(no_trade,total):.1f}%)')
        print(f'hit_rate_proxy={pct(hit,max(1,brier_n)):.1f}%')
        print(f'brier_proxy={(brier_sum / max(1,brier_n)):.4f}')
        print(f'quality_gate_fail_reasons (min bucket={fail_bucket_min}):')
        if fail_reasons:
            for reason, cnt in fail_reasons:
                print(f'  - {reason}: {cnt}')
        else:
            print('  - none')
        print('per_preset_performance:')
        for preset in sorted(preset_perf):
            b = preset_perf[preset]
            n = max(1, b['n'])
            print(f"  - {preset}: n={b['n']}, avg_ev={b['avg_ev']/n:+.3f}R, avg_conf={b['avg_conf']/n:.3f}")
        print()


if __name__ == '__main__':
    main()
