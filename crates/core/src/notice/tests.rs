use super::*;

#[test]
fn text_of_one_to_the_limit_is_accepted() {
    assert_eq!(check_notice_text("Security maintenance: withdrawals are paused."), Ok(()));
    assert_eq!(check_notice_text("a"), Ok(()));
    assert_eq!(check_notice_text(&"a".repeat(MAX_NOTICE_CHARS)), Ok(()));
    // The limit counts characters, not bytes.
    assert_eq!(check_notice_text(&"é".repeat(MAX_NOTICE_CHARS)), Ok(()));
}

#[test]
fn text_over_the_limit_is_refused_naming_its_length() {
    let reason = check_notice_text(&"a".repeat(MAX_NOTICE_CHARS + 1)).unwrap_err();
    assert_eq!(reason, "the notice is 281 characters, over the 280 allowed");
    assert!(check_notice_text(&"é".repeat(MAX_NOTICE_CHARS + 1)).is_err());
}

#[test]
fn empty_blank_and_padded_text_is_refused() {
    for text in ["", " ", "\t", " paused", "paused "] {
        assert!(check_notice_text(text).is_err(), "{text:?}");
    }
}

#[test]
fn control_characters_are_refused() {
    for text in ["a\nb", "a\rb", "a\tb", "a\0b", "a\u{7f}b"] {
        assert_eq!(
            check_notice_text(text),
            Err("the notice holds a control or invisible character".into()),
            "{text:?}"
        );
    }
}

#[test]
fn line_separators_and_invisible_characters_are_refused_alone_and_inside_the_text() {
    // The first and last of each refused range.
    let invisible = [
        '\u{00ad}',
        '\u{034f}',
        '\u{061c}',
        '\u{115f}',
        '\u{1160}',
        '\u{17b4}',
        '\u{17b5}',
        '\u{180b}',
        '\u{180f}',
        '\u{200b}',
        '\u{200f}',
        '\u{2028}',
        '\u{2029}',
        '\u{202a}',
        '\u{202e}',
        '\u{2060}',
        '\u{2065}',
        '\u{206a}',
        '\u{206f}',
        '\u{3164}',
        '\u{fe00}',
        '\u{fe0f}',
        '\u{feff}',
        '\u{ffa0}',
        '\u{fff0}',
        '\u{fff9}',
        '\u{fffb}',
        '\u{1bca0}',
        '\u{1bca3}',
        '\u{1d173}',
        '\u{1d17a}',
        '\u{e0000}',
        '\u{e0020}',
        '\u{e0fff}',
        '\u{0085}',
    ];
    for c in invisible {
        for text in [format!("Paused{c}Next update"), c.to_string(), format!("a{c}")] {
            assert!(check_notice_text(&text).is_err(), "{c:?}: {text:?}");
        }
    }
    // The characters just outside each range stay.
    let visible = [
        '\u{00ac}',
        '\u{00ae}',
        '\u{034e}',
        '\u{0350}',
        '\u{061b}',
        '\u{061d}',
        '\u{115e}',
        '\u{1161}',
        '\u{17b3}',
        '\u{17b6}',
        '\u{180a}',
        '\u{2027}',
        '\u{202f}',
        '\u{2070}',
        '\u{3163}',
        '\u{3165}',
        '\u{fdff}',
        '\u{fe10}',
        '\u{fefe}',
        '\u{ff9f}',
        '\u{ffa1}',
        '\u{ffef}',
        '\u{fffc}',
        '\u{1bc9f}',
        '\u{1bca4}',
        '\u{1d172}',
        '\u{1d17b}',
        '\u{d7ff}',
        '\u{e1000}',
        '\u{e9}',
        '\u{4e2d}',
    ];
    for c in visible {
        assert_eq!(check_notice_text(&format!("a{c}b")), Ok(()), "{c:?}");
    }
}

#[test]
fn markup_is_refused() {
    for text in ["<b>paused</b>", "a > b", "R&D", "[paused](x)", "a]"] {
        assert!(check_notice_text(text).unwrap_err().contains("markup"), "{text:?}");
    }
}

#[test]
fn links_are_refused_in_any_case() {
    for text in ["see https://example.com", "see HTTP://example.com", "see www.example.com", "write to MailTo:a@b.c"] {
        assert!(check_notice_text(text).unwrap_err().contains("link"), "{text:?}");
    }
    // Ordinary text that merely resembles one stays.
    assert_eq!(check_notice_text("Updated: withdrawals resume at 14:00 UTC."), Ok(()));
    assert_eq!(check_notice_text("Market data: some US share prices are delayed."), Ok(()));
}
