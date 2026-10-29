use crate::error::AppError;
use rusqlite::Connection;

use super::*;

#[test]
fn trims_the_javascript_whitespace_set_not_rusts() {
    // U+FEFF is JS whitespace but not Unicode `White_Space`; U+0085 is the reverse.
    assert_eq!(js_trim("\u{feff} acme \u{feff}"), "acme");
    assert_eq!(js_trim("\u{85}acme"), "\u{85}acme");
    assert_eq!(js_trim("\u{3000}acme\u{2029}"), "acme");
}

#[test]
fn measures_and_truncates_in_utf16_code_units() {
    assert_eq!(utf16_len("abc"), 3);
    assert_eq!(utf16_len("😀"), 2);
    assert_eq!(truncate_utf16("😀😀", 3), "😀");
    assert_eq!(truncate_utf16("abc", 10), "abc");
}

#[test]
fn accepts_only_node_shaped_org_names() {
    assert!(is_valid_org_name("a"));
    assert!(is_valid_org_name("A-c.m_e0"));
    assert!(is_valid_org_name(&format!("a{}", "b".repeat(40))));
    assert!(!is_valid_org_name(&format!("a{}", "b".repeat(41))));
    assert!(!is_valid_org_name(""));
    assert!(!is_valid_org_name("-acme"));
    assert!(!is_valid_org_name("ac me"));
    assert!(!is_valid_org_name("acme\n"));
}

#[test]
fn requires_at_least_one_dot_in_a_domain() {
    assert!(is_valid_domain("example.com"));
    assert!(is_valid_domain("a.b"));
    assert!(is_valid_domain("sub.example.co.uk"));
    assert!(is_valid_domain("x-y.example"));
    assert!(!is_valid_domain("example"));
    assert!(!is_valid_domain("-bad.example"));
    assert!(!is_valid_domain("bad-.example"));
    assert!(!is_valid_domain("bad..example"));
    assert!(!is_valid_domain(".example.com"));
    assert!(!is_valid_domain("example.com."));
}

#[test]
fn matches_node_email_validation() {
    assert!(is_valid_email("a@b.c"));
    assert!(is_valid_email("first.last+tag@example.co.uk"));
    assert!(!is_valid_email("@example.com"));
    assert!(!is_valid_email("a@@example.com"));
    assert!(!is_valid_email("a b@example.com"));
    assert!(!is_valid_email(".a@example.com"));
    assert!(!is_valid_email("a.@example.com"));
    assert!(!is_valid_email("a..b@example.com"));
    assert!(!is_valid_email("a@example"));
    assert!(!is_valid_email(""));
    assert!(!is_valid_email(&format!("{}@example.com", "a".repeat(65))));
}

#[test]
fn collapses_and_caps_category_names() {
    assert_eq!(norm_category("  design   docs \n"), "design docs");
    assert_eq!(norm_category("\u{feff}"), "");
    assert_eq!(norm_category(&"x".repeat(70)).len(), CATEGORY_MAX_LENGTH);
}

#[test]
fn accepts_three_and_six_digit_hex_colors() {
    assert!(is_valid_color("#abc"));
    assert!(is_valid_color("#356B9F"));
    assert!(!is_valid_color("356B9F"));
    assert!(!is_valid_color("#abcd"));
    assert!(!is_valid_color("#gggggg"));
}

#[test]
fn add_category_reports_unknown_org_before_empty_category() {
    let conn = Connection::open_in_memory().expect("open in-memory database");
    conn.execute_batch(
        "CREATE TABLE orgs (name TEXT PRIMARY KEY);
             CREATE TABLE org_categories (org TEXT NOT NULL, name TEXT NOT NULL);",
    )
    .expect("create organization tables");

    assert_eq!(
        add_category(&conn, "missing", " \t "),
        Err(AppError::Validation(
            "Unknown organization \"missing\".".to_owned()
        ))
    );
}

#[test]
fn add_domain_reports_unknown_org_before_invalid_domain() {
    let conn = Connection::open_in_memory().expect("open in-memory database");
    conn.execute_batch("CREATE TABLE orgs (name TEXT PRIMARY KEY);")
        .expect("create organizations table");

    assert_eq!(
        add_domain(&conn, "missing", "not a domain"),
        Err(AppError::Validation(
            "Unknown organization \"missing\".".to_owned()
        ))
    );
}
