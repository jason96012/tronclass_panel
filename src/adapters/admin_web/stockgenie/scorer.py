"""綜合分、檢查清單、三面向共振、買入裁決。"""

from __future__ import annotations

from typing import Any

from pro import EARNINGS_AFTER, EARNINGS_BEFORE, earnings_in_window, rec_is_buy, valuation_target


def composite(market: str, tech: float, fund: float | None, chip: float | None) -> dict[str, Any]:
    if market == "TW":
        t, f, c = 0.45, 0.30, 0.25
        fund_v = fund if fund is not None else 0.0
        chip_v = chip if chip is not None else 0.0
        if fund is None and chip is None:
            t, f, c = 1.0, 0.0, 0.0
        elif fund is None:
            t, f, c = 0.65, 0.0, 0.35
        elif chip is None:
            t, f, c = 0.55, 0.45, 0.0
        score = t * tech + f * fund_v + c * chip_v
        weights = {"tech": t, "fund": f, "chip": c}
    else:
        t, f = 0.55, 0.45
        fund_v = fund if fund is not None else 0.0
        if fund is None:
            t, f = 1.0, 0.0
        score = t * tech + f * fund_v
        weights = {"tech": t, "fund": f, "chip": 0.0}
    score = max(-100.0, min(100.0, score))
    if score >= 55:
        bias, bias_key = "看多（強）", "bull_strong"
    elif score >= 30:
        bias, bias_key = "看多（溫和）", "bull"
    elif score > -30:
        bias, bias_key = "中性盤整", "neutral"
    elif score > -55:
        bias, bias_key = "看空（溫和）", "bear"
    else:
        bias, bias_key = "看空（強）", "bear_strong"
    return {"score": round(score, 1), "bias": bias, "bias_key": bias_key, "weights": weights}


def checklist(
    market: str,
    df_last: dict,
    tech: dict,
    fund: dict,
    chips: dict | None,
    rps: int | None,
    regime: dict | None = None,
    setups: dict | None = None,
    plan: dict | None = None,
    sector_rs: int | None = None,
) -> list[dict]:
    price = df_last["close"]
    ma120 = tech.get("ma120")
    ma200 = tech.get("ma200")
    ma_score = next((i["score"] for i in tech["items"] if i["key"] == "均線架構"), 0)
    items = []
    setups = setups or {}
    plan = plan or {}
    regime = regime or {}

    def tick(key, ok, detail, group="info"):
        items.append({"key": key, "ok": bool(ok), "detail": detail, "group": group})

    tick("大盤允許多", regime.get("allow_long", True), regime.get("label") or "未評估", "gate")
    wkey = (setups.get("weekly") or {}).get("key")
    tick("週線非空", wkey not in ("bear", "bear_flat"), (setups.get("weekly") or {}).get("label") or "未評估", "gate")
    ed = fund.get("earnings_days")
    tick("非財報窗口", not earnings_in_window(ed), f"距財報 {ed} 日（窗口 −{EARNINGS_AFTER}..+{EARNINGS_BEFORE}）" if ed is not None else "無財報日（不攔截）", "gate")
    tick("非長上影", not setups.get("long_upper"), "當根上影未超過振幅 40% 且未達 0.4×ATR（亦排除射擊之星）", "gate")
    integ = setups.get("integrity") or {}
    tick("價格還原正常", integ.get("ok", True), "；".join(integ.get("issues") or []) or "OHLC 合法且無可疑跳空", "gate")
    tick("非除權息窗口", not setups.get("exdiv"), (setups.get("exdiv") or {}).get("date") or "近期無除權息", "gate")
    tick("非假突破", not setups.get("failed_breakout"), "昨收未跌回前高之下", "gate")
    bq = (plan.get("buy") or {}).get("quality")
    tick("買區品質 ≥ 40", bq is not None and bq >= 40, f"品質 {bq}／家族 { (plan.get('buy') or {}).get('n_src') }" if bq is not None else "無買區", "gate")
    tick("現價在買區（≤3%）", bool(setups.get("near_buy")), "在區內或距上緣≤3%" if setups.get("near_buy") else "不在買區，不能標可買", "gate")
    tick("回檔確認根", bool(setups.get("pullback_confirm")), "鎚頭／陽包陰／量縮或區內放量陽線" if setups.get("pullback_confirm") else "未確認", "gate")
    tick("族群相對強度 ≥ 40", sector_rs is None or sector_rs >= 40, f"族群 RS {sector_rs}" if sector_rs is not None else "未計算", "gate")
    tick("站上 200 日線", ma200 is not None and price >= ma200, f"MA200 {ma200:.2f}" if ma200 else "MA200 不足", "gate")

    tick("站上半年線 MA120", ma120 is not None and price >= ma120, f"現價 {price:.2f}／MA120 {ma120:.2f}" if ma120 else "MA120 不足", "bonus")
    tick("技術面多頭排列", ma_score >= 10, f"均線架構 {ma_score}/25", "bonus")
    tick("RPS ≥ 87", rps is not None and rps >= 87, f"RPS {rps}" if rps is not None else "未計算", "bonus")
    if market == "TW":
        net10 = (chips or {}).get("net10")
        tick("投信5日淨買且外資10日非賣", bool(chips and (chips.get("trust5") or 0) > 0 and (chips.get("foreign10") or 0) >= 0), f"投信5日 { (chips or {}).get('trust5') }／外資10日 { (chips or {}).get('foreign10') }" if chips else "無 T86", "bonus")
        tick("三大法人 10 日合計淨買", net10 is not None and net10 > 0, f"{net10:,} 股" if net10 is not None else "無 T86", "info")
    else:
        ok = rec_is_buy(fund.get("rec_key") or "", fund.get("rec_mean"))
        tick("分析師評級買進（美股無法人買賣超）", ok, fund.get("rec_key") or "無評級", "info")

    fpe = fund.get("forward_pe") or fund.get("pe")
    tick("預期PE < 25", fpe is not None and fpe < 25, f"{fpe:.1f}" if fpe else "無 PE", "info")
    roe = fund.get("roe")
    tick("ROE > 15%", roe is not None and roe > 15, f"{roe:.1f}%" if roe is not None else "無 ROE", "info")
    rg = fund.get("rev_growth")
    tick("營收年增 > 10%", rg is not None and rg > 10, f"{rg:.1f}%" if rg is not None else "無營收年增", "info")
    eg = fund.get("earn_growth")
    tick("獲利正成長", eg is not None and eg > 0, f"{eg:.1f}%" if eg is not None else "無獲利年增", "info")
    de = fund.get("debt_equity")
    tick("負債權益比 < 150%", de is not None and de < 150, f"{de:.0f}%" if de is not None else "無負債比", "info")
    tick("自由現金流為正", fund.get("fcf_positive") is True, "FCF>0" if fund.get("fcf_positive") else "FCF 非正或無資料", "info")
    return items


def resonance(market: str, tech_score: float, fund_score: float | None, chips: dict | None, fund: dict, price: float) -> bool:
    if fund_score is None or tech_score < 30 or fund_score < 20:
        return False
    if market == "TW":
        return bool(chips and (chips.get("net10") or 0) > 0)
    t = fund.get("target_mean")
    if not t or not price:
        return False
    return (t - price) / price > 0.05


def verdict(
    market: str,
    comp: dict,
    tech: dict,
    fund: dict,
    chips: dict | None,
    plan: dict,
    reso: bool,
    signals: list,
    setups: dict | None = None,
    regime: dict | None = None,
    sector_rs: int | None = None,
    rps: int | None = None,
) -> dict[str, Any]:
    """
    兩條產品互不否決：
    - 回檔單：最近支撐買區，RR<1.5 放棄，買區品質<40 不當計劃性買點。
    - 突破單：A 級（放量陽線＋RPS）用 2R 目標，不看最近壓力。
    大盤空頭／財報窗口（前15日至後7日）／週線空頭／長上影／除權息／價格還原異常 → 禁止可買。
    """
    flags = []
    setups = setups or {}
    regime = regime or {}
    rr = plan.get("rr_plan")
    rr_now = plan.get("rr_now")
    use_rr = rr if rr is not None else rr_now
    bias = comp["bias_key"]
    rsi = tech.get("rsi") or 50
    near_sell = any(s["code"] == "NEAR_SELL" for s in signals)
    near_buy = any(s["code"] == "NEAR_BUY" for s in signals) or setups.get("near_buy")
    grade = setups.get("breakout_grade")
    weekly_key = (setups.get("weekly") or {}).get("key")
    rvol = setups.get("rvol")
    dump = any(s["code"] == "RVOL_DUMP" for s in signals)
    ed = fund.get("earnings_days")
    earnings_blackout = earnings_in_window(ed)
    htf_bear = weekly_key in ("bear", "bear_flat")
    buy_valid = bool(plan.get("buy_valid"))
    bp = plan.get("breakout_plan")

    path = "none"
    key = title = tone = reason = None
    pb_confirm = bool(setups.get("pullback_confirm"))
    gap = float(setups.get("gap") or 0)
    ma_dead = any(s.get("code") == "MA_DEAD" for s in signals)
    regime_key = regime.get("key") or "unknown"
    sector_block = sector_rs is not None and sector_rs < 40
    allow_new = bool(regime.get("allow_long", True))

    if regime_key == "bear" or not allow_new:
        title = "觀望 · 大盤空頭" if regime_key == "bear" else "觀望 · 大盤警戒"
        key, tone = "wait_regime", "bad" if regime_key == "bear" else "warn"
        reason = (regime.get("label") or "大盤不允許多") + "。不開新多。"
        flags.append("大盤空頭" if regime_key == "bear" else "大盤警戒")
        path = "blocked"
    elif earnings_blackout:
        key, title, tone = "wait_event", "觀望 · 財報窗口", "warn"
        reason = f"距財報約 {ed} 日（窗口：前 {EARNINGS_BEFORE} 日至後 {EARNINGS_AFTER} 日）。跳空可能穿過停損，不開新倉。"
        flags.append("財報窗口")
        path = "blocked"
    elif setups.get("exdiv"):
        xd = setups["exdiv"]
        key, title, tone = "wait_event", "觀望 · 除權息", "warn"
        reason = f"即將除權息（{xd.get('date')} {xd.get('kind')}）。價位幾何會錯位，不當突破／回檔。"
        flags.append("除權息")
        path = "blocked"
    elif setups.get("integrity") and not setups["integrity"].get("ok", True):
        key, title, tone = "wait", "觀望 · 價格還原異常", "warn"
        reason = "；".join(setups["integrity"].get("issues") or ["OHLC／跳空異常"])
        flags.append("價格還原")
        path = "blocked"
    elif setups.get("long_upper"):
        key, title, tone = "wait", "觀望 · 長上影線", "warn"
        reason = "當根上影過長，買盤未能守住高點。突破與回檔買點一併剔除。"
        flags.append("長上影")
        path = "blocked"
    elif grade == "A" and setups.get("weak_close"):
        key, title, tone = "wait", "觀望 · 突破收弱", "warn"
        reason = "收盤落在當日振幅下段，盤中刺穿前高但沒守住。不當突破確認。"
        flags.append("收弱")
        path = "blocked"
    elif setups.get("failed_breakout"):
        key, title, tone = "wait", "觀望 · 假突破", "warn"
        reason = "昨收站上前高、今收跌回之下。結構沒守住，不追突破、也不把回檔當確認。"
        flags.append("假突破")
        path = "blocked"
    elif setups.get("monthly") and (setups["monthly"].get("yoy") is not None) and setups["monthly"]["yoy"] <= -20:
        yoy = setups["monthly"]["yoy"]
        key, title, tone = "wait_event", "觀望 · 月營收急縮", "warn"
        reason = f"最新月營收年增 {yoy:.1f}% ≤ −20%。事件風險未消化前不開新倉。"
        flags.append("月營收")
        path = "blocked"
    elif htf_bear:
        key, title, tone = "wait_htf", "觀望 · 週線空頭", "warn"
        reason = "週線仍空。日線回檔或突破都不開新多，避免多週期逆勢。"
        flags.append("週線空頭")
        path = "blocked"
    elif grade == "A" and bias in ("bull", "bull_strong", "neutral") and not sector_block:
        # 突破單：不被最近壓力的 RR 否決，也不因逼近壓力改減碼
        key, title, tone = "buy_breakout", "可買 · 突破確認", "good"
        reason = (
            f"放量陽線突破（RVOL {rvol}≥1.4"
            + (f"、RPS {rps}" if rps is not None else "")
            + "）。目標 1R/2R/3R 分批；走過 1R 後停損上移，不把最近前高當唯一賣點。"
        )
        flags.append("突破A級")
        path = "breakout"
        if reso:
            flags.append("共振")
        if setups.get("contraction", {}).get("vcp"):
            flags.append("VCP收縮")
        if setups.get("contraction", {}).get("htf"):
            flags.append("高窄旗")
    elif bias in ("bear", "bear_strong"):
        key, title, tone = "skip", "放棄 · 趨勢偏空", "bad"
        reason = "綜合分落在看空區。逆勢摸底不是這套系統的進場方式。"
        flags.append("看空")
        path = "pullback"
    elif near_sell and bias in ("bull", "bull_strong") and grade != "A":
        key, title, tone = "trim", "減碼 · 逼近壓力", "warn"
        reason = "回檔單的報酬被最近壓力壓縮（≤2.5%）。沒有放量突破確認時優先分批停利。"
        flags.append("逼近壓力")
        path = "pullback"
    elif sector_block and near_buy:
        key, title, tone = "wait", "觀望 · 弱勢族群", "warn"
        reason = f"同產業 20 日相對強度 {sector_rs}<40。不在落後族群開新倉。"
        flags.append("弱勢族群")
        path = "pullback"
    elif (not buy_valid) and near_buy and bias in ("bull", "bull_strong"):
        key, title, tone = "wait", "觀望 · 支撐品質不足", "warn"
        reason = "買區來源家族／有效彈回不夠（品質 <40），不當計劃性買點。"
        flags.append("弱支撐")
        path = "pullback"
    elif dump and near_buy:
        key, title, tone = "wait", "觀望 · 價跌量增", "warn"
        reason = f"回檔區出現 RVOL {rvol} 的賣壓。等量縮或鎚頭／陽包陰確認再執行。"
        flags.append("價跌量增")
        path = "pullback"
    elif near_buy and rsi > 80:
        key, title, tone = "wait", "觀望 · RSI 超買", "warn"
        reason = f"RSI {rsi:.0f}>80，回檔單不追。突破單須走 A 級放量陽線。"
        flags.append("RSI超買")
        path = "pullback"
    elif near_buy and ma_dead:
        key, title, tone = "wait", "觀望 · 均線死叉", "warn"
        reason = "近 20 日 MA20 下穿 MA60，回檔買點失效。"
        flags.append("均線死叉")
        path = "pullback"
    elif near_buy and gap > 0.03:
        key, title, tone = "wait", "觀望 · 跳空追價", "warn"
        reason = f"今開相對昨收跳空 {gap:.1%}。不在缺口上追回檔單。"
        flags.append("跳空")
        path = "pullback"
    elif near_buy and buy_valid and not pb_confirm:
        key, title, tone = "wait", "觀望 · 等確認根", "warn"
        reason = "人在買區，但沒有鎚頭／陽包陰／量縮或區內放量陽線。不把陰線當買點。"
        flags.append("缺確認根")
        path = "pullback"
    elif use_rr is not None and use_rr < 1.5 and grade != "A":
        key, title, tone = "skip", "放棄 · 風險報酬不足", "bad"
        reason = f"回檔單 RR {use_rr:.2f} < 1.5。停損到目標划不來。突破單須放量陽線才走另一條。"
        flags.append("RR<1.5")
        path = "pullback"
    elif reso and (use_rr is None or use_rr >= 1.5) and bias in ("bull", "bull_strong") and buy_valid and near_buy and pb_confirm:
        key, title, tone = "buy", "可買 · 三面向共振", "good"
        reason = "人在買區、確認根成立、品質與 RR 達標，技術／基本面／籌碼同向。不是保證獲利。"
        flags.append("共振")
        path = "pullback"
        if setups.get("confirm"):
            flags.append("K線確認")
        if (setups.get("third_buy") or {}).get("ok"):
            flags.append("結構三買")
    elif reso and (use_rr is None or use_rr >= 1.5) and bias in ("bull", "bull_strong") and buy_valid and not near_buy:
        key, title, tone = "wait_pullback", "觀望 · 共振等回檔", "neutral"
        reason = "三面向已共振，但現價不在買區（≤3%）。不把「方向對」當成「現在可下單」。"
        flags.append("共振")
        path = "watch"
    elif bias in ("bull", "bull_strong") and (use_rr is None or use_rr >= 1.5):
        if near_buy and buy_valid and pb_confirm:
            key, title, tone = "plan", "可買 · 計劃性買點", "good"
            reason = "趨勢仍在、價格回到有品質的買區，且確認根成立。用買區進場，不要市價追高。"
            path = "pullback"
            if setups.get("confirm"):
                flags.append("K線確認")
        elif grade == "B":
            key, title, tone = "wait", "觀望 · 突破未滿配", "warn"
            reason = "海龜或前高條件出現，但量能或 RPS 未達 A 級。等放量陽線再追，跌回不追。"
            flags.append("突破B級")
            path = "breakout"
        else:
            key, title, tone = "wait_pullback", "觀望 · 等回檔", "neutral"
            reason = "方向偏多，但現價不在買區、沒有確認根、也沒有 A 級突破。"
            path = "watch"
    else:
        key, title, tone = "wait", "觀望", "neutral"
        reason = "訊號尚未對齊。寧可空手，也不用模糊單去填別人的流動性。"
        path = "watch"

    if rsi > 80:
        flags.append("RSI超買")
    if plan.get("rr_poor") and path != "breakout":
        flags.append("性價比不佳")
    if regime.get("key") == "caution":
        flags.append("大盤警戒")
    if sector_rs is not None and sector_rs < 40:
        flags.append("弱勢族群")
    if htf_bear:
        flags.append("週線空頭")
    if setups.get("contraction", {}).get("vcp"):
        flags.append("VCP")

    if path == "breakout" and bp:
        entry = bp["entry"]
        stop = bp.get("live_stop") or bp["stop"]
        target = bp["t2"]
        use_rr = bp.get("rr") or 2.0
    elif key in ("buy", "plan"):
        entry = (plan.get("buy") or {}).get("mid")
        stop = plan.get("live_stop") or plan.get("stop")
        target = (plan.get("target") or {}).get("mid")
    else:
        entry = plan.get("price")
        stop = plan.get("live_stop") or plan.get("stop")
        target = (plan.get("target") or {}).get("mid")

    risk_pct = None
    if entry and stop and entry > stop:
        risk_pct = (entry - stop) / entry * 100

    return {
        "key": key,
        "title": title,
        "tone": tone,
        "reason": reason,
        "flags": flags,
        "rr": use_rr,
        "risk_pct": None if risk_pct is None else round(risk_pct, 2),
        "entry": entry,
        "stop": stop,
        "target": target,
        "path": path,
        "setup_kind": setups.get("kind"),
        "breakout_grade": grade,
    }


def targets_block(price: float, fund: dict, plan: dict) -> dict[str, Any]:
    t_mean = fund.get("target_mean")
    upside = None if not t_mean or not price else round((t_mean - price) / price * 100, 1)
    warned = bool(t_mean and price and price > t_mean)
    val = valuation_target(price, fund.get("pe"), fund.get("forward_pe"))
    return {
        "analyst": {
            "mean": t_mean,
            "high": fund.get("target_high"),
            "low": fund.get("target_low"),
            "count": fund.get("analyst_count"),
            "upside_pct": upside,
            "already_above": warned,
        },
        "valuation": val,
        "technical": plan.get("target"),
        "disclaimer": "分析師 12–18 個月目標價準確率約 30%（Investopedia）。三種目標是情境參考，不是承諾。",
    }


def rps_ranks(returns: dict[str, float]) -> dict[str, int]:
    """120 日漲幅在池內百分位，映射到 1–99。"""
    if not returns:
        return {}
    items = sorted(returns.items(), key=lambda kv: kv[1])
    n = len(items)
    out = {}
    for i, (sym, _) in enumerate(items):
        out[sym] = max(1, min(99, int(round((i / max(n - 1, 1)) * 98 + 1))))
    return out
