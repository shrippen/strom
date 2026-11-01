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
/// - userinfo: see [`without_userinfo`].
/// - query: each value whose key is secret (`passphrase=***`).
/// - `rtmp*` schemes: as [`rtmp_url`].
///
/// ```text
/// srt://h:9000?mode=caller&passphrase=abc -> srt://h:9000?mode=caller&passphrase=***
/// https://u:p@h/a.mp4?token=abc           -> https://***@h/a.mp4?token=***
/// ```
pub fn url(raw: &str) -> String {
    let scheme = raw.split(':').next().unwrap_or("").to_ascii_lowercase();
    if scheme.starts_with("rtmp") {
        return rtmp_url(raw);
    }

    let Some(plain) = without_userinfo(raw) else {
        return MASK.to_string();
    };
    match plain.split_once('?') {
        Some((head, query)) => format!("{}?{}", head, mask_params(query)),
        None => plain,
    }
}

/// Mask the secrets in an RTMP URL, scheme or not.
///
/// Besides userinfo, the last path segment, the stream key, which for most
/// servers is the whole authorisation, and the whole query, which servers use
/// for tokens under any name. Host and application stay, so a line still names
/// its output.
///
/// ```text
/// rtmp://u:p@h/live/key?t=1 -> rtmp://***@h/live/***?***
/// ```
pub fn rtmp_url(raw: &str) -> String {
    let Some(plain) = without_userinfo(raw) else {
        return MASK.to_string();
    };
    let (head, query) = match plain.split_once('?') {
        Some((head, _)) => (head, true),
        None => (plain.as_str(), false),
    };

    // Only with a path, so a bare host is left alone.
    let after_scheme = head.find("://").map(|i| i + 3).unwrap_or(0);
    let head = match head[after_scheme..].rfind('/') {
        Some(rel) if rel > 0 => format!("{}{}", &head[..after_scheme + rel + 1], MASK),
        _ => head.to_string(),
    };

    if query {
        return format!("{}?{}", head, MASK);
    }
    head
}

/// Mask userinfo: everything before the `@` that ends it.
///
/// Fails closed. A password with an unencoded `/` or `@` must not end the
/// authority early, so that is the **last** `@` before the query. A password
/// with an unencoded `?` must not hide behind the query either, so an `@`
/// after the first `?` ends userinfo too, unless the text up to it is a
/// parameter (holds `=` or `&`): `?streamid=u=a@b` stays a query for
/// [`mask_params`]. `None` when the `@` sits inside the scheme: nothing of
/// that string is worth echoing.
fn without_userinfo(raw: &str) -> Option<String> {
    let at = match raw.find('?') {
        Some(q) => raw[..q].rfind('@').or_else(|| {
            let tail = &raw[q + 1..];
            tail.find('@')
                .filter(|&i| !tail[..i].contains(['=', '&']))
                .map(|i| q + 1 + i)
        }),
        None => raw.rfind('@'),
    };
    let Some(at) = at else {
        return Some(raw.to_string());
    };
    let after_scheme = raw.find("://").map(|i| i + 3).unwrap_or(0);
    if at < after_scheme {
        return None;
    }
    Some(format!("{}{}{}", &raw[..after_scheme], MASK, &raw[at..]))
}

/// Mask the value of each secret `key=value` pair in a query string.
fn mask_params(query: &str) -> String {
    query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, _)) if is_secret_name(key) => format!("{}={}", key, MASK),
            _ => pair.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Mask every URL and every secret `name=value` pair in free text: a log line,
/// a GStreamer error, a gst-launch string, a `{:?}` dump.
///
/// Tokens end at whitespace and quotes only, not at `,` or `)`, because an SRT
/// stream id such as `#!::r=live/feed,m=publish` contains both.
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
        let word = &rest[..end];
        out.push_str(&token(word));
        rest = &rest[end..];

        // `passphrase="two words"`: the value is the quoted run that follows.
        let Some(name) = word.strip_suffix('=') else {
            continue;
        };
        if !is_secret_name(name) {
            continue;
        }
        let Some(quote) = ["\\\"", "\"", "'"]
            .into_iter()
            .find(|q| rest.starts_with(q))
        else {
            continue;
        };
        let close = rest[quote.len()..]
            .find(quote)
            .map(|i| quote.len() + i + quote.len())
            .unwrap_or(rest.len());
        out.push_str(quote);
        out.push_str(MASK);
        out.push_str(quote);
        rest = &rest[close..];
    }

    if out == s {
        return Cow::Borrowed(s);
    }
    Cow::Owned(out)
}

/// Where a token in free text ends.
fn is_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '`' | '<' | '>')
}

/// Mask one whitespace-free token: a URL, possibly behind `uri=`, or a pair.
fn token(t: &str) -> Cow<'_, str> {
    if let Some(sep) = t.find("://") {
        // Walk back over the scheme, so `uri=srt://…` keeps its `uri=`.
        let start = t[..sep]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map(|i| i + 1)
            .unwrap_or(0);
        return Cow::Owned(format!("{}{}", &t[..start], url(&t[start..])));
    }

    match t.split_once('=') {
        Some((name, value)) if !value.is_empty() && is_secret_name(name) => {
            Cow::Owned(format!("{}={}", name, MASK))
        }
        _ => Cow::Borrowed(t),
    }
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

    /// An `@` in the query (a stream id `u=user@domain`, a passphrase) used to
    /// count as the end of userinfo, which swallowed the `?` and left every
    /// parameter after it unmasked.
    #[test]
    fn an_at_sign_in_the_query_does_not_unmask_it() {
        assert_eq!(
            url("srt://192.0.2.1:9000?streamid=#!::u=a@b,r=x&passphrase=topsecret1"),
            "srt://192.0.2.1:9000?streamid=***&passphrase=***"
        );
        assert_eq!(
            url("srt://192.0.2.1:9000?passphrase=top@secret1&latency=120"),
            "srt://192.0.2.1:9000?passphrase=***&latency=120"
        );
        assert_eq!(
            url("https://example.com/a.mp4?token=abc123&mail=x@y.z"),
            "https://example.com/a.mp4?token=***&mail=x@y.z"
        );
        // A password with an unencoded `?` still loses everything up to its `@`.
        let masked = url("rtmp://user:p?ss@example.com/live/key");
        assert!(!masked.contains("p?ss"), "{}", masked);
        assert!(masked.contains("example.com/live/"), "{}", masked);
    }

    /// gst-launch quotes a value with blanks: `passphrase="two words"`.
    #[test]
    fn a_quoted_secret_value_is_masked_whole() {
        assert_eq!(
            text("srtsink passphrase=\"two secret words\" latency=120"),
            "srtsink passphrase=\"***\" latency=120"
        );
        assert_eq!(text("auth-token='a b'"), "auth-token='***'");
        let dump = format!("{:?}", "passphrase=\"a b\" x");
        assert_eq!(text(&dump), "\"passphrase=\\\"***\\\" x\"");
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
        assert_eq!(url("us@er://x"), MASK);
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
        assert_eq!(text(&dump), "String(\"rtmp://h/live/***\")");
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
