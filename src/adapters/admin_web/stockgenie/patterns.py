"""K 線形態：實體大小以 ATR 校準，只看最近 1–3 根。"""

from __future__ import annotations

from typing import Any

import pandas as pd


def upper_wick(df: pd.DataFrame) -> dict[str, Any]:
    """
    長上影：上影佔振幅 ≥40% 且 ≥0.4×ATR，或射擊之星（上影≥2 實體、下影小）。
    看板與回測同一條，用來剔除當根買點。
    """
    empty = {"long_upper": False, "shooting": False, "ratio": 0.0, "atr_mult": 0.0, "close_loc": 0.5}
    if df is None or len(df) < 2:
        return empty
    row = df.iloc[-1]
    o, h, l, c = float(row["open"]), float(row["high"]), float(row["low"]), float(row["close"])
    atr = float(row["atr"]) if "atr" in df.columns and pd.notna(row.get("atr")) else 0.0
    if atr <= 0:
        atr = abs(c) * 0.02
    rng = h - l
    upper = h - max(o, c)
    lower = min(o, c) - l
    body = abs(c - o)
    ratio = (upper / rng) if rng > 0 else 0.0
    close_loc = ((c - l) / rng) if rng > 0 else 0.5
    shooting = bool(body > 0 and upper >= 2 * body and lower <= 0.4 * body)
    long_upper = bool((rng > 0 and ratio >= 0.40 and upper >= 0.4 * atr) or shooting)
    return {
        "long_upper": long_upper,
        "shooting": shooting,
        "ratio": round(ratio, 3),
        "atr_mult": round(upper / atr, 2) if atr else 0.0,
        "close_loc": round(close_loc, 3),
        "upper": round(upper, 4),
        "rng": round(rng, 4),
    }


def detect(df: pd.DataFrame) -> list[dict[str, Any]]:
    if df is None or len(df) < 5:
        return []
    atr = float(df["atr"].iloc[-1]) if pd.notna(df["atr"].iloc[-1]) else 0.0
    if atr <= 0:
        atr = float(df["close"].iloc[-1]) * 0.02
    bars = df.iloc[-5:]
    o = bars["open"].to_numpy()
    h = bars["high"].to_numpy()
    l = bars["low"].to_numpy()
    c = bars["close"].to_numpy()
    found: list[dict[str, Any]] = []

    def body(i):
        return abs(c[i] - o[i])

    def bull(i):
        return c[i] > o[i]

    def bear(i):
        return c[i] < o[i]

    i = -1  # last bar
    b = body(i)
    upper = h[i] - max(o[i], c[i])
    lower = min(o[i], c[i]) - l[i]
    wick = upper_wick(df)

    if b >= 1.2 * atr and bull(i):
        found.append({"code": "BIG_BULL", "title": "大陽線", "detail": f"實體 {b:.2f} ≥ 1.2×ATR {atr:.2f}", "tone": "long"})
    if b >= 1.2 * atr and bear(i):
        found.append({"code": "BIG_BEAR", "title": "大陰線", "detail": f"實體 {b:.2f} ≥ 1.2×ATR {atr:.2f}", "tone": "short"})

    # 鎚子：下影線 ≥ 2 實體，上影線小
    if b > 0 and lower >= 2 * b and upper <= 0.4 * b:
        found.append({"code": "HAMMER", "title": "鎚子線", "detail": "下影線長、實體在上方，常見於短線止跌", "tone": "long"})
    if wick.get("shooting"):
        found.append({"code": "SHOOTING", "title": "射擊之星", "detail": "上影線長、實體在下方，常見於短線遇壓", "tone": "short"})
    if wick.get("long_upper"):
        found.append({"code": "LONG_UPPER", "title": "長上影線", "detail": f"上影佔振幅 {wick['ratio']:.0%}（{wick['atr_mult']:.1f}×ATR），當日買盤未能守住，剔除當根買點", "tone": "short"})

    # 陽包陰 / 陰包陽（最近兩根）
    if len(c) >= 2:
        if bull(i) and bear(i - 1) and o[i] <= c[i - 1] and c[i] >= o[i - 1]:
            found.append({"code": "ENGULF_BULL", "title": "陽包陰", "detail": "今日陽線完全包住昨日陰線", "tone": "long"})
        if bear(i) and bull(i - 1) and o[i] >= c[i - 1] and c[i] <= o[i - 1]:
            found.append({"code": "ENGULF_BEAR", "title": "陰包陽", "detail": "今日陰線完全包住昨日陽線", "tone": "short"})
        # 烏雲蓋頂：昨日大陽，今日開高收在昨日實體中段以下
        if bull(i - 1) and bear(i) and o[i] > c[i - 1] and c[i] < (o[i - 1] + c[i - 1]) / 2 and c[i] > o[i - 1]:
            found.append({"code": "DARK_CLOUD", "title": "烏雲蓋頂", "detail": "開高卻收進前陽實體下半", "tone": "short"})
        # 向上跳空
        if l[i] > h[i - 1] and (l[i] - h[i - 1]) >= 0.15 * atr:
            found.append({"code": "GAP_UP", "title": "向上跳空", "detail": "今低高於昨高，缺口未補", "tone": "long"})

    # 晨星（三根）
    if len(c) >= 3:
        if bear(i - 2) and body(i - 2) >= 0.6 * atr and body(i - 1) <= 0.4 * atr and bull(i) and c[i] > (o[i - 2] + c[i - 2]) / 2:
            found.append({"code": "MORNING", "title": "晨星", "detail": "大陰 + 小實體 + 大陽，短線轉折", "tone": "long"})
        # 紅三兵
        if bull(i) and bull(i - 1) and bull(i - 2) and c[i] > c[i - 1] > c[i - 2] and body(i) >= 0.4 * atr:
            found.append({"code": "THREE_SOLDIERS", "title": "紅三兵", "detail": "連續三根揚升陽線", "tone": "long"})

    return found
