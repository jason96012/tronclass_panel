"""StockGenie 智慧選股 API。只聽 127.0.0.1（由面板 /admin/stock-api 反代）。"""

from __future__ import annotations

import json
import math
import threading
import time
import traceback
import gc
from datetime import datetime, timedelta, timezone
from pathlib import Path

from fastapi import FastAPI, HTTPException
from fastapi.responses import FileResponse, JSONResponse
from fastapi.staticfiles import StaticFiles

from collections import defaultdict

from backtest import pack_symbol_hist, run_market, run_symbol
from forecast import analog_pack, build_forecast
from data import (
    load_exrights_upcoming,
    load_info_many,
    load_intraday_60m,
    load_margin_map,
    load_monthly_revenue,
    load_ohlcv_many,
    load_t86_history,
    load_latest_twse_closes,
    load_latest_twse_session,
    load_symbol_ohlcv,
    load_twse_rps,
    load_info,
    summarize_intraday,
    t86_by_code,
)
from news import load_news
from pipeline import analyze, summary_row
from portfolio import apply as apply_portfolio, beta_vs
from positions import close_position, list_open, open_position
from regime import detect_regime
from scheduler import write_daily_report
from scorer import rps_ranks
from universe import by_market, meta as uni_meta, ranking_symbols, search_symbols

ROOT = Path(__file__).resolve().parent
STATIC = ROOT / "static"
TAIPEI = timezone(timedelta(hours=8))

class SafeJSON(JSONResponse):
    """numpy／NaN／Timestamp 不能直接 json.dumps，否則詳情 HTTP 500。"""

    def render(self, content) -> bytes:
        cleaned = _clean_json(content)
        try:
            text = json.dumps(cleaned, ensure_ascii=False, allow_nan=False, default=str)
        except Exception:
            try:
                text = json.dumps(cleaned, ensure_ascii=False, allow_nan=True, default=str)
                text = (
                    text.replace(": NaN", ": null")
                    .replace(": Infinity", ": null")
                    .replace(": -Infinity", ": null")
                )
            except Exception as exc:
                text = json.dumps({"error": f"序列化失敗：{exc}"}, ensure_ascii=False)
        return text.encode("utf-8")


app = FastAPI(
    title="StockGenie",
    version="2.0",
    default_response_class=SafeJSON,
    docs_url=None,
    redoc_url=None,
    openapi_url=None,
)
app.mount("/static", StaticFiles(directory=str(STATIC)), name="static")


@app.exception_handler(Exception)
def _unhandled(_, exc):
    if isinstance(exc, HTTPException):
        detail = exc.detail if isinstance(exc.detail, str) else "request error"
        return SafeJSON({"error": detail}, status_code=exc.status_code)
    traceback.print_exc()
    return SafeJSON({"error": "內部錯誤"}, status_code=500)

_lock = threading.Lock()
_state = {
    "phase": "idle",
    "progress": 0,
    "message": "尚未掃描",
    "updated": None,
    "error": None,
}
_cache: dict[str, dict] = {"TW": {}, "US": {}}  # symbol -> full payload without bars
_ohlcv = {}
_t86 = {}
_backtest: dict[str, dict] = {}
_hist: dict[str, dict] = {}
_regime: dict[str, dict] = {}
_scan_busy: set[str] = set()
# STOCKGENIE_LOOKUP_FULL_V1：搜尋個股的完整詳情，不寫進每日 30 檔看板。
_detail_memo: dict[str, dict] = {}
_DETAIL_MEMO_MAX = 40


def _set(**kwargs):
    with _lock:
        _state.update(kwargs)
        if "message" in kwargs:
            _state["updated"] = datetime.now(TAIPEI).strftime("%Y-%m-%d %H:%M:%S")


def _hist_for(symbol: str, df, market: str) -> dict:
    with _lock:
        cached = _hist.get(symbol)
        if cached is not None:
            return cached
        if market == "TW":
            idx = _ohlcv.get("^TWII") or _ohlcv.get("0050.TW")
            vix = None
        else:
            idx = _ohlcv.get("^GSPC")
            vix = _ohlcv.get("^VIX")
    try:
        raw = run_symbol(df, market, idx, vix)
        hist = pack_symbol_hist(raw)
    except Exception:
        hist = pack_symbol_hist(None)
    with _lock:
        _hist[symbol] = hist
    return hist


def _code_of(symbol: str) -> str:
    return symbol.split(".")[0]


def _clean_json(obj, depth: int = 0):
    if depth > 40:
        return str(obj)
    if obj is None or isinstance(obj, (str, bool)):
        return obj
    if isinstance(obj, int) and not isinstance(obj, bool):
        return int(obj)
    if isinstance(obj, float):
        if math.isnan(obj) or math.isinf(obj):
            return None
        return float(obj)
    if isinstance(obj, dict):
        return {str(k): _clean_json(v, depth + 1) for k, v in obj.items()}
    if isinstance(obj, (list, tuple, set)):
        return [_clean_json(v, depth + 1) for v in obj]
    if isinstance(obj, datetime):
        return obj.strftime("%Y-%m-%d %H:%M:%S")
    name = type(obj).__name__
    mod = type(obj).__module__ or ""
    if name in ("NAType", "NaTType"):
        return None
    if name in ("Timestamp", "datetime64", "date", "time"):
        try:
            if hasattr(obj, "strftime"):
                return obj.strftime("%Y-%m-%d")
            return obj.isoformat()
        except Exception:
            return str(obj)
    if mod.startswith("numpy") or name in ("int64", "int32", "float64", "float32", "bool_", "bool8"):
        try:
            if hasattr(obj, "shape") and getattr(obj, "shape", None) == ():
                return _clean_json(obj.item(), depth + 1)
            if hasattr(obj, "tolist"):
                return _clean_json(obj.tolist(), depth + 1)
            if hasattr(obj, "item"):
                return _clean_json(obj.item(), depth + 1)
        except Exception:
            return str(obj)
    return str(obj)


def _norm_symbol(symbol: str) -> str:
    from data import guess_yahoo_symbol

    s = (symbol or "").strip().upper().replace("．", ".")
    got = guess_yahoo_symbol(s)
    if got:
        return got
    if 1 <= len(s) <= 16 and all(c.isascii() and (c.isalnum() or c in ".-") for c in s):
        return s
    return ""


def _df_of(symbol: str):
    with _lock:
        df = _ohlcv.get(symbol)
    if df is not None:
        return df
    df = load_symbol_ohlcv(symbol)
    if df is None:
        got = load_ohlcv_many([symbol], period="1y")
        df = got.get(symbol)
    if df is not None:
        with _lock:
            _ohlcv[symbol] = df
    return df


def _remember_detail(symbol: str, full: dict) -> None:
    slim = {k: v for k, v in full.items() if k != "bars"}
    with _lock:
        _detail_memo[symbol] = slim
        extra = len(_detail_memo) - _DETAIL_MEMO_MAX
        if extra > 0:
            for k in list(_detail_memo.keys())[:extra]:
                _detail_memo.pop(k, None)


def _offpool_context(symbol: str, market: str, df) -> dict:
    """搜尋個股補齊與潛力股同一套輸入：基本面、T86、RPS、月營收、大盤。失敗就缺那一塊，不 502。"""
    info: dict = {}
    try:
        from concurrent.futures import ThreadPoolExecutor

        with ThreadPoolExecutor(max_workers=1) as pool:
            info = pool.submit(load_info, symbol).result(timeout=12) or {}
    except Exception:
        info = {}

    t86_hist: dict = {}
    with _lock:
        t86_hist = dict(_t86)
    if market == "TW" and not t86_hist:
        try:
            t86_hist = load_t86_history(20)
            with _lock:
                if t86_hist and not _t86:
                    _t86.update(t86_hist)
        except Exception:
            t86_hist = {}
    series = t86_by_code(t86_hist, _code_of(symbol)) if market == "TW" else []

    margin = exdiv = monthly = official = None
    official_asof = None
    rps = None
    if market == "TW":
        code = _code_of(symbol)
        try:
            margin = load_margin_map().get(code)
        except Exception:
            pass
        try:
            exdiv = load_exrights_upcoming().get(code)
        except Exception:
            pass
        try:
            monthly = load_monthly_revenue().get(code)
        except Exception:
            pass
        try:
            official_asof, closes = load_latest_twse_session()
            official = (closes or {}).get(code)
        except Exception:
            official = None
            official_asof = None
        try:
            rps = load_twse_rps().get(code)
        except Exception:
            rps = None

    with _lock:
        ohlcv_all = dict(_ohlcv)
        regime = _regime.get(market)
    ohlcv_all[symbol] = df
    if rps is None:
        rets = {}
        for s, d in ohlcv_all.items():
            if d is None or len(d) < 21:
                continue
            try:
                if len(d) > 120:
                    rets[s] = float(d["close"].iloc[-1] / d["close"].iloc[-120] - 1)
                else:
                    rets[s] = float(d["close"].iloc[-1] / d["close"].iloc[0] - 1)
            except Exception:
                continue
        rps = rps_ranks(rets).get(symbol)
    sector_rs = None
    factor_rank = None
    try:
        ranks = {symbol: rps} if rps is not None else {}
        sector_rs = _sector_rs(ohlcv_all).get(symbol)
        factor_rank = _factor_ranks({symbol: df}, ranks).get(symbol)
    except Exception:
        pass
    if regime is None:
        try:
            if market == "TW":
                idx_map = load_ohlcv_many(["^TWII", "0050.TW"], period="2y")
                idx = idx_map.get("^TWII") or idx_map.get("0050.TW")
                regime = detect_regime("TW", idx, breadth=None)
            else:
                idx_map = load_ohlcv_many(["^GSPC", "^VIX"], period="2y")
                idx = idx_map.get("^GSPC")
                regime = detect_regime("US", idx, idx_map.get("^VIX"), breadth=None)
            with _lock:
                _ohlcv.update(idx_map)
                if regime:
                    _regime[market] = regime
        except Exception:
            regime = None
    beta = None
    try:
        idx = ohlcv_all.get("^TWII") if market == "TW" else ohlcv_all.get("^GSPC")
        if idx is None:
            with _lock:
                idx = _ohlcv.get("^TWII") if market == "TW" else _ohlcv.get("^GSPC")
        b = beta_vs(idx, df)
        beta = None if b is None else round(float(b), 2)
    except Exception:
        beta = None
    return {
        "info": info,
        "series": series,
        "rps": rps,
        "sector_rs": sector_rs,
        "factor_rank": factor_rank,
        "margin": margin,
        "exdiv": exdiv,
        "monthly": monthly,
        "official": official,
        "official_asof": official_asof,
        "regime": regime,
        "beta": beta,
    }


def _kick_scan(market: str, force: bool = False) -> None:
    market = market.upper()
    with _lock:
        if market in _scan_busy:
            return
        _scan_busy.add(market)

    def run():
        try:
            scan_market(market, force=force)
        except MemoryError:
            _set(phase="error", error="memory", message="掃描記憶體不足")
        except Exception as exc:
            _set(phase="error", error=str(exc), message="掃描失敗")
            traceback.print_exc()
        finally:
            with _lock:
                _scan_busy.discard(market)

    threading.Thread(target=run, daemon=True).start()


def _sector_rs(ohlcv: dict) -> dict[str, int]:
    buckets: dict[str, list[tuple[str, float]]] = defaultdict(list)
    for s, df in ohlcv.items():
        if len(df) < 21:
            continue
        ret = float(df["close"].iloc[-1] / df["close"].iloc[-20] - 1)
        buckets[uni_meta(s)["sector"]].append((s, ret))
    out = {}
    for items in buckets.values():
        items.sort(key=lambda kv: kv[1])
        n = len(items)
        for i, (s, _) in enumerate(items):
            out[s] = max(1, min(99, int(round((i / max(n - 1, 1)) * 98 + 1))))
    return out


def _factor_ranks(ohlcv: dict, rps: dict[str, int]) -> dict[str, int]:
    """qlib 風格的可解釋因子排序：RPS + 20 日動量百分位 + 量比百分位，不是 ML。"""
    mom, vr = {}, {}
    for s, df in ohlcv.items():
        if len(df) < 21:
            continue
        mom[s] = float(df["close"].iloc[-1] / df["close"].iloc[-20] - 1)
        v20 = float(df["volume"].iloc[-20:].mean())
        vr[s] = float(df["volume"].iloc[-1] / v20) if v20 > 0 else 1.0
    rm = rps_ranks(mom)
    rv = rps_ranks(vr)
    blend = {}
    for s in ohlcv:
        parts = [rps.get(s), rm.get(s), rv.get(s)]
        parts = [p for p in parts if p is not None]
        if parts:
            blend[s] = sum(parts) / len(parts)
    return rps_ranks(blend)


def _apply_crowding(packed: dict) -> None:
    """同一族群最多保留 2 檔可買（因子排序高者），其餘降為觀望。"""
    by_sec: dict[str, list] = defaultdict(list)
    for full in packed.values():
        v = full.get("verdict") or {}
        if v.get("key") in ("buy", "plan", "buy_breakout"):
            by_sec[full.get("sector") or ""].append(full)
    for sec, items in by_sec.items():
        if not sec or len(items) < 3:
            continue
        items.sort(
            key=lambda x: (x.get("factor_rank") or 0, (x.get("composite") or {}).get("score") or 0),
            reverse=True,
        )
        for loser in items[2:]:
            v = loser.get("verdict") or {}
            v["key"] = "wait"
            v["title"] = "觀望 · 族群擁擠"
            v["tone"] = "warn"
            v["reason"] = f"{sec} 已有兩檔可買，其餘降為觀望，避免同一筆交易重複曝險。"
            flags = list(v.get("flags") or [])
            if "族群擁擠" not in flags:
                flags.append("族群擁擠")
            v["flags"] = flags
            loser["verdict"] = v


def scan_market(market: str, force: bool = False) -> None:
    market = market.upper()
    if market == "TW":
        from picks import daily_tw_rows

        rows = daily_tw_rows(force=force)
        if not rows:
            rows = by_market(market)
    else:
        rows = by_market(market)
    symbols = [r["symbol"] for r in rows]
    if market == "TW":
        _set(phase="download", progress=5, message=f"下載今日潛力股日K（{len(symbols)} 檔，非全市場）…", error=None)
        need = list(dict.fromkeys(symbols + ["^TWII", "0050.TW"]))
        ohlcv_all = load_ohlcv_many(need, period="1y")
        ohlcv = {s: ohlcv_all[s] for s in symbols if s in ohlcv_all and len(ohlcv_all[s]) >= 40}
        if len(ohlcv) < 8:
            _set(phase="download", progress=8, message="潛力股日K不足，改抓精選後備池…")
            rank_syms = ranking_symbols(market)
            all_syms = list(dict.fromkeys(symbols + rank_syms[:24] + ["^TWII", "0050.TW"]))
            yf_map = load_ohlcv_many(all_syms, period="1y")
            ohlcv_all.update(yf_map)
            ohlcv = {s: ohlcv_all[s] for s in symbols if s in ohlcv_all and len(ohlcv_all[s]) >= 40}
            if not ohlcv:
                ohlcv = {s: df for s, df in yf_map.items() if s.endswith(".TW") and len(df) >= 40}
    else:
        _set(phase="download", progress=5, message=f"下載{market}日線 2 年（看板＋RPS 擴充池）…", error=None)
        rank_syms = ranking_symbols(market)
        all_syms = list(dict.fromkeys(symbols + rank_syms))
        ohlcv_all = load_ohlcv_many(all_syms, period="2y")
        ohlcv = {s: ohlcv_all[s] for s in symbols if s in ohlcv_all}
    with _lock:
        _ohlcv.update(ohlcv_all)

    t86_hist = {}
    if market == "TW":
        _set(phase="chips", progress=20, message="下載證交所三大法人 T86（近20日）…")
        t86_hist = load_t86_history(20)
        with _lock:
            _t86.clear()
            _t86.update(t86_hist)
        _set(phase="chips", progress=24, message="下載融資融券、除權息、月營收…")
        margin_map = load_margin_map()
        exrights = load_exrights_upcoming()
        monthly_map = load_monthly_revenue()
        market_rps = {}
    else:
        margin_map, exrights, monthly_map, market_rps = {}, {}, {}, {}

    info_syms = [s for s in symbols if s in ohlcv]
    _set(phase="info", progress=30, message=f"抓取基本面（{len(info_syms)} 檔）…")
    infos = load_info_many(info_syms)

    _set(phase="regime", progress=42, message="評估大盤狀態…")
    n_ma, n_ab = 0, 0
    for df in ohlcv.values():
        if len(df) < 21:
            continue
        n_ma += 1
        ma20 = float(df["close"].rolling(20).mean().iloc[-1])
        if float(df["close"].iloc[-1]) > ma20:
            n_ab += 1
    breadth = (n_ab / n_ma) if n_ma else None
    if market == "TW":
        idx_map = load_ohlcv_many(["^TWII", "0050.TW"], period="2y")
        idx = idx_map.get("^TWII")
        if idx is None:
            idx = idx_map.get("0050.TW")
        regime = detect_regime("TW", idx, breadth=breadth)
    else:
        idx_map = load_ohlcv_many(["^GSPC", "^VIX"], period="2y")
        idx = idx_map.get("^GSPC")
        regime = detect_regime("US", idx, idx_map.get("^VIX"), breadth=breadth)
    if idx is not None:
        try:
            regime["forecast"] = analog_pack(idx)
        except Exception:
            pass
    with _lock:
        _ohlcv.update(idx_map)
        _regime[market] = regime

    # RPS／族群 RS：擴充池分母
    rets = {}
    for s, df in ohlcv_all.items():
        if len(df) > 120:
            rets[s] = float(df["close"].iloc[-1] / df["close"].iloc[-120] - 1)
        elif len(df) > 20:
            rets[s] = float(df["close"].iloc[-1] / df["close"].iloc[0] - 1)
    ranks = rps_ranks(rets)
    if market == "TW" and market_rps:
        for s in list(ranks):
            code = _code_of(s)
            if code in market_rps:
                ranks[s] = market_rps[code]
    sec_rs = _sector_rs(ohlcv_all)
    factors = _factor_ranks(ohlcv, ranks)

    twse_asof, twse_closes = (None, {})
    if market == "TW":
        try:
            twse_asof, twse_closes = load_latest_twse_session()
        except Exception:
            twse_asof, twse_closes = None, {}
    _set(phase="score", progress=55, message=f"評分與買賣點聚合（{len(ohlcv)} 檔）…")
    packed = {}
    n = max(len(ohlcv), 1)
    for i, (sym, df) in enumerate(ohlcv.items()):
        try:
            series = t86_by_code(t86_hist, _code_of(sym)) if market == "TW" else []
            code = _code_of(sym)
            full = analyze(
                sym, df, infos.get(sym) or {}, series, ranks.get(sym),
                include_chart=False, regime=regime,
                sector_rs=sec_rs.get(sym), factor_rank=factors.get(sym),
                margin=margin_map.get(code),
                exdiv=exrights.get(code),
                monthly=monthly_map.get(code),
                official_close=twse_closes.get(code),
                official_asof=twse_asof,
            )
            packed[sym] = full
        except Exception as exc:
            packed[sym] = {"symbol": sym, "error": str(exc)}
        if i % 8 == 0:
            _set(progress=55 + int(40 * i / n), message=f"評分 {i+1}/{n}…")
        if i % 25 == 0:
            gc.collect()

    for sym, full in packed.items():
        if "verdict" not in full:
            continue
        b = beta_vs(idx_map.get("^TWII") if market == "TW" else idx_map.get("^GSPC"), ohlcv.get(sym))
        full["beta"] = None if b is None else round(b, 2)
    apply_portfolio(packed, list_open())

    # 每日約 30 檔日K留著給詳情；不要掃完就丟，否則點開又上網逾時 502。
    keep = {"^TWII", "0050.TW", "^GSPC", "^VIX"} | set(packed.keys())
    with _lock:
        _cache[market] = packed
        drop = [k for k in _hist if uni_meta(k)["market"] == market]
        for k in drop:
            _hist.pop(k, None)
        for k in [k for k in _ohlcv if k not in keep]:
            try:
                if uni_meta(k)["market"] == market:
                    _ohlcv.pop(k, None)
            except Exception:
                continue
    gc.collect()
    try:
        rows = [summary_row(f) for f in packed.values() if "verdict" in f]
        write_daily_report(market, rows, regime)
    except Exception:
        pass
    _set(phase="ready", progress=100, message=f"{market} 掃描完成，{len(packed)} 檔 · {regime.get('label','')}")


@app.on_event("startup")
def _boot():
    # 只掃台股；美股等使用者切換再掃。避免開機就把容器 RAM 打爆導致 sidecar 退出。
    _set(phase="idle", message="智選已啟動，背景掃描台股…")
    _kick_scan("TW")


@app.get("/")
def index():
    return FileResponse(STATIC / "index.html")


@app.get("/api/health")
def health():
    with _lock:
        st = dict(_state)
        st["tw"] = len(_cache.get("TW") or {})
        st["us"] = len(_cache.get("US") or {})
        st["regime"] = {k: v for k, v in _regime.items()}
    return st


@app.get("/api/lookup")
def api_lookup(q: str = "", market: str = "TW"):
    """STOCKGENIE_LOOKUP_V1：代號／名稱建議。名單沒有就上網。永不 500。"""
    q = (q or "").strip()[:40]
    hits = []
    try:
        hits = search_symbols(q)
    except Exception:
        traceback.print_exc()
        hits = []
    if not hits:
        try:
            from data import guess_yahoo_symbol

            g = guess_yahoo_symbol(q)
            if g:
                hits = [
                    {
                        "symbol": g,
                        "name": q,
                        "sector": "",
                        "market": "TW" if g.endswith(".TW") or g.endswith(".TWO") else "US",
                    }
                ]
        except Exception:
            pass
    m = (market or "TW").upper()
    if m in ("TW", "US") and hits:
        prefer = [h for h in hits if h.get("market") == m]
        rest = [h for h in hits if h.get("market") != m]
        hits = prefer + rest
    return {"q": q, "hits": hits[:12]}


@app.get("/api/scan")
def api_scan(market: str = "TW"):
    market = market.upper()
    if market not in ("TW", "US"):
        raise HTTPException(400, "market must be TW or US")
    with _lock:
        data = _cache.get(market) or {}
        st = dict(_state)
        regime = _regime.get(market)
        busy = market in _scan_busy
    if not data and not busy:
        _kick_scan(market)
    rows = []
    for full in data.values():
        if "error" in full and "verdict" not in full:
            continue
        try:
            rows.append(summary_row(full))
        except Exception:
            continue
    order = {"buy": 0, "buy_breakout": 0, "plan": 1, "trim": 2, "wait_pullback": 3}
    rows.sort(key=lambda r: (order.get(r["verdict_key"], 4), -(r.get("factor_rank") or 0), -(r["composite"] or -999)))
    return {"state": st, "market": market, "regime": regime, "rows": rows, "count": len(rows)}


@app.get("/api/stock/{symbol:path}")
def api_stock(symbol: str):
    """池內重用掃描結果；池外（搜尋）補齊同一套基本面／籌碼／預測。永不 502。"""
    symbol = _norm_symbol(symbol)
    if not symbol:
        return _stub_detail("INVALID", "代號不合法")
    try:
        return _pack_detail(symbol)
    except HTTPException as exc:
        detail = exc.detail if isinstance(exc.detail, str) else "request error"
        return _stub_detail(symbol, str(detail)[:200])
    except Exception:
        traceback.print_exc()
        return _stub_detail(symbol, "計算失敗")


def _stub_detail(symbol: str, reason: str) -> dict:
    meta = {}
    try:
        meta = uni_meta(symbol) or {}
    except Exception:
        meta = {}
    return _clean_json(
        {
            "symbol": symbol,
            "name": meta.get("name") or symbol,
            "sector": meta.get("sector") or "",
            "market": meta.get("market") or ("TW" if str(symbol).endswith(".TW") else "US"),
            "price": None,
            "asof": None,
            "live_ok": False,
            "timing": {
                "buy": {
                    "do": False,
                    "state": "blocked",
                    "title": "現在不要買",
                    "when": "無法計算買賣點，空手。",
                    "price": None,
                    "zone": None,
                    "abort": "",
                    "why": reason,
                },
                "sell": {
                    "do": False,
                    "state": "none",
                    "title": "未持有，不用賣",
                    "when": "沒有部位就沒有賣出動作。",
                    "stop": None,
                    "target": None,
                    "why": reason,
                },
            },
            "tech": {"score": 0, "items": [], "rsi": None},
            "fund": {"score": 0, "available": False, "items": []},
            "chips": None,
            "composite": {"score": 0, "bias": "—", "bias_key": "neutral"},
            "plan": {"buy": None, "stop": None, "target": None, "rr_plan": None},
            "setup": {},
            "regime": None,
            "signals": [],
            "patterns": [],
            "cyq": None,
            "rps": None,
            "checklist": [],
            "checks_ok": 0,
            "checks_n": 0,
            "resonance": False,
            "verdict": {
                "key": "wait",
                "title": "無法計算買賣點",
                "tone": "warn",
                "reason": reason,
                "flags": [],
                "entry": None,
                "stop": None,
                "target": None,
            },
            "targets": {
                "analyst": {"mean": None, "count": 0, "high": None, "low": None, "upside_pct": None},
                "valuation": None,
                "disclaimer": reason,
            },
            "integrity": None,
            "vp40": None,
            "beta": None,
            "hist": {"n": 0},
            "forecast": {"ok": False, "bias_label": reason},
            "news": {
                "ok": False,
                "items": [],
                "gate": False,
                "label": "沒有可用新聞",
                "note": reason,
            },
            "bars": [],
            "error": reason,
        }
    )


def _attach_bars(full: dict, df) -> dict:
    if df is None:
        full.setdefault("bars", [])
        return full
    try:
        from engine import add_indicators, bars_payload

        hard = full.get("status") == "DATA_MISMATCH" or (full.get("integrity") or {}).get("hard_stop")
        full["bars"] = bars_payload(df if hard else add_indicators(df), 180)
    except Exception:
        full["bars"] = []
    return full


def _pack_detail(symbol: str) -> dict:
    from data import cached_quote_meta
    from engine import add_indicators, bars_payload

    symbol = _norm_symbol(symbol)
    if not symbol:
        return _stub_detail("INVALID", "代號不合法")
    meta = uni_meta(symbol)
    try:
        live = cached_quote_meta(symbol)
        if live and live.get("name"):
            meta = {**meta, **{k: live[k] for k in ("name", "sector", "market") if live.get(k)}}
    except Exception:
        pass
    market = meta.get("market") or ("TW" if symbol.endswith(".TW") or symbol.endswith(".TWO") else "US")
    with _lock:
        cached = (_cache.get(market) or {}).get(symbol)
        memo = _detail_memo.get(symbol)
        regime = _regime.get(market)
    df = _df_of(symbol)
    if df is not None and len(df) < 80:
        try:
            got = load_ohlcv_many([symbol], period="2y")
            extra = got.get(symbol)
            if extra is not None and len(extra) > len(df):
                df = extra
                with _lock:
                    _ohlcv[symbol] = df
        except Exception:
            pass

    def _finish_payload(full: dict) -> dict:
        if meta.get("name"):
            full["name"] = meta["name"]
            full["sector"] = meta.get("sector") or full.get("sector") or ""
        hard = full.get("status") == "DATA_MISMATCH" or (full.get("integrity") or {}).get("hard_stop")
        full = _attach_bars(full, df)
        if hard:
            full["hist"] = {"n": 0}
            full["forecast"] = {"ok": False, "bias_label": "資料源不一致，停止產出技術訊號"}
            full["live_ok"] = False
        if not full.get("news"):
            try:
                full["news"] = load_news(symbol, full.get("name"))
            except Exception:
                full["news"] = {
                    "ok": False, "items": [], "gate": False,
                    "note": "新聞抓取失敗，不影響買賣點。",
                    "label": "沒有可用新聞",
                }
        if hard:
            return _clean_json(full)
        if not full.get("hist"):
            try:
                full["hist"] = _hist_for(symbol, df, market) if df is not None else {"n": 0}
            except Exception:
                full["hist"] = {"n": 0}
        if not full.get("forecast") or not (full.get("forecast") or {}).get("ok"):
            try:
                full["forecast"] = build_forecast(
                    df,
                    plan=full.get("plan"),
                    setup=full.get("setup"),
                    regime=full.get("regime") or regime,
                    hist=full.get("hist"),
                    tech=full.get("tech"),
                )
            except Exception:
                if not full.get("forecast"):
                    full["forecast"] = {"ok": False, "bias_label": "路徑計算失敗"}
        fc = full.get("forecast") or {}
        if full.get("live_ok") and (fc.get("overfit") or fc.get("no_edge") or fc.get("conflict") or fc.get("regime_shift")):
            full["live_ok"] = False
            plan = dict(full.get("plan") or {})
            plan["live_ok"] = False
            plan["sandbox"] = True
            full["plan"] = plan
            v = dict(full.get("verdict") or {})
            if v.get("key") in ("buy", "plan", "buy_breakout"):
                v["key"] = "wait_guard"
                v["title"] = "觀望 · 品質否決"
                v["tone"] = "warn"
                v["entry"] = None
                v["stop"] = None
                v["target"] = None
                v["reason"] = ((v.get("reason") or "") + " 預測／回測未通過品質閘門，不強行進場。").strip()
                flags = list(v.get("flags") or [])
                if "品質否決" not in flags:
                    flags.append("品質否決")
                v["flags"] = flags
                full["verdict"] = v
        return _clean_json(full)

    # 看板已算過：只補 K 線／預測，避免再打 Yahoo 全套而逾時 502
    if cached and cached.get("verdict"):
        return _finish_payload(dict(cached))
    if memo and memo.get("verdict"):
        return _finish_payload(dict(memo))
    if df is None:
        return _stub_detail(symbol, f"找不到 {symbol} 的日線（Yahoo／證交所皆無）")
    # STOCKGENIE_LOOKUP_FULL_V1：搜尋個股走與潛力股同一套 analyze＋hist＋forecast。
    ctx = _offpool_context(symbol, market, df)
    try:
        full = analyze(
            symbol, df, ctx["info"], ctx["series"], ctx["rps"], include_chart=True,
            regime=ctx["regime"] or regime,
            sector_rs=ctx["sector_rs"], factor_rank=ctx["factor_rank"],
            margin=ctx["margin"], exdiv=ctx["exdiv"], monthly=ctx["monthly"],
            official_close=ctx["official"],
            official_asof=ctx.get("official_asof"),
            beta=ctx["beta"],
        )
    except Exception as exc:
        traceback.print_exc()
        stub = _stub_detail(symbol, f"計算失敗：{exc}")
        try:
            stub["bars"] = bars_payload(add_indicators(df), 180)
            stub["price"] = round(float(df["close"].iloc[-1]), 4)
        except Exception:
            pass
        return stub
    if ctx.get("beta") is not None and full.get("beta") is None:
        full["beta"] = ctx["beta"]
    full["intraday"] = None
    hard = full.get("status") == "DATA_MISMATCH" or (full.get("integrity") or {}).get("hard_stop")
    if hard:
        full["hist"] = {"n": 0}
        full["forecast"] = {"ok": False, "bias_label": "資料源不一致，停止產出技術訊號"}
        full["live_ok"] = False
        _remember_detail(symbol, full)
        return _finish_payload(full)
    try:
        full["hist"] = _hist_for(symbol, df, market)
    except Exception:
        full["hist"] = {"n": 0}
    try:
        full["forecast"] = build_forecast(
            df,
            plan=full.get("plan"),
            setup=full.get("setup"),
            regime=full.get("regime") or ctx.get("regime") or regime,
            hist=full.get("hist"),
            tech=full.get("tech"),
        )
    except Exception:
        full["forecast"] = {"ok": False, "bias_label": "路徑計算失敗"}
    _remember_detail(symbol, full)
    return _finish_payload(full)


@app.post("/api/refresh")
def api_refresh(market: str = "TW"):
    market = market.upper()

    def run():
        try:
            scan_market(market, force=True)
        except Exception as exc:
            _set(phase="error", error=str(exc), message="重新掃描失敗")

    threading.Thread(target=run, daemon=True).start()
    return {"ok": True, "message": f"已開始重掃 {market}"}


@app.get("/api/report")
def api_report(market: str = "TW"):
    market = market.upper()
    with _lock:
        data = _cache.get(market) or {}
        st = dict(_state)
        regime = _regime.get(market)
    rows = []
    for full in data.values():
        if "verdict" not in full:
            continue
        rows.append(summary_row(full))
    reso = [r for r in rows if r["resonance"]]
    buys = [r for r in rows if r["verdict_key"] in ("buy", "plan", "buy_breakout")]
    breakouts = [r for r in rows if r["verdict_key"] == "buy_breakout"]
    skips = [r for r in rows if r["verdict_key"] == "skip"]
    tops = sorted(rows, key=lambda r: r["composite"] or -999, reverse=True)[:5]
    bots = sorted(rows, key=lambda r: r["composite"] or 999)[:5]
    bias_count = {}
    for r in rows:
        bias_count[r["bias"]] = bias_count.get(r["bias"], 0) + 1
    return {
        "state": st,
        "market": market,
        "n": len(rows),
        "bias": bias_count,
        "resonance": reso,
        "actionable": buys,
        "avoid": skips[:12],
        "top": tops,
        "bottom": bots,
        "generated": st.get("updated"),
        "regime": regime,
        "breakouts": breakouts,
    }


@app.get("/api/positions")
def api_positions():
    return {"positions": list_open()}


@app.post("/api/positions")
def api_pos_open(symbol: str, entry: float, stop: float, target: float | None = None, qty: float = 0, path: str = "pullback"):
    symbol = _norm_symbol(symbol)
    if not symbol:
        raise HTTPException(400, "invalid symbol")
    if path not in ("pullback", "breakout"):
        path = "pullback"
    return open_position(symbol, {"entry": entry, "stop": stop, "target": target, "qty": qty, "path": path})


@app.post("/api/positions/close")
def api_pos_close(symbol: str, reason: str = "manual"):
    symbol = _norm_symbol(symbol)
    if not symbol:
        raise HTTPException(400, "invalid symbol")
    return {"ok": True, "pos": close_position(symbol, reason[:40])}


@app.get("/api/backtest")
def api_backtest(market: str = "TW"):
    market = market.upper()
    with _lock:
        if market in _backtest:
            return _backtest[market]
        ohlcv = {s: df for s, df in _ohlcv.items() if uni_meta(s)["market"] == market}
    if len(ohlcv) < 5:
        return JSONResponse({"ready": False, "message": "日線尚未載入完成"}, status_code=202)
    _set(message=f"回測 {market} 進行中（五年 walk-forward）…")
    extra_idx = ["^TWII", "0050.TW"] if market == "TW" else ["^GSPC", "^VIX"]
    loaded = load_ohlcv_many(list(ohlcv.keys()) + extra_idx, period="5y")
    ohlcv = {s: loaded.get(s, df) for s, df in ohlcv.items()}
    idx = loaded.get("^TWII") if market == "TW" else loaded.get("^GSPC")
    if idx is None:
        idx = loaded.get("0050.TW") or _ohlcv.get("0050.TW")
    vix = loaded.get("^VIX") if market == "US" else None
    result = run_market(ohlcv, index_df=idx, market=market, vix_df=vix)
    result["ready"] = True
    result["market"] = market
    result["horizon"] = "5y"
    with _lock:
        _backtest[market] = result
    return result
