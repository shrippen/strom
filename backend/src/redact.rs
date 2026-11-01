//! Mask secrets before they reach a log line or an error text.
//!
//! Stream targets carry their credentials inline: an SRT `passphrase=` or
//! `streamid=` in the query, `user:pass@` before the host, an RTMP stream key in
//! the path, a token in an HTTP query. Blocks log the URI they configure, a flow
//! dump at `debug` carries every property, and GStreamer error texts may quote a
//! URI, so all of them go through here.
//!
//! The rules fail closed by construction: a part of a URL is shown only when it
//! matches a short list of plain forms (a host, a port, a path of plain
//! segments, `key=value` pairs of plain characters), everything else is masked.
//! Over-redaction is the acceptable direction: a log line may lose a harmless
//! part, a secret never survives.

use std::borrow::Cow;

use strom_types::{Flow, PropertyValue};

/// What a masked secret is replaced with.
pub const MASK: &str = "***";

/// Endings of a name (property or query key) whose value is a secret, matched
/// on the name in lower case without `-`, `_` and other separators:
/// `passphrase`, `auth_token`, `stream-key`, `streamKey`, `X-Amz-Signature`.
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
    "credentials",
    "authorization",
    "cookie",
    "streamid",
    "hash",
    "hmac",
    "md5",
];

/// Values that name an authorization scheme, behind which the secret follows
/// as one more word: `Authorization: Bearer abc`.
const AUTH_SCHEMES: &[&str] = &["bearer", "basic", "digest"];

/// Whether a property or query key names a secret.
pub fn is_secret_name(name: &str) -> bool {
    let plain = plain_name(name);
    SECRET_ENDINGS.iter().any(|end| plain.ends_with(end))
}

/// `name` in lower case, `%xx` decoded, letters and digits only: `Auth_Token`,
/// `auth-token`, `authToken` and `auth%5Ftoken` all read `authtoken`.
fn plain_name(name: &str) -> String {
    let bytes = name.as_bytes();
    let mut out = String::with_capacity(name.len());
    let mut i = 0;
    while i < bytes.len() {
        let mut byte = bytes[i];
        let hex = |b: u8| (b as char).to_digit(16);
        if byte == b'%' && i + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                byte = (high * 16 + low) as u8;
                i += 2;
            }
        }
        if byte.is_ascii_alphanumeric() {
            out.push(byte.to_ascii_lowercase() as char);
        }
        i += 1;
    }
    out
}

/// Mask the secrets in one URL.
///
/// Shown are only the scheme, a plain `host[:port]` or `[v6][:port]`, path
/// segments of plain characters (letters, digits, `.-_~%`) and query pairs
/// whose key and value are plain characters (letters, digits, `.-_~`). The
/// value of a secret key is masked; the first part that is not plain is masked
/// together with all after it. Userinfo runs to the last `@` before the query
/// and is masked; an `@` in the query (a password with a raw `?`, or a value)
/// masks everything after the scheme. `rtmp*` schemes: as [`rtmp_url`];
/// `http`, `https`, `ws`, `wss`: as [`origin`]. Text that does not start
/// with `scheme://` is masked whole.
///
/// ```text
/// srt://h:9000?mode=caller&passphrase=abc -> srt://h:9000?mode=caller&passphrase=***
/// https://u:p@h/a.mp4?token=abc           -> https://***@h/***
/// srt://u:p?w=x@h:9000                    -> srt://***
/// ```
pub fn url(raw: &str) -> String {
    url_as(raw, Rules::PLAIN)
}

/// Mask the secrets in an RTMP URL.
///
/// As [`url`], and besides everything after the application (the first path
/// segment), the stream key, which for most servers is the whole
/// authorisation, and the whole query, which servers use for tokens under any
/// name. Host and application stay, so a line still names its output.
///
/// ```text
/// rtmp://u:p@h/live/key?t=1 -> rtmp://***@h/live/***
/// ```
pub fn rtmp_url(raw: &str) -> String {
    url_as(raw, Rules::RTMP)
}

/// Only where a URL goes: `scheme://host[:port]`, the path and query masked.
///
/// For HTTP endpoints (WHIP, WHEP, web pages), whose path may be the whole
/// credential: Cloudflare Stream publishes to
/// `https://customer-<id>.cloudflarestream.com/<key>/webRTC/publish`.
///
/// ```text
/// https://h:8443/abc/webRTC/publish?x=1 -> https://h:8443/***
/// ```
pub fn origin(raw: &str) -> String {
    url_as(raw, Rules::ORIGIN)
}

/// [`url`] with `rules`, or stricter ones when the scheme asks for them.
fn url_as(raw: &str, rules: Rules) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let Some(sep) = raw.find("://") else {
        return MASK.to_string();
    };
    let scheme = &raw[..sep];
    if !is_scheme(scheme) {
        return MASK.to_string();
    }
    mask_url(raw, sep, rules.and(Rules::of(scheme))).shown
}

/// What of a URL's path and query may show.
#[derive(Clone, Copy)]
struct Rules {
    /// How many path segments may show.
    segments: usize,
    /// Whether query pairs may show.
    query: bool,
}

impl Rules {
    const PLAIN: Rules = Rules {
        segments: usize::MAX,
        query: true,
    };
    /// The application; the stream key and query are masked.
    const RTMP: Rules = Rules {
        segments: 1,
        query: false,
    };
    const ORIGIN: Rules = Rules {
        segments: 0,
        query: false,
    };

    /// The rules of a scheme: RTMP for `rtmp*`, origin only for HTTP and
    /// WebSocket (a key in the path is common there), else plain.
    fn of(scheme: &str) -> Rules {
        let scheme = scheme.to_ascii_lowercase();
        if scheme.starts_with("rtmp") {
            return Rules::RTMP;
        }
        match scheme.as_str() {
            "http" | "https" | "ws" | "wss" => Rules::ORIGIN,
            _ => Rules::PLAIN,
        }
    }

    /// The stricter of both.
    fn and(self, other: Rules) -> Rules {
        Rules {
            segments: self.segments.min(other.segments),
            query: self.query && other.query,
        }
    }
}

/// Whether `name` is a URL scheme: a letter, then letters, digits, `+-.`.
fn is_scheme(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_alphabetic()) && name.chars().all(is_scheme_char)
}

fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// A URL as shown, and where masking began.
struct Masked {
    shown: String,
    /// Byte offset in the URL of the first masked part, and in `shown`.
    from: Option<(usize, usize)>,
    /// Whether the URL ends in a masked part.
    tail: bool,
}

impl Masked {
    fn show(&mut self, part: &str) {
        self.shown.push_str(part);
        if !part.is_empty() {
            self.tail = false;
        }
    }

    fn mask(&mut self, at: usize) {
        self.from.get_or_insert((at, self.shown.len()));
        self.shown.push_str(MASK);
        self.tail = true;
    }
}

/// Mask `raw`, a URL whose scheme ends at `sep` (the `://`); see [`url`].
fn mask_url(raw: &str, sep: usize, rules: Rules) -> Masked {
    let body = sep + 3;
    let mut m = Masked {
        shown: raw[..body].to_string(),
        from: None,
        tail: false,
    };
    let rest = &raw[body..];
    let query = rest.find('?');
    let before = &rest[..query.unwrap_or(rest.len())];
    if query.is_some_and(|q| rest[q..].contains('@')) {
        m.mask(body);
        return m;
    }

    let host_at = match before.rfind('@') {
        Some(at) => {
            m.mask(body);
            m.shown.push('@');
            at + 1
        }
        None => 0,
    };
    let after = &before[host_at..];
    let slash = after.find('/').unwrap_or(after.len());
    if !plain_host(&after[..slash]) {
        m.mask(body + host_at);
        return m;
    }
    m.show(&after[..slash]);

    let mut at = body + host_at + slash;
    if let Some(path) = after[slash..].strip_prefix('/') {
        for (n, segment) in path.split('/').enumerate() {
            m.shown.push('/');
            at += 1;
            let plain = segment.bytes().all(|b| plain_byte(b) || b == b'%');
            if !segment.is_empty() && (n >= rules.segments || !plain) {
                m.mask(at);
                return m;
            }
            m.show(segment);
            at += segment.len();
        }
    }

    let Some(q) = query else {
        return m;
    };
    m.shown.push('?');
    let mut at = body + q + 1;
    if !rules.query {
        m.mask(at);
        return m;
    }
    for (n, pair) in rest[q + 1..].split('&').enumerate() {
        if n > 0 {
            m.shown.push('&');
        }
        match pair.split_once('=') {
            Some((key, value)) if !key.is_empty() && key.bytes().all(plain_byte) => {
                m.show(key);
                m.shown.push('=');
                let value_at = at + key.len() + 1;
                if !value.bytes().all(plain_byte) {
                    m.mask(value_at);
                    return m;
                }
                match is_secret_name(key) {
                    true => m.mask(value_at),
                    false => m.show(value),
                }
            }
            _ if pair.is_empty() => {}
            _ => {
                m.mask(at);
                return m;
            }
        }
        at += pair.len() + 1;
    }
    m
}

/// A byte of a plain path segment, query key or value.
fn plain_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'~')
}

/// Whether `authority` is `host[:port]` or `[v6%zone][:port]`, port digits
/// only. Anything else may be (part of) a credential.
fn plain_host(authority: &str) -> bool {
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let Some((addr, after)) = v6.split_once(']') else {
                return false;
            };
            let (addr, zone) = addr.split_once('%').unwrap_or((addr, ""));
            let plain = !addr.is_empty()
                && addr
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() || matches!(c, ':' | '.'))
                && zone.bytes().all(plain_byte);
            if !plain {
                return false;
            }
            match after.strip_prefix(':') {
                Some(port) => ("", port),
                None if after.is_empty() => ("", ""),
                None => return false,
            }
        }
        None => authority.split_once(':').unwrap_or((authority, "")),
    };
    host.chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && port.bytes().all(|b| b.is_ascii_digit())
}

/// Mask every URL and every secret `name=value` or `name: value` in free text:
/// a log line, a GStreamer error, a gst-launch string, a `{:?}` dump.
///
/// A word is a run of non-blanks; quotes do not end it.
/// - A word with `://` is a URL behind a plain prefix (`uri=`, `String("`,
///   `<`): the URL goes through [`url`] (so HTTP shows only its origin), and
///   a trailing `:`, `,`, `)`, quote or the like stays outside it
///   (`srt://h:9000: refused` keeps its host). A URL inside an open quote
///   (`uri="srt://…`, `"uri=srt://…`) is the whole quoted run, blanks
///   included, up to the partner of that quote, when it is masked or an `@`
///   follows. Any other prefix masks the whole word.
/// - At each `=` or `:` of a word (quotes around the name allowed), a name
///   that ends in a secret ending ([`is_secret_name`]) masks the value: up to
///   the end of the word, or the next word when the separator ends this one
///   (`passphrase: abc`), and one more behind `Bearer`, `Basic`, `Digest`.
/// - `user:pass@host` without `//` is masked whole.
/// - A quote that opens in a masked part (`passphrase="two words"`,
///   `passphrase=(string)"a b"`, `\"` in a `{:?}` dump) masks through its
///   partner with as many backslashes before it, or to the end of the text.
/// - Quotes are followed from the start of the text: a `"`, `'` or `` ` ``
///   at the start of a word or behind a non-letter opens, the same quote with
///   as many backslashes closes. Whenever masking begins inside an open quote
///   (`"passphrase=a b"`, `'uri=srt://h?passphrase=a b'`, a quote opened in
///   an earlier word), the mask runs through the partner of the innermost
///   one, or to the end of the text; on past it when a letter follows (an
///   apostrophe in the value), to the end of its word when anything but a
///   blank or `,;)]}` follows (`'a'\''b'`) or an `@` is in that word.
///
/// Not covered, because nothing tells them from prose: a raw blank inside a
/// URL that is not quoted (`srt://u:a b@h` shows `b@h`), a raw `&` inside a
/// secret query value followed by `name=` (`passphrase=a&b=c` shows `b=c`), a
/// secret in the path of a URL that is neither HTTP nor RTMP, and a secret
/// behind a name that has no secret ending or with no `=` or `:` between
/// them. A stray quote before a quoted secret (`the '90s … 'passphrase=a b'`)
/// pairs with the wrong one and leaves the rest of the secret outside.
/// A property value is no prose: [`value`] masks a URL in it whole.
///
/// A bug in here must neither abort strom (bus messages are masked in a
/// GLib callback, where a panic aborts) nor let a secret through: a panic
/// masks the whole text.
pub fn text(s: &str) -> Cow<'_, str> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| mask_text(s)))
        .unwrap_or_else(|_| Cow::Owned(MASK.to_string()))
}

/// [`text`] without the guard against a panic.
fn mask_text(s: &str) -> Cow<'_, str> {
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    let mut word = Word {
        end: 0,
        at_sign: None,
    };
    let mut value_next = false;
    let mut quotes = Quotes::default();
    while let Some(c) = s[at..].chars().next() {
        if c.is_whitespace() {
            out.push(c);
            at += c.len_utf8();
            continue;
        }
        if at >= word.end {
            let end = s[at..]
                .find(char::is_whitespace)
                .map_or(s.len(), |i| at + i);
            let at_sign = s[at..end].rfind('@').map(|i| at + i);
            word = Word { end, at_sign };
        }
        let step = match value_next {
            true => {
                let w = &s[at..word.end];
                let shown_at = out.len();
                out.push_str(MASK);
                Step {
                    next: masked_end(s, at, at, word.end),
                    value_next: AUTH_SCHEMES.iter().any(|a| w.eq_ignore_ascii_case(a)),
                    mask: Some((at, shown_at)),
                }
            }
            false => mask_word(s, at, word, &mut quotes, &mut out),
        };
        (at, value_next) = (step.next, step.value_next);
        // Masking began inside an open quote: the secret runs to its partner.
        if let Some((from, shown_at)) = step.mask {
            if let Some(stop) = quotes.masked_to(s, from) {
                out.truncate(shown_at);
                out.push_str(MASK);
                // Never short of what `mask_word` masked.
                (at, value_next) = (stop.max(step.next), false);
            }
        }
    }

    if out == s {
        return Cow::Borrowed(s);
    }
    Cow::Owned(out)
}

/// What [`mask_word`] did: where to go on, whether the next word is a
/// secret value, and where masking began (in `s` and in the output).
struct Step {
    next: usize,
    value_next: bool,
    mask: Option<(usize, usize)>,
}

impl Step {
    fn new(next: usize, value_next: bool, mask: Option<(usize, usize)>) -> Self {
        Step {
            next,
            value_next,
            mask,
        }
    }
}

/// The quotes open in the text read so far, innermost last: a `"`, `'` or
/// `` ` `` at the start of a word or behind a non-letter opens (an
/// apostrophe in `can't` does not), the same quote with as many backslashes
/// before it closes it and any opened inside it.
#[derive(Default)]
struct Quotes {
    open: Vec<(usize, usize)>,
    /// How far the text is read.
    read: usize,
    /// The last partner looked up: a quote open over many words is asked
    /// for once per URL in them.
    known: Option<(usize, Option<usize>)>,
    /// The next `@` from where it was last looked up, or none to the end.
    at_sign: Option<(usize, Option<usize>)>,
}

impl Quotes {
    /// How deep a closing quote looks for its opening one.
    const DEPTH: usize = 16;

    /// Read on to `to`.
    fn read_to(&mut self, s: &str, to: usize) {
        let b = s.as_bytes();
        for i in self.read..to {
            if !matches!(b[i], b'"' | b'\'' | b'`') {
                continue;
            }
            let escapes = backslashes_before(b, 0, i);
            let depth = self.open.len();
            let matching = self
                .open
                .iter()
                .rev()
                .take(Self::DEPTH)
                .position(|&(o, e)| b[o] == b[i] && e == escapes);
            match matching {
                Some(k) => self.open.truncate(depth - 1 - k),
                None => {
                    let after_letter = s[..i]
                        .chars()
                        .next_back()
                        .is_some_and(char::is_alphanumeric);
                    if !after_letter {
                        self.open.push((i, escapes));
                    }
                }
            }
        }
        self.read = self.read.max(to);
    }

    /// The partner of the open quote at `open` ([`partner`]), cached.
    fn partner(&mut self, s: &str, open: usize, escapes: usize) -> Option<usize> {
        if let Some((o, close)) = self.known {
            if o == open {
                return close;
            }
        }
        let close = partner(s.as_bytes(), open, escapes, s.len());
        self.known = Some((open, close));
        close
    }

    /// The first `@` at or behind `from`, cached: asked once per URL in a
    /// quote open over many words.
    fn at_sign_from(&mut self, s: &str, from: usize) -> Option<usize> {
        if let Some((looked, found)) = self.at_sign {
            if looked <= from && found.is_none_or(|a| a >= from) {
                return found;
            }
        }
        let found = s[from..].find('@').map(|i| from + i);
        self.at_sign = Some((from, found));
        found
    }

    /// The innermost quote open at `at`, and its backslashes. A quote that
    /// closed in a part skipped as masked is dropped here.
    fn open_at(&mut self, s: &str, at: usize) -> Option<(usize, usize)> {
        self.read_to(s, at);
        while let Some(&(open, escapes)) = self.open.last() {
            match self.partner(s, open, escapes) {
                Some(close) if close < at => {
                    self.open.pop();
                }
                _ => return Some((open, escapes)),
            }
        }
        None
    }

    /// Where a mask that begins at `from` ends when a quote is open there:
    /// behind its partner, or the end of `s`. Past the partner the mask goes
    /// on when a quote opened in the value runs past it (`masked_end`), to
    /// the next such quote when a letter follows (`'it's a'`: the partner was
    /// an apostrophe), and to the end of the partner's word when anything
    /// but a blank or `,;)]}` follows (`'a'\''b'`, `"a"'b'`: the value goes
    /// on) or an `@` follows in that word (a password with a quote).
    fn masked_to(&mut self, s: &str, from: usize) -> Option<usize> {
        let (open, escapes) = self.open_at(s, from)?;
        self.open.pop();
        let b = s.as_bytes();
        let Some(close) = self.partner(s, open, escapes) else {
            self.open.clear();
            self.read = b.len();
            return Some(b.len());
        };
        let inner = masked_end(s, from, from, close);
        let close_end = s[close..]
            .find(char::is_whitespace)
            .map_or(s.len(), |i| close + i);
        let after = s[close + 1..].chars().next();
        let ends = after.is_none_or(|c| c.is_whitespace() || ",;)]}".contains(c));
        let stop = if inner > close {
            inner
        } else if after.is_some_and(char::is_alphanumeric) {
            // An apostrophe: the value goes on to the next one.
            masked_end(s, close, close - escapes, close + 1).max(masked_end(
                s,
                close + 1,
                close + 1,
                close_end,
            ))
        } else if !ends || s[close..close_end].contains('@') {
            masked_end(s, close + 1, close + 1, close_end)
        } else {
            close + 1
        };
        self.read = self.read.max(stop);
        Some(stop)
    }
}

/// The word of [`text`] being masked: where it ends and its last `@`.
#[derive(Clone, Copy)]
struct Word {
    end: usize,
    at_sign: Option<usize>,
}

/// Mask the word, or the rest of it, that starts at `at` into `out`. Returns
/// where to go on, and whether the next word is a secret value.
fn mask_word(s: &str, at: usize, word: Word, quotes: &mut Quotes, out: &mut String) -> Step {
    let end = word.end;
    let w = &s[at..end];
    let url = w.find("://").map(|sep| {
        let start = w[..sep]
            .char_indices()
            .rev()
            .find(|&(_, c)| !is_scheme_char(c))
            .map_or(0, |(i, c)| i + c.len_utf8());
        (start, sep)
    });

    let names = url.map_or(w, |(start, _)| &w[..start]);
    if let Some(value) = secret_value(names) {
        out.push_str(&w[..value]);
        if value == w.len() {
            return Step::new(end, true, None);
        }
        let mask = Some((at + value, out.len()));
        out.push_str(MASK);
        // `Authorization:Bearer abc`: the secret is the next word.
        if AUTH_SCHEMES
            .iter()
            .any(|a| w[value..].eq_ignore_ascii_case(a))
        {
            return Step::new(end, true, mask);
        }
        return Step::new(masked_end(s, at + value, at + value, end), false, mask);
    }

    let Some((start, sep)) = url else {
        // `user:pass@host`, a URL without `//`.
        if word
            .at_sign
            .is_some_and(|a| a > at && s[at..a].contains(':'))
        {
            let mask = Some((at, out.len()));
            out.push_str(MASK);
            return Step::new(masked_end(s, at, at, end), false, mask);
        }
        out.push_str(w);
        return Step::new(end, false, None);
    };
    let prefix = &w[..start];
    let scheme = &w[start..sep];
    if !is_scheme(scheme) || !prefix.chars().all(is_prefix_char) {
        let mask = Some((at, out.len()));
        out.push_str(MASK);
        return Step::new(masked_end(s, at, at, end), false, mask);
    }
    out.push_str(prefix);
    let url_at = at + start;
    let sep = sep - start;
    let rules = Rules::of(scheme);

    // A URL in quotes (`uri="srt://…"`, `"uri=srt://…"`, a quote open since
    // an earlier word) is the quoted run up to the partner of the quote
    // (`"srt://h?a=1","b":2`), or to the end of the partner's word when an
    // `@` follows it there (a password with a quote). Behind a blank the run
    // counts when the URL in this word is masked (`"srt://h?passphrase=a b"`)
    // or an `@` follows before the partner (`"srt://u:1 2@h"`); otherwise a
    // clean URL ends with its word, so a quoted message that starts with one
    // stays readable. A quote without a partner masks all to the end.
    let mut stop = end;
    if let Some((open, escapes)) = quotes.open_at(s, url_at) {
        let Some(close) = quotes.partner(s, open, escapes) else {
            out.push_str(&s[url_at..url_at + sep + 3]);
            let mask = Some((url_at + sep + 3, out.len()));
            out.push_str(MASK);
            return Step::new(s.len(), false, mask);
        };
        let (close_end, at_after) = match close < end {
            true => (end, word.at_sign.is_some_and(|a| a > close)),
            false => {
                let close_end = s[close..]
                    .find(char::is_whitespace)
                    .map_or(s.len(), |i| close + i);
                (close_end, s[close..close_end].contains('@'))
            }
        };
        let take = close < end
            || at_after
            || quotes.at_sign_from(s, end).is_some_and(|a| a < close)
            || mask_url(
                &s[url_at..url_at + url_len(&s[url_at..end], sep)],
                sep,
                rules,
            )
            .from
            .is_some();
        if take {
            stop = if at_after { close_end } else { close - escapes };
        }
        // Never before the end of the scheme (a stale quote must not cut
        // the URL backwards).
        stop = stop.max(url_at + sep + 3);
    }

    let part = &s[url_at..stop];
    let url_end = url_len(part, sep);
    let m = mask_url(&part[..url_end], sep, rules);
    let shown_at = out.len();
    out.push_str(&m.shown);
    let Some((from, from_shown)) = m.from else {
        out.push_str(&part[url_end..]);
        return Step::new(stop, false, None);
    };
    let mask = Some((url_at + from, shown_at + from_shown));
    let url_stop = url_at + url_end;
    let next = masked_end(s, url_at + from, url_at, url_stop);
    if next > url_stop {
        return Step::new(next, false, mask);
    }
    // A masked end may go on behind the punctuation: drop it.
    if !m.tail {
        out.push_str(&part[url_end..]);
    }
    Step::new(stop, false, mask)
}

/// The length of the URL at the start of `part` without the punctuation
/// behind it ([`is_trailing`]); `sep` is where its `://` starts.
fn url_len(part: &str, sep: usize) -> usize {
    let bracket = part.contains('[');
    let mut len = part.len();
    while len > sep + 3 && is_trailing(part.as_bytes()[len - 1], bracket) {
        len -= 1;
    }
    len
}

/// Characters that may stand before a URL in a word: `uri=`, `String("`,
/// `<`, `(location:`, `\"`.
fn is_prefix_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '_' | '.' | '-' | '=' | ':' | ',' | '(' | '[' | '{' | '<' | '"' | '\'' | '`' | '\\'
        )
}

/// Punctuation behind a URL that is not part of it: `srt://h:9000:`,
/// `(srt://h)`, `"srt://h",`. A `]` only when the URL has no `[`.
fn is_trailing(b: u8, bracket: bool) -> bool {
    matches!(
        b,
        b':' | b',' | b';' | b'.' | b'!' | b'?' | b')' | b'}' | b'>' | b'"' | b'\'' | b'`' | b'\\'
    ) || (b == b']' && !bracket)
}

/// Where the value of the first secret `name=` or `name:` in `w` starts. The
/// name is the run of name characters before the separator, also in quotes:
/// `"passphrase":`, `\"token\"=`.
fn secret_value(w: &str) -> Option<usize> {
    for (i, c) in w.char_indices() {
        if c != '=' && c != ':' {
            continue;
        }
        let mut name_end = i;
        if let Some(unquoted) = w[..i].strip_suffix(['"', '\'']) {
            name_end = unquoted.trim_end_matches('\\').len();
        }
        let name_start = w[..name_end]
            .char_indices()
            .rev()
            .find(|&(_, c)| !is_name_char(c))
            .map_or(0, |(j, c)| j + c.len_utf8());
        if is_secret_name(&w[name_start..name_end]) {
            return Some(i + 1);
        }
    }
    None
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '%')
}

/// The end of a masked run from `from` to at least `end`: a quote that opens
/// in it masks through its partner, wherever that is, or to the end of `s`.
/// Backslashes before a quote are counted back to `run_at`.
fn masked_end(s: &str, from: usize, run_at: usize, end: usize) -> usize {
    let b = s.as_bytes();
    let mut end = end;
    let mut i = from;
    while i < end {
        if matches!(b[i], b'"' | b'\'' | b'`') {
            let escapes = backslashes_before(b, run_at, i);
            match partner(b, i, escapes, b.len()) {
                Some(close) => {
                    i = close + 1;
                    end = end.max(i);
                }
                None => return b.len(),
            }
            continue;
        }
        i += 1;
    }
    end
}

/// How many backslashes stand right before `at`, back to `from`.
fn backslashes_before(b: &[u8], from: usize, at: usize) -> usize {
    b[from..at]
        .iter()
        .rev()
        .take_while(|&&c| c == b'\\')
        .count()
}

/// The partner of the quote at `open` that has `escapes` backslashes before
/// it: the next same quote with exactly as many (an inner quote is escaped
/// once more), before `end`.
fn partner(b: &[u8], open: usize, escapes: usize, end: usize) -> Option<usize> {
    let quote = b[open];
    (open + 1..end).find(|&i| b[i] == quote && backslashes_before(b, open + 1, i) == escapes)
}

/// A property value as it may be logged: masked whole when its name is secret
/// (`auth_token`, `passphrase`); a string that is a URL as one URL (blanks in
/// it are no word ends), only its origin when the name ends in `endpoint`
/// (`whip_endpoint`: the path may be the key); any other string as [`text`].
pub fn value<'a>(name: &str, v: &'a PropertyValue) -> Cow<'a, PropertyValue> {
    if is_secret_name(name) {
        return Cow::Owned(PropertyValue::String(MASK.to_string()));
    }
    let PropertyValue::String(s) = v else {
        return Cow::Borrowed(v);
    };
    let trimmed = s.trim();
    let is_url = trimmed
        .find("://")
        .is_some_and(|sep| is_scheme(&trimmed[..sep]));
    let shown = if plain_name(name).ends_with("endpoint") {
        Cow::Owned(origin(trimmed))
    } else if is_url {
        Cow::Owned(url(trimmed))
    } else {
        text(s)
    };
    if shown == s.as_str() {
        return Cow::Borrowed(v);
    }
    Cow::Owned(PropertyValue::String(shown.into_owned()))
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

    // Shared test vectors: keep identical in strom `backend/src/redact.rs` and
    // lmix `lmix-types/src/srt.rs`. Every input of every review round, each
    // also checked as a `{:?}` dump and a dump of a dump.

    /// Inputs and the secrets in them that must not survive.
    const LEAKS: &[(&str, &[&str])] = &[
        // An `@` in the query.
        ("https://h/a?foo@bar&token=SEKRET", &["SEKRET"]),
        ("https://h/a?mail@x.de=1&token=SEKRET", &["SEKRET"]),
        ("https://h?my@token=SEKRET", &["SEKRET"]),
        (
            "https://user:p@ss@h?x@y&passphrase=SEKRET",
            &["SEKRET", "p@ss"],
        ),
        ("srt://h?passphrase=SEKRET&x=@", &["SEKRET"]),
        ("srt://h:9000?streamid=#!::u=a@b,r=SEKRET", &["SEKRET"]),
        (
            "srt://192.0.2.1:9000?streamid=#!::u=a@b,r=x&passphrase=SEKRET",
            &["SEKRET"],
        ),
        (
            "srt://192.0.2.1:9000?passphrase=top@SEKRET&latency=120",
            &["SEKRET"],
        ),
        ("srt://[::1]?passphrase=p@SEKRET", &["SEKRET"]),
        ("srt://ü?ä@ö&token=SEKRET", &["SEKRET"]),
        // Userinfo with `@`, `/`, `?`, `[`, quotes, backslashes.
        ("rtmp://u:SEKRET@h/app/KEYX?x@y", &["SEKRET", "KEYX"]),
        ("rtmp://h?a@b/KEYX", &["KEYX"]),
        ("http://h/@/x?token=SEKRET", &["SEKRET"]),
        (
            "srt://user:pa?ss=SEKA@h:9000?passphrase=SEKB",
            &["SEKA", "SEKB"],
        ),
        ("srt://user:SEKRET?R@h:9000", &["SEKRET"]),
        ("srt://u:SEKA?ss=x@h:9000", &["SEKA"]),
        ("srt://u:SEKA/b@h:9000", &["SEKA"]),
        ("srt://user:1234?x=SEKRET@h:9000", &["SEKRET"]),
        ("srt://user:?x=SEKRET@h:9000", &["SEKRET"]),
        ("srt://:12?x=SEKRET@h", &["SEKRET"]),
        ("srt://user:12/w?x=SEKRET@h", &["SEKRET"]),
        ("srt://u:[x]?a=SEKRET@h", &["SEKRET"]),
        ("srt://u:[SEKRET]@h", &["SEKRET"]),
        ("srt://us(SEKRET)er:9000", &["SEKRET"]),
        ("srt://u:p@ss?w=SEKRET@h", &["SEKRET"]),
        ("srt://u:SEKRET@[::1]:9000?x@y", &["SEKRET"]),
        ("srt://ü:SEKA@hö?passphrase=SEKB", &["SEKA", "SEKB"]),
        ("https://u%3ASEKRET@h/x", &["SEKRET"]),
        ("http://u:SEKRET@[fe80::1%eth0]:8080/x", &["SEKRET"]),
        ("turn://user:SEKRET@turn.example.com:3478", &["SEKRET"]),
        ("rtmp://user:p/ss@host/live/KEYX", &["p/ss", "KEYX"]),
        ("rtmp://user:1234?x=SEKRET@h/app/KEYX", &["SEKRET", "KEYX"]),
        (
            "rtmps://alice:SEKRET@example.com/live/KEYX",
            &["SEKRET", "KEYX"],
        ),
        ("srt://user:'SEKRET@h:9000?mode=caller", &["SEKRET"]),
        ("srt://u:12'SEKRET@h:9000", &["SEKRET"]),
        ("uri=srt://u:1'SEKRET@h", &["SEKRET"]),
        ("srt://:'SEKRET@h:9000", &["SEKRET"]),
        ("srt://h:9000'SEKRET@x", &["SEKRET"]),
        ("https://user:\"SEKRET@h/x", &["SEKRET"]),
        ("srt://user:\"SEKRET@h", &["SEKRET"]),
        ("srt://\\SEKRET@h", &["SEKRET"]),
        ("srt://u:`SEKRET@h", &["SEKRET"]),
        ("srt://u:<SEKRET@h", &["SEKRET"]),
        ("srt://u:ab/c\"SEKRET@h", &["SEKRET"]),
        ("String(\"srt://u:12\\\"SEKRET@h:9000\")", &["SEKRET"]),
        ("\"srt://u:pa\"SEKRET@h\"", &["SEKRET"]),
        ("\"srt://user\"SEKRET@h\"", &["SEKRET"]),
        ("uri=\"srt://u:'SEKA SEKB'@h\"", &["SEKA", "SEKB"]),
        ("us@er://x", &["us@"]),
        // Without `//`.
        ("rtmp:/user:SEKRET@host/live/KEYX", &["SEKRET", "KEYX"]),
        ("turn:user:SEKRET@turn.example.com", &["SEKRET"]),
        // Query values and keys.
        ("srt://h:9000?mode=caller&passphrase=SEKRET", &["SEKRET"]),
        ("srt://[::1]:9000?passphrase=SEKRET", &["SEKRET"]),
        ("srt://h:9000?a=1?passphrase=SEKRET", &["SEKRET"]),
        ("https://h/a#x?token=SEKRET", &["SEKRET"]),
        ("https://h/x?token=SEKA%40SEKB", &["SEKA", "SEKB"]),
        ("https://h/x?auth_token=SEKRET", &["SEKRET"]),
        ("https://h/x?X-Amz-Signature=SEKRET&x=1", &["SEKRET"]),
        ("srt://u:p@h:9000?x=1'Y&passphrase=SEKRET", &["SEKRET"]),
        ("uri=srt://h?cb=https://u:SEKRET@x/y", &["SEKRET"]),
        ("https://h/?next=srt://h?passphrase=SEKRET", &["SEKRET"]),
        ("uri=srt://h?passphrase=ab'SEKRET x", &["SEKRET"]),
        ("uri=srt://h?passphrase=\"SEKA SEKB\"", &["SEKA", "SEKB"]),
        (
            "{\"uri\":\"srt://h:1?passphrase=SEKRET\",\"latency\":120}",
            &["SEKRET"],
        ),
        (
            "{\"uri\":\"srt://h?passphrase=ab\\\"SEKRET\",\"x\":1}",
            &["SEKRET"],
        ),
        ("<srt://h:1?streamid=SEKRET>", &["SEKRET"]),
        (
            "Could not open srt://192.0.2.1:9000?passphrase=SEKRET: refused",
            &["SEKRET"],
        ),
        (
            "srtsink uri=srt://h:1?streamid=SEKA passphrase=SEKB latency=120",
            &["SEKA", "SEKB"],
        ),
        // RTMP stream keys.
        ("rtmp://h/live/KEYX", &["KEYX"]),
        ("rtmp://h/live/KEYX: Connection refused", &["KEYX"]),
        ("rtmp://u:SEKA@h/app/KEYX?x=SEKB", &["SEKA", "SEKB", "KEYX"]),
        ("rtmp://ü/ä/KEYX?ö@ü", &["KEYX"]),
        ("rtmp?://h/app/KEYX", &["KEYX"]),
        ("rtmp:/h/KEYX?r=http://x", &["KEYX"]),
        ("String(\"rtmp://h/live/KEYX\")", &["KEYX"]),
        // A URL in quotes is the quoted run, blanks and all.
        ("srtsink uri=\"srt://h:9000?passphrase=SEKA SEKB\" sync=false", &["SEKA", "SEKB"]),
        ("uri='srt://h:9000?mode=caller&passphrase=SEKA SEKB'", &["SEKA", "SEKB"]),
        ("uri=\"srt://u:SEKA SEKB@h:9000\"", &["SEKA", "SEKB"]),
        ("\"srt://u:12 SEKRET@h:9000\"", &["SEKRET"]),
        ("location=\"rtmp://h/live/SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("whip-endpoint=\"https://h/whip/ep?token=SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("\"srt://h:9000?passphrase=SEKA&x=1 SEKB\"", &["SEKA", "SEKB"]),
        ("String(\"srt://h:9000?passphrase=SEKA SEKB\")", &["SEKA", "SEKB"]),
        (
            "{\"srt_uri\": String(\"srt://h:9000?mode=caller&passphrase=SEKA SEKB\")}",
            &["SEKA", "SEKB"],
        ),
        ("x \"srt://h:9000?passphrase=SEKA SEKB", &["SEKA", "SEKB"]),
        (
            "gst-launch-1.0 srtsink \"uri=srt://h:9000?mode=caller&passphrase=SEKA SEKB\"",
            &["SEKA", "SEKB"],
        ),
        ("\"uri=srt://h:9000?passphrase=SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("'location=rtmp://h/live/SEKA SEKB' x", &["SEKA", "SEKB"]),
        ("uri=`srt://h?passphrase=SEKA SEKB`", &["SEKA", "SEKB"]),
        ("uri='srt://h?passphrase=SEKA's SEKB'", &["SEKA", "SEKB"]),
        // A quote around the whole argument holds a secret value too.
        (
            "gst-launch-1.0 srtsrc \"passphrase=SEKA SEKB\" uri=srt://h:9000 ! fakesink",
            &["SEKA", "SEKB"],
        ),
        ("srtsrc 'passphrase=SEKA SEKB' latency=120", &["SEKA", "SEKB"]),
        ("srtsink \"streamid=#!::r=live,passphrase=SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("\"name=out uri=srt://h:9000?passphrase=SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("\"name=out passphrase=SEKA SEKB\" x", &["SEKA", "SEKB"]),
        ("srtsrc 'passphrase=SEKA's SEKB' x", &["SEKA", "SEKB"]),
        ("\"srt://u:pa\"-SEKRET@h\"", &["SEKRET"]),
        // A key in the path of an HTTP endpoint (Cloudflare Stream WHIP).
        (
            "whipclientsink0: Unexpected status 401 for https://customer-abc.cloudflarestream.com/SEKRETKEY0123/webRTC/publish",
            &["SEKRETKEY0123"],
        ),
        ("error sending request for url (https://h/SEKRETKEY/webRTC/publish)", &["SEKRETKEY"]),
        ("wss://h/SEKRETKEY", &["SEKRETKEY"]),
        // Pairs: names, several in a word, a secret name before `://`.
        ("auth_token=SEKRET", &["SEKRET"]),
        ("auth-token=SEKRET", &["SEKRET"]),
        ("stream_key=SEKRET", &["SEKRET"]),
        ("streamKey=SEKRET", &["SEKRET"]),
        ("X-Amz-Signature=SEKRET", &["SEKRET"]),
        ("pass%70hrase=SEKRET", &["SEKRET"]),
        ("pass%77ord=SEKRET", &["SEKRET"]),
        ("(passphrase=SEKRET)", &["SEKRET"]),
        ("latency=120&passphrase=SEKRET", &["SEKRET"]),
        ("mode=caller;passphrase=SEKRET", &["SEKRET"]),
        ("opts=latency=1,streamid=SEKRET", &["SEKRET"]),
        ("a=b,passphrase=SEKRET", &["SEKRET"]),
        ("caps=application/x-srt,passphrase=SEKRET", &["SEKRET"]),
        ("passphrase=SEKRET://x", &["SEKRET"]),
        ("token=ab:SEKRET//x://y", &["SEKRET"]),
        ("{\"details\":\"passphrase=SEKRET\"}", &["SEKRET"]),
        // Quotes and backslashes in values.
        ("passphrase=ab\\SEKRET x", &["SEKRET"]),
        ("passphrase=\\SEKRET", &["SEKRET"]),
        ("passphrase=ab'SEKRET x", &["SEKRET"]),
        ("e: \"passphrase=ab\\\"SEKRET\" x\"", &["SEKRET"]),
        ("passphrase='SEKA\\'SEKB' x", &["SEKA", "SEKB"]),
        ("token=\"SEKA\\\"SEKB\" x", &["SEKA", "SEKB"]),
        ("passphrase=\\\\\"SEKA SEKB\\\\\"", &["SEKA", "SEKB"]),
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
        ("name=\"v\" passphrase=\"SEKRET\" after \"q\"", &["SEKRET"]),
        ("a passphrase='SEKRET' b 'c' d", &["SEKRET"]),
        ("auth-token='SEKA SEKB'", &["SEKA", "SEKB"]),
        ("passphrase=(string)\"SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("passphrase=ab\"SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("é?passphrase=\"SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("passphrase=\"abc\"SEKRET", &["SEKRET"]),
        ("passphrase= SEKRET", &["SEKRET"]),
        // `name: value` and quoted names.
        ("\"passphrase\"=\"SEKRET\"", &["SEKRET"]),
        ("'passphrase'='SEKRET'", &["SEKRET"]),
        ("{\"passphrase\":\"SEKRET\",\"x\":1}", &["SEKRET"]),
        ("\"passphrase\": \"SEKA SEKB\"", &["SEKA", "SEKB"]),
        ("Foo { passphrase: \"SEKRET\" }", &["SEKRET"]),
        ("Authorization: Bearer SEKRET", &["SEKRET"]),
        ("Authorization:Bearer SEKRET", &["SEKRET"]),
        ("Authorization=Bearer SEKRET", &["SEKRET"]),
        ("srt://h?md5=SEKRET&expires=1", &["SEKRET"]),
        ("srt://h?wowzatokenhash=SEKRET", &["SEKRET"]),
        ("hdnts=exp=1~hmac=SEKRET", &["SEKRET"]),
        // Nothing to leak, nothing may panic.
        ("token=\"", &[]),
        ("token=\\\"", &[]),
        ("a=\"ü", &[]),
        ("ü=\"ä", &[]),
        ("é://x", &[]),
        ("ü@é:ö", &[]),
        ("\"'\\", &[]),
        ("?a@b", &[]),
        ("?@", &[]),
        ("@", &[]),
        ("@?", &[]),
        ("?", &[]),
        ("", &[]),
        ("://", &[]),
        ("?://@", &[]),
        ("h/live/KEYX?cb=http://x", &[]),
        // A mask must not end before the end of the word `mask_word` masked.
        ("srtsrc 'passphrase=SEKA'\\''SEKB' latency=1", &["SEKA", "SEKB"]),
        ("srtsrc 'passphrase=SEKA'\"'\"'SEKB'", &["SEKA", "SEKB"]),
        ("srtsrc \"passphrase=SEKA\"'SEKB'", &["SEKA", "SEKB"]),
        ("'passphrase=SEKA'SEKB'SEKC'", &["SEKA", "SEKB", "SEKC"]),
    ];

    /// Ordinary lines that must come back unchanged.
    const UNCHANGED: &[&str] = &[
        "flow started id=abc name=\"Main\" latency=120",
        "srt://192.0.2.1:9000?mode=caller&latency=120",
        "srt://:9000?mode=listener&latency=200&pbkeylen=16",
        "key-int-max=60 bitrate=4000 pbkeylen=16",
        "Could not connect to srt://h:9000: Connection refused",
        "<srt://h:9000>",
        "pad src (caps=\"video/x-raw, width=1920\")",
        "Element { name: \"srtsink0\", uri: \"srt://h:9000?mode=caller\" }",
        "{\"uri\":\"srt://h:9000?mode=caller\",\"latency\":120}",
        "srt://kamera-büro.local:9000?latency=200",
        "http://[fe80::1%eth0]:8080",
        "Failed to open srt://h:9000?mode=caller&latency=120: Connection refused",
        "rtmp://a.rtmp.youtube.com/live2: Connection refused",
        "srtsink uri=\"srt://h:9000?mode=caller\" sync=false",
        "srtsink \"uri=srt://h:9000?mode=caller\" sync=false",
        "name=\"a\" uri=srt://h:9000 sync=false",
        "can't open srt://h:9000: Connection refused",
        "https://example.com/ refused",
        "rtmps://example.com/live",
        "Loading file: file:///home/user/My%20Video.mp4",
        "mail from user@example.com at 12:30:00 user=alice",
        "state changed to PLAYING",
    ];

    /// `s`, `{:?}` of it and `{:?}` of that.
    fn dumps(s: &str) -> [String; 3] {
        let once = format!("{:?}", s);
        let twice = format!("{:?}", once);
        [s.to_string(), once, twice]
    }

    /// Inputs that would be slow on a quadratic path, and odd ones.
    fn stress_inputs() -> Vec<String> {
        let n = 1 << 16;
        let mut inputs: Vec<String> = [
            "a",
            "\"",
            "'",
            "\\",
            "\\\"",
            "=",
            ":",
            "@",
            "a=",
            "x:",
            "passphrase=\"",
            "passphrase=a ",
            "srt://",
            "a://",
            "\"srt://h\"",
            "@?",
            "srt://h?a=1&",
            "ü",
            "😀=",
            "passphrase=😀\"",
        ]
        .iter()
        .map(|unit| unit.repeat(n / unit.len().max(1)))
        .collect();
        inputs.push(format!("passphrase=\"{}", "\\\"".repeat(n)));
        inputs.push(format!("srt://h?{}", "a=1&".repeat(n)));
        inputs
    }

    /// Inputs that once panicked: a quote that closed in a skipped part,
    /// a backtick, more nested quotes than are tracked.
    fn panic_inputs() -> Vec<String> {
        let mut inputs: Vec<String> = [
            "'til now: srtsrc \"passphrase=it's\" ! srtsink uri='srt://h:9000'",
            "the '90s srtsrc \"passphrase=it's x\" ! srtsink 'uri=srt://h:9000'",
            "a `\"passphrase=x` b\" srt://h:9000",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        for quote in ["\"", "'", "`"] {
            let nested: String = (0..20)
                .map(|i| format!(" {}{}a", "\\".repeat(i % 4), quote))
                .collect();
            inputs.push(format!("{} passphrase=x srt://h:9000?a=1 b", nested));
            inputs.push(format!("passphrase={} srt://h:9000 x", nested));
        }
        inputs
    }

    /// `n` pseudo-random gst-like lines of quotes, backslashes, pairs and
    /// URLs, the same on every run.
    fn fuzz_inputs(n: usize) -> impl Iterator<Item = String> {
        const TOKENS: &[&str] = &[
            "\"",
            "'",
            "`",
            "\\",
            " ",
            " ",
            "a",
            "it's",
            "=",
            ":",
            "@",
            "?",
            "&",
            ",",
            ")",
            "!",
            "srt://h:9000",
            "srt://u:p@h",
            "https://h/p?x=1",
            "rtmp://h/live/k",
            "passphrase=",
            "uri=",
            "token:",
            "Bearer ",
            "?passphrase=",
            "ü",
            "\n",
        ];
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        (0..n).map(move |_| {
            let len = 1 + next() % 24;
            (0..len)
                .map(|_| TOKENS[(next() % TOKENS.len() as u64) as usize])
                .collect()
        })
    }

    #[test]
    fn shared_vectors_leak_nothing() {
        for (raw, secrets) in LEAKS {
            for input in dumps(raw) {
                let shown = text(&input);
                for secret in *secrets {
                    assert!(!shown.contains(secret), "{:?} -> {:?}", input, shown);
                }
                let shown = url(&input);
                for secret in *secrets {
                    assert!(!shown.contains(secret), "url {:?} -> {:?}", input, shown);
                }
            }
        }
    }

    #[test]
    fn shared_vectors_keep_ordinary_lines() {
        for line in UNCHANGED {
            for input in dumps(line) {
                assert_eq!(text(&input), input.as_str());
            }
        }
    }

    #[test]
    fn long_and_odd_inputs_do_not_panic() {
        for input in stress_inputs() {
            let _ = text(&input);
            let _ = url(&input);
            let _ = rtmp_url(&input);
        }
    }

    /// The masking itself must not panic, not only `text` behind its guard.
    #[test]
    fn fuzzed_inputs_do_not_panic() {
        for input in panic_inputs().into_iter().chain(fuzz_inputs(100_000)) {
            let run = std::panic::catch_unwind(|| {
                let _ = mask_text(&input);
                let _ = url(&input);
                let _ = rtmp_url(&input);
            });
            assert!(run.is_ok(), "panicked on {:?}", input);
        }
    }

    #[test]
    fn srt_passphrase_and_streamid_are_masked() {
        assert_eq!(
            url("srt://192.0.2.1:9000?mode=caller&passphrase=topsecret1&latency=120"),
            "srt://192.0.2.1:9000?mode=caller&passphrase=***&latency=120"
        );
        // A value that is not plain masks all after it.
        assert_eq!(
            url("srt://h:9000?passphrase=topsecret1&streamid=#!::r=live/feed&latency=120"),
            "srt://h:9000?passphrase=***&streamid=***"
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
        assert_eq!(url("srt://us(er)x:9000?a=1"), "srt://***");
        assert_eq!(url("srt://[::1]:9000?a=1"), "srt://[::1]:9000?a=1");
        assert_eq!(
            url("srt://kamera-büro.local:9000"),
            "srt://kamera-büro.local:9000"
        );
    }

    /// Only plain parts survive: a path segment or query pair with anything
    /// else masks the rest, and text without `scheme://` is masked whole.
    #[test]
    fn what_is_not_plain_is_masked() {
        assert_eq!(url("srt://h/a b/c?x=1"), "srt://h/***");
        assert_eq!(url("srt://h/a?x=1&flag&y=2"), "srt://h/a?x=1&***");
        assert_eq!(url("srt://h/a?cb=http%3A%2F%2Fx"), "srt://h/a?cb=***");
        assert_eq!(url("turn:user:pw@h"), "***");
        assert_eq!(url(" rtmp://h/live/key"), "***");
        assert_eq!(url(""), "");
    }

    #[test]
    fn srt_without_secrets_is_unchanged() {
        let raw = "srt://:9000?mode=listener&latency=200&pbkeylen=16";
        assert_eq!(url(raw), raw);
    }

    #[test]
    fn userinfo_is_masked_by_the_last_at_sign() {
        assert_eq!(
            url("srt://user:p@ss/word@example.com/a"),
            "srt://***@example.com/a"
        );
        assert_eq!(url("us@er://x"), "***");
    }

    #[test]
    fn rtmp_stream_key_and_query_are_masked() {
        assert_eq!(
            url("rtmps://example.com/live/streamkey123?token=abc"),
            "rtmps://example.com/live/***"
        );
        assert_eq!(
            url("rtmps://example.com/live?token=abc"),
            "rtmps://example.com/live?***"
        );
        assert_eq!(url("rtmps://example.com"), "rtmps://example.com");
        assert_eq!(rtmp_url("h/live/key"), "***");
        // A mistyped scheme is still masked.
        assert_eq!(url("rtmp:/user:pass@host/live/key"), "***");
    }

    #[test]
    fn http_signature_is_masked() {
        assert_eq!(
            url("s3://example.com/a.mp4?X-Amz-Credential=c&X-Amz-Signature=s&x=1"),
            "s3://example.com/a.mp4?X-Amz-Credential=***&X-Amz-Signature=***&x=1"
        );
    }

    /// An HTTP path may be the whole key (Cloudflare Stream WHIP), so HTTP
    /// shows only where it goes.
    #[test]
    fn http_shows_only_its_origin() {
        let cloudflare = "https://customer-abc.cloudflarestream.com/0123abcd/webRTC/publish";
        assert_eq!(
            url(cloudflare),
            "https://customer-abc.cloudflarestream.com/***"
        );
        assert_eq!(
            origin(cloudflare),
            "https://customer-abc.cloudflarestream.com/***"
        );
        assert_eq!(origin("https://u:p@h:8443?x=1"), "https://***@h:8443?***");
        assert_eq!(origin("srt://h:9000/a?b=1"), "srt://h:9000/***");
        assert_eq!(origin("https://h"), "https://h");
        assert_eq!(origin("stage"), "***");
    }

    #[test]
    fn secret_names() {
        for name in [
            "passphrase",
            "auth_token",
            "auth-token",
            "authToken",
            "stream_key",
            "streamKey",
            "wowzatokenhash",
            "hmac",
            "md5",
            "password",
            "streamid",
            "key",
            "Authorization",
            "pass%70hrase",
        ] {
            assert!(is_secret_name(name), "{}", name);
        }
        for name in [
            "pbkeylen",
            "key-int-max",
            "srt_uri",
            "location",
            "latency",
            "",
        ] {
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

        let quoted = "srtsink passphrase=\"two secret words\" latency=120";
        assert_eq!(text(quoted), "srtsink passphrase=*** latency=120");

        let header = "Authorization: Bearer abc next";
        assert_eq!(text(header), "Authorization: *** *** next");

        let dump = format!("{:?}", PropertyValue::String("rtmp://h/live/key".into()));
        // The stream key may hold a quote, so the mask runs to the blank.
        // The mask runs through the quote the key is in.
        assert_eq!(text(&dump), "String(\"rtmp://h/live/***)");

        let launch = "srtsink uri=\"srt://h:1?passphrase=two words\" sync=false";
        assert_eq!(
            text(launch),
            "srtsink uri=\"srt://h:1?passphrase=*** sync=false"
        );

        let json = "{\"uri\":\"srt://h:1?mode=caller\",\"passphrase\":\"abc\"}";
        assert_eq!(
            text(json),
            "{\"uri\":\"srt://h:1?mode=caller\",\"passphrase\":***"
        );
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

        // A URL property is one URL, blanks and all.
        let spaced = PropertyValue::String("srt://h:1?passphrase=two words".into());
        assert_eq!(
            format!("{:?}", value("srt_uri", &spaced)),
            "String(\"srt://h:1?passphrase=***\")"
        );

        // An endpoint shows only where it goes.
        let endpoint = PropertyValue::String("https://h/0123abcd/webRTC/publish".into());
        assert_eq!(
            format!("{:?}", value("whip_endpoint", &endpoint)),
            "String(\"https://h/***\")"
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
