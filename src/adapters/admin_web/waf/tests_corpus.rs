//! WAF 正常使用語料測試（benign_corpus.txt）：量測誤判，不以假設代替。

#[cfg(test)]
mod benign_corpus_tests {
    //! WAF_BENIGN_CORPUS_V1：正常使用語料的誤判回歸測試。
    //! 一般語料不得被攔截；標記 T 的取捨則鎖定為「會被攔截」。
    use super::super::*;
    use axum::http::{header, HeaderMap, HeaderValue, Method, Uri};
    use std::str::FromStr;

    const CORPUS: &str = include_str!("benign_corpus.txt");

    /// 與瀏覽器相同的編碼：空白、非 ASCII 與不允許的字元做百分比編碼。
    fn encode_for_uri(s: &str) -> String {
        let mut out = String::new();
        for c in s.chars() {
            if c.is_ascii() && !matches!(c, ' ' | '"' | '<' | '>' | '{' | '}' | '|' | '\\' | '^' | '`') {
                out.push(c);
            } else {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("%{b:02X}"));
                }
            }
        }
        out
    }

    /// 回傳 Some(描述) 代表被攔截。
    fn verdict(kind: char, payload: &str) -> Result<Option<String>, String> {
        match kind {
            'U' => {
                let uri = Uri::from_str(&encode_for_uri(payload)).map_err(|e| format!("URI 解析失敗: {e}"))?;
                Ok(inspect_waf_threat(&uri, &Method::GET, &HeaderMap::new()).map(|t| t.description()))
            }
            'R' | 'C' => {
                let mut h = HeaderMap::new();
                let name = if kind == 'R' { header::REFERER } else { header::COOKIE };
                let value = HeaderValue::from_str(&encode_for_uri(payload))
                    .map_err(|e| format!("標頭解析失敗: {e}"))?;
                h.insert(name, value);
                let uri = Uri::from_static("/courses");
                Ok(inspect_waf_threat(&uri, &Method::GET, &h).map(|t| t.description()))
            }
            'J' => Ok(inspect_waf_body(payload, false).map(|t| t.description())),
            'F' => Ok(inspect_waf_body(payload, true).map(|t| t.description())),
            other => Err(format!("未知類型 {other}")),
        }
    }

    #[test]
    fn benign_corpus_is_not_blocked() {
        let mut false_positives = Vec::new();
        let mut tradeoff_passed = Vec::new();
        let mut total = 0usize;
        let mut blocked_tradeoffs = 0usize;
        for raw in CORPUS.lines() {
            let line = raw.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((tag, payload)) = line.split_once('|') else {
                continue;
            };
            let expect_block = tag.starts_with('T');
            let kind = tag.chars().last().unwrap_or('?');
            total += 1;
            match verdict(kind, payload) {
                Err(e) => false_positives.push(format!("[{kind}] {e} :: {payload}")),
                Ok(Some(desc)) if !expect_block => {
                    false_positives.push(format!("[{kind}] {payload}  => {desc}"));
                }
                Ok(None) if expect_block => {
                    tradeoff_passed.push(format!("[{kind}] {payload}"));
                }
                Ok(Some(_)) => blocked_tradeoffs += 1,
                Ok(None) => {}
            }
        }
        eprintln!(
            "WAF 語料：共 {total} 筆，誤判 {}，取捨鎖定 {blocked_tradeoffs}，取捨意外放行 {}",
            false_positives.len(),
            tradeoff_passed.len()
        );
        for f in &false_positives {
            eprintln!("  誤判: {f}");
        }
        for t in &tradeoff_passed {
            eprintln!("  取捨意外放行: {t}");
        }
        assert!(false_positives.is_empty(), "正常使用被誤判 {} 筆", false_positives.len());
        assert!(tradeoff_passed.is_empty(), "已知取捨不再攔截 {} 筆，請確認是否為預期", tradeoff_passed.len());
    }
}
