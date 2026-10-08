#!/usr/bin/env python3
"""
Kestrel Daily Posts — schedule and post 3 tweets/day at random times,
written in Tim's voice (warm, funny, no corporate-speak).

Three post types, randomly assigned to time slots spread across the day:
  blog  — a random recent post from one of Tim's 3 blogs (link in a reply)
  repo  — a random public non-archived ninepointlabs GitHub repo (link in a reply)

Links go in a summoned reply under a plain tweet rather than in the tweet
itself: plain post + reply is far cheaper on the X API than one post with a URL.
  daily — today's git commits or Obsidian daily note (no link, no URL tax)

Run with no args to schedule today's posts via `at`.  Cron entry:
  0 7 * * * cd ~/Projects/kestrel && python3 scripts/daily-posts.py
"""

from __future__ import annotations

import argparse
import json
import os
import random
import re
import subprocess
import sys
import textwrap
import urllib.error
import urllib.parse
import urllib.request
import xml.etree.ElementTree as ET
from datetime import datetime, timedelta
from html import unescape as html_unescape

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
        return json.loads(resp.read())


def send_telegram(text: str) -> bool:
    env = _load_env()
    token = env.get("TELEGRAM_BOT_TOKEN", "")
    chat_id = env.get("TELEGRAM_HOME_CHANNEL", "")
    if not token or not chat_id:
        print("  ⚠  Telegram not configured", file=sys.stderr)
        return False
    try:
        _telegram_api(token, "sendMessage", chat_id=chat_id, text=text)
        return True
    except Exception as exc:
        print(f"  ⚠  Telegram send failed: {exc}", file=sys.stderr)
        return False


def post_tweet(
    text: str, image: str | None = None, reply_to: str | None = None,
) -> tuple[bool, str]:
    cmd = [KESTREL_BIN, "post", text]
    if image:
        cmd.extend(["--image", image])
    if reply_to:
        cmd.extend(["--reply-to", reply_to])
    result = subprocess.run(
        cmd, capture_output=True, text=True, timeout=60,
        env={**os.environ, "KESTREL_LOG": "warn"},
    )
    if result.returncode == 0:
        return True, result.stdout.strip()
    return False, result.stderr.strip() or "(no output)"


def _tweet_id(kestrel_output: str) -> str | None:
    """Pull the tweet ID out of kestrel's `Posted: https://x.com/i/status/<id>` line."""
    m = re.search(r"/status/(\d+)", kestrel_output)
    return m.group(1) if m else None


def post_with_link_reply(main: str, reply: str, dry_run: bool = False) -> str:
    """Post `main` as a plain tweet, then `reply` (carrying the link) as a reply to it.

    Cheaper than one tweet with a URL in it: plain post + summoned reply.
    """
    print(f"  Tweet ({len(main)} chars): {main[:100]}...")
    print(f"  Reply ({len(reply)} chars): {reply[:100]}")
    if dry_run:
        print("  [DRY RUN] would post:")
        print(textwrap.indent(main, "    1│ "))
        print(textwrap.indent(reply, "    2│ "))
        return main

    ok, out = post_tweet(main)
    if not ok:
        print(f"  ✗  {out}")
        send_telegram(f"🐦 Post FAILED:\n\n{out}")
        return main
    print(f"  ✓  {out}")

    parent_id = _tweet_id(out)
    if parent_id is None:
        msg = f"posted main tweet but couldn't parse its ID from kestrel output: {out}"
        print(f"  ✗  {msg}")
        send_telegram(f"🐦 Posted to X (link reply SKIPPED — {msg}):\n\n{main}")
        return main

    ok, rout = post_tweet(reply, reply_to=parent_id)
    if ok:
        print(f"  ✓  reply: {rout}")
        send_telegram(f"🐦 Posted to X:\n\n{main}\n\n↳ {reply}")
    else:
        print(f"  ✗  reply: {rout}")
        send_telegram(
            f"🐦 Posted to X, but link reply FAILED:\n\n{main}\n\n{out}\n\nReply error: {rout}"
        )
    return main


def _rss_items(feed_url: str) -> list[dict[str, str]]:
    """Return [{'title':..., 'link':..., 'desc':...}, ...] from an RSS feed."""
    items: list[dict[str, str]] = []
    with urllib.request.urlopen(feed_url, timeout=20) as resp:
        body = resp.read()
    root = ET.fromstring(body)
    for item in root.findall(".//item"):
        title = (item.findtext("title") or "").strip()
        link = (item.findtext("link") or "").strip()
        desc_el = item.find("description")
        desc = ""
        if desc_el is not None and desc_el.text:
            desc = html_unescape(desc_el.text.strip())
            desc = re.sub(r"<[^>]+>", "", desc)
            desc = desc.strip()
            if len(desc) > 200:
                desc = desc[:197] + "..."
        if title and link:
            items.append({"title": title, "link": link, "desc": desc})
    return items


TWEET_BUDGET = 4000  # X Premium: 25,000; we stay well under that

def _fit(text: str, budget: int = TWEET_BUDGET) -> str:
    """Truncate only when truly needed. X Premium gives us 25K chars to work with."""
    if len(text) <= budget:
        return text
    return text[: budget - 3].rstrip() + "..."


# ── voice: tweet crafters ────────────────────────────────────────────────────

def _blog_tweet(blog_name: str, title: str, link: str, desc: str) -> tuple[str, str]:
    """Craft a blog-promo tweet in Tim's casual, warm voice.

    Returns (main, reply): the main tweet has no link; the reply carries it.
    """

    desc = re.sub(r"^[A-Z][a-z]{2} \d{1,2}, \d{4}\s*[—–-]\s*", "", desc)

    hooks = [
        f"New on the {blog_name} blog: {title}",
        f"Wrote a thing: {title}",
        f"Just posted: {title}",
        f"Fresh off the keyboard — {title}",
        f"I put some words together about {title.split(':')[0].strip().lower()}",
    ]
    closers = [
        f"from {blog_name}",
        f"from {blog_name} — come hang out",
        f"from {blog_name} — tell me I'm wrong",
        f"Read it here, from {blog_name}",
    ]

    hook = random.choice(hooks)
    closer = random.choice(closers)

    if desc and len(desc) > 30:
        main = f"{hook}\n\n{desc}"
    else:
        main = hook

    return _fit(main), f"{link}\n\n{closer}"


_REPO_INTROS = [
    "Built {} — {}",
    "{}: {}",
    "Made a thing called {}. {}",
    "I shipped {} because {}",
    "{} exists now. {}",
    "Remember {}? {}",
    "{} — {}",
]

_REPO_CLOSERS = [
    "{}",
    "{} — code's public, go look",
    "GitHub: {}",
    "{} — pull requests welcome, or don't, I'm not your mom",
    "{} — it's probably got bugs but it's my bugs",
]


def _repo_tweet(name: str, desc: str, url: str) -> tuple[str, str]:
    """Returns (main, reply): the main tweet has no link; the reply carries it."""
    if not desc:
        desc = "it does a thing and it does it pretty well"

    intro_t = random.choice(_REPO_INTROS)
    closer_t = random.choice(_REPO_CLOSERS)
    intro = intro_t.format(name, desc)
    closer = closer_t.format(url)

    return _fit(intro), closer


_DAILY_INTROS = [
    "Today in Tim-land:",
    "What I actually did today:",
    "Today's damage report:",
    "The git log says I",
    "So today I",
    "Monday? No idea. But today I",
    "Another day, another",
    "What got shipped today:",
]


def _daily_tweet(commits: list[tuple[str, str]], note: str | None) -> str | None:
    if not commits and not note:
        return None

    parts: list[str] = []

    if commits:
        proj_counts: dict[str, int] = {}
        for proj, _ in commits:
            base = proj.replace("-", " ").replace("_", " ")
            proj_counts[base] = proj_counts.get(base, 0) + 1
        top = sorted(proj_counts.items(), key=lambda x: -x[1])[:3]
        proj_bits = []
        for p, c in top:
            s = "s" if c > 1 else ""
            proj_bits.append(f"{c} commit{s} in {p}")
        proj_str = " · ".join(proj_bits)
        parts.append(f"pushed {proj_str}")

    if note:
        parts.append(note)

    intro = random.choice(_DAILY_INTROS)
    body = " · ".join(parts)

    tweet = f"{intro} {body}"

    if random.random() < 0.4:
        tags = [
            "\n\n✌️",
            "\n\nThat's the update. Back to it.",
            "\n\nProbably should've napped instead.",
            "\n\nShipping > sleeping, apparently.",
        ]
        tweet += random.choice(tags)

    return _fit(tweet)


# ── post-type handlers ───────────────────────────────────────────────────────

def do_blog(dry_run: bool = False) -> str | None:
    blog_name, feed_url = random.choice(BLOG_FEEDS)
    print(f"  Blog: {blog_name}")

    try:
        items = _rss_items(feed_url)
    except Exception as exc:
        msg = f"RSS fetch failed for {feed_url}: {exc}"
        print(f"  ✗  {msg}")
        send_telegram(f"🐦 Blog post FAILED: {msg}")
        return None

    if not items:
        print(f"  ✗  No posts in {blog_name}")
        return None

    pool = items[: min(30, len(items))]
    post = random.choice(pool)
    main, reply = _blog_tweet(blog_name, post["title"], post["link"], post.get("desc", ""))
    return post_with_link_reply(main, reply, dry_run=dry_run)


def do_repo(dry_run: bool = False) -> str | None:
    result = subprocess.run(
        ["gh", "repo", "list", "ninepointlabs", "--limit", "50",
         "--json", "name,description,isPrivate,isArchived"],
        capture_output=True, text=True, timeout=15,
    )
    if result.returncode != 0:
        msg = f"gh repo list failed: {result.stderr.strip()}"
        print(f"  ✗  {msg}")
        send_telegram(f"🐦 Repo post FAILED: {msg}")
        return None

    repos = json.loads(result.stdout)
    public = [r for r in repos if not r.get("isPrivate") and not r.get("isArchived")]
    if not public:
        print("  ✗  No public repos found")
        return None

    repo = random.choice(public)
    name = repo["name"]
    desc = repo.get("description") or ""
    url = f"https://github.com/ninepointlabs/{name}"

    main, reply = _repo_tweet(name, desc, url)

    print(f"  Repo: {name}")
    return post_with_link_reply(main, reply, dry_run=dry_run)


def _todays_git_commits() -> list[tuple[str, str]]:
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
    today = datetime.now().strftime("%Y-%m-%d")
    note_path = os.path.expanduser(f"~/Documents/Notes/daily/{today}.md")
    if not os.path.exists(note_path):
        return None
    try:
        with open(note_path, encoding="utf-8") as fh:
            text = fh.read(800)
    except Exception:
        return None
    for line in text.split("\n"):
        stripped = line.strip().lstrip("#- ").strip()
        if stripped and len(stripped) > 10:
            return _fit(stripped, 140)
    return None


def do_daily(dry_run: bool = False) -> str | None:
    commits = _todays_git_commits()
    note = _todays_obsidian_snippet()

    tweet = _daily_tweet(commits, note)
    if tweet is None:
        print("  No activity found — skipping daily post")
        return None

    print(f"  Tweet ({len(tweet)} chars): {tweet[:100]}...")
    if dry_run:
        print("  [DRY RUN]")
        return tweet

    ok, out = post_tweet(tweet)
    if ok:
        print(f"  ✓  {out}")
        send_telegram(f"🐦 Posted to X:\n\n{tweet}")
    else:
        print(f"  ✗  {out}")
        send_telegram(f"🐦 Post FAILED:\n\n{out}")
    return tweet


# ── scheduling ───────────────────────────────────────────────────────────────

def schedule_posts() -> None:
    os.makedirs(LOG_DIR, exist_ok=True)
    now = datetime.now()
    start = now.replace(hour=9, minute=0, second=0, microsecond=0)
    end = now.replace(hour=21, minute=0, second=0, microsecond=0)

    candidates: list[datetime] = []
    t = start
    while t <= end - timedelta(hours=1):
        candidates.append(t)
        t += timedelta(minutes=15)

    if len(candidates) < 3:
        print("Not enough time slots left today", file=sys.stderr)
        return

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
            input=cmd, capture_output=True, text=True, timeout=10,
        )
        if result.returncode == 0:
            print(f"Scheduled {mode} at {time_str}  →  {log_file}")
        else:
            print(f"Failed to schedule {mode} at {time_str}: {result.stderr.strip()}")


# ── main ─────────────────────────────────────────────────────────────────────

def main() -> None:
    parser = argparse.ArgumentParser(
        description="Kestrel Daily Posts — schedule or post a tweet in Tim's voice",
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
        "--mode", choices=["schedule", "blog", "repo", "daily"], default="schedule",
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