"""STOCKGENIE_NEWS_REF_V1：標題關鍵詞情緒。只供參考，不准改可買／可賣／停損。"""

from __future__ import annotations

import json
import re
import time
import xml.etree.ElementTree as ET
from typing import Any
from urllib.parse import quote

import httpx

from paths import cache_dir, ensure_dir

CACHE = cache_dir()
HEADERS = {"User-Agent": "Mozilla/5.0 (compatible; StockGenie/2.0; research)"}


def _read_json(path, ttl: float):
    if not path.exists():
        return None
    try:
        obj = json.loads(path.read_text())
        if time.time() - obj.get("_cached_at", 0) <= ttl:
            return obj.get("data")
    except Exception:
        return None
    return None


def _write_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"_cached_at": time.time(), "data": data}, ensure_ascii=False, default=str))

NEWS_TTL = 4 * 3600
_MAX_ITEMS = 8

_BULL = (
    "調升", "上修", "買超", "創高", "超預期", "強勁", "成長", "擴產", "訂單", "看好",
    "亮眼", "大買", "加碼", "新高", "盈餘成長", "upgrade", "beat", "surge", "rally",
    "record high", "outperform", "raised", "buy rating",
)
_BEAR = (
    "調降", "下修", "賣超", "虧損", "衰退", "裁員", "重挫", "暴跌", "警示", "違約",
    "停工", "掏空", "下市", "減資", "downgrade", "miss", "plunge", "fraud", "lawsuit",
    "profit warning", "underperform", "cut rating",
)

_TAG = re.compile(r"<[^>]+>")


def _clean(text: str) -> str:
    s = _TAG.sub("", text or "")
    s = re.sub(r"\s+", " ", s).strip()
    return s[:160]


def _tone_of(title: str) -> tuple[str, int]:
    t = title or ""
    tl = t.lower()
    b = sum(1 for w in _BULL if w.lower() in tl)
    s = sum(1 for w in _BEAR if w.lower() in tl)
    score = b - s
    if score > 0:
        return "long", score
    if score < 0:
        return "short", score
    return "neutral", 0


def _empty(reason: str) -> dict[str, Any]:
    return {
        "ok": False,
        "n": 0,
        "tone": "neutral",
        "label": "沒有可用新聞",
        "score": 0,
        "items": [],
        "note": reason,
        "gate": False,
    }


def _rss_items(text: str, source: str) -> list[dict]:
    out: list[dict] = []
    try:
        root = ET.fromstring(text)
    except Exception:
        return out
    for item in root.iter("item"):
        title = _clean(item.findtext("title") or "")
        if not title:
            continue
        when = (item.findtext("pubDate") or "").strip()[:32]
        src = source
        # Google 標題常是「標題 - 媒體」
        if " - " in title:
            left, right = title.rsplit(" - ", 1)
            if 1 < len(right) <= 20:
                title, src = left, right
        out.append({"title": title, "when": when, "source": src})
        if len(out) >= 12:
            break
    return out


def _fetch_rss(url: str) -> str:
    r = httpx.get(url, headers=HEADERS, timeout=6.0, follow_redirects=True)
    r.raise_for_status()
    return r.text or ""


def _sources(symbol: str, name: str | None) -> list[tuple[str, str]]:
    code = (symbol or "").split(".")[0]
    q_tw = quote(f"{code} {name or ''} 股票".strip())
    q_us = quote(f"{symbol} stock")
    yahoo = (
        f"https://feeds.finance.yahoo.com/rss/2.0/headline?s={quote(symbol)}"
        "&region=TW&lang=zh-Hant-TW"
    )
    g_tw = f"https://news.google.com/rss/search?q={q_tw}&hl=zh-TW&gl=TW&ceid=TW:zh-Hant"
    g_us = f"https://news.google.com/rss/search?q={q_us}&hl=en-US&gl=US&ceid=US:en"
    market_tw = str(symbol).endswith(".TW") or str(symbol).endswith(".TWO")
    if market_tw:
        return [("Yahoo", yahoo), ("Google新聞", g_tw)]
    return [("Yahoo", yahoo), ("Google News", g_us)]


def load_news(symbol: str, name: str | None = None) -> dict[str, Any]:
    """抓標題做關鍵詞計分。失敗就空白。永不回傳可買／可賣。"""
    note = "新聞／情緒只供參考，不改可買、可賣、停損，也不送委託。"
    if not symbol:
        return _empty(note + " 沒有代號。")
    ensure_dir(CACHE / "news")
    path = CACHE / "news" / f"{symbol.replace('/', '_')}.json"
    cached = _read_json(path, NEWS_TTL)
    if isinstance(cached, dict) and "items" in cached:
        cached["gate"] = False
        cached["note"] = note
        return cached

    collected: list[dict] = []
    seen: set[str] = set()
    for source, url in _sources(symbol, name):
        try:
            raw = _fetch_rss(url)
        except Exception:
            continue
        for it in _rss_items(raw, source):
            key = it["title"]
            if key in seen:
                continue
            seen.add(key)
            tone, score = _tone_of(it["title"])
            it["tone"] = tone
            it["score"] = score
            collected.append(it)
        if len(collected) >= _MAX_ITEMS:
            break

    items = collected[:_MAX_ITEMS]
    if not items:
        return _empty("抓不到新聞標題（來源逾時或空白）。" + note)

    net = sum(int(it.get("score") or 0) for it in items)
    if net >= 2:
        tone, label = "bull", "偏多（關鍵詞計分，不是預測）"
    elif net <= -2:
        tone, label = "bear", "偏空（關鍵詞計分，不是預測）"
    else:
        tone, label = "neutral", "情緒中性（關鍵詞計分，不是預測）"
    out = {
        "ok": True,
        "n": len(items),
        "tone": tone,
        "label": label,
        "score": net,
        "items": items,
        "note": note,
        "gate": False,
    }
    try:
        _write_json(path, out)
    except Exception:
        pass
    return out
