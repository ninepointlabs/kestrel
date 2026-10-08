#!/usr/bin/env python3
"""
Kestrel Daily Posts — schedule and post 3 tweets/day at random times.

Three post types, randomly assigned time slots spread across the day:
  blog  — a random recent post from one of Tim's 3 blogs (includes link)
  repo  — a random public non-archived ninepointlabs GitHub repo (includes link)
  daily — today's git commits or Obsidian daily note (no link)

Run with no args to schedule today's posts via `at`.  Add a daily cron entry:
  0 7 * * * cd ~/Projects/kestrel && python3 scripts/daily-posts.py
"""

from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
import textwrap
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from datetime import datetime, timedelta
from pathlib import Path

HERMES_HOME = os.environ.get("HERMES_HOME", os.path.expanduser("~/.hermes"))
KESTREL_BIN = os.path.expanduser("~/.cargo/bin/kestrel")
SCRIPT = os.path.abspath(__file__)
LOG_DIR = os.path.expanduser("~/.config/kestrel/logs")

BLOG_FEEDS = [
    ("My Azeroth Life", "https://myazerothlife.com/feed.xml"),
    ("Nine Point Labs Blog", "https://blog.ninepointlabs.com/feed.xml"),
    ("Tim Apple's Desk", "https://timapple.com/feed.xml"),
]


# ── helpers ──────────────────────────────────────────────────────────────────

def _load_env() -> dict[str, str]:
    """Read HERMES_HOME/.env for TELEGRAM_BOT_TOKEN / TELEGRAM_HOME_CHANNEL."""
    env: dict[str, str] = {}
    env_path = os.path.join(HERMES_HOME, ".env")
    try:
        with open(env_path, encoding="utf-8") as fh:
            for line in fh:
                line = line.strip()
                if line and not line.startswith("#") and "=" in line:
                    k, v = line.split("=", 1)
                    env[k.strip()] = v.strip().strip("\"'")
    except FileNotFoundError:
        pass
    return env


def _telegram_api(token: str, method: str, **params) -> dict:
    url = f"https://api.telegram.org/bot{token}/{method}"
    if params:
        url += "?" + urllib.parse.urlencode(params)
    with urllib.request.urlopen(url, timeout=15) as resp:
        return json.loads(resp.read())  # type: ignore[no-any-return]


def send_telegram(text: str) -> bool:
    """Deliver a message to Tim's home channel.  Returns True on success."""
    env = _load_env()
    token = env.get("TELEGRAM_BOT_TOKEN", "")
    chat_id = env.get("TELEGRAM_HOME_CHANNEL", "")
    if not token or not chat_id:
        print("  ⚠  Telegram not configured — skipping notification", file=sys.stderr)
        return False
    try:
        _telegram_api(token, "sendMessage", chat_id=chat_id, text=text)
        return True
    except Exception as exc:
        print(f"  ⚠  Telegram send failed: {exc}", file=sys.stderr)
        return False


def post_tweet(text: str, image: str | None = None) -> tuple[bool, str]:
    """Post via kestrel CLI.  Returns (success, stdout_or_stderr)."""
    cmd = [KESTREL_BIN, "post", text]
    if image:
        cmd.extend(["--image", image])
    result = subprocess.run(
        cmd, capture_output=True, text=True, timeout=60,
        env={**os.environ, "KESTREL_LOG": "warn"},
    )
    if result.returncode == 0:
        return True, result.stdout.strip()
    return False, result.stderr.strip() or "(no output)"


def _rss_items(feed_url: str) -> list[tuple[str, str]]:
    """Return [(title, link), ...] from an RSS feed."""
    items: list[tuple[str, str]] = []
    with urllib.request.urlopen(feed_url, timeout=20) as resp:
        body = resp.read()
    # folderblog RSS may have a default namespace; handle both ns and no-ns
    root = ET.fromstring(body)
    ns = "http://purl.org/rss/1.0/modules/content/"
    for item in root.findall(".//item"):
        title_el = item.find("title")
        link_el = item.find("link")
        title = (title_el.text or "").strip() if title_el is not None else ""
        link = (link_el.text or "").strip() if link_el is not None else ""
        if title and link:
            items.append((title, link))
    return items


# ── post-type handlers ───────────────────────────────────────────────────────

def do_blog(dry_run: bool = False) -> str | None:
    """Pick a random post from a random blog, post with link.  Returns tweet text or None."""
    blog_name, feed_url = random.choice(BLOG_FEEDS)
    print(f"  Blog: {blog_name}")

    try:
        items = _rss_items(feed_url)
    except Exception as exc:
        msg = f"Failed to fetch {feed_url}: {exc}"
        print(f"  ✗  {msg}")
        send_telegram(f"🐦 Blog post FAILED: {msg}")
        return None

    if not items:
        msg = f"No posts found in {blog_name}"
        print(f"  ✗  {msg}")
        return None

    # pick from the 30 most recent so we don't dig up ancient posts
    pool = items[: min(30, len(items))]
    title, link = random.choice(pool)

    # craft tweet — keep title + link under 280 chars
    tweet = f"{title}\n\n{link}"
    if len(tweet) > 270:
        # truncate title, keep link
        max_title = 270 - len(link) - 4  # 4 for "\n\n" and "..."
        tweet = f"{title[:max_title]}...\n\n{link}"

    print(f"  Tweet: {tweet[:80]}...")
    if dry_run:
        print(f"  [DRY RUN]")
        return tweet

    ok, out = post_tweet(tweet)
    if ok:
        print(f"  ✓  {out}")
        send_telegram(f"🐦 Blog post from {blog_name}:\n\n{tweet}\n\n{out}")
    else:
        print(f"  ✗  {out}")
        send_telegram(f"🐦 Blog post FAILED:\n\n{out}")
    return tweet


def do_repo(dry_run: bool = False) -> str | None:
    """Pick a random public non-archived ninepointlabs repo, post with link."""
    result = subprocess.run(
        [
            "gh", "repo", "list", "ninepointlabs",
            "--limit", "50", "--json", "name,description,isPrivate,isArchived",
        ],
        capture_output=True, text=True, timeout=15,
    )
    if result.returncode != 0:
        msg = f"gh repo list failed: {result.stderr.strip()}"
        print(f"  ✗  {msg}")
        send_telegram(f"🐦 Repo post FAILED: {msg}")
        return None

    repos = json.loads(result.stdout)
    public = [
        r for r in repos
        if not r.get("isPrivate") and not r.get("isArchived")
    ]
    if not public:
        print("  ✗  No public repos found")
        return None

    repo = random.choice(public)
    name = repo["name"]
    desc = repo.get("description") or "Check it out"
    url = f"https://github.com/ninepointlabs/{name}"

    # craft tweet — url is ~50 chars, leaving ~220 for description
    tweet = f"{desc}\n\n{url}"
    if len(tweet) > 270:
        max_desc = 270 - len(url) - 4
        tweet = f"{desc[:max_desc]}...\n\n{url}"

    print(f"  Repo: {name}")
    print(f"  Tweet: {tweet[:80]}...")
    if dry_run:
        print(f"  [DRY RUN]")
        return tweet

    ok, out = post_tweet(tweet)
    if ok:
        print(f"  ✓  {out}")
        send_telegram(f"🐦 Repo highlight:\n\n{tweet}\n\n{out}")
    else:
        print(f"  ✗  {out}")
        send_telegram(f"🐦 Repo post FAILED:\n\n{out}")
    return tweet


def _todays_git_commits() -> list[tuple[str, str]]:
    """Return [(project, oneline), ...] for today's commits by Tim."""
    today = datetime.now().strftime("%Y-%m-%d")
    projects_dir = os.path.expanduser("~/Projects")
    commits: list[tuple[str, str]] = []
    try:
        for proj in sorted(os.listdir(projects_dir)):
            proj_path = os.path.join(projects_dir, proj)
            if not os.path.isdir(os.path.join(proj_path, ".git")):
                continue
            result = subprocess.run(
                ["git", "-C", proj_path, "log", "--oneline",
                 f"--since={today}T00:00", "--author=Tim"],
                capture_output=True, text=True, timeout=10,
            )
            for line in result.stdout.strip().split("\n"):
                if line:
                    commits.append((proj, line))
    except Exception:
        pass
    return commits


def _todays_obsidian_snippet() -> str | None:
    """Return first paragraph of today's daily note, or None."""
    today = datetime.now().strftime("%Y-%m-%d")
    note_path = os.path.expanduser(f"~/Documents/Notes/daily/{today}.md")
    if not os.path.exists(note_path):
        return None
    try:
        with open(note_path, encoding="utf-8") as fh:
            text = fh.read(600)
    except Exception:
        return None
    # grab first non-empty, non-heading line(s)
    for line in text.split("\n"):
        stripped = line.strip().lstrip("#").strip()
        if stripped and not stripped.startswith("---"):
            if len(stripped) > 120:
                stripped = stripped[:117] + "..."
            return stripped
    return None


def do_daily(dry_run: bool = False) -> str | None:
    """Craft a no-link tweet about today's activity.  Skips if nothing found."""
    commits = _todays_git_commits()
    note = _todays_obsidian_snippet()

    if not commits and not note:
        print("  No activity found — skipping daily post")
        return None

    parts: list[str] = []

    if commits:
        # group by project
        proj_counts: dict[str, int] = {}
        for proj, _ in commits:
            proj_counts[proj] = proj_counts.get(proj, 0) + 1
        proj_str = " · ".join(
            f"{c} commit{'s' if c > 1 else ''} in {p}"
            for p, c in sorted(proj_counts.items(), key=lambda x: -x[1])[:3]
        )
        parts.append(f"Today's code: {proj_str}")

    if note:
        parts.append(note)

    tweet = " 📝 ".join(parts)
    if len(tweet) > 270:
        tweet = tweet[:267] + "..."

    print(f"  Tweet: {tweet[:80]}...")
    if dry_run:
        print(f"  [DRY RUN]")
        return tweet

    ok, out = post_tweet(tweet)
    if ok:
        print(f"  ✓  {out}")
        send_telegram(f"🐦 Daily update:\n\n{tweet}\n\n{out}")
    else:
        print(f"  ✗  {out}")
        send_telegram(f"🐦 Daily post FAILED:\n\n{out}")
    return tweet


# ── scheduling ───────────────────────────────────────────────────────────────

def schedule_posts() -> None:
    """Pick 3 random time slots between 9:00 and 21:00 and schedule at(1) jobs."""
    os.makedirs(LOG_DIR, exist_ok=True)

    now = datetime.now()
    start = now.replace(hour=9, minute=0, second=0, microsecond=0)
    end = now.replace(hour=21, minute=0, second=0, microsecond=0)

    # generate candidate times: every 15 minutes between start and end
    candidates: list[datetime] = []
    t = start
    while t <= end - timedelta(hours=1):
        candidates.append(t)
        t += timedelta(minutes=15)

    if len(candidates) < 3:
        print("Not enough time slots left today", file=sys.stderr)
        return

    # pick 3 times, at least 2 hours apart
    times: list[datetime] = []
    for _ in range(50):
        random.shuffle(candidates)
        chosen = sorted(candidates[:3])
        if all(
            (chosen[i + 1] - chosen[i]).total_seconds() >= 7200
            for i in range(len(chosen) - 1)
        ):
            times = chosen
            break

    if len(times) < 3:
        # fallback: pick 3 random from candidates regardless of spacing
        times = sorted(random.sample(candidates, min(3, len(candidates))))

    modes = ["blog", "repo", "daily"]
    random.shuffle(modes)

    for mode, t in zip(modes, times):
        time_str = t.strftime("%H:%M")
        log_file = os.path.join(
            LOG_DIR, f"{t.strftime('%Y-%m-%d')}-{mode}-{t.strftime('%H%M')}.log"
        )
        cmd = (
            f"cd {os.path.dirname(SCRIPT)}/.. && "
            f"python3 {SCRIPT} --mode {mode} >> {log_file} 2>&1"
        )
        result = subprocess.run(
            ["at", time_str],
            input=cmd,
            capture_output=True, text=True, timeout=10,
        )
        if result.returncode == 0:
            print(f"Scheduled {mode} at {time_str}  →  {log_file}")
        else:
            print(f"Failed to schedule {mode} at {time_str}: {result.stderr.strip()}")


# ── main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(
        description="Kestrel Daily Posts — schedule or post a tweet",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=textwrap.dedent("""\
            examples:
              %(prog)s                    schedule today's 3 posts via at(1)
              %(prog)s --mode blog        post a blog highlight now
              %(prog)s --mode repo        post a repo highlight now
              %(prog)s --mode daily       post today's activity now
              %(prog)s --mode blog --dry-run   preview tweet without posting
        """),
    )
    parser.add_argument(
        "--mode", choices=["schedule", "blog", "repo", "daily"],
        default="schedule",
    )
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()

    if args.mode == "schedule":
        schedule_posts()
        return

    print(f"── {args.mode} post  {datetime.now().strftime('%H:%M')} ──")
    if args.mode == "blog":
        do_blog(dry_run=args.dry_run)
    elif args.mode == "repo":
        do_repo(dry_run=args.dry_run)
    elif args.mode == "daily":
        do_daily(dry_run=args.dry_run)

    print()


if __name__ == "__main__":
    main()