//! UTF-8 安全的字符串切片辅助

/// 找到小于等于 `target` 的最近有效 UTF-8 字符边界
pub(crate) fn find_char_boundary(s: &str, target: usize) -> usize {
    if target >= s.len() {
        return s.len();
    }
    if target == 0 {
        return 0;
    }
    let mut pos = target;
    while pos > 0 && !s.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_char_boundary_avoids_splitting_multibyte_chars() {
        let s = "基于 GPT-5 的编码助手";
        let target = s.len().saturating_sub("<thinking>".len());
        let boundary = find_char_boundary(s, target);
        assert!(s.is_char_boundary(boundary));
        assert_eq!(&s[..boundary], "基于 GPT-5 的");
    }
}
