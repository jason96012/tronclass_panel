"""股票池：台股為每日輪換潛力股（約 30 檔）；美股仍為精選大盤股。
硬編碼 TW/US 清單當證交所資料尚未就緒時的後備。"""

TW = [
    ("2330.TW", "台積電", "半導體"),
    ("2454.TW", "聯發科", "半導體"),
    ("2303.TW", "聯電", "半導體"),
    ("3711.TW", "日月光投控", "半導體"),
    ("2379.TW", "瑞昱", "半導體"),
    ("3034.TW", "聯詠", "半導體"),
    ("2408.TW", "南亞科", "半導體"),
    ("2344.TW", "華邦電", "半導體"),
    ("3008.TW", "大立光", "半導體"),
    ("3661.TW", "世芯-KY", "半導體"),
    ("5269.TW", "祥碩", "半導體"),
    ("2308.TW", "台達電", "電子"),
    ("2317.TW", "鴻海", "代工"),
    ("2382.TW", "廣達", "代工"),
    ("2357.TW", "華碩", "代工"),
    ("3231.TW", "緯創", "代工"),
    ("4938.TW", "和碩", "代工"),
    ("6669.TW", "緯穎", "代工"),
    ("3017.TW", "奇鋐", "電子"),
    ("2395.TW", "研華", "電子"),
    ("2301.TW", "光寶科", "電子"),
    ("2327.TW", "國巨", "電子"),
    ("3037.TW", "欣興", "電子"),
    ("2345.TW", "智邦", "通信"),
    ("2412.TW", "中華電", "通信"),
    ("3045.TW", "台灣大", "通信"),
    ("4904.TW", "遠傳", "通信"),
    ("2881.TW", "富邦金", "金融"),
    ("2882.TW", "國泰金", "金融"),
    ("2886.TW", "兆豐金", "金融"),
    ("2891.TW", "中信金", "金融"),
    ("2884.TW", "玉山金", "金融"),
    ("2885.TW", "元大金", "金融"),
    ("2880.TW", "華南金", "金融"),
    ("2892.TW", "第一金", "金融"),
    ("5880.TW", "合庫金", "金融"),
    ("2887.TW", "台新金", "金融"),
    ("2890.TW", "永豐金", "金融"),
    ("5871.TW", "中租-KY", "金融"),
    ("1216.TW", "統一", "傳產"),
    ("1301.TW", "台塑", "傳產"),
    ("1303.TW", "南亞", "傳產"),
    ("1326.TW", "台化", "傳產"),
    ("2002.TW", "中鋼", "傳產"),
    ("2207.TW", "和泰車", "傳產"),
    ("2912.TW", "統一超", "傳產"),
    ("1101.TW", "台泥", "傳產"),
    ("2603.TW", "長榮", "航運"),
    ("2609.TW", "陽明", "航運"),
    ("2615.TW", "萬海", "航運"),
    ("6505.TW", "台塑化", "傳產"),
    ("9910.TW", "豐泰", "傳產"),
    ("8454.TW", "富邦媒", "傳產"),
    ("2105.TW", "正新", "傳產"),
    ("9904.TW", "寶成", "傳產"),
    ("2356.TW", "英業達", "代工"),
]

US = [
    ("AAPL", "蘋果", "科技"),
    ("MSFT", "微軟", "科技"),
    ("NVDA", "輝達", "半導體"),
    ("GOOGL", "Alphabet", "科技"),
    ("AMZN", "亞馬遜", "消費"),
    ("META", "Meta", "科技"),
    ("TSLA", "特斯拉", "汽車"),
    ("BRK-B", "波克夏B", "金融"),
    ("AVGO", "博通", "半導體"),
    ("LLY", "禮來", "醫療"),
    ("JPM", "摩根大通", "金融"),
    ("V", "Visa", "金融"),
    ("UNH", "聯合健康", "醫療"),
    ("XOM", "艾克森美孚", "能源"),
    ("JNJ", "嬌生", "醫療"),
    ("WMT", "沃爾瑪", "消費"),
    ("MA", "Mastercard", "金融"),
    ("PG", "寶僑", "消費"),
    ("HD", "家得寶", "消費"),
    ("CVX", "雪佛龍", "能源"),
    ("MRK", "默克", "醫療"),
    ("ABBV", "艾伯維", "醫療"),
    ("COST", "好市多", "消費"),
    ("PEP", "百事", "消費"),
    ("KO", "可口可樂", "消費"),
    ("ORCL", "甲骨文", "科技"),
    ("AMD", "超微", "半導體"),
    ("NFLX", "網飛", "科技"),
    ("ADBE", "Adobe", "科技"),
    ("CRM", "Salesforce", "科技"),
    ("QCOM", "高通", "半導體"),
    ("TXN", "德州儀器", "半導體"),
    ("AMAT", "應用材料", "半導體"),
    ("INTC", "英特爾", "半導體"),
    ("CSCO", "思科", "科技"),
    ("IBM", "IBM", "科技"),
    ("GE", "奇異", "工業"),
    ("CAT", "卡特彼勒", "工業"),
    ("BA", "波音", "工業"),
    ("DIS", "迪士尼", "消費"),
    ("NKE", "Nike", "消費"),
    ("MCD", "麥當勞", "消費"),
    ("PFE", "輝瑞", "醫療"),
    ("TMO", "賽默飛", "醫療"),
    ("ISRG", "直覺外科", "醫療"),
    ("NOW", "ServiceNow", "科技"),
    ("PANW", "Palo Alto", "科技"),
    ("CRWD", "CrowdStrike", "科技"),
    ("PLTR", "Palantir", "科技"),
    ("ASML", "ASML", "半導體"),
    ("TSM", "台積電(ADR)", "半導體"),
    ("ACN", "埃森哲", "科技"),
    ("LIN", "林德", "原物料"),
    ("GS", "高盛", "金融"),
    ("MS", "摩根士丹利", "金融"),
    ("BLK", "貝萊德", "金融"),
    ("HON", "霍尼韋爾", "工業"),
    ("UNP", "聯合太平洋", "工業"),
]


# 僅供 RPS／族群 RS 分母，不進看板。把權值池的百分位拉開。
RANK_TW = [
    ("3035.TW", "智原", "半導體"),
    ("3443.TW", "創意", "半導體"),
    ("6415.TW", "矽力*-KY", "半導體"),
    ("3533.TW", "嘉澤", "半導體"),
    ("3653.TW", "健策", "半導體"),
    ("3034.TW", "聯詠", "半導體"),
    ("2449.TW", "京元電子", "半導體"),
    ("3189.TW", "景碩", "半導體"),
    ("6239.TW", "力成", "半導體"),
    ("2409.TW", "友達", "電子"),
    ("3481.TW", "群創", "電子"),
    ("2474.TW", "可成", "電子"),
    ("1590.TW", "亞德客-KY", "電子"),
    ("2049.TW", "上銀", "電子"),
    ("2376.TW", "技嘉", "代工"),
    ("2377.TW", "微星", "代工"),
    ("2353.TW", "宏碁", "代工"),
    ("2324.TW", "仁寶", "代工"),
    ("3231.TW", "緯創", "代工"),  # 可能與看板重複，ranking_rows 會去重
    ("3702.TW", "大聯大", "電子"),
    ("3036.TW", "文曄", "電子"),
    ("2347.TW", "聯強", "電子"),
    ("2618.TW", "長榮航", "航運"),
    ("2610.TW", "華航", "航運"),
    ("2606.TW", "裕民", "航運"),
    ("2801.TW", "彰銀", "金融"),
    ("2834.TW", "臺企銀", "金融"),
    ("2883.TW", "開發金", "金融"),
    ("5876.TW", "上海商銀", "金融"),
    ("1102.TW", "亞泥", "傳產"),
    ("1402.TW", "遠東新", "傳產"),
    ("1476.TW", "儒鴻", "傳產"),
    ("1504.TW", "東元", "傳產"),
    ("1605.TW", "華新", "傳產"),
    ("1717.TW", "長興", "傳產"),
    ("1904.TW", "泉吉", "傳產"),
    ("2006.TW", "東鋼", "傳產"),
    ("2101.TW", "南港", "傳產"),
    ("2201.TW", "裕隆", "傳產"),
    ("2204.TW", "中華", "傳產"),
    ("2915.TW", "潤泰全", "傳產"),
    ("2542.TW", "興富發", "傳產"),
    ("9917.TW", "中保科", "傳產"),
    ("9921.TW", "巨大", "傳產"),
    ("9907.TW", "統一實", "傳產"),
    ("1210.TW", "大成", "傳產"),
    ("1227.TW", "佳格", "傳產"),
    ("2414.TW", "精技", "電子"),
    ("2360.TW", "致茂", "電子"),
    ("2385.TW", "群光", "電子"),
    ("6446.TW", "藥華藥", "傳產"),
    ("6770.TW", "力積電", "半導體"),
    ("4919.TW", "新唐", "半導體"),
    ("6176.TW", "瑞儀", "電子"),
    ("3406.TW", "玉晶光", "半導體"),
    ("3005.TW", "神基", "電子"),
    ("3023.TW", "信邦", "電子"),
    ("2337.TW", "旺宏", "半導體"),
    ("2451.TW", "創見", "電子"),
    ("3044.TW", "健鼎", "電子"),
    ("4958.TW", "臻鼎-KY", "電子"),
    ("3532.TW", "台勝科", "半導體"),
    ("8046.TW", "南電", "電子"),
    ("2059.TW", "川湖", "電子"),
    ("3665.TW", "貿聯-KY", "電子"),
    ("6409.TW", "旭隼", "電子"),
    ("6414.TW", "樺漢", "電子"),
    ("2633.TW", "台灣高鐵", "傳產"),
    ("9914.TW", "美利達", "傳產"),
    ("8464.TW", "億豐", "傳產"),
    ("9941.TW", "裕融", "金融"),
    ("6592.TW", "和潤企業", "金融"),
    ("2352.TW", "佳世達", "代工"),
    ("2392.TW", "正崴", "電子"),
    ("9945.TW", "潤泰新", "傳產"),
]

RANK_US = [
    ("ABT", "雅培", "醫療"),
    ("AMGN", "安進", "醫療"),
    ("GILD", "吉立亞", "醫療"),
    ("MDT", "美敦力", "醫療"),
    ("SYK", "史賽克", "醫療"),
    ("VRTX", "Vertex", "醫療"),
    ("REGN", "再生元", "醫療"),
    ("ELV", "Elevance", "醫療"),
    ("INTU", "Intuit", "科技"),
    ("ADP", "ADP", "科技"),
    ("IBM", "IBM", "科技"),
    ("NOW", "ServiceNow", "科技"),
    ("SHOP", "Shopify", "科技"),
    ("UBER", "Uber", "科技"),
    ("MU", "美光", "半導體"),
    ("KLAC", "KLA", "半導體"),
    ("LRCX", "拉姆研", "半導體"),
    ("SNPS", "新思", "半導體"),
    ("ARM", "Arm", "半導體"),
    ("WFC", "富國銀行", "金融"),
    ("BAC", "美銀", "金融"),
    ("C", "花旗", "金融"),
    ("SCHW", "嘉信", "金融"),
    ("AXP", "運通", "金融"),
    ("SPGI", "標普全球", "金融"),
    ("GS", "高盛", "金融"),
    ("VZ", "Verizon", "通信"),
    ("T", "AT&T", "通信"),
    ("CMCSA", "康卡斯特", "通信"),
    ("NEE", "NextEra", "能源"),
    ("RTX", "RTX", "工業"),
    ("LMT", "洛克希德", "工業"),
    ("DE", "迪爾", "工業"),
    ("HON", "霍尼韋爾", "工業"),
    ("LOW", " Lowe's", "消費"),
    ("TJX", "TJX", "消費"),
    ("SBUX", "星巴克", "消費"),
    ("BKNG", "Booking", "消費"),
    ("MELI", "MercadoLibre", "消費"),
    ("PYPL", "PayPal", "金融"),
    ("COIN", "Coinbase", "金融"),
    ("SNOW", "Snowflake", "科技"),
    ("APP", "AppLovin", "科技"),
    ("MSTR", "MicroStrategy", "科技"),
    ("CRWD", "CrowdStrike", "科技"),
    ("PANW", "Palo Alto", "科技"),
]


def all_rows():
    out = []
    for sym, name, sector in TW:
        out.append({"symbol": sym, "name": name, "sector": sector, "market": "TW"})
    for sym, name, sector in US:
        out.append({"symbol": sym, "name": name, "sector": sector, "market": "US"})
    return out


_listed_tw_cache: list | None = None
_listed_tw_by_sym: dict | None = None


def listed_tw_rows() -> list:
    global _listed_tw_cache, _listed_tw_by_sym
    if _listed_tw_cache is not None:
        return _listed_tw_cache
    try:
        from data import load_twse_listed_rows

        rows = load_twse_listed_rows() or []
    except Exception:
        rows = []
    _listed_tw_cache = rows
    _listed_tw_by_sym = {r["symbol"]: r for r in rows}
    return rows


def by_market(market: str):
    m = (market or "TW").upper()
    if m == "TW":
        try:
            from picks import daily_tw_rows

            rows = daily_tw_rows()
            if rows:
                return rows
        except Exception:
            pass
    return [r for r in all_rows() if r["market"] == m]


def ranking_rows(market: str | None = None):
    """看板池 + 排名擴充池，代號去重。"""
    seen = set()
    out = []
    extra = RANK_TW + RANK_US
    for src in (TW, US, extra):
        for sym, name, sector in src:
            if sym in seen:
                continue
            seen.add(sym)
            mkt = "TW" if sym.endswith(".TW") else "US"
            out.append({"symbol": sym, "name": name, "sector": sector, "market": mkt})
    if market:
        m = market.upper()
        out = [r for r in out if r["market"] == m]
    return out


def ranking_symbols(market: str) -> list[str]:
    return [r["symbol"] for r in ranking_rows(market)]


def _hit_has_name(h: dict) -> bool:
    if not h:
        return False
    name = (h.get("name") or "").strip()
    code = str(h.get("symbol") or "").replace(".TW", "").replace(".TWO", "")
    return bool(name) and name not in (h.get("symbol"), code)


def search_symbols(q: str, limit: int = 12) -> list:
    """STOCKGENIE_LOOKUP_V1：先查本地名單；沒有真實名稱就上網 Yahoo（STOCKGENIE_LOOKUP_LIVE_V1）。"""
    q = (q or "").strip()
    if not q:
        return []
    if len(q) > 40:
        q = q[:40]
    q_up = q.upper().replace("．", ".")
    pool = []
    seen = set()

    def add(row):
        if not row:
            return
        sym = row.get("symbol") or ""
        if not sym or sym in seen:
            return
        seen.add(sym)
        pool.append(row)

    for r in ranking_rows():
        add(r)
    try:
        for r in listed_tw_rows():
            add(r)
    except Exception:
        pass
    try:
        from data import cached_quote_meta, guess_yahoo_symbol

        g = guess_yahoo_symbol(q_up)
        if g:
            got = cached_quote_meta(g)
            if got:
                add(got)
    except Exception:
        pass
    hits = []
    for r in pool:
        sym = r["symbol"]
        name = r.get("name") or ""
        code = sym.replace(".TW", "").replace(".TWO", "")
        name_up = name.upper()
        if (
            q_up == code
            or q_up == sym.upper()
            or q in name
            or q_up in name_up
            or code.startswith(q_up)
            or sym.upper().startswith(q_up)
        ):
            hits.append(
                {
                    "symbol": sym,
                    "name": name,
                    "sector": r.get("sector") or "",
                    "market": r.get("market") or ("TW" if sym.endswith(".TW") or sym.endswith(".TWO") else "US"),
                }
            )
    hits.sort(
        key=lambda h: (
            0 if h["symbol"].replace(".TW", "").replace(".TWO", "") == q_up or h["symbol"].upper() == q_up else 1,
            0 if (h.get("name") or "").startswith(q) else 1,
            h["symbol"],
        )
    )
    if not any(_hit_has_name(h) for h in hits):
        try:
            from data import search_live_quotes

            live = search_live_quotes(q, limit=max(8, int(limit or 12)))
        except Exception:
            live = []
        by = {h["symbol"]: h for h in hits}
        for h in live:
            old = by.get(h["symbol"])
            if old is None:
                hits.append(h)
                by[h["symbol"]] = h
            elif not _hit_has_name(old):
                old["name"] = h.get("name") or old.get("name")
                old["sector"] = h.get("sector") or old.get("sector") or ""
    if not hits:
        try:
            from data import guess_yahoo_symbol

            g = guess_yahoo_symbol(q_up)
        except Exception:
            g = None
        if g:
            hits.append(
                {
                    "symbol": g,
                    "name": g.replace(".TW", "").replace(".TWO", ""),
                    "sector": "",
                    "market": "TW" if g.endswith(".TW") or g.endswith(".TWO") else "US",
                }
            )
    cap = max(1, min(int(limit or 12), 20))
    return hits[:cap]


def meta(symbol: str):
    symbol = (symbol or "").strip()
    for r in ranking_rows():
        if r["symbol"] == symbol:
            return r
    try:
        from data import cached_quote_meta, guess_yahoo_symbol

        want = guess_yahoo_symbol(symbol) or symbol
        got = cached_quote_meta(want)
        if got:
            return got
    except Exception:
        want = symbol
    if (symbol or "").endswith(".TW") or (symbol or "").endswith(".TWO") or str(symbol).isdigit():
        want = symbol if str(symbol).endswith(".TW") or str(symbol).endswith(".TWO") else f"{symbol}.TW"
        listed_tw_rows()
        if _listed_tw_by_sym and want in _listed_tw_by_sym:
            return _listed_tw_by_sym[want]
        return {"symbol": want, "name": symbol, "sector": "", "market": "TW"}
    return {"symbol": symbol, "name": symbol, "sector": "", "market": "US" if "." not in symbol else "TW"}
