"""真實公開資料：Yahoo 日線／基本面、證交所 T86 籌碼。帶 TTL 磁碟快取。"""

from __future__ import annotations

import json
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path

import httpx
import numpy as np
import pandas as pd
import yfinance as yf

from paths import cache_dir, ensure_dir

CACHE = cache_dir()
ensure_dir(CACHE / "info")
ensure_dir(CACHE / "ohlcv")
ensure_dir(CACHE / "t86")

TAIPEI = timezone(timedelta(hours=8))
HEADERS = {"User-Agent": "Mozilla/5.0 (compatible; StockGenie/2.0; research)"}

PRICE_TTL = 3600
INFO_TTL = 24 * 3600
T86_TTL = 12 * 3600


def now_ts() -> float:
    return time.time()


def _read_json(path: Path, ttl: float):
    if not path.exists():
        return None
    try:
        obj = json.loads(path.read_text())
        if now_ts() - obj.get("_cached_at", 0) <= ttl:
            return obj.get("data")
    except Exception:
        return None
    return None


def _write_json(path: Path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps({"_cached_at": now_ts(), "data": data}, ensure_ascii=False, default=str))


def _ohlcv_path(symbol: str) -> Path:
    return CACHE / "ohlcv" / f"{symbol.replace('/', '_')}.pkl"


def load_ohlcv_many(symbols: list[str], period: str = "1y") -> dict[str, pd.DataFrame]:
    """批量下載復權日線。回傳 symbol -> DataFrame[open,high,low,close,volume]。"""
    out: dict[str, pd.DataFrame] = {}
    need = []
    min_ok = 800 if period == "5y" else 200 if period == "2y" else 60
    for sym in symbols:
        p = _ohlcv_path(sym)
        if p.exists() and now_ts() - p.stat().st_mtime <= PRICE_TTL:
            try:
                df = pd.read_pickle(p)
                if df is not None and len(df) >= min_ok:
                    out[sym] = df
                    continue
            except Exception:
                pass
        need.append(sym)

    if need:
        try:
            raw = yf.download(
                need,
                period=period,
                interval="1d",
                group_by="ticker",
                auto_adjust=True,
                threads=True,
                progress=False,
            )
        except Exception:
            raw = None
        if raw is not None and not raw.empty:
            if isinstance(raw.columns, pd.MultiIndex):
                tickers = raw.columns.get_level_values(0).unique()
                for sym in need:
                    if sym not in tickers:
                        continue
                    piece = raw[sym].copy()
                    df = _normalize_ohlcv(piece)
                    if df is not None:
                        out[sym] = df
                        try:
                            df.to_pickle(_ohlcv_path(sym))
                        except Exception:
                            pass
            else:
                # 單一代碼時沒有 MultiIndex
                df = _normalize_ohlcv(raw)
                if df is not None and len(need) == 1:
                    out[need[0]] = df
                    try:
                        df.to_pickle(_ohlcv_path(need[0]))
                    except Exception:
                        pass

    # 漏網之魚逐檔補
    missing = [s for s in symbols if s not in out]
    for sym in missing:
        try:
            raw = yf.download(sym, period=period, interval="1d", auto_adjust=True, progress=False, threads=False)
            df = _normalize_ohlcv(raw)
            if df is not None:
                out[sym] = df
                df.to_pickle(_ohlcv_path(sym))
        except Exception:
            continue
    return out


def _normalize_ohlcv(df: pd.DataFrame, min_bars: int = 60) -> pd.DataFrame | None:
    if df is None or df.empty:
        return None
    cols = {c.lower(): c for c in df.columns}
    mapping = {}
    for std in ("open", "high", "low", "close", "volume"):
        if std in cols:
            mapping[cols[std]] = std
        elif std.title() in df.columns:
            mapping[std.title()] = std
        elif std.capitalize() in df.columns:
            mapping[std.capitalize()] = std
    if len(mapping) < 5:
        return None
    out = df.rename(columns=mapping)[list(mapping.values())].copy()
    out = out.dropna(subset=["close", "high", "low"])
    idx = pd.to_datetime(out.index)
    try:
        if getattr(idx, "tz", None) is not None:
            idx = idx.tz_convert(None)
    except Exception:
        try:
            idx = idx.tz_localize(None)
        except Exception:
            pass
    out.index = idx
    out = out.sort_index()
    if len(out) < min_bars:
        return None
    return out


def _delta_days(value) -> int | None:
    """把各種財報日欄位收成『距今天的日曆日』。"""
    if value is None or (isinstance(value, float) and value != value):
        return None
    now_d = datetime.now(TAIPEI).date()
    try:
        if isinstance(value, (int, float)) and not isinstance(value, bool):
            unix = float(value)
            if unix > 1e12:
                unix /= 1000.0
            if unix > 1e9:
                d0 = datetime.fromtimestamp(unix, tz=TAIPEI).date()
                return (d0 - now_d).days
        if hasattr(value, "date") and not isinstance(value, datetime):
            d0 = value.date() if callable(getattr(value, "date", None)) else value
            if hasattr(d0, "year"):
                return (d0 - now_d).days
        ts = pd.Timestamp(value)
        if ts.tzinfo is not None:
            ts = ts.tz_convert(TAIPEI).tz_localize(None)
        return (ts.date() - now_d).days
    except Exception:
        return None


def _pick_earnings_delta(values) -> int | None:
    best = None
    for v in values:
        if isinstance(v, (list, tuple)):
            for inner in v:
                d = _delta_days(inner)
                if d is not None and -30 <= d <= 400:
                    if best is None or abs(d) < abs(best):
                        best = d
            continue
        d = _delta_days(v)
        if d is not None and -30 <= d <= 400:
            if best is None or abs(d) < abs(best):
                best = d
    return best


def _next_earnings_days(symbol: str, raw: dict | None = None) -> int | None:
    """距最近財報的日曆日（過去 30 日～未來 400 日取最近一檔）。多來源，減少空白。"""
    epath = CACHE / "earnings" / f"{symbol.replace('/', '_')}.json"
    epath.parent.mkdir(parents=True, exist_ok=True)
    cached = _read_json(epath, 6 * 3600)
    if cached is not None and isinstance(cached, dict) and "days" in cached:
        return cached.get("days")

    cand = []
    if raw:
        for key in ("earningsTimestamp", "earningsTimestampStart", "earningsTimestampEnd", "earningsDate"):
            if raw.get(key) is not None:
                cand.append(raw.get(key))
    days = _pick_earnings_delta(cand)
    if days is None:
        try:
            tk = yf.Ticker(symbol)
            cal = getattr(tk, "calendar", None)
            if isinstance(cal, dict):
                cand.extend([cal.get("Earnings Date"), cal.get("earningsDate")])
            elif cal is not None:
                try:
                    if hasattr(cal, "index") and "Earnings Date" in list(getattr(cal, "index", [])):
                        cand.append(cal.loc["Earnings Date"])
                except Exception:
                    pass
            days = _pick_earnings_delta(cand)
            if days is None:
                ed = None
                try:
                    ed = tk.get_earnings_dates(limit=12)
                except Exception:
                    ed = getattr(tk, "earnings_dates", None)
                if ed is not None and len(ed) > 0:
                    idx = list(getattr(ed, "index", []))
                    days = _pick_earnings_delta(idx)
        except Exception:
            days = None

    try:
        _write_json(epath, {"days": days})
    except Exception:
        pass
    return days


def load_info(symbol: str) -> dict:
    path = CACHE / "info" / f"{symbol.replace('/', '_')}.json"
    cached = _read_json(path, INFO_TTL)
    if cached is not None:
        if cached.get("earnings_days") is None:
            cached["earnings_days"] = _next_earnings_days(symbol, cached)
        return cached
    info = {}
    try:
        raw = yf.Ticker(symbol).info or {}
        keys = [
            "shortName",
            "longName",
            "trailingPE",
            "forwardPE",
            "priceToBook",
            "returnOnEquity",
            "revenueGrowth",
            "earningsGrowth",
            "earningsQuarterlyGrowth",
            "freeCashflow",
            "targetMeanPrice",
            "targetHighPrice",
            "targetLowPrice",
            "numberOfAnalystOpinions",
            "recommendationKey",
            "recommendationMean",
            "profitMargins",
            "debtToEquity",
            "dividendYield",
            "payoutRatio",
            "trailingEps",
            "forwardEps",
            "sharesOutstanding",
            "currency",
            "exchange",
            "earningsTimestamp",
            "earningsTimestampStart",
            "earningsTimestampEnd",
            "earningsDate",
            "averageDailyVolume10Day",
            "averageVolume",
        ]
        info = {k: raw.get(k) for k in keys}
        info["fetched"] = True
        info["earnings_days"] = _next_earnings_days(symbol, raw)
    except Exception as exc:
        info = {"fetched": False, "error": str(exc)}
    _write_json(path, info)
    return info


def load_info_many(symbols: list[str], workers: int = 8) -> dict[str, dict]:
    from concurrent.futures import ThreadPoolExecutor, as_completed

    out = {}
    with ThreadPoolExecutor(max_workers=workers) as pool:
        futs = {pool.submit(load_info, s): s for s in symbols}
        for fut in as_completed(futs):
            s = futs[fut]
            try:
                out[s] = fut.result()
            except Exception as exc:
                out[s] = {"fetched": False, "error": str(exc)}
    return out


def _parse_int(value) -> int:
    if value is None:
        return 0
    s = str(value).replace(",", "").replace(" ", "").replace("+", "")
    if s in ("", "--", "nan", "None"):
        return 0
    try:
        return int(float(s))
    except Exception:
        return 0


def _t86_dates(n: int = 14) -> list[str]:
    """由台北今天往回找日曆日（實際交易日由 API stat 過濾）。"""
    today = datetime.now(TAIPEI).date()
    out = []
    d = today
    for _ in range(n * 3):
        out.append(d.strftime("%Y%m%d"))
        d -= timedelta(days=1)
        if len(out) >= n * 2:
            break
    return out


def fetch_t86_day(date_yyyymmdd: str) -> list[dict] | None:
    path = CACHE / "t86" / f"{date_yyyymmdd}.json"
    cached = _read_json(path, T86_TTL * 4)
    if cached is not None:
        return cached
    url = "https://www.twse.com.tw/rwd/zh/fund/T86"
    try:
        r = httpx.get(
            url,
            params={"date": date_yyyymmdd, "selectType": "ALLBUT0999", "response": "json"},
            headers=HEADERS,
            timeout=30,
        )
        payload = r.json()
    except Exception:
        return None
    if payload.get("stat") != "OK":
        _write_json(path, [])
        return []
    fields = payload.get("fields") or []
    rows = []
    for row in payload.get("data") or []:
        item = dict(zip(fields, row))
        code = str(item.get("證券代號", "")).strip()
        if not code:
            continue
        rows.append(
            {
                "code": code,
                "name": str(item.get("證券名稱", "")).strip(),
                "foreign": _parse_int(item.get("外陸資買賣超股數(不含外資自營商)")),
                "trust": _parse_int(item.get("投信買賣超股數")),
                "dealer": _parse_int(item.get("自營商買賣超股數")),
                "total": _parse_int(item.get("三大法人買賣超股數")),
            }
        )
    _write_json(path, rows)
    return rows


def load_t86_history(n_days: int = 10) -> dict[str, list[dict]]:
    """
    回傳 {date: [rows]}，只含真正有資料的交易日，最近 n_days 個。
    各檔可再組 10 日淨買超。
    """
    hist = []
    for d in _t86_dates(n_days):
        rows = fetch_t86_day(d)
        if rows:
            hist.append((d, rows))
        if len(hist) >= n_days:
            break
        time.sleep(0.15)
    return {d: rows for d, rows in hist}


def t86_by_code(history: dict[str, list[dict]], code: str) -> list[dict]:
    """時間由舊到新。"""
    series = []
    for date in sorted(history.keys()):
        for row in history[date]:
            if row["code"] == code:
                item = dict(row)
                item["date"] = date
                series.append(item)
                break
    return series


def load_margin_map() -> dict[str, dict]:
    """證交所 MI_MARGN 融資融券（盤後）。{代號: {margin, short, margin_chg}}。"""
    path = CACHE / "t86" / "margin_latest.json"
    cached = _read_json(path, T86_TTL)
    if cached is not None:
        return cached
    url = "https://www.twse.com.tw/rwd/zh/marginTrading/MI_MARGN"
    try:
        r = httpx.get(url, params={"response": "json", "selectType": "ALL"}, headers=HEADERS, timeout=30)
        payload = r.json()
    except Exception:
        return {}
    if payload.get("stat") != "OK":
        return {}
    out = {}
    tables = payload.get("tables") or []
    if len(tables) < 2:
        return {}
    for row in tables[1].get("data") or []:
        if not row or len(row) < 13:
            continue
        code = str(row[0]).strip()
        if not code.isdigit():
            continue
        margin = _parse_int(row[6])
        prev_m = _parse_int(row[5])
        short = _parse_int(row[12])
        out[code] = {"margin": margin, "margin_prev": prev_m, "margin_chg": margin - prev_m, "short": short}
    _write_json(path, out)
    return out


def load_exrights_upcoming() -> dict[str, dict]:
    """未來約一個月除權息（TWT49U）。"""
    path = CACHE / "t86" / "exrights.json"
    cached = _read_json(path, T86_TTL)
    if cached is not None:
        return cached
    today = datetime.now(TAIPEI).date()
    start = today.strftime("%Y%m%d")
    end = (today + timedelta(days=40)).strftime("%Y%m%d")
    url = "https://www.twse.com.tw/rwd/zh/exRight/TWT49U"
    try:
        r = httpx.get(url, params={"response": "json", "strDate": start, "endDate": end}, headers=HEADERS, timeout=30)
        payload = r.json()
    except Exception:
        return {}
    if payload.get("stat") != "OK":
        return {}
    out = {}
    for row in payload.get("data") or []:
        if not row or len(row) < 3:
            continue
        code = str(row[1]).strip()
        date_s = str(row[0])
        out[code] = {"date": date_s, "name": str(row[2]), "kind": str(row[6]) if len(row) > 6 else "", "value": row[5] if len(row) > 5 else None}
    _write_json(path, out)
    return out


def load_intraday_60m(symbol: str) -> pd.DataFrame | None:
    """近 5 日 60 分 K，用來確認收盤是否守住，不是盤中刺穿。"""
    path = CACHE / "ohlcv" / f"{symbol.replace('/', '_')}_60m.pkl"
    if path.exists() and now_ts() - path.stat().st_mtime <= 3600:
        try:
            df = pd.read_pickle(path)
            if df is not None and len(df) >= 6:
                return df
        except Exception:
            pass
    try:
        raw = yf.download(symbol, period="5d", interval="60m", auto_adjust=True, progress=False, threads=False)
        df = _normalize_ohlcv(raw, min_bars=4)
        if df is not None and len(df) >= 4:
            df.to_pickle(path)
            return df
    except Exception:
        return None
    return None


def summarize_intraday(df: pd.DataFrame | None) -> dict | None:
    if df is None or len(df) < 4:
        return None
    try:
        last_day = df.index[-1].date()
    except Exception:
        return None
    day = df[df.index.date == last_day] if hasattr(df.index, "date") else df.iloc[-7:]
    if len(day) < 2:
        day = df.iloc[-7:]
    hi, lo = float(day["high"].max()), float(day["low"].min())
    close = float(day["close"].iloc[-1])
    rng = hi - lo
    loc = (close - lo) / rng if rng > 0 else 0.5
    tp = (day["high"] + day["low"] + day["close"]) / 3.0
    vwap = float((tp * day["volume"]).sum() / max(float(day["volume"].sum()), 1))
    last_vol = float(day["volume"].iloc[-2:].sum())
    day_vol = float(day["volume"].sum())
    late = last_vol / day_vol if day_vol else 0
    return {
        "close_loc": round(loc, 3),
        "vwap": round(vwap, 4),
        "above_vwap": close >= vwap,
        "late_vol_share": round(late, 3),
        "weak_close": loc < 0.40,
    }

def _parse_num(value) -> float | None:
    s = str(value).replace(",", "").replace("--", "").replace("%", "").strip()
    if s in ("", "nan", "None", "null"):
        return None
    try:
        return float(s)
    except Exception:
        return None


def load_monthly_revenue() -> dict[str, dict]:
    """上市公司最新月營收（t187ap05_L 快照）。"""
    path = CACHE / "t86" / "monthly_rev.json"
    cached = _read_json(path, T86_TTL)
    if cached is not None:
        return cached
    url = "https://openapi.twse.com.tw/v1/opendata/t187ap05_L"
    try:
        r = httpx.get(url, headers=HEADERS, timeout=30)
        rows = r.json()
    except Exception:
        return {}
    if not isinstance(rows, list):
        return {}
    out = {}
    for row in rows:
        code = str(row.get("公司代號") or "").strip()
        if not code.isdigit():
            continue
        out[code] = {
            "ym": row.get("資料年月"),
            "yoy": _parse_num(row.get("營業收入-去年同月增減(%)")),
            "mom": _parse_num(row.get("營業收入-上月比較增減(%)")),
            "yoy_acc": _parse_num(row.get("累計營業收入-前期比較增減(%)")),
        }
    _write_json(path, out)
    return out


def load_twse_session_closes(date_yyyymmdd: str) -> dict[str, float] | None:
    """MI_INDEX 當日上市股票收盤。{}=休市已快取；None=失敗可重試。"""
    path = CACHE / "twse_mi" / f"{date_yyyymmdd}.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    cached = _read_json(path, 10 * 365 * 86400)
    if cached is not None:
        return cached
    url = "https://www.twse.com.tw/rwd/zh/afterTrading/MI_INDEX"
    try:
        r = httpx.get(
            url,
            params={"response": "json", "type": "ALLBUT0999", "date": date_yyyymmdd},
            headers=HEADERS,
            timeout=30,
        )
        payload = r.json()
    except Exception:
        return None
    if payload.get("stat") != "OK":
        _write_json(path, {})
        return {}
    out: dict[str, float] = {}
    for table in payload.get("tables") or []:
        fields = table.get("fields") or []
        if "證券代號" not in fields or "收盤價" not in fields:
            continue
        ci, pi = fields.index("證券代號"), fields.index("收盤價")
        for row in table.get("data") or []:
            if not row or len(row) <= max(ci, pi):
                continue
            code = str(row[ci]).strip()
            if not (code.isdigit() and len(code) == 4):
                continue
            px = _parse_num(row[pi])
            if px:
                out[code] = px
        if out:
            break
    if not out:
        return None
    _write_json(path, out)
    return out


def load_twse_rps(lookback: int = 120) -> dict[str, int]:
    """全上市報酬百分位（1–99）。分母是市場，不是看板池。"""
    path = CACHE / "t86" / "twse_rps.json"
    cached = _read_json(path, T86_TTL)
    if cached is not None:
        return cached
    mi_dir = CACHE / "twse_mi"
    n_cached = len(list(mi_dir.glob("*.json"))) if mi_dir.exists() else 0
    want = lookback
    max_new = 8 if n_cached >= 20 else 40
    today = datetime.now(TAIPEI).date()
    sessions: list[dict[str, float]] = []
    d = today
    tries = 0
    new_dl = 0
    while len(sessions) < want and tries < want + 50:
        tries += 1
        d -= timedelta(days=1)
        if d.weekday() >= 5:
            continue
        ds = d.strftime("%Y%m%d")
        was = (mi_dir / f"{ds}.json").exists()
        closes = load_twse_session_closes(ds)
        if closes is None:
            time.sleep(0.12)
            continue
        if closes:
            sessions.append(closes)
        if not was:
            new_dl += 1
            time.sleep(0.12)
            if new_dl >= max_new and len(sessions) >= 20:
                break
    sessions.reverse()
    if len(sessions) < 20:
        return {}
    first, last = sessions[0], sessions[-1]
    rets = {}
    for code, px_old in first.items():
        px_new = last.get(code)
        if px_new and px_old:
            rets[code] = px_new / px_old - 1.0
    items = sorted(rets.items(), key=lambda kv: kv[1])
    n = len(items)
    ranks = {}
    for i, (code, _) in enumerate(items):
        ranks[code] = max(1, min(99, int(round((i / max(n - 1, 1)) * 98 + 1))))
    _write_json(path, ranks)
    return ranks


def load_latest_twse_closes() -> dict[str, float]:
    """最近一份有資料的 MI_INDEX 收盤（給價格還原檢查對帳）。"""
    mi_dir = CACHE / "twse_mi"
    if not mi_dir.exists():
        return {}
    files = sorted(mi_dir.glob("*.json"), reverse=True)
    for f in files[:12]:
        cached = _read_json(f, 10 * 365 * 86400)
        if cached:
            return cached
    today = datetime.now(TAIPEI).date()
    for i in range(1, 8):
        ds = (today - timedelta(days=i)).strftime("%Y%m%d")
        closes = load_twse_session_closes(ds)
        if closes:
            return closes
    return {}

def _is_listed_equity_code(code: str) -> bool:
    """上市普通股：四碼數字，排除 00xx ETF／指數。"""
    return code.isdigit() and len(code) == 4 and not code.startswith("00")


def load_twse_industry_map() -> dict[str, str]:
    """openapi t187ap03_L 產業類別。失敗回空。"""
    path = CACHE / "t86" / "industry.json"
    cached = _read_json(path, 7 * 86400)
    if cached is not None:
        return cached
    url = "https://openapi.twse.com.tw/v1/opendata/t187ap03_L"
    try:
        r = httpx.get(url, headers=HEADERS, timeout=30)
        rows = r.json()
    except Exception:
        return {}
    if not isinstance(rows, list):
        return {}
    out: dict[str, str] = {}
    for row in rows:
        if not isinstance(row, dict):
            continue
        code = str(row.get("公司代號") or "").strip()
        if not _is_listed_equity_code(code):
            continue
        sector = str(row.get("產業類別") or row.get("產業別") or "").strip()
        if sector:
            out[code] = sector
    if out:
        _write_json(path, out)
    return out


def load_twse_session_bars(date_yyyymmdd: str) -> dict | None:
    """當日上市 OHLC。{code: {name,open,high,low,close,volume}}；{}=休市；None=失敗。"""
    path = CACHE / "twse_bar" / f"{date_yyyymmdd}.json"
    cached = _read_json(path, 10 * 365 * 86400)
    if cached is not None:
        return cached
    url = "https://www.twse.com.tw/rwd/zh/afterTrading/MI_INDEX"
    try:
        r = httpx.get(
            url,
            params={"response": "json", "type": "ALLBUT0999", "date": date_yyyymmdd},
            headers=HEADERS,
            timeout=30,
        )
        payload = r.json()
    except Exception:
        return None
    if payload.get("stat") != "OK":
        _write_json(path, {})
        return {}
    out: dict = {}
    for table in payload.get("tables") or []:
        fields = table.get("fields") or []
        need = ("證券代號", "開盤價", "最高價", "最低價", "收盤價")
        if any(k not in fields for k in need):
            continue
        ci = fields.index("證券代號")
        oi = fields.index("開盤價")
        hi = fields.index("最高價")
        li = fields.index("最低價")
        pi = fields.index("收盤價")
        ni = fields.index("證券名稱") if "證券名稱" in fields else None
        vi = fields.index("成交股數") if "成交股數" in fields else None
        for row in table.get("data") or []:
            if not row or len(row) <= max(ci, oi, hi, li, pi):
                continue
            code = str(row[ci]).strip()
            if not _is_listed_equity_code(code):
                continue
            close = _parse_num(row[pi])
            if not close:
                continue
            open_px = _parse_num(row[oi]) or close
            high = _parse_num(row[hi]) or close
            low = _parse_num(row[li]) or close
            vol = _parse_int(row[vi]) if vi is not None and len(row) > vi else 0
            name = str(row[ni]).strip() if ni is not None and len(row) > ni else ""
            out[code] = {
                "name": name,
                "open": open_px,
                "high": high,
                "low": low,
                "close": close,
                "volume": vol,
            }
        if out:
            break
    if not out:
        return None
    _write_json(path, out)
    return out


def load_twse_listed_rows() -> list[dict]:
    """最新交易日全部上市普通股（代號／名稱／產業）。"""
    industry = load_twse_industry_map()
    today = datetime.now(TAIPEI).date()
    bars = None
    d = today
    for _ in range(14):
        if d.weekday() < 5:
            got = load_twse_session_bars(d.strftime("%Y%m%d"))
            if got:
                bars = got
                break
        d -= timedelta(days=1)
    if not bars:
        return []
    rows = []
    for code, rec in sorted(bars.items()):
        rows.append(
            {
                "symbol": f"{code}.TW",
                "name": rec.get("name") or code,
                "sector": industry.get(code) or "",
                "market": "TW",
            }
        )
    return rows


def load_twse_market_ohlcv(lookback: int = 260, max_new: int = 80) -> dict[str, pd.DataFrame]:
    """用 MI_INDEX 日K組全市場 DataFrame（鍵為 2330.TW）。首次會補歷史，之後走快取。"""
    bar_dir = CACHE / "twse_bar"
    n_cached = len(list(bar_dir.glob("*.json"))) if bar_dir.exists() else 0
    want = lookback
    cap_new = max_new if n_cached >= 40 else max(max_new, 120)
    today = datetime.now(TAIPEI).date()
    sessions: list[tuple[str, dict]] = []
    d = today
    tries = 0
    new_dl = 0
    while len(sessions) < want and tries < want + 80:
        tries += 1
        d -= timedelta(days=1)
        if d.weekday() >= 5:
            continue
        ds = d.strftime("%Y%m%d")
        was = (bar_dir / f"{ds}.json").exists()
        bars = load_twse_session_bars(ds)
        if bars is None:
            time.sleep(0.12)
            continue
        if bars:
            sessions.append((ds, bars))
        if not was:
            new_dl += 1
            time.sleep(0.12)
            if new_dl >= cap_new and len(sessions) >= 40:
                break
    sessions.reverse()
    if len(sessions) < 40:
        return {}
    by_code: dict[str, list] = {}
    for ds, bars in sessions:
        ts = pd.Timestamp(ds)
        for code, rec in bars.items():
            by_code.setdefault(code, []).append(
                {
                    "date": ts,
                    "open": rec["open"],
                    "high": rec["high"],
                    "low": rec["low"],
                    "close": rec["close"],
                    "volume": rec.get("volume") or 0,
                }
            )
    out: dict[str, pd.DataFrame] = {}
    for code, rows in by_code.items():
        if len(rows) < 40:
            continue
        df = pd.DataFrame(rows).set_index("date").sort_index()
        out[f"{code}.TW"] = df
    return out


def load_symbol_ohlcv(symbol: str, lookback: int = 260) -> pd.DataFrame | None:
    """單檔日K：台股走 twse_bar 快取，美股走 ohlcv pkl。不掃全市場。"""
    symbol = (symbol or "").upper()
    if not symbol:
        return None
    code = symbol.split(".")[0]
    if symbol.endswith(".TW") or symbol.endswith(".TWO") or (code.isdigit() and 3 <= len(code) <= 6):
        bar_dir = CACHE / "twse_bar"
        if not bar_dir.exists():
            got = load_ohlcv_many([symbol], period="1y")
            return got.get(symbol)
        files = sorted(bar_dir.glob("*.json"))[-lookback:]
        rows: list[dict] = []
        for f in files:
            payload = _read_json(f, 10 * 365 * 86400)
            if not isinstance(payload, dict):
                continue
            rec = payload.get(code)
            if not rec:
                continue
            try:
                ts = pd.Timestamp(f.stem)
            except Exception:
                continue
            rows.append(
                {
                    "date": ts,
                    "open": rec.get("open"),
                    "high": rec.get("high"),
                    "low": rec.get("low"),
                    "close": rec.get("close"),
                    "volume": rec.get("volume") or 0,
                }
            )
        if len(rows) >= 40:
            return pd.DataFrame(rows).set_index("date").sort_index()
        # 日K快取不足（每日潛力股只留一盤）→ 單檔上網 Yahoo
        got = load_ohlcv_many([symbol], period="1y")
        return got.get(symbol)
    path = CACHE / "ohlcv" / f"{symbol.replace('/', '_')}.pkl"
    if path.exists():
        try:
            return pd.read_pickle(path)
        except Exception:
            pass
    got = load_ohlcv_many([symbol], period="2y")
    return got.get(symbol)


def cached_quote_meta(symbol: str) -> dict | None:
    symbol = (symbol or "").upper()
    if not symbol:
        return None
    return _read_json(CACHE / "lookup" / f"{symbol.replace('/', '_')}.json", 14 * 86400)


def remember_quote_meta(row: dict) -> dict:
    sym = str((row or {}).get("symbol") or "").upper()
    if not sym:
        return row or {}
    item = {
        "symbol": sym,
        "name": str(row.get("name") or sym),
        "sector": str(row.get("sector") or ""),
        "market": row.get("market") or ("TW" if sym.endswith(".TW") or sym.endswith(".TWO") else "US"),
    }
    try:
        _write_json(CACHE / "lookup" / f"{sym.replace('/', '_')}.json", item)
    except Exception:
        pass
    return item


def guess_yahoo_symbol(q: str) -> str | None:
    s = (q or "").strip().upper().replace("．", ".")
    if not s:
        return None
    if s.endswith(".TW") or s.endswith(".TWO"):
        return s
    raw = s.replace(".TW", "").replace(".TWO", "")
    if raw.isdigit() and 3 <= len(raw) <= 6:
        return f"{raw}.TW"
    if 4 <= len(raw) <= 7 and raw[:4].isdigit() and raw.isalnum():
        return f"{raw}.TW"
    if 1 <= len(s) <= 10 and all(c.isascii() and (c.isalnum() or c in ".-") for c in s) and not s.isdigit():
        return s
    return None


def resolve_yahoo_meta(symbol: str) -> dict | None:
    """單檔名稱：Yahoo info。本地 lookup 快取優先。"""
    symbol = (symbol or "").upper()
    got = cached_quote_meta(symbol)
    code = symbol.replace(".TW", "").replace(".TWO", "")
    if got and (got.get("name") or "") not in ("", symbol, code):
        return got
    try:
        raw = yf.Ticker(symbol).info or {}
        name = raw.get("shortName") or raw.get("longName") or raw.get("displayName")
        if name:
            return remember_quote_meta(
                {
                    "symbol": symbol,
                    "name": str(name),
                    "sector": str(raw.get("sector") or raw.get("quoteType") or ""),
                    "market": "TW" if symbol.endswith(".TW") or symbol.endswith(".TWO") else "US",
                }
            )
    except Exception:
        pass
    return got


def search_live_quotes(q: str, limit: int = 8) -> list:
    """STOCKGENIE_LOOKUP_LIVE_V1：名單沒有就上網查 Yahoo，不掃全市場。"""
    q = (q or "").strip()
    if not q:
        return []
    safe = "".join(
        ch if ch.isalnum() or ch in "._-" or "\u4e00" <= ch <= "\u9fff" else "_"
        for ch in q
    )[:80] or "q"
    path = CACHE / "lookup" / f"q_{safe}.json"
    cached = _read_json(path, 6 * 3600)
    if isinstance(cached, list) and cached:
        return cached[:limit]
    out: list[dict] = []
    payload: dict = {}
    try:
        r = httpx.get(
            "https://query1.finance.yahoo.com/v1/finance/search",
            params={
                "q": q,
                "lang": "zh-Hant-TW",
                "region": "TW",
                "quotesCount": max(4, min(int(limit or 8), 12)),
                "newsCount": 0,
                "listsCount": 0,
            },
            headers=HEADERS,
            timeout=12,
        )
        if r.status_code == 200:
            payload = r.json() or {}
    except Exception:
        payload = {}
    seen: set[str] = set()
    for row in payload.get("quotes") or []:
        if not isinstance(row, dict):
            continue
        sym = str(row.get("symbol") or "").upper().strip()
        if not sym or sym in seen or "=" in sym:
            continue
        qtype = str(row.get("quoteType") or "").upper()
        if qtype in ("OPTION", "CRYPTOCURRENCY", "FUTURE", "CURRENCY"):
            continue
        name = str(row.get("shortname") or row.get("longname") or row.get("shortName") or "").strip()
        if not name:
            continue
        market = "TW" if sym.endswith(".TW") or sym.endswith(".TWO") else "US"
        sector = str(row.get("sector") or row.get("industry") or qtype or "").strip()
        seen.add(sym)
        out.append(remember_quote_meta({"symbol": sym, "name": name, "sector": sector, "market": market}))
        if len(out) >= limit:
            break
    guess = guess_yahoo_symbol(q)
    if guess and guess not in seen:
        meta = resolve_yahoo_meta(guess)
        if meta:
            out.insert(0, meta)
    try:
        _write_json(path, out)
    except Exception:
        pass
    return out[: max(1, min(int(limit or 8), 20))]
