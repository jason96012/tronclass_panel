//! 字串安全截斷工具（UTF-8 邊界感知）。
//!
//! REDIRECT_UTF8_SLICE_V1
//! 專案中多處「錯誤訊息預覽 / 日誌截斷」原本寫成
//! `&s[..s.len().min(N)]` —— 這是**位元組**切片。當第 N 個位元組剛好落在
//! 多位元組 UTF-8 字元（中文 3 bytes、emoji 4 bytes）中間時，會直接
//! `panic: byte index N is not a char boundary`，把一條原本只是「上游回應
//! 解析失敗」的可恢復錯誤升級成整個 task panic。
//!
//! 典型案例：Tronclass 回傳中文錯誤訊息、使用者傳送長中文指令給 LINE Bot、
//! QR code data 夾帶非 ASCII 內容。這些都可能被截斷用於 log 顯示。
//!
//! 所有此類截斷一律改用 [`truncate_chars`]：以「字元數」為單位，保證
//! 切點永遠落在字元邊界上。
//!
//! 注意（語意變化）：對純 ASCII 內容行為與舊寫法完全相同；對中文內容，
//! 舊寫法取「前 N 位元組」（N=200 時約 66 個中文字），新寫法取「前 N 個字」，
//! 顯示上限略為變長，屬合理改善。

/// 依「字元數」截斷字串，保證不切在多位元組 UTF-8 字元中間。
///
/// ```text
/// truncate_chars("阿爾法測試", 2) == "阿爾"
/// truncate_chars("abcdef", 200)  == "abcdef"   // 不夠長就原樣返回
/// ```
pub(crate) fn truncate_chars(value: &str, max_chars: usize) -> &str {
    // 快速路徑：位元組數 ≤ 上限時，字元數必然也 ≤ 上限（一個字元至少佔 1 byte），
    // 一定不需截斷，直接回傳原字串。
    if value.len() <= max_chars {
        return value;
    }

    // char_indices().nth(n) 回傳第 n 個字元的「起始位元組位置」；
    // 在該邊界切一刀即為前 n 個字元。字元總數不足 n 時為 None → 原樣返回。
    match value.char_indices().nth(max_chars) {
        Some((idx, _)) => &value[..idx],
        None => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 基本行為 ─────────────────────────────────────────────────────────────

    #[test]
    fn test_short_ascii_unchanged() {
        assert_eq!(truncate_chars("abc", 200), "abc");
        assert_eq!(truncate_chars("", 200), "");
    }

    #[test]
    fn test_exact_length_ascii() {
        assert_eq!(truncate_chars("abcdef", 6), "abcdef");
    }

    #[test]
    fn test_long_ascii_truncated() {
        assert_eq!(truncate_chars("abcdefghij", 4), "abcd");
    }

    #[test]
    fn test_max_chars_zero() {
        assert_eq!(truncate_chars("abc", 0), "");
        assert_eq!(truncate_chars("", 0), "");
    }

    // ── 多位元組字元（重點回歸測試）─────────────────────────────────────────
    // 舊寫法 `&s[..s.len().min(N)]` 在這些案例上會直接 panic。

    #[test]
    fn test_chinese_truncated_on_char_boundary() {
        // 「錯」是 3-byte UTF-8 字元；取前 2 個字元 = 6 bytes。
        let s = "錯誤訊息預覽測試";
        assert_eq!(truncate_chars(s, 2), "錯誤");
        assert_eq!(truncate_chars(s, 2).len(), 6);
    }

    #[test]
    fn test_byte_boundary_would_panic_case_now_safe() {
        // 100 個中文字 = 300 bytes。舊寫法 `&s[..s.len().min(200)]` 的第 200
        // 個位元組落在第 67 個字元的中間 → panic。
        // 新函式以「字元數」計：此字串只有 100 個字元 < 200，不需截斷，
        // 必須安全地原樣返回。
        let s = "錯".repeat(100);
        let preview = truncate_chars(&s, 200);
        assert_eq!(preview.chars().count(), 100);
        assert_eq!(preview.len(), 300); // 100 個中文字 × 3 bytes
    }

    #[test]
    fn test_truncate_more_chars_than_needed_mid_boundary() {
        // 真正發生截斷、且切點後面緊接多位元組字元的案例：取前 67 字 = 201 bytes。
        let s = "錯".repeat(100);
        let preview = truncate_chars(&s, 67);
        assert_eq!(preview.chars().count(), 67);
        assert_eq!(preview.len(), 201);
        // 舊寫法在相同輸入、N=67 時：min(300,67)=67，byte 67 落在第 23 個字元
        // 中間（67 = 22×3+1）→ panic。新寫法安全。
    }

    #[test]
    fn test_mixed_ascii_and_chinese() {
        // 'a'(1) + 'b'(1) + '錯'(3) + '誤'(3) = 8 bytes / 4 chars
        let s = "ab錯誤測試";
        assert_eq!(truncate_chars(s, 3), "ab錯");
        assert_eq!(truncate_chars(s, 4), "ab錯誤");
    }

    #[test]
    fn test_emoji_four_byte_chars() {
        // emoji 佔 4 bytes，同樣不能切在中間。
        let s = "🎉🎉🎉🎉";
        assert_eq!(truncate_chars(s, 2), "🎉🎉");
        assert_eq!(truncate_chars(s, 2).len(), 8);
    }

    #[test]
    fn test_char_count_fewer_than_max() {
        let s = "中文";
        assert_eq!(truncate_chars(s, 200), "中文");
    }

    #[test]
    fn test_traditional_chinese_error_message_realistic() {
        // 模擬真實案例：上游回傳中文 JSON 解析失敗訊息。
        let body = "無法解析的回應：伺服器維護中，請稍後再試。".repeat(30);
        let preview = truncate_chars(&body, 200);
        assert!(preview.chars().count() <= 200);
        assert!(preview.chars().count() > 0);
        // 保證切點在字元邊界（這行不 panic 即代表成功）。
        let _first = preview.chars().next().unwrap();
    }
}
