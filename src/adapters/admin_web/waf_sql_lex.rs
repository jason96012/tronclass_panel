//! C9／E1：SQLi 詞法分析與 token 層規則（純標準庫，不引入 sqlparser）。
//!
//! # 在偵測流程中的位置
//!
//! `waf_detect::detect_sqli` 的順序：
//! 1. 正規表達式規則（對 `normalize` 的結果）。
//! 2. 同一組規則再跑一次，但先移除 `#` 之後的文字（只會增加攔截）。
//! 3. 本模組的 `token_sqli_label`，輸入是 `decode_only(原文)`（見下方「輸入文字」）。
//!
//! token 層只會增加攔截，不會改變既有規則的標籤（parity 基準已驗證）。
//!
//! # 輸入文字
//!
//! 輸入必須是「解碼但保留註解」的文字（`waf_detect::decode_only`）。
//! 不能用 `normalize` 的輸出：`normalize` 會先刪除 `/* … */`，連同 MySQL 版本註解
//! `/*!50000union*/` 裡真正會執行的 SQL 一起刪掉。
//!
//! # Token 種類（`Tok`）
//!
//! | Token | 來源 |
//! |---|---|
//! | `Word` | 字母開頭的識別字或關鍵字（小寫，輸入已小寫化） |
//! | `Num` | 數字開頭的字串（含小數點與英數字，如 `1.10`、`0x1f`） |
//! | `Quote` | `'` 或 `"`。每個引號都是獨立 token，不是字串的開頭與結尾 |
//! | `Op` | `=`、`<`、`>`、`<=`、`>=`、`<>`、`!=`、`||`、`&&` |
//! | `LParen`／`RParen` | `(`、`)` |
//! | `Other` | 其他字元（反引號、逗號、點號等） |
//!
//! ## 註解處理
//!
//! - `/*!N … */`（MySQL 版本註解）：視為程式碼。跳過開頭的版本號數字，內容照常分析。
//! - `/* … */`（一般區塊註解）：整段略過。未閉合時略過到結尾。
//! - `--`：只有後面接空白（或字串結尾）才是行註解，與 MySQL 規則一致。
//! - `#`：一律是行註解，到行尾為止。
//!
//! # 引號不是字串起點
//!
//! 注入情境中，引號通常是「閉合原本字串」的字元（`1' or 'x' like 'x`）。
//! 如果把引號當成字串起點，`1' or 'x' like 'x` 會被切錯。因此引號只是 `Quote` token，
//! 運算元由「引號 + 單一值 + 引號（可省略）」組成（見 `operand`）。
//! 不要改成字串起點的分詞方式。
//!
//! # 規則
//!
//! 1. **UNION SELECT**（`union_select`）：`union`，後面可接 `all` 或 `distinct`，再接 `select`。
//! 2. **恆真條件**（`tautology`）：布林連接詞 `or`、`and`、`xor`、`||` 之後接下列形式之一：
//!    - `or true`。
//!    - `or not X`，且 X 為 0 或 `false`（只限 `or`，不限 `and`）。
//!    - `值 比較 值`，比較符為 `= <> != < > <= >= like`，且求值結果為真。
//!
//!    運算元可以是數字、識別字（`true`／`false` 以數字 1／0 計）、單一引號值。
//!    括號只會被略過，不會被解析。
//!
//! ## 求值語義
//!
//! - 兩邊都能解析為數字（數字 token，或字串內容可解析為數字）：以數字比較，模擬 MySQL 的型別轉換。
//! - 兩邊都是字串：只支援 `=`、`<>`、`!=`、`like`。`like` 只接受完全相等，或右側為 `%`。
//!   其他比較符回傳「無法判斷」，不命中。
//!
//! # 刻意不做的事
//!
//! - **`or N` 收尾不算恆真**：`chapter 1 or 2` 這類自然語言會誤判。
//! - **`order by N` 不加規則**：這是欄位數探測（偵察），正常排序參數中很常見，誤判風險高。
//!   `1 order by 3--`、`1' order by 1#` 因此維持為已知漏報（見 CHANGELOG）。
//! - **不做完整語法解析**：只看上述形式，不建立語法樹。
//!
//! # 已知限制
//!
//! - 引號內只支援單一值。`'a b'` 這類含空白的字串無法求值，不會命中。
//! - 不區分 `ANSI_QUOTES`：一律把 `"` 當成引號。
//! - 反引號識別字（`` `union` ``）會被切成 `Other` + `Word`，不會被當成關鍵字，這與 MySQL 一致。
//! - 子查詢與括號巢狀不展開。子查詢由 `waf_detect` 的正規表達式規則負責。
//! - 效能未量測。token 層對每次 SQLi 檢查的輸入都會配置 `Vec<char>` 與 token 陣列。
//!   大型本體（前後各 2 MiB）可能增加延遲。
//!
//! # 新增規則的流程
//!
//! 1. 在 `waf/tests_bypass.rs` 加入攻擊樣本（`query` 或 `body`），記錄升級前後的 survey。
//! 2. 在 `waf/benign_corpus.txt` 加入會誤判風險高的正常文字（`U|` URL、`J|` JSON 等）。
//! 3. 比對 parity 輸出：既有列必須完全不變，只允許新增列。
//! 4. 在本檔的 `tests` 加入「應命中」與「不應命中」的案例。
//! 5. 在 `CHANGELOG_KB.md` 記錄量測數字。

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Tok {
    Word(String),
    Num(String),
    Quote,
    Op(&'static str),
    LParen,
    RParen,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
enum Val {
    Num(f64),
    Str(String),
}

/// 詞法分析。輸入應已解碼並小寫化（見 waf_detect 的 decode_only）。
pub(crate) fn tokenize(src: &str) -> Vec<Tok> {
    let c: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            if c.get(i + 2) == Some(&'!') {
                // MySQL 版本註解：`/*!50000 …*/` 內容執行，跳過前綴數字。
                i += 3;
                while i < c.len() && c[i].is_ascii_digit() {
                    i += 1;
                }
            } else {
                // 一般區塊註解：跳到 `*/`。
                i += 2;
                while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
        } else if ch == '*' && c.get(i + 1) == Some(&'/') {
            // 版本註解的結尾。
            i += 2;
        } else if (ch == '-' && c.get(i + 1) == Some(&'-') && is_comment_start(c.get(i + 2)))
            || ch == '#'
        {
            // 行註解：`--` 後需接空白（MySQL 規則）或結尾；`#` 直接開始。
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '\'' || ch == '"' {
            out.push(Tok::Quote);
            i += 1;
        } else if ch.is_ascii_digit() {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.') {
                i += 1;
            }
            out.push(Tok::Num(c[start..i].iter().collect()));
        } else if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok::Word(c[start..i].iter().collect()));
        } else {
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            let op = match two.as_str() {
                "<>" => Some("<>"),
                "!=" => Some("!="),
                "<=" => Some("<="),
                ">=" => Some(">="),
                "||" => Some("||"),
                "&&" => Some("&&"),
                _ => None,
            };
            if let Some(op) = op {
                out.push(Tok::Op(op));
                i += 2;
                continue;
            }
            out.push(match ch {
                '=' => Tok::Op("="),
                '<' => Tok::Op("<"),
                '>' => Tok::Op(">"),
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                _ => Tok::Other,
            });
            i += 1;
        }
    }
    out
}

fn is_comment_start(next: Option<&char>) -> bool {
    next.map_or(true, |c| c.is_whitespace())
}

/// token 層的 SQLi 判斷。回傳規則名稱；未命中回傳 None。
pub(crate) fn token_sqli_label(src: &str) -> Option<&'static str> {
    let toks = tokenize(src);
    if union_select(&toks) {
        return Some("UNION SELECT 注入（語法分析）");
    }
    if tautology(&toks) {
        return Some("恆真條件（語法分析）");
    }
    None
}

/// `union [all|distinct] select`，允許註解與版本註解分隔（詞法層已移除註解）。
fn union_select(toks: &[Tok]) -> bool {
    toks.iter().enumerate().any(|(i, t)| {
        if *t != Tok::Word("union".into()) {
            return false;
        }
        let mut j = i + 1;
        if matches!(toks.get(j), Some(Tok::Word(w)) if w == "all" || w == "distinct") {
            j += 1;
        }
        matches!(toks.get(j), Some(Tok::Word(w)) if w == "select")
    })
}

/// 布林連接詞（or／and／xor／||）後接「值 比較 值」，或 `or true`、`or not 0`、`or 1` 收尾。
fn tautology(toks: &[Tok]) -> bool {
    for (i, t) in toks.iter().enumerate() {
        let connector = match t {
            Tok::Word(w) => matches!(w.as_str(), "or" | "and" | "xor"),
            Tok::Op(op) => *op == "||",
            _ => false,
        };
        if !connector {
            continue;
        }
        let is_or = !matches!(t, Tok::Word(w) if w == "and");
        let mut j = skip_parens(toks, i + 1, Tok::LParen);
        // or true：直接恆真。
        if is_or && toks.get(j) == Some(&Tok::Word("true".into())) {
            return true;
        }
        // or not X：X 為假（0 或 false）即恆真
        if matches!(toks.get(j), Some(Tok::Word(w)) if w == "not") {
            if let Some((Val::Num(n), _)) = operand(toks, j + 1) {
                if n == 0.0 && is_or {
                    return true;
                }
            }
            continue;
        }
        let Some((left, after_left)) = operand(toks, j) else {
            continue;
        };
        j = skip_parens(toks, after_left, Tok::RParen);
        match toks.get(j) {
            Some(Tok::Op(op)) if is_cmp(op) => {
                let k = skip_parens(toks, j + 1, Tok::LParen);
                if let Some((right, _)) = operand(toks, k) {
                    if compare(&left, &right, op) == Some(true) {
                        return true;
                    }
                }
            }
            Some(Tok::Word(w)) if w == "like" => {
                let k = skip_parens(toks, j + 1, Tok::LParen);
                if let Some((right, _)) = operand(toks, k) {
                    if compare(&left, &right, "like") == Some(true) {
                        return true;
                    }
                }
            }
            // 注意：不以「or N」收尾判斷恆真，自然語言（`chapter 1 or 2`）會誤判。
            _ => {}
        }
    }
    false
}

fn is_cmp(op: &str) -> bool {
    matches!(op, "=" | "<>" | "!=" | "<" | ">" | "<=" | ">=")
}

fn skip_parens(toks: &[Tok], mut j: usize, kind: Tok) -> usize {
    while toks.get(j) == Some(&kind) {
        j += 1;
    }
    j
}

/// 運算元：`值`、`'值'`、`"值"`。回傳值與下一個位置。
fn operand(toks: &[Tok], j: usize) -> Option<(Val, usize)> {
    let quoted = toks.get(j) == Some(&Tok::Quote);
    let k = if quoted { j + 1 } else { j };
    let val = match toks.get(k)? {
        Tok::Num(n) if !quoted => Val::Num(n.parse().ok()?),
        Tok::Num(n) => Val::Str(n.clone()),
        Tok::Word(w) if !quoted => match w.as_str() {
            "true" => Val::Num(1.0),
            "false" => Val::Num(0.0),
            _ => return None,
        },
        Tok::Word(w) => Val::Str(w.clone()),
        _ => return None,
    };
    let mut end = k + 1;
    if quoted {
        // 結尾允許沒有閉合的引號（注入載荷常被截斷：`' or 'a'='a`）。
        if toks.get(end) == Some(&Tok::Quote) {
            end += 1;
        } else if end < toks.len() {
            return None;
        }
    }
    Some((val, end))
}

fn compare(a: &Val, b: &Val, op: &str) -> Option<bool> {
    let as_num = |v: &Val| match v {
        Val::Num(n) => Some(*n),
        Val::Str(s) => s.parse::<f64>().ok(),
    };
    if let (Some(x), Some(y)) = (as_num(a), as_num(b)) {
        // 數字比較；字串以數字解析（MySQL 型別轉換）。
        let ord = x.partial_cmp(&y)?;
        return Some(match op {
            "=" => ord.is_eq(),
            "<>" | "!=" => ord.is_ne(),
            "<" => ord.is_lt(),
            ">" => ord.is_gt(),
            "<=" => ord.is_le(),
            ">=" => ord.is_ge(),
            "like" => ord.is_eq(),
            _ => return None,
        });
    }
    match (a, b) {
        (Val::Str(x), Val::Str(y)) => match op {
            "=" | "like" => Some(x == y || (op == "like" && y == "%")),
            "<>" | "!=" => Some(x != y),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::token_sqli_label as t;

    #[test]
    fn token_layer_detects_obfuscated_sqli() {
        for s in [
            "1 /*!50000union*/ /*!50000select*/ 1,2",
            "1 /*!union*/ select 1,2",
            "1 union/*!select*/1,2",
            "1 union#\nselect 1,2",
            "1 union select 1,2 #",
            "1/*!12345 or 1=1*/",
            "1 or 2>1",
            "1 or true--",
            "1 or not 0",
            "1' or 'x' like 'x'-- -",
            "1' or 1=1-- -",
            "' or 'a'='a",
        ] {
            assert!(t(s).is_some(), "應命中：{s}");
        }
    }

    #[test]
    fn token_layer_ignores_normal_text() {
        for s in [
            "color or red",
            "union of sets",
            "x = 1 or y = 2",
            "1 and 1=2",
            "or 'a' = 'b'",
            "chapter 1 or 2",
            "select one from list",
            "version 1.10 or later",
        ] {
            assert!(t(s).is_none(), "不應命中：{s}");
        }
    }
}
