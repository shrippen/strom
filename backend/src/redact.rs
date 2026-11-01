//! Mask secrets before they reach a log line or an error text.
//!
//! Stream targets carry their credentials inline: an SRT `passphrase=` or
//! `streamid=` in the query, `user:pass@` before the host, an RTMP stream key in
//! the last path segment, a token in an HTTP query. Blocks log the URI they
//! configure, a flow dump at `debug` carries every property, and GStreamer error
//! texts may quote a URI, so all of them go through here.
//!
//! Over-redaction is the acceptable direction: a log line may lose a harmless
//! part, a secret never survives.

use std::borrow::Cow;

use strom_types::{Flow, PropertyValue};

/// What a masked secret is replaced with.
pub const MASK: &str = "***";

/// Endings of a name (property or query key) whose value is a secret, matched on
/// the last word: `passphrase`, `auth_token`, `stream-key`, `X-Amz-Signature`.
/// `pbkeylen` or `key-int-max` do not end in one, so they stay readable.
const SECRET_ENDINGS: &[&str] = &[
    "key",
    "sig",
    "auth",
    "pass",
    "pwd",
    "passwd",
    "password",
    "passphrase",
    "token",
    "secret",
    "signature",
    "credential",
    "streamid",
];

/// Whether a property or query key names a secret.
pub fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let last = lower
        .rsplit(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or("");
    SECRET_ENDINGS.iter().any(|end| last.ends_with(end))
}

/// Mask the secrets in one URL.
///
/// - userinfo: everything up to the last `@` before the query. A password with
///   an unencoded `/` or `@` must not end it early.
/// - authority: anything that is not plain `host[:port]` or `[v6][:port]` is
///   masked up to the first `/` or `?`.
/// - query: each value whose key is secret (`passphrase=***`); pairs end at
///   `&` or `?`.
/// - an `@` behind the first `?` can be a password with a raw `?` as well as a
///   query value; the two cannot be told apart, so all after the scheme goes.
/// - `rtmp*` schemes: as [`rtmp_url`].
///
/// ```text
/// srt://h:9000?mode=caller&passphrase=abc -> srt://h:9000?mode=caller&passphrase=***
/// https://u:p@h/a.mp4?token=abc           -> https://***@h/a.mp4?token=***
/// srt://u:p?w=x@h:9000                    -> srt://***
/// ```
pub fn url(raw: &str) -> String {
    let scheme = raw.split(':').next().unwrap_or("").to_ascii_lowercase();
    if scheme.starts_with("rtmp") {
        return rtmp_url(raw);
    }

    let Some(parts) = Parts::of(raw) else {
        return format!("{}{}", scheme_of(raw), MASK);
    };
    let mut out = parts.head();
    out.push_str(parts.path);
    if let Some(query) = parts.query {
        out.push('?');
        out.push_str(&mask_params(query));
    }
    out
}

/// Mask the secrets in an RTMP URL, scheme or not.
///
/// As [`url`], and besides the last path segment, the stream key, which for
/// most servers is the whole authorisation, and the whole query, which servers
/// use for tokens under any name. Host and application stay, so a line still
/// names its output.
///
/// ```text
/// rtmp://u:p@h/live/key?t=1 -> rtmp://***@h/live/***?***
/// ```
pub fn rtmp_url(raw: &str) -> String {
    let Some(parts) = Parts::of(raw) else {
        return format!("{}{}", scheme_of(raw), MASK);
    };
    let mut out = parts.head();

    // Only with a path, so a bare host is left alone.
    match parts.path.rfind('/') {
        Some(slash) if !parts.path.is_empty() => {
            out.push_str(&parts.path[..=slash]);
            out.push_str(MASK);
        }
        _ => out.push_str(parts.path),
    }
    if parts.query.is_some() {
        out.push('?');
        out.push_str(MASK);
    }
    out
}

/// A URL cut at the places that matter for masking.
struct Parts<'a> {
    /// `srt://`, or empty when there is no scheme before any `/`, `?` or `@`.
    scheme: &'a str,
    /// Whether there was userinfo before the host.
    userinfo: bool,
    /// The host and port, or `None` when it is no plain `host[:port]`.
    authority: Option<&'a str>,
    /// From the first `/` after the authority up to the query.
    path: &'a str,
    /// After the first `?`.
    query: Option<&'a str>,
}

impl<'a> Parts<'a> {
    /// `None` when an `@` follows the first `?` (see [`url`]).
    fn of(raw: &'a str) -> Option<Self> {
        let scheme = scheme_of(raw);
        let rest = &raw[scheme.len()..];
        let (before, query) = match rest.split_once('?') {
            Some((before, query)) => (before, Some(query)),
            None => (rest, None),
        };
        if query.is_some_and(|q| q.contains('@')) {
            return None;
        }
        let (userinfo, after) = match before.rfind('@') {
            Some(at) => (true, &before[at + 1..]),
            None => (false, before),
        };
        let (authority, path) = match after.find('/') {
            Some(slash) => after.split_at(slash),
            None => (after, ""),
        };
        Some(Parts {
            scheme,
            userinfo,
            authority: plain_host(authority).then_some(authority),
            path,
            query,
        })
    }

    /// Scheme, masked userinfo and the authority, masked when not plain.
    fn head(&self) -> String {
        let mut out = self.scheme.to_string();
        if self.userinfo {
            out.push_str(MASK);
            out.push('@');
        }
        out.push_str(self.authority.unwrap_or(MASK));
        out
    }
}

/// `srt://` of `srt://h`: a run of scheme characters and `://`, or empty.
fn scheme_of(raw: &str) -> &str {
    let Some(sep) = raw.find("://") else {
        return "";
    };
    let name = &raw[..sep];
    if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    {
        return &raw[..sep + 3];
    }
    ""
}

/// Whether `authority` is `host[:port]` or `[v6][:port]`, port digits only.
/// Anything else may be (part of) a credential.
fn plain_host(authority: &str) -> bool {
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((addr, after)) => {
                let plain_addr = addr
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.' | '%'));
                if !plain_addr {
                    return false;
                }
                match after.strip_prefix(':') {
                    Some(port) => ("", port),
                    None if after.is_empty() => ("", ""),
                    None => return false,
                }
            }
            None => return false,
        },
        None => authority.split_once(':').unwrap_or((authority, "")),
    };
    let host_ok = host
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'));
    host_ok && port.chars().all(|c| c.is_ascii_digit())
}

/// Mask the value of each secret `key=value` pair in a query string. Pairs
/// end at `&`, and at `?` (a second query mark).
fn mask_params(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for pair in query.split_inclusive(['&', '?']) {
        let (body, sep) = match pair.strip_suffix(['&', '?']) {
            Some(body) => (body, &pair[body.len()..]),
            None => (pair, ""),
        };
        match body.split_once('=') {
            Some((key, _)) if is_secret_name(key) => {
                out.push_str(key);
                out.push('=');
                out.push_str(MASK);
            }
            _ => out.push_str(body),
        }
        out.push_str(sep);
    }
    out
}

/// Mask every URL and every secret `name=value` pair in free text: a log line,
/// a GStreamer error, a gst-launch string, a `{:?}` dump.
///
/// Words end at blanks, quotes, backslashes and angle brackets, not at `,` or
/// `)`, because an SRT stream id such as `#!::r=live/feed,m=publish` holds
/// both. A secret value is never cut at such a mark, though (fail closed): one
/// that opens with a quote, also an escaped `\"`, runs to the matching quote
/// not preceded by a backslash, or to the end; any other runs to the next
/// blank. `{:?}` escapes every quote inside a value, so this is what keeps
/// `passphrase=ab\"cd` whole.
pub fn text(s: &str) -> Cow<'_, str> {
    if !s.contains("://") && !s.contains('=') {
        return Cow::Borrowed(s);
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(c) = rest.chars().next() {
        if is_boundary(c) {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let end = rest.find(is_boundary).unwrap_or(rest.len());
        let (masked, open) = token(&rest[..end]);
        rest = &rest[end..];

        match (open, opening_quote(rest)) {
            (Open::No, _) => out.push_str(&masked),
            // `passphrase="two words"`: the quoted run is the value.
            (Open::Empty, Some(quote)) => {
                out.push_str(masked.strip_suffix(MASK).unwrap_or(&masked));
                out.push_str(quote);
                out.push_str(MASK);
                out.push_str(quote);
                rest = &rest[quoted_end(rest, quote)..];
            }
            // The value goes on behind a quote or backslash: up to a blank.
            _ => {
                out.push_str(&masked);
                let blank = rest.find(char::is_whitespace).unwrap_or(rest.len());
                rest = &rest[blank..];
            }
        }
    }

    if out == s {
        return Cow::Borrowed(s);
    }
    Cow::Owned(out)
}

/// Where a word in free text ends.
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '`' | '<' | '>')
}

/// Whether a masked word leaves a secret value open at its end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Open {
    /// Nothing open.
    No,
    /// `passphrase=`: the value has not started yet.
    Empty,
    /// `passphrase=ab`: the value may go on.
    Started,
}

/// Mask one word: a URL, possibly behind `uri=`, or a pair.
fn token(t: &str) -> (Cow<'_, str>, Open) {
    if let Some(sep) = t.find("://") {
        // Walk back over the scheme, so `uri=srt://…` keeps its `uri=`.
        let start = t[..sep]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map(|i| i + 1)
            .unwrap_or(0);
        // Only a plain `name=` is kept in front; `rtmp?://…` or
        // `rtmp:/h/key?r=http://…` is one odd URL.
        let plain_name = t[..start].strip_suffix('=').is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        });
        let start = match plain_name {
            true => start,
            false => 0,
        };
        let masked = format!("{}{}", &t[..start], url(&t[start..]));
        let open = match masked.ends_with(MASK) && masked != t {
            false => Open::No,
            true if t.ends_with('=') => Open::Empty,
            true => Open::Started,
        };
        return (Cow::Owned(masked), open);
    }

    match t.split_once('=') {
        Some((name, value)) if is_secret_name(name) => {
            let open = match value.is_empty() {
                true => Open::Empty,
                false => Open::Started,
            };
            (Cow::Owned(format!("{}={}", name, MASK)), open)
        }
        _ => (Cow::Borrowed(t), Open::No),
    }
}

/// The quote that opens `rest`, with the backslashes escaping it: `"`, `'`,
/// `\"` in a `{:?}` dump, `\\\"` in a dump of a dump.
fn opening_quote(rest: &str) -> Option<&str> {
    let escapes = rest.len() - rest.trim_start_matches('\\').len();
    match rest[escapes..].chars().next() {
        Some(q @ ('"' | '\'')) => Some(&rest[..escapes + q.len_utf8()]),
        _ => None,
    }
}

/// The end of a value that opens with `quote` at the start of `rest`: behind
/// the same quote with exactly as many backslashes before it (an inner quote
/// is escaped once more), or the end of `rest`.
fn quoted_end(rest: &str, quote: &str) -> usize {
    let escapes = quote.len() - 1;
    let mark = &quote[escapes..];
    let mut from = quote.len();
    while let Some(i) = rest[from..].find(mark) {
        let at = from + i;
        let before = &rest[quote.len()..at];
        let run = before.len() - before.trim_end_matches('\\').len();
        if run == escapes {
            return at + 1;
        }
        from = at + 1;
    }
    rest.len()
}

/// A property value as it may be logged: masked whole when its name is secret
/// (`auth_token`, `passphrase`), its URLs masked when it is a string.
pub fn value<'a>(name: &str, v: &'a PropertyValue) -> Cow<'a, PropertyValue> {
    if is_secret_name(name) {
        return Cow::Owned(PropertyValue::String(MASK.to_string()));
    }
    let PropertyValue::String(s) = v else {
        return Cow::Borrowed(v);
    };
    match text(s) {
        Cow::Borrowed(_) => Cow::Borrowed(v),
        Cow::Owned(masked) => Cow::Owned(PropertyValue::String(masked)),
    }
}

/// A copy of a flow with every property passed through [`value`], for a
/// `{:?}` dump in a log. Only built when the line is actually emitted.
pub fn flow(flow: &Flow) -> Flow {
    let mut copy = flow.clone();
    for element in &mut copy.elements {
        mask_all(&mut element.properties);
        for props in element.pad_properties.values_mut() {
            mask_all(props);
        }
    }
    for block in &mut copy.blocks {
        mask_all(&mut block.properties);
    }
    copy
}

/// Replace each property value by its loggable form.
fn mask_all(props: &mut std::collections::HashMap<String, PropertyValue>) {
    for (name, v) in props.iter_mut() {
        if let Cow::Owned(masked) = value(name, v) {
            *v = masked;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::builtin::mpegtssrt::MpegTsSrtOutputBuilder;
    use crate::blocks::builtin::mpegtssrt_input::MpegTsSrtInputBuilder;
    use crate::blocks::{BlockBuildContext, BlockBuilder};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Everything logged on this thread while `f` runs, at every level.
    fn capture_logs(f: impl FnOnce()) -> String {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || Writer(sink.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    struct Writer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Writer {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The SRT blocks logged their whole `srt_uri` at `info` on every build,
    /// passphrase and stream id included.
    #[test]
    fn srt_blocks_log_no_passphrase() {
        gstreamer::init().expect("GStreamer init");
        let uri = "srt://192.0.2.1:9000?mode=caller&passphrase=topsecret1&streamid=feed42";
        let mut properties = HashMap::new();
        properties.insert("srt_uri".to_string(), PropertyValue::String(uri.into()));
        let ctx = BlockBuildContext::new(vec![], "all".to_string());

        let logs = capture_logs(|| {
            MpegTsSrtInputBuilder
                .build("srt_in", &properties, &ctx)
                .expect("input block");
            MpegTsSrtOutputBuilder
                .build("srt_out", &properties, &ctx)
                .expect("output block");
        });

        assert!(logs.contains("SRT source configured"), "{}", logs);
        assert!(logs.contains("SRT sink configured"), "{}", logs);
        for secret in ["topsecret1", "feed42"] {
            assert!(!logs.contains(secret), "{} in:\n{}", secret, logs);
        }
    }

    #[test]
    fn srt_passphrase_and_streamid_are_masked() {
        let raw = "srt://192.0.2.1:9000?mode=caller&passphrase=topsecret1&streamid=#!::r=live/feed,m=publish&latency=120";
        let masked = url(raw);
        assert_eq!(
            masked,
            "srt://192.0.2.1:9000?mode=caller&passphrase=***&streamid=***&latency=120"
        );
    }

    /// An `@` behind the `?` can be a password with a raw `?` or a query
    /// value (a stream id `u=user@domain`); everything after the scheme goes.
    #[test]
    fn an_at_sign_in_the_query_masks_all_after_the_scheme() {
        for raw in [
            "srt://192.0.2.1:9000?streamid=#!::u=a@b,r=x&passphrase=topsecret1",
            "srt://192.0.2.1:9000?passphrase=top@secret1&latency=120",
            "srt://user:pa?ss=x@h:9000",
        ] {
            assert_eq!(url(raw), "srt://***", "{}", raw);
        }
        assert_eq!(url("https://h/a?foo@bar&token=abc"), "https://***");
        assert_eq!(url("rtmp://user:pw?x=1@host/app/key"), "rtmp://***");
    }

    /// Anything but plain `host[:port]` in the authority may be a credential.
    #[test]
    fn an_odd_authority_is_masked() {
        assert_eq!(url("srt://u:[x]@h:1?a=1"), "srt://***@h:1?a=1");
        assert_eq!(url("srt://us(er)x:9000?a=1"), "srt://***?a=1");
        assert_eq!(url("srt://[::1]:9000?a=1"), "srt://[::1]:9000?a=1");
        assert_eq!(
            url("srt://kamera-büro.local:9000"),
            "srt://kamera-büro.local:9000"
        );
    }

    /// The reviewers' adversarial inputs: no secret survives, nothing panics.
    /// URL-shaped ones go through `url` and `text`, the rest through one of them.
    #[test]
    fn adversarial_inputs_leak_nothing() {
        let urls: &[(&str, &[&str])] = &[
            ("https://h/a?foo@bar&token=SEKRET", &["SEKRET"]),
            ("https://h/a?mail@x.de=1&token=SEKRET", &["SEKRET"]),
            ("https://h?my@token=SEKRET", &["SEKRET"]),
            ("?a@b", &[]),
            ("?@", &[]),
            ("@", &[]),
            ("@?", &[]),
            ("?", &[]),
            ("", &[]),
            ("://", &[]),
            ("?://@", &[]),
            (
                "https://user:p@ss@h?x@y&passphrase=SEKRET",
                &["SEKRET", "p@ss"],
            ),
            ("srt://h?passphrase=SEKRET&x=@", &["SEKRET"]),
            ("rtmp://u:SEKRET@h/app/KEYX?x@y", &["SEKRET", "KEYX"]),
            ("rtmp://h?a@b/KEYX", &["KEYX"]),
            ("http://h/@/x?token=SEKRET", &["SEKRET"]),
            (
                "srt://user:pa?ss=SEKA@h:9000?passphrase=SEKB",
                &["SEKA", "SEKB"],
            ),
            ("srt://user:SEKRET?R@h:9000", &["SEKRET"]),
            ("https://h/x?token=SEKA%40SEKB", &["SEKA", "SEKB"]),
            ("https://u%3ASEKRET@h/x", &["SEKRET"]),
            ("srt://ü:SEKA@hö?passphrase=SEKB", &["SEKA", "SEKB"]),
            ("srt://ü?ä@ö&token=SEKRET", &["SEKRET"]),
            ("rtmp://ü/ä/KEYX?ö@ü", &["KEYX"]),
            ("srt://h:9000?mode=caller&passphrase=SEKRET", &["SEKRET"]),
            ("srt://[::1]:9000?passphrase=SEKRET", &["SEKRET"]),
            ("srt://u:SEKRET@[::1]:9000?x@y", &["SEKRET"]),
            ("srt://[::1]?passphrase=p@SEKRET", &["SEKRET"]),
            ("srt://h:9000?streamid=#!::u=a@b,r=SEKRET", &["SEKRET"]),
            ("srt://user:1234?x=SEKRET@h:9000", &["SEKRET"]),
            ("srt://user:?x=SEKRET@h:9000", &["SEKRET"]),
            ("srt://:12?x=SEKRET@h", &["SEKRET"]),
            ("srt://user:12/w?x=SEKRET@h", &["SEKRET"]),
            ("srt://u:[x]?a=SEKRET@h", &["SEKRET"]),
            ("srt://u:[SEKRET]@h", &["SEKRET"]),
            ("srt://us(SEKRET)er:9000", &["SEKRET"]),
            ("rtmp://user:1234?x=SEKRET@h/app/KEYX", &["SEKRET", "KEYX"]),
            ("srt://u:p@ss?w=SEKRET@h", &["SEKRET"]),
            ("srt://h:9000?a=1?passphrase=SEKRET", &["SEKRET"]),
            ("https://h/a#x?token=SEKRET", &["SEKRET"]),
            ("rtmp?://h/app/KEYX", &["KEYX"]),
            ("rtmp:/h/KEYX?r=http://x", &["KEYX"]),
            ("h/live/KEYX?cb=http://x", &[]),
            ("rtmp://user:p/ss@host/live/KEYX", &["p/ss", "KEYX"]),
            ("us@er://x", &["us@"]),
            ("turn://user:SEKRET@turn.example.com:3478", &["SEKRET"]),
        ];
        // No `://`: text cannot tell these from prose, the callers use `url`.
        let url_only: &[(&str, &[&str])] = &[
            ("rtmp:/user:SEKRET@host/live/KEYX", &["SEKRET", "KEYX"]),
            ("turn:user:SEKRET@turn.example.com", &["SEKRET"]),
        ];
        let texts: &[(&str, &[&str])] = &[
            ("passphrase=ab\\SEKRET x", &["SEKRET"]),
            ("passphrase=ab'SEKRET x", &["SEKRET"]),
            ("uri=srt://h?passphrase=ab'SEKRET x", &["SEKRET"]),
            ("e: \"passphrase=ab\\\"SEKRET\" x\"", &["SEKRET"]),
            ("passphrase='SEKA\\'SEKB' x", &["SEKA", "SEKB"]),
            ("token=\"SEKA\\\"SEKB\" x", &["SEKA", "SEKB"]),
            ("passphrase=\\SEKRET", &["SEKRET"]),
            ("passphrase=\\\\\"SEKA SEKB\\\\\"", &["SEKA", "SEKB"]),
            ("uri=srt://h?passphrase=\"SEKA SEKB\"", &["SEKA", "SEKB"]),
            (
                "srtsink passphrase=\"two SEKA words\" latency=120",
                &["SEKA"],
            ),
            (
                "{\"e\":\"passphrase=\\\"a\\\\\\\"SEKA\\\" SEKB\"}",
                &["SEKA"],
            ),
            ("passphrase=\"SEKRET", &["SEKRET"]),
            ("x passphrase=\"ü SEKRET ö\" y", &["SEKRET"]),
            ("PassPhrase='SEKA SEKB'", &["SEKA", "SEKB"]),
            ("token=\"", &[]),
            ("token=\\\"", &[]),
            ("a=\"ü", &[]),
            ("ü=\"ä", &[]),
            ("name=\"v\" passphrase=\"SEKRET\" after \"q\"", &["SEKRET"]),
            ("a passphrase='SEKRET' b 'c' d", &["SEKRET"]),
            (
                "Could not open srt://192.0.2.1:9000?passphrase=SEKRET: refused",
                &["SEKRET"],
            ),
            (
                "srtsink uri=srt://h:1?streamid=SEKA passphrase=SEKB latency=120",
                &["SEKA", "SEKB"],
            ),
            ("String(\"rtmp://h/live/KEYX\")", &["KEYX"]),
            ("auth-token='SEKA SEKB'", &["SEKA", "SEKB"]),
        ];

        for (raw, secrets) in urls {
            for shown in [url(raw), text(raw).into_owned()] {
                for secret in *secrets {
                    assert!(!shown.contains(secret), "{:?} -> {:?}", raw, shown);
                }
            }
        }
        for (raw, secrets) in url_only {
            let shown = url(raw);
            for secret in *secrets {
                assert!(!shown.contains(secret), "{:?} -> {:?}", raw, shown);
            }
        }
        for (raw, secrets) in texts {
            let _ = url(raw);
            let shown = text(raw);
            for secret in *secrets {
                assert!(!shown.contains(secret), "{:?} -> {:?}", raw, shown);
            }
        }
        // Each text also as a `{:?}` dump, which escapes its quotes once more.
        for (raw, secrets) in texts {
            let dump = format!("{:?}", raw);
            let shown = text(&dump);
            for secret in *secrets {
                assert!(!shown.contains(secret), "{:?} -> {:?}", dump, shown);
            }
        }
    }

    /// `rtmp_url` cut a slice with an offset taken from another string.
    #[test]
    fn a_query_mark_before_the_scheme_does_not_panic() {
        for raw in [
            "rtmp?://h/app/key",
            "rtmp:/h/key?r=http://x",
            "h/live/key?cb=http://x",
        ] {
            assert!(!rtmp_url(raw).contains("key"), "{}", raw);
            let _ = url(raw);
        }
    }

    #[test]
    fn srt_without_secrets_is_unchanged() {
        let raw = "srt://:9000?mode=listener&latency=200&pbkeylen=16";
        assert_eq!(url(raw), raw);
    }

    #[test]
    fn userinfo_is_masked_by_the_last_at_sign() {
        assert_eq!(
            url("https://user:p@ss/word@example.com/a.mp4"),
            "https://***@example.com/a.mp4"
        );
        assert_eq!(url("us@er://x"), "***@er://x");
    }

    #[test]
    fn rtmp_stream_key_and_query_are_masked() {
        assert_eq!(
            url("rtmps://example.com/live/streamkey123?token=abc"),
            "rtmps://example.com/live/***?***"
        );
        assert_eq!(url("rtmps://example.com"), "rtmps://example.com");
        // A mistyped scheme is still RTMP, and still masked.
        let masked = url("rtmp:/user:pass@host/live/key");
        assert!(!masked.contains("pass"), "{}", masked);
        assert!(!masked.ends_with("/key"), "{}", masked);
    }

    #[test]
    fn http_signature_is_masked() {
        assert_eq!(
            url("https://example.com/a.mp4?X-Amz-Credential=c&X-Amz-Signature=s&x=1"),
            "https://example.com/a.mp4?X-Amz-Credential=***&X-Amz-Signature=***&x=1"
        );
    }

    #[test]
    fn secret_names() {
        for name in [
            "passphrase",
            "auth_token",
            "auth-token",
            "authToken",
            "stream_key",
            "password",
            "streamid",
            "key",
        ] {
            assert!(is_secret_name(name), "{}", name);
        }
        for name in ["pbkeylen", "key-int-max", "srt_uri", "location", "latency"] {
            assert!(!is_secret_name(name), "{}", name);
        }
    }

    #[test]
    fn free_text_masks_urls_and_pairs() {
        let line = "Could not open srt://192.0.2.1:9000?passphrase=abcdefghij: refused";
        assert_eq!(
            text(line),
            "Could not open srt://192.0.2.1:9000?passphrase=*** refused"
        );

        let launch =
            "videotestsrc ! srtsink uri=srt://h:1?streamid=feed passphrase=abcdefghij latency=120";
        assert_eq!(
            text(launch),
            "videotestsrc ! srtsink uri=srt://h:1?streamid=*** passphrase=*** latency=120"
        );

        let dump = format!("{:?}", PropertyValue::String("rtmp://h/live/key".into()));
        // The stream key may hold a quote, so the mask runs to the blank.
        assert_eq!(text(&dump), "String(\"rtmp://h/live/***");
    }

    #[test]
    fn free_text_without_secrets_is_borrowed() {
        assert!(matches!(text("state changed to PLAYING"), Cow::Borrowed(_)));
        assert!(matches!(text("latency=120 srt://h:1"), Cow::Borrowed(_)));
    }

    #[test]
    fn property_values() {
        let token = PropertyValue::String("bearer-123".into());
        assert_eq!(
            format!("{:?}", value("auth_token", &token)),
            "String(\"***\")"
        );

        let uri = PropertyValue::String("srt://h:1?passphrase=abcdefghij".into());
        assert_eq!(
            format!("{:?}", value("srt_uri", &uri)),
            "String(\"srt://h:1?passphrase=***\")"
        );

        let latency = PropertyValue::Int(120);
        assert!(matches!(value("latency", &latency), Cow::Borrowed(_)));
    }

    #[test]
    fn flow_dump_has_no_secrets() {
        let json = serde_json::json!({
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "f",
            "elements": [{
                "id": "e",
                "element_type": "srtsink",
                "properties": {"passphrase": "abcdefghij", "uri": "srt://h:1?passphrase=abcdefghij"},
                "pad_properties": {},
                "position": [0.0, 0.0]
            }],
            "blocks": [{
                "id": "b",
                "block_definition_id": "builtin.whip_output",
                "properties": {"auth_token": "bearer-123", "srt_uri": "srt://h:2?streamid=feed"},
                "position": {"x": 0.0, "y": 0.0}
            }]
        });
        let flow: Flow = serde_json::from_value(json).expect("flow");
        let dump = format!("{:?}", super::flow(&flow));
        for secret in ["abcdefghij", "bearer-123", "feed"] {
            assert!(!dump.contains(secret), "{} in {}", secret, dump);
        }
        assert!(dump.contains("srt://h:1"), "{}", dump);
    }
}
