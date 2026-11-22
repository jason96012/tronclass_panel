//! WAF 行為基準線：env 閘控（WAF_PARITY_OUT）時輸出檢測結果，供重構前後比對。

#[cfg(test)]
mod parity_tests {
    //! WAF_PARITY_V1：重構前後的行為基準。
    //! 設定環境變數 `WAF_PARITY_OUT=<路徑>` 時，把大量輸入的檢測結果寫入檔案；
    //! 重構前後各跑一次並 diff，結果必須完全相同。未設定時直接略過。
    use super::super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Uri};
    use std::str::FromStr;

    const CORPUS: &str = include_str!("benign_corpus.txt");

    fn enc(s: &str) -> String {
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

    fn rec_uri(out: &mut String, label: &str, method: &Method, target: &str, headers: &HeaderMap) {
        let res = match Uri::from_str(&enc(target)) {
            Ok(u) => format!("{:?}", inspect_waf_threat(&u, method, headers).map(|t| t.description())),
            Err(e) => format!("URI_ERR {e}"),
        };
        out.push_str(&format!("{label}\t{target}\t{res}\n"));
    }

    fn rec_body(out: &mut String, label: &str, body: &str, is_form: bool) {
        let res = format!("{:?}", inspect_waf_body(body, is_form).map(|t| t.description()));
        out.push_str(&format!("{label}\t{body}\t{res}\n"));
    }

    fn one_header(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            HeaderValue::from_str(&enc(value)).expect("header value"),
        );
        h
    }

    #[test]
    fn parity_dump_threat_and_body() {
        let Ok(path) = std::env::var("WAF_PARITY_OUT") else {
            return;
        };
        let mut out = String::new();
        let get = Method::GET;
        let none = HeaderMap::new();

        // 1. 蜜罐路徑（signatures 全部項目 × 幾種變形）
        for probe in KNOWN_HONEYPOT_PATHS {
            for t in [probe.to_string(), format!("{probe}/x"), format!("{probe}?a=1"), format!("/x{probe}")] {
                rec_uri(&mut out, "honeypot", &get, &t, &none);
            }
        }
        // 2. 危險副檔名（全部項目）與雙重副檔名
        for ext in KNOWN_DANGEROUS_EXTENSIONS {
            for t in [format!("/f{ext}"), format!("/admin/static/f{ext}"), format!("/f{ext}?x=1")] {
                rec_uri(&mut out, "extension", &get, &t, &none);
            }
        }
        for t in ["/hw.php.zip", "/x.jsp.txt", "/pub/share?path=/hw3.py.zip", "/a.aspx.bak", "/y.phtml.jpg", "/a.sh.png"] {
            rec_uri(&mut out, "webshell", &get, t, &none);
        }
        // 3. 惡意 UA（全部項目）
        for bot in KNOWN_MALICIOUS_USER_AGENTS {
            rec_uri(&mut out, "ua", &get, "/courses", &one_header("user-agent", bot));
            rec_uri(&mut out, "ua", &get, "/courses", &one_header("user-agent", &format!("Mozilla/5.0 {bot}/1.0")));
        }
        // 4. 攻擊與近似正常的 URL
        let attacks = [
            "/search?q=1' or 1=1--", "/x?id=1 union select 1,2", "/x?a=<script>alert(1)</script>",
            "/x?a=<img src=x onerror=alert(1)>", "/x?a=javascript:alert(1)", "/x?a=${jndi:ldap://a}",
            "/x?a=$(curl evil)", "/x?a=;whoami", "/x?a=|whoami", "/x?a=../../etc/passwd",
            "/x?a=..%2f..%2fetc%2fpasswd", "/x?a=%252e%252e%252f", "/x?a=%00", "/x?a=%0d%0aSet-Cookie",
            "/x?a=169.254.169.254", "/x?u=http://127.0.0.1/", "/x?u=http://10.0.0.1/", "/x?u=http://localtest.me",
            "/x?u=0x7f000001", "/x?q=__proto__", "/x?q=constructor.prototype.x", "/x?q=constructor[prototype]",
            "/x?q=Object.setPrototypeOf(a,b)", "/x?q={{7*7}}", "/x?q={{config.items()}}", "/x?q=${7*7}",
            "/x?q=#{7*7}", "/x?q=<%= 7*7 %>", "/x?q=*{t(java.lang.Runtime)}", "/x?q=#set($x=1)",
            "/x?q=<!DOCTYPE foo>", "/x?q=ro0AB", "/x?q=fastjson", "/x?q=sleep(5)", "/x?q=benchmark(1000000,md5(1))",
            "/x?q=waitfor delay '0:0:5'", "/x?q=extractvalue(1,concat(0x7e,version()))",
            "/x?q=into outfile '/tmp/x'", "/x?q=1 and 1=1", "/x?q=admin'--", "/x?q=' or '1'='1",
            "/x?q=1;drop table users", "/x?q=select * from users where id=1", "/x?q=union/**/select/**/1",
            "/x?q=<script>", "/x?q=<svg/onload=alert(1)>", "/x?q=<details open ontoggle=alert(1)>",
            "/x?q=document.cookie", "/x?q=eval(base64_decode('x'))", "/x?q=system($_GET[x])", "/x?q=bash -i",
            "/x?q=/dev/tcp/1.2.3.4/80", "/x?q=powershell -enc", "/x?q=certutil -urlcache -f http://x",
            "/index.php/../../x", "/x?q=hello world", "/x?q=%e2%80%ae", "/x?q=%u002e%u002e%2f", "/x?q=%c0%af",
            "/x?q=%ef%bc%8f", "/x%2e%2e/etc/passwd", "/courses?page=2", "/courses/../secret",
            "/admin/api/status?include_history=false", "/a/b/c?d=e&f=g", "/x?q=%", "/x?q=%zz",
            "/x?q=%E8%B3%87%E6%96%99", "/x?a=1&a=2&a=3", "/x?q=%2e%2e%5c", "/x?q=..\\..\\windows",
            "/x?q=%3Cscript%3E", "/x?q=%3Csvg%2Fonload%3Dalert(1)%3E", "/pub/share?path=/x.py.zip",
            "/course/12345/homework?page=2&sort=due_date", "/x?q=select%20a%20from%20b", "/x?q=or%201=1",
        ];
        for t in attacks {
            rec_uri(&mut out, "attack", &get, t, &none);
        }
        let long = format!("/x?q={}", "a".repeat(5000));
        rec_uri(&mut out, "long", &get, &long, &none);
        // 5. HTTP 方法
        for m in ["GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD", "TRACE", "TRACK", "CONNECT", "PURGE"] {
            let method = Method::from_bytes(m.as_bytes()).expect("method");
            rec_uri(&mut out, "method", &method, "/courses", &none);
        }
        // 6. 標頭
        let header_cases: Vec<(&str, &str)> = vec![
            ("x-original-url", "/admin"), ("x-rewrite-url", "/admin"), ("x-custom-ip-authorization", "1.2.3.4"),
            ("host", "evil.com@good.com"), ("host", "good.edu.tw"), ("host", "x y"),
            ("referer", "https://www.google.com/search?q=union+select+x"),
            ("referer", "https://www.google.com/search?q=hello"),
            ("referer", "https://example.com/?x=1' or 1=1"),
            ("referer", "https://x/?a=<script>alert(1)</script>"),
            ("cookie", "session=abc123; theme=dark"), ("cookie", "note=%3Csvg%20onload%3Dalert(1)%3E"),
            ("cookie", "a=union select 1"),
        ];
        for (name, value) in header_cases {
            rec_uri(&mut out, "header", &get, "/courses", &one_header(name, value));
        }
        let mut smuggle = HeaderMap::new();
        smuggle.insert(axum::http::header::CONTENT_LENGTH, HeaderValue::from_static("5"));
        smuggle.insert(axum::http::header::TRANSFER_ENCODING, HeaderValue::from_static("chunked"));
        rec_uri(&mut out, "smuggle", &Method::POST, "/courses", &smuggle);
        // 7. 本體
        let bodies = [
            r#"{"message":"請解釋 SELECT 語法"}"#, "' OR 1=1 --", "union select", r#"{"__proto__":{"x":1}}"#,
            "{{7*7}}", "<img src=x onerror=alert(1)>", "<script>alert(1)</script>", "q=union+select",
            "a=%3Cscript%3E", "javascript:alert(1)", r#"{"text":"<p>第三週作業</p>"}"#, "hello world",
            "SELECT * FROM a JOIN b ON a.id = b.id;", "constructor.prototype.x = 1", "",
        ];
        for b in bodies {
            rec_body(&mut out, "body_json", b, false);
            rec_body(&mut out, "body_form", b, true);
        }
        // 8. 語料（U/R/C/J/F）逐行套用
        for raw in CORPUS.lines() {
            let line = raw.trim_end();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((tag, payload)) = line.split_once('|') else { continue };
            match tag.chars().last().unwrap_or('?') {
                'U' => rec_uri(&mut out, "corpus_u", &get, payload, &none),
                'R' => rec_uri(&mut out, "corpus_r", &get, "/courses", &one_header("referer", payload)),
                'C' => rec_uri(&mut out, "corpus_c", &get, "/courses", &one_header("cookie", payload)),
                'J' => rec_body(&mut out, "corpus_j", payload, false),
                'F' => rec_body(&mut out, "corpus_f", payload, true),
                _ => {}
            }
        }
        std::fs::write(&path, out.as_bytes()).expect("write parity");
    }
}
