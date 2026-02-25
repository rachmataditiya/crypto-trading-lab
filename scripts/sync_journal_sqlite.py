#!/usr/bin/env python3
import sqlite3, re, json
from pathlib import Path
from datetime import datetime

ROOT = Path('/Users/rachmataditiya/.openclaw/workspace')
JDIR = ROOT / 'data' / 'trading-journal'
DB = JDIR / 'sim_history.db'


def init_db(conn):
    conn.execute('''
    CREATE TABLE IF NOT EXISTS simulation_events (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      trade_date TEXT NOT NULL,
      trade_time TEXT NOT NULL,
      regime TEXT,
      setup_idea TEXT,
      quality_gate TEXT,
      decision TEXT,
      rationale TEXT,
      checklist_json TEXT,
      raw_block TEXT,
      p_up REAL,
      p_down REAL,
      confidence REAL,
      expected_value_r REAL,
      prior_up REAL,
      posterior_up REAL,
      tech_likelihood REAL,
      news_likelihood REAL,
      news_source TEXT,
      news_sentiment REAL,
      news_catalyst_score REAL,
      inserted_at TEXT NOT NULL,
      UNIQUE(trade_date, trade_time, setup_idea)
    )
    ''')
    conn.execute('''
    CREATE TABLE IF NOT EXISTS news_events (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      trade_date TEXT NOT NULL,
      trade_time TEXT NOT NULL,
      raw_block TEXT,
      inserted_at TEXT NOT NULL,
      UNIQUE(trade_date, trade_time, raw_block)
    )
    ''')
    conn.execute('''
    CREATE TABLE IF NOT EXISTS quality_gate_fail_reasons (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      trade_date TEXT NOT NULL,
      trade_time TEXT NOT NULL,
      reason TEXT NOT NULL,
      inserted_at TEXT NOT NULL,
      UNIQUE(trade_date, trade_time, reason)
    )
    ''')

    # migration-safe for older DB
    cols = [
        ('p_up', 'REAL'), ('p_down', 'REAL'), ('confidence', 'REAL'), ('expected_value_r', 'REAL'),
        ('prior_up', 'REAL'), ('posterior_up', 'REAL'), ('tech_likelihood', 'REAL'), ('news_likelihood', 'REAL'),
        ('news_source', 'TEXT'), ('news_sentiment', 'REAL'), ('news_catalyst_score', 'REAL')
    ]
    for col, typ in cols:
        try:
            conn.execute(f'ALTER TABLE simulation_events ADD COLUMN {col} {typ}')
        except sqlite3.OperationalError:
            pass
    conn.execute('''DELETE FROM quality_gate_fail_reasons
                    WHERE id NOT IN (
                      SELECT MIN(id) FROM quality_gate_fail_reasons
                      GROUP BY trade_date, trade_time, reason
                    )''')
    conn.execute('CREATE UNIQUE INDEX IF NOT EXISTS idx_qg_fail_unique ON quality_gate_fail_reasons(trade_date, trade_time, reason)')


def split_blocks(text, title):
    lines = text.splitlines()
    out = []
    i = 0
    while i < len(lines):
        if lines[i].startswith(title):
            start = i
            i += 1
            while i < len(lines) and not lines[i].startswith('## '):
                i += 1
            out.append('\n'.join(lines[start:i]).strip())
        else:
            i += 1
    return out


def parse_sim_block(block):
    tm = re.search(r'## Hourly Simulation - (\d{2}:\d{2}) WIB', block)
    regime = re.search(r'Market regime.*\*\*(.+?)\*\*', block)
    setup = re.search(r'Ide setup simulasi \(paper\): \*\*(.+?)\*\*', block)
    qgate = re.search(r'Quality gate: `(.+?)`', block)
    decision = re.search(r'Keputusan: \*\*(.+?)\*\*', block)
    rationale = re.search(r'- Alasan: (.+)', block)
    checklist = re.findall(r'^  - (.+)$', block, flags=re.M)

    prob = re.search(r'Probability engine: P\(up\)=([\d.]+), P\(down\)=([\d.]+), confidence=([\d.]+), EV=([+-]?[\d.]+)R', block)
    bayes = re.search(r'Bayes scaffold: prior_up=([\d.]+) -> posterior_up=([\d.]+) \(tech_lh=([\d.]+), news_lh=([\d.]+), feature=([+-]?[\d.]+)\)', block)
    news = re.search(r'News catalyst context: source=([^,]+), sentiment=([+-]?[\d.]+), catalyst=([\d.]+),', block)

    return {
        'time': tm.group(1) if tm else None,
        'regime': regime.group(1) if regime else None,
        'setup': setup.group(1) if setup else None,
        'qgate': qgate.group(1) if qgate else None,
        'decision': decision.group(1) if decision else None,
        'rationale': rationale.group(1) if rationale else None,
        'checklist': checklist,
        'p_up': float(prob.group(1)) if prob else None,
        'p_down': float(prob.group(2)) if prob else None,
        'confidence': float(prob.group(3)) if prob else None,
        'expected_value_r': float(prob.group(4)) if prob else None,
        'prior_up': float(bayes.group(1)) if bayes else None,
        'posterior_up': float(bayes.group(2)) if bayes else None,
        'tech_likelihood': float(bayes.group(3)) if bayes else None,
        'news_likelihood': float(bayes.group(4)) if bayes else None,
        'news_source': news.group(1).strip() if news else None,
        'news_sentiment': float(news.group(2)) if news else None,
        'news_catalyst_score': float(news.group(3)) if news else None,
    }


def fail_reasons(checklist):
    return [x.split(':')[0].strip() for x in checklist if '❌' in x]


def main():
    JDIR.mkdir(parents=True, exist_ok=True)
    conn = sqlite3.connect(DB)
    init_db(conn)
    today = datetime.now().strftime('%Y-%m-%d')
    md = JDIR / f'{today}.md'
    if not md.exists():
        print('no journal today')
        return
    text = md.read_text(encoding='utf-8', errors='ignore')

    sim_blocks = split_blocks(text, '## Hourly Simulation - ')
    news_blocks = split_blocks(text, '## News Catalyst - ')

    for b in sim_blocks:
        p = parse_sim_block(b)
        if not p['time']:
            continue
        conn.execute(
            '''INSERT OR IGNORE INTO simulation_events
               (trade_date, trade_time, regime, setup_idea, quality_gate, decision, rationale, checklist_json, raw_block,
                p_up, p_down, confidence, expected_value_r, prior_up, posterior_up, tech_likelihood, news_likelihood,
                news_source, news_sentiment, news_catalyst_score, inserted_at)
               VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)''',
            (
                today, p['time'], p['regime'], p['setup'], p['qgate'], p['decision'], p['rationale'],
                json.dumps(p['checklist'], ensure_ascii=False), b,
                p['p_up'], p['p_down'], p['confidence'], p['expected_value_r'], p['prior_up'], p['posterior_up'],
                p['tech_likelihood'], p['news_likelihood'], p['news_source'], p['news_sentiment'], p['news_catalyst_score'],
                datetime.now().isoformat()
            )
        )

        if (p['decision'] or '').startswith('NO_TRADE'):
            for reason in fail_reasons(p['checklist']):
                conn.execute(
                    '''INSERT OR IGNORE INTO quality_gate_fail_reasons (trade_date, trade_time, reason, inserted_at)
                       VALUES (?, ?, ?, ?)''',
                    (today, p['time'], reason, datetime.now().isoformat())
                )

    for b in news_blocks:
        tm = re.search(r'## News Catalyst - (\d{2}:\d{2}) WIB', b)
        if not tm:
            continue
        conn.execute(
            '''INSERT OR IGNORE INTO news_events (trade_date, trade_time, raw_block, inserted_at)
               VALUES (?, ?, ?, ?)''',
            (today, tm.group(1), b, datetime.now().isoformat())
        )

    conn.commit()
    cur = conn.cursor()
    c1 = cur.execute('SELECT COUNT(*) FROM simulation_events WHERE trade_date=?', (today,)).fetchone()[0]
    c2 = cur.execute('SELECT COUNT(*) FROM news_events WHERE trade_date=?', (today,)).fetchone()[0]
    print(f'sqlite synced: sim_rows_today={c1}, news_rows_today={c2}')


if __name__ == '__main__':
    main()
