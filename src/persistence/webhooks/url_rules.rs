// ---------------------------------------------------------------------------
// URL masking and the Discord allowlist
// ---------------------------------------------------------------------------

/// Port of the exported `maskUrl(value)` — [lib/webhooks.js:39-47].
///
/// `${parsed.protocol}//${parsed.host}…${url.slice(-4)}`, falling back to `…${url.slice(-4)}` when
/// the value does not parse as a URL. Note the missing `/` relative to
/// [`mask_webhook_url`](crate::persistence::migrations::mask_webhook_url); the difference is in the
/// reference and is load-bearing for the admin UI's rendered value.
#[must_use]
pub fn mask_url(value: &str) -> String {
    let suffix = last_four_utf16(value);
    url::Url::parse(value).map_or_else(
        |_| format!("…{suffix}"),
        |parsed| format!("{}://{}…{suffix}", parsed.scheme(), js_host(&parsed)),
    )
}

/// JavaScript `URL.host`: hostname plus `:port` only when the port is not the scheme default.
///
/// `url::Url::port()` already returns `None` for a default port, so the two agree.
fn js_host(parsed: &url::Url) -> String {
    let host = parsed.host_str().unwrap_or_default();
    parsed
        .port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"))
}

/// Equivalent of JavaScript `value.slice(-4)`, which counts UTF-16 code units.
///
/// Splitting a surrogate pair is impossible in Rust, so a value whose 4th-from-last code unit is
/// the low half of an astral character yields the whole character instead of a lone surrogate.
/// Every value reaching this function is a URL or an already-masked URL, both ASCII in practice.
fn last_four_utf16(value: &str) -> String {
    if utf16_len(value) <= 4 {
        return value.to_owned();
    }
    let mut taken = 0;
    let mut start = value.len();
    for (index, character) in value.char_indices().rev() {
        taken += character.len_utf16();
        start = index;
        if taken >= 4 {
            break;
        }
    }
    value[start..].to_owned()
}

/// Number of UTF-16 code units in a string, i.e. JavaScript's `String.prototype.length`.
#[must_use]
pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// Equivalent of JavaScript `value.slice(0, max)` over UTF-16 code units.
///
/// As with [`last_four_utf16`], a cut that would land inside a surrogate pair keeps the whole
/// character rather than producing an unrepresentable lone surrogate.
#[must_use]
pub fn truncate_utf16(value: &str, max: usize) -> String {
    let mut taken = 0;
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if taken + width > max {
            return value[..index].to_owned();
        }
        taken += width;
    }
    value.to_owned()
}

/// Port of `DISCORD_WEBHOOK_RE.test(url)` — [lib/webhooks.js:23,87].
///
/// The regex is `^https://(discord|discordapp)\.com/api/webhooks/` with the `i` flag, anchored at
/// the start only. It is a **prefix allowlist on the raw string**, which is what makes lookalikes
/// such as `https://discord.com.evil.tld/api/webhooks/1/t` fail: the character after `discord.com`
/// must be `/`.
///
/// This is the SSRF guard. It is deliberately not re-expressed as "parse the URL, then compare the
/// host", because a parser difference between the two runtimes would then become a *security*
/// difference; matching the reference's literal prefix test keeps the two decisions identical.
#[must_use]
pub fn is_discord_webhook_url(value: &str) -> bool {
    const PREFIXES: [&str; 2] = [
        "https://discord.com/api/webhooks/",
        "https://discordapp.com/api/webhooks/",
    ];
    // `eq_ignore_ascii_case` over a prefix slice reproduces the regex's `i` flag, which is
    // ASCII-only for these literals.
    PREFIXES.iter().any(|prefix| {
        value
            .as_bytes()
            .get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix.as_bytes()))
    })
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use crate::persistence::migrations::mask_webhook_url;

    #[test]
    fn event_names_match_the_frozen_order() {
        assert_eq!(
            EVENTS.iter().map(event_name).collect::<Vec<&str>>(),
            [
                "published",
                "updated",
                "restored",
                "deleted",
                "feedback",
                "resolved"
            ]
        );
        for event in &EVENTS {
            assert_eq!(event_from_name(event_name(event)).as_ref(), Some(event));
        }
        assert_eq!(event_from_name("nope"), None);
    }

    #[test]
    fn stored_events_drop_unknown_names() {
        assert_eq!(
            parse_stored_events("published,nope,resolved"),
            [WebhookEvent::Published, WebhookEvent::Resolved]
        );
        assert_eq!(parse_stored_events(""), Vec::<WebhookEvent>::new());
        assert_eq!(
            encode_events(&EVENTS),
            "published,updated,restored,deleted,feedback,resolved"
        );
    }

    #[test]
    fn normalization_defaults_and_deduplicates_in_order() {
        assert_eq!(normalize_events(None, true), EVENTS.to_vec());
        assert_eq!(normalize_events(None, false), Vec::<WebhookEvent>::new());
        assert_eq!(
            normalize_events(
                Some(&[
                    WebhookEvent::Resolved,
                    WebhookEvent::Published,
                    WebhookEvent::Resolved
                ]),
                true
            ),
            [WebhookEvent::Resolved, WebhookEvent::Published]
        );
    }

    #[test]
    fn display_mask_omits_the_slash_the_stored_mask_keeps() {
        let url = "https://discord.com/api/webhooks/123456789012345678/secret-token";
        assert_eq!(mask_url(url), "https://discord.com…oken");
        assert_eq!(mask_webhook_url(url), "https://discord.com/…oken");
        // Masking the already-masked stored value is what `publicRow` actually does.
        assert_eq!(mask_url(&mask_webhook_url(url)), "https://discord.com…oken");
        assert_eq!(
            mask_url("https://hooks.example.com:8443/abc/secret"),
            "https://hooks.example.com:8443…cret"
        );
        assert_eq!(mask_url("not a url"), "… url");
        assert_eq!(mask_url("abc"), "…abc");
        assert_eq!(mask_url(""), "…");
    }

    #[test]
    fn allowlist_accepts_only_the_two_discord_prefixes() {
        for accepted in [
            "https://discord.com/api/webhooks/1/t",
            "https://discordapp.com/api/webhooks/1/t",
            "HTTPS://DISCORD.COM/API/WEBHOOKS/1/t",
            "https://Discord.com/api/webhooks/",
        ] {
            assert!(is_discord_webhook_url(accepted), "rejected {accepted}");
        }
        for rejected in [
            "",
            "http://discord.com/api/webhooks/1/t",
            "https://discord.com.evil.tld/api/webhooks/1/t",
            "https://evil.tld/https://discord.com/api/webhooks/1/t",
            "https://discord.com@evil.tld/api/webhooks/1/t",
            "https://discord.com/api/webhook/1/t",
            "https://discordapp.com.evil/api/webhooks/1/t",
            "https://169.254.169.254/api/webhooks/1/t",
            " https://discord.com/api/webhooks/1/t",
        ] {
            assert!(!is_discord_webhook_url(rejected), "accepted {rejected}");
        }
    }

    #[test]
    fn utf16_slicing_matches_javascript_for_ascii_and_keeps_pairs_whole() {
        assert_eq!(last_four_utf16("abcdef"), "cdef");
        assert_eq!(last_four_utf16("abc"), "abc");
        assert_eq!(
            truncate_utf16(&"x".repeat(200), MAX_LABEL_UTF16),
            "x".repeat(80)
        );
        assert_eq!(truncate_utf16("short", MAX_LABEL_UTF16), "short");
        // "🎉" is two UTF-16 units: a one-unit budget cannot include it.
        assert_eq!(truncate_utf16("🎉", 1), "");
        assert_eq!(truncate_utf16("a🎉", 2), "a");
        assert_eq!(truncate_utf16("a🎉", 3), "a🎉");
    }
}
