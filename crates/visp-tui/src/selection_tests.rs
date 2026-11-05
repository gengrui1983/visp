use super::*;

#[test]
fn test_selection_default_inactive() {
    let sel = TextSelection::default();
    assert!(!sel.is_active());
    assert!(!sel.is_highlighting());
    assert!(!sel.contains(0, 0, 0));
}

#[test]
fn test_selection_single_line() {
    let sel = TextSelection {
        start: Some((5, 3)),
        end: Some((10, 3)),
    };
    assert!(sel.is_active());
    assert!(sel.is_highlighting());
    assert!(sel.contains(5, 3, 0));
    assert!(sel.contains(7, 3, 0));
    assert!(sel.contains(10, 3, 0));
    assert!(!sel.contains(4, 3, 0));
    assert!(!sel.contains(11, 3, 0));
    assert!(!sel.contains(5, 2, 0));
}

#[test]
fn test_selection_multi_line() {
    // 选择从 (3, 2) 到 (7, 4)
    let sel = TextSelection {
        start: Some((3, 2)),
        end: Some((7, 4)),
    };
    assert!(sel.is_active());
    // 首行：列 >= 3
    assert!(sel.contains(3, 2, 0));
    assert!(sel.contains(50, 2, 0));
    assert!(!sel.contains(2, 2, 0));
    // 中间行：全选
    assert!(sel.contains(0, 3, 0));
    assert!(sel.contains(99, 3, 0));
    // 末行：列 <= 7
    assert!(sel.contains(0, 4, 0));
    assert!(sel.contains(7, 4, 0));
    assert!(!sel.contains(8, 4, 0));
    // 超出范围
    assert!(!sel.contains(0, 1, 0));
    assert!(!sel.contains(0, 5, 0));
}

#[test]
fn test_selection_with_scroll() {
    // 内容坐标选择 (5, 10) 到 (10, 10)，滚动 y=5
    // 屏幕坐标 row = 10 - 5 = 5
    let sel = TextSelection {
        start: Some((5, 10)),
        end: Some((10, 10)),
    };
    assert!(sel.contains(5, 5, 5));
    assert!(sel.contains(7, 5, 5));
    assert!(!sel.contains(5, 4, 5));
    assert!(!sel.contains(5, 6, 5));
}

#[test]
fn test_selection_reversed_coords() {
    // end 在 start 之前
    let sel = TextSelection {
        start: Some((10, 5)),
        end: Some((3, 2)),
    };
    assert!(sel.is_active());
    assert!(sel.contains(5, 3, 0));
    assert!(sel.contains(10, 5, 0));
    assert!(sel.contains(3, 2, 0));
}

#[test]
fn test_selection_clear() {
    let mut sel = TextSelection {
        start: Some((0, 0)),
        end: Some((5, 5)),
    };
    assert!(sel.is_active());
    sel.clear();
    assert!(!sel.is_active());
    assert!(!sel.is_highlighting());
}

#[test]
fn test_base64_encode_empty() {
    assert_eq!(base64_encode(b""), "");
}

#[test]
fn test_base64_encode_short() {
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
    assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
}

#[test]
fn test_base64_encode_unicode() {
    // "你好" in UTF-8
    assert_eq!(base64_encode("你好".as_bytes()), "5L2g5aW9");
}

#[test]
fn test_extract_selected_text_single_line() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
    // 写入一些文本到第 2 行
    for (i, ch) in "Hello World".chars().enumerate() {
        buf[(i as u16 + 2, 2)].set_symbol(ch.to_string().as_str());
    }
    // 选择 col 2..=6 = "Hello"
    let sel = TextSelection {
        start: Some((2, 2)),
        end: Some((6, 2)),
    };
    let text = extract_selected_text(
        &buf,
        &sel,
        0,
        &crate::app::BlockRows::default(),
        ratatui::layout::Rect::new(0, 0, 20, 5),
    );
    assert_eq!(text, "Hello");
}

#[test]
fn test_extract_selected_text_multi_line() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 5));
    for (i, ch) in "Line1".chars().enumerate() {
        buf[(i as u16, 1)].set_symbol(ch.to_string().as_str());
    }
    for (i, ch) in "Line2".chars().enumerate() {
        buf[(i as u16, 2)].set_symbol(ch.to_string().as_str());
    }
    // 选择从第 1 行 col 2 到第 2 行 col 3
    // 第 1 行: col 2..=19 → "ne1" (trim_end)
    // 第 2 行: col 0..=3 → "Line"
    let sel = TextSelection {
        start: Some((2, 1)),
        end: Some((3, 2)),
    };
    let text = extract_selected_text(
        &buf,
        &sel,
        0,
        &crate::app::BlockRows::default(),
        ratatui::layout::Rect::new(0, 0, 20, 5),
    );
    assert_eq!(text, "ne1\nLine");
}

#[test]
fn test_extract_selected_text_chinese() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    let mut buf = Buffer::empty(Rect::new(0, 0, 20, 3));
    // 使用 set_string 正确渲染中文（会自动处理全角字符的 continuation cells）
    buf.set_string(0, 1, "你好世界", Style::default());
    buf.set_string(0, 2, "Hello 你好", Style::default());
    // 选择整行
    let sel = TextSelection {
        start: Some((0, 1)),
        end: Some((19, 2)),
    };
    let text = extract_selected_text(
        &buf,
        &sel,
        0,
        &crate::app::BlockRows::default(),
        ratatui::layout::Rect::new(0, 0, 20, 5),
    );
    // 不应有空格在中文之间
    assert_eq!(text, "你好世界\nHello 你好");
}
