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

