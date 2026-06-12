//! Digest access authentication (RFC 2617 / RFC 7616 subset).
//!
//! Port of vscode-restclient `utils/auth/digest.ts` (a got afterResponse
//! hook): `Authorization: Digest <user> <pass>` is stripped from the request
//! before sending; when the server answers 401 with a `WWW-Authenticate:
//! Digest ...` challenge, the digest response header is computed and the
//! request retried exactly once.
//!
//! Like the original, only MD5 and MD5-sess are supported and `nc` is always
//! `00000001`. A `SHA-256` challenge is still answered with an MD5 digest
//! (echoing `algorithm=SHA-256` back), faithfully mirroring digest.ts.
//!
//! Implemented in-tree rather than via an integration crate (e.g. `diqwest`)
//! because those wrap the client and drive their own request/retry cycle,
//! while we must reuse `execute`'s configured client (cookie jar, redirect
//! policy, timeout, invalid-cert acceptance) and mirror digest.ts's
//! permissive challenge regex rather than strict RFC parsing.

/// Credentials extracted from an `Authorization: Digest <user> <pass>` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub pass: String,
}

/// Mirrors httpClient.ts auth dispatch: the value splits on whitespace into
/// `[scheme, user, ...args]`; digest needs a case-insensitive `digest` scheme
/// and at least one arg after the user (the password, spaces re-joined).
/// `Digest <single-token>` is NOT digest shorthand — the original sends that
/// header to the wire untouched (there is no `user:pass` colon form either).
pub fn parse_credentials(authorization: &str) -> Option<Credentials> {
    let mut parts = authorization.split_whitespace();
    let scheme = parts.next()?;
    if !scheme.eq_ignore_ascii_case("digest") {
        return None;
    }
    let user = parts.next()?;
    let args: Vec<&str> = parts.collect();
    if args.is_empty() {
        return None;
    }
    Some(Credentials {
        user: user.to_string(),
        pass: args.join(" "),
    })
}

/// digest.ts: `header.split(' ')[0].toLowerCase() === 'digest'`.
pub fn is_digest_challenge(www_authenticate: &str) -> bool {
    www_authenticate
        .split(' ')
        .next()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("digest"))
}

/// Computes the `Authorization` value answering a Digest challenge, or `None`
/// when the response is not one (not 401 / no `WWW-Authenticate` / not a
/// Digest scheme). `url` must be the *response* URL (after redirects); the
/// digest `uri` is its path plus query, like node's `url.parse(...).path`.
pub fn answer_challenge(
    creds: &Credentials,
    method: &str,
    status: u16,
    www_authenticate: Option<&str>,
    url: &reqwest::Url,
) -> Option<String> {
    let header = www_authenticate?;
    if status != 401 || !is_digest_challenge(header) {
        return None;
    }
    let uri = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    // uuidv4().replace(/-/g, '') in the original.
    let cnonce = uuid::Uuid::new_v4().simple().to_string();
    Some(authorization_with_cnonce(
        creds,
        method,
        &uri,
        &parse_challenge(header),
        &cnonce,
    ))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Challenge {
    realm: String,
    nonce: String,
    qop: String,
    opaque: String,
    algorithm: String,
}

/// `[a-z0-9_-]` with the `i` flag, i.e. ASCII alphanumeric plus `_` and `-`.
fn is_param_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// Port of digest.ts's global scan with
/// `/([a-z0-9_-]+)=(?:"([^"]+)"|([a-z0-9_-]+))/gi` (itself copied from
/// request's lib/auth.js): quoted values win over bare tokens, quoted values
/// must be non-empty and have no escape handling, and anything that does not
/// match is skipped over (so `realm=""` simply never sets realm).
fn scan_params(header: &str) -> Vec<(&str, &str)> {
    let bytes = header.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_param_char(bytes[i]) {
            i += 1;
            continue;
        }
        let key_start = i;
        while i < bytes.len() && is_param_char(bytes[i]) {
            i += 1;
        }
        // Every suffix of this run ends at the same boundary byte, so when
        // the pair fails here, resuming after the run (or after the `=`)
        // visits exactly the positions the JS regex would retry and match.
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        let key = &header[key_start..i];
        let value_start = i + 1;
        if bytes.get(value_start) == Some(&b'"') {
            // quoted-string: `"([^"]+)"` — 1+ chars, no escapes
            let content_start = value_start + 1;
            match bytes[content_start..].iter().position(|&b| b == b'"') {
                Some(len) if len > 0 => {
                    out.push((key, &header[content_start..content_start + len]));
                    i = content_start + len + 1;
                }
                // Empty (`""`) or unterminated quote: no match; the regex
                // keeps scanning from inside the quoted region.
                _ => i = value_start,
            }
        } else {
            let mut j = value_start;
            while j < bytes.len() && is_param_char(bytes[j]) {
                j += 1;
            }
            if j > value_start {
                out.push((key, &header[value_start..j]));
            }
            i = j.max(value_start);
        }
    }
    out
}

fn parse_challenge(header: &str) -> Challenge {
    let mut challenge = Challenge::default();
    for (key, value) in scan_params(header) {
        // digest.ts assigns `challenge[match[1]]` verbatim, so only the exact
        // lowercase spellings reach the known fields (`Realm="x"` would land
        // on a stray property and leave realm empty); later duplicates win.
        match key {
            "realm" => challenge.realm = value.to_string(),
            "nonce" => challenge.nonce = value.to_string(),
            "qop" => challenge.qop = value.to_string(),
            "opaque" => challenge.opaque = value.to_string(),
            "algorithm" => challenge.algorithm = value.to_string(),
            _ => {}
        }
    }
    challenge
}

/// Port of `/(^|,)\s*auth\s*($|,)/.test(challenge.qop) && 'auth'` — the qop
/// list must contain a bare `auth` item (case-sensitive; `auth-int` alone
/// does not count and falls back to the legacy RFC 2069 computation).
fn negotiated_qop(qop_list: &str) -> Option<&'static str> {
    qop_list
        .split(',')
        .any(|item| item.trim() == "auth")
        .then_some("auth")
}

fn md5_hex(input: &str) -> String {
    use md5::{Digest as _, Md5};
    Md5::digest(input.as_bytes())
        .iter()
        .fold(String::with_capacity(32), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

fn push_param(params: &mut Vec<String>, key: &str, value: &str, quoted: bool) {
    // digest.ts skips falsy values, so empty fields are omitted entirely.
    if value.is_empty() {
        return;
    }
    if quoted {
        params.push(format!("{key}=\"{value}\""));
    } else {
        params.push(format!("{key}={value}"));
    }
}

/// Pure computation with an injectable cnonce so RFC test vectors apply.
fn authorization_with_cnonce(
    creds: &Credentials,
    method: &str,
    uri: &str,
    challenge: &Challenge,
    cnonce: &str,
) -> String {
    let qop = negotiated_qop(&challenge.qop);
    let nc = qop.map(|_| "00000001");
    let cnonce = qop.map(|_| cnonce);

    let mut ha1 = md5_hex(&format!(
        "{}:{}:{}",
        creds.user, challenge.realm, creds.pass
    ));
    if challenge.algorithm.eq_ignore_ascii_case("md5-sess") {
        // digest.ts hashes `ha1:nonce:cnonce` even when qop (and thus cnonce)
        // is absent, concatenating the JS literal `false`; mirrored as-is.
        let cnonce = cnonce.unwrap_or("false");
        ha1 = md5_hex(&format!("{ha1}:{}:{cnonce}", challenge.nonce));
    }
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let response = match (qop, nc, cnonce) {
        (Some(qop), Some(nc), Some(cnonce)) => md5_hex(&format!(
            "{ha1}:{}:{nc}:{cnonce}:{qop}:{ha2}",
            challenge.nonce
        )),
        _ => md5_hex(&format!("{ha1}:{}:{ha2}", challenge.nonce)),
    };

    // Same key order and quoting as digest.ts: qop/nc/algorithm bare, the
    // rest quoted, empty values dropped.
    let mut params = Vec::new();
    push_param(&mut params, "username", &creds.user, true);
    push_param(&mut params, "realm", &challenge.realm, true);
    push_param(&mut params, "nonce", &challenge.nonce, true);
    push_param(&mut params, "uri", uri, true);
    push_param(&mut params, "qop", qop.unwrap_or(""), false);
    push_param(&mut params, "response", &response, true);
    push_param(&mut params, "nc", nc.unwrap_or(""), false);
    push_param(&mut params, "cnonce", cnonce.unwrap_or(""), true);
    push_param(&mut params, "algorithm", &challenge.algorithm, false);
    push_param(&mut params, "opaque", &challenge.opaque, true);
    format!("Digest {}", params.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- credential detection (httpClient.ts dispatch) ---

    #[test]
    fn credentials_user_and_pass() {
        assert_eq!(
            parse_credentials("Digest user pass"),
            Some(Credentials {
                user: "user".into(),
                pass: "pass".into()
            })
        );
    }

    #[test]
    fn credentials_scheme_case_insensitive() {
        assert!(parse_credentials("digest user pass").is_some());
        assert!(parse_credentials("DIGEST user pass").is_some());
    }

    #[test]
    fn credentials_password_with_spaces_joined() {
        assert_eq!(
            parse_credentials("Digest user my secret pass")
                .unwrap()
                .pass,
            "my secret pass"
        );
    }

    #[test]
    fn credentials_single_token_is_not_digest_shorthand() {
        // Original requires args after the user; `Digest <token>` goes to the
        // wire as-is (e.g. a real pre-computed Digest header).
        assert_eq!(parse_credentials("Digest user"), None);
        // Unlike Basic there is no `user:pass` colon form for Digest.
        assert_eq!(parse_credentials("Digest user:pass"), None);
    }

    #[test]
    fn credentials_other_schemes_ignored() {
        assert_eq!(parse_credentials("Basic user pass"), None);
        assert_eq!(parse_credentials("Bearer token x"), None);
        assert_eq!(parse_credentials(""), None);
    }

    // --- challenge scheme detection ---

    #[test]
    fn challenge_scheme_detection() {
        assert!(is_digest_challenge("Digest realm=\"x\""));
        assert!(is_digest_challenge("digest realm=\"x\""));
        assert!(is_digest_challenge("DIGEST"));
        assert!(!is_digest_challenge("Basic realm=\"x\""));
        assert!(!is_digest_challenge(""));
    }

    // --- challenge parsing edges ---

    fn challenge(header: &str) -> Challenge {
        parse_challenge(header)
    }

    #[test]
    fn parse_quoted_values_with_spaces_and_specials() {
        let c = challenge(
            r#"Digest realm="test realm @ host.com", nonce="n0/n=ce+value", opaque="o,p aque""#,
        );
        assert_eq!(c.realm, "test realm @ host.com");
        assert_eq!(c.nonce, "n0/n=ce+value"); // quoted values may contain `=` and `,`
        assert_eq!(c.opaque, "o,p aque");
    }

    #[test]
    fn parse_unquoted_token_values() {
        let c = challenge(r#"Digest realm="r", algorithm=MD5, qop=auth, nonce=abc123"#);
        assert_eq!(c.algorithm, "MD5");
        assert_eq!(c.qop, "auth");
        assert_eq!(c.nonce, "abc123");
    }

    #[test]
    fn parse_qop_list_keeps_full_list() {
        let c = challenge(r#"Digest realm="r", qop="auth,auth-int", nonce="n""#);
        assert_eq!(c.qop, "auth,auth-int");
        assert_eq!(negotiated_qop(&c.qop), Some("auth"));
    }

    #[test]
    fn qop_auth_int_only_is_not_auth() {
        // `/(^|,)\s*auth\s*($|,)/` does not match inside `auth-int`.
        assert_eq!(negotiated_qop("auth-int"), None);
        assert_eq!(negotiated_qop(""), None);
        assert_eq!(negotiated_qop("AUTH"), None); // case-sensitive, like the original
    }

    #[test]
    fn qop_list_with_whitespace_matches() {
        assert_eq!(negotiated_qop(" auth , auth-int"), Some("auth"));
        assert_eq!(negotiated_qop("auth-int, auth"), Some("auth"));
    }

    #[test]
    fn parse_duplicate_param_last_wins() {
        let c = challenge(r#"Digest realm="first", realm="second""#);
        assert_eq!(c.realm, "second");
    }

    #[test]
    fn parse_unknown_params_ignored() {
        let c = challenge(
            r#"Digest realm="r", domain="/protected", stale=false, charset=UTF-8, nonce="n""#,
        );
        assert_eq!(c.realm, "r");
        assert_eq!(c.nonce, "n");
        assert_eq!(
            c,
            Challenge {
                realm: "r".into(),
                nonce: "n".into(),
                ..Challenge::default()
            }
        );
    }

    #[test]
    fn parse_empty_quoted_value_skipped_but_rest_parsed() {
        // `"([^"]+)"` needs 1+ chars: `opaque=""` never matches, so opaque
        // stays empty while later params still parse.
        let c = challenge(r#"Digest opaque="", realm="r", nonce="n""#);
        assert_eq!(c.opaque, "");
        assert_eq!(c.realm, "r");
        assert_eq!(c.nonce, "n");
    }

    #[test]
    fn parse_param_names_case_sensitive_like_original() {
        // digest.ts writes `challenge[match[1]]` verbatim: `Realm` lands on a
        // stray property, leaving the lowercase field untouched.
        let c = challenge(r#"Digest Realm="x", NONCE="y", nonce="z""#);
        assert_eq!(c.realm, "");
        assert_eq!(c.nonce, "z");
    }

    #[test]
    fn parse_unterminated_quote_does_not_panic() {
        let c = challenge(r#"Digest nonce="n", realm="unterminated"#);
        assert_eq!(c.nonce, "n");
        assert_eq!(c.realm, "");
    }

    #[test]
    fn parse_scheme_word_is_not_a_param() {
        // "Digest" itself is a param-char run but has no `=` after it.
        let c = challenge("Digest nonce=abc");
        assert_eq!(c.nonce, "abc");
    }

    // --- digest computation against published vectors ---

    /// RFC 2617 §3.5 example (qop=auth, no algorithm param).
    #[test]
    fn rfc2617_example_vector() {
        let creds = Credentials {
            user: "Mufasa".into(),
            pass: "Circle Of Life".into(),
        };
        let header = concat!(
            r#"Digest realm="testrealm@host.com", qop="auth,auth-int", "#,
            r#"nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", "#,
            r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#
        );
        let auth = authorization_with_cnonce(
            &creds,
            "GET",
            "/dir/index.html",
            &parse_challenge(header),
            "0a4f113b",
        );
        assert_eq!(
            auth,
            concat!(
                r#"Digest username="Mufasa", realm="testrealm@host.com", "#,
                r#"nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", uri="/dir/index.html", "#,
                r#"qop=auth, response="6629fae49393a05397450978507c4ef1", "#,
                r#"nc=00000001, cnonce="0a4f113b", "#,
                r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#
            )
        );
    }

    /// RFC 7616 §3.9.1 MD5 example.
    #[test]
    fn rfc7616_md5_example_vector() {
        let creds = Credentials {
            user: "Mufasa".into(),
            pass: "Circle of Life".into(),
        };
        let header = concat!(
            r#"Digest realm="http-auth@example.org", qop="auth, auth-int", algorithm=MD5, "#,
            r#"nonce="7ypf/xlj9XXwfDPEoM4URrv/xwf94BcCAzFZH4GiTo0v", "#,
            r#"opaque="FQhe/qaU925kfnzjCev0ciny7QMkPqMAFRtzCUYo5tdS""#
        );
        let auth = authorization_with_cnonce(
            &creds,
            "GET",
            "/dir/index.html",
            &parse_challenge(header),
            "f2/wE4q74E6zIJEtWaHKaf5wv/H5QzzpXusqGemxURZJ",
        );
        assert!(
            auth.contains(r#"response="8ca523f5e9506fed4657c9700eebdbec""#),
            "{auth}"
        );
        assert!(auth.contains("algorithm=MD5"), "{auth}");
    }

    /// MD5-sess over the RFC 2617 inputs; expected value independently
    /// computed with python hashlib (the RFCs publish no MD5-sess example).
    #[test]
    fn md5_sess_vector() {
        let creds = Credentials {
            user: "Mufasa".into(),
            pass: "Circle Of Life".into(),
        };
        let header = concat!(
            r#"Digest realm="testrealm@host.com", qop="auth", algorithm=MD5-sess, "#,
            r#"nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093""#
        );
        let auth = authorization_with_cnonce(
            &creds,
            "GET",
            "/dir/index.html",
            &parse_challenge(header),
            "0a4f113b",
        );
        assert!(
            auth.contains(r#"response="8e3825c57e897f5a0dec6c2d4e5059d0""#),
            "{auth}"
        );
        assert!(auth.contains("algorithm=MD5-sess"), "{auth}");
    }

    /// No qop → legacy RFC 2069 formula `md5(ha1:nonce:ha2)`; qop/nc/cnonce
    /// omitted from the header. Expected value computed with python hashlib.
    #[test]
    fn legacy_no_qop_vector() {
        let creds = Credentials {
            user: "Mufasa".into(),
            pass: "CircleOfLife".into(),
        };
        let header = concat!(
            r#"Digest realm="testrealm@host.com", "#,
            r#"nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093""#
        );
        let auth = authorization_with_cnonce(
            &creds,
            "GET",
            "/dir/index.html",
            &parse_challenge(header),
            "ignored-without-qop",
        );
        assert_eq!(
            auth,
            concat!(
                r#"Digest username="Mufasa", realm="testrealm@host.com", "#,
                r#"nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", uri="/dir/index.html", "#,
                r#"response="1949323746fe6a43ef61f9606e7febea""#
            )
        );
    }

    // --- answer_challenge gating + uri derivation ---

    fn url(s: &str) -> reqwest::Url {
        s.parse().unwrap()
    }

    const CREDS: fn() -> Credentials = || Credentials {
        user: "user".into(),
        pass: "pass".into(),
    };

    #[test]
    fn answer_requires_401_digest_challenge() {
        let u = url("https://example.com/x");
        let digest = Some(r#"Digest realm="r", nonce="n""#);
        assert!(answer_challenge(&CREDS(), "GET", 401, digest, &u).is_some());
        assert!(answer_challenge(&CREDS(), "GET", 200, digest, &u).is_none());
        assert!(answer_challenge(&CREDS(), "GET", 403, digest, &u).is_none());
        assert!(answer_challenge(&CREDS(), "GET", 401, None, &u).is_none());
        assert!(answer_challenge(&CREDS(), "GET", 401, Some(r#"Basic realm="r""#), &u).is_none());
    }

    #[test]
    fn answer_uri_is_path_plus_query() {
        let u = url("https://example.com/api/items?page=2&q=a%20b");
        let auth = answer_challenge(
            &CREDS(),
            "GET",
            401,
            Some(r#"Digest realm="r", nonce="n""#),
            &u,
        )
        .unwrap();
        assert!(
            auth.contains(r#"uri="/api/items?page=2&q=a%20b""#),
            "{auth}"
        );
    }

    #[test]
    fn answer_generates_32_hex_cnonce() {
        let u = url("https://example.com/");
        let auth = answer_challenge(
            &CREDS(),
            "GET",
            401,
            Some(r#"Digest realm="r", nonce="n", qop="auth""#),
            &u,
        )
        .unwrap();
        let cnonce = auth
            .split(r#"cnonce=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap();
        assert_eq!(cnonce.len(), 32);
        assert!(cnonce.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // --- live network (opt-in) ---

    #[tokio::test]
    #[ignore = "hits httpbin.org; run with cargo test -- --ignored"]
    async fn httpbin_digest_auth_round_trip() {
        use crate::parser::{ParsedRequest, RequestMetadata};
        use crate::settings::HttpSettings;
        use reqwest_cookie_store::CookieStoreMutex;
        use std::sync::Arc;

        let request = ParsedRequest {
            method: "GET".into(),
            url: "https://httpbin.org/digest-auth/auth/user/passwd/MD5".into(),
            http_version: None,
            headers: vec![("Authorization".into(), "Digest user passwd".into())],
            body: None,
        };
        let jar = Arc::new(CookieStoreMutex::default());
        let response = crate::http::execute(
            &request,
            &RequestMetadata::default(),
            &HttpSettings::default(),
            jar,
        )
        .await
        .unwrap();
        assert_eq!(response.status, 200, "digest challenge should be answered");
    }
}
