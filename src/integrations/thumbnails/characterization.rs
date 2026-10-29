use super::{PNG_SIGNATURE, safe_color, valid_png};

#[test]
fn public_helpers_preserve_the_thumbnail_validation_and_color_contract() {
    assert!(valid_png(&PNG_SIGNATURE, PNG_SIGNATURE.len() as u64));
    assert!(!valid_png(&PNG_SIGNATURE, (PNG_SIGNATURE.len() - 1) as u64));
    assert_eq!(safe_color(Some(" #abc "), "acme"), "#aabbcc");
    assert_eq!(safe_color(Some("blue"), "acme"), "hsl(55 68% 44%)");
}
