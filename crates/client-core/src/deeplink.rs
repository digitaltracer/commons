//! `commons://` links. Any website can open one and the OS doesn't say which, so a link is
//! untrusted input: it is checked strictly here and only ever fills in a form.

/// Longer links are refused unread.
pub const MAX_LEN: usize = 2048;

/// A checked `commons://invite?server=<origin>#<token>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InviteLink {
    /// The server's origin, such as `https://commons.example.org`.
    pub server_url: String,
    /// Host, and port when it isn't the scheme's default.
    pub host: String,
    pub token: String,
}

/// Why a link was refused. The wording is for the person; none of it echoes the link.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LinkError {
    #[error("That link is too long to be a Commons link.")]
    TooLong,
    #[error("Commons doesn't recognise that link.")]
    Unknown,
    #[error("That invite link doesn't name a valid server.")]
    BadServer,
    #[error("That invite link names a server without a secure (https) address.")]
    Insecure,
    #[error("That invite link is missing its invite code, or the code is damaged.")]
    BadToken,
}

/// Parses an invite link. `http` servers are allowed only for `localhost` and `127.0.0.1`,
/// and only in debug builds.
pub fn parse(raw: &str) -> Result<InviteLink, LinkError> {
    parse_with(raw, cfg!(debug_assertions))
}

pub fn parse_with(raw: &str, allow_local_http: bool) -> Result<InviteLink, LinkError> {
    if raw.len() > MAX_LEN {
        return Err(LinkError::TooLong);
    }
    let url = url::Url::parse(raw.trim()).map_err(|_| LinkError::Unknown)?;
    if url.scheme() != "commons"
        || url.host_str() != Some("invite")
        || !matches!(url.path(), "" | "/")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(LinkError::Unknown);
    }
    let mut pairs = url.query_pairs();
    let server = match (pairs.next(), pairs.next()) {
        (Some((k, v)), None) if k == "server" => v.into_owned(),
        _ => return Err(LinkError::BadServer),
    };
    let (server_url, host) = check_server(&server, allow_local_http)?;
    let token = url.fragment().unwrap_or("");
    if !proto::api::is_invite_token(token) {
        return Err(LinkError::BadToken);
    }
    Ok(InviteLink {
        server_url,
        host,
        token: token.to_string(),
    })
}

/// A bare origin: `https`, a host, no user info, path, query or fragment of its own.
fn check_server(raw: &str, allow_local_http: bool) -> Result<(String, String), LinkError> {
    let u = url::Url::parse(raw).map_err(|_| LinkError::BadServer)?;
    if !u.username().is_empty()
        || u.password().is_some()
        || u.path() != "/"
        || u.query().is_some()
        || u.fragment().is_some()
        // `Url` drops an empty `?` or `#`; the raw text must not have them either.
        || raw.contains(['?', '#'])
    {
        return Err(LinkError::BadServer);
    }
    let host = u.host_str().ok_or(LinkError::BadServer)?;
    match u.scheme() {
        "https" => {}
        "http" if allow_local_http && matches!(host, "localhost" | "127.0.0.1") => {}
        "http" => return Err(LinkError::Insecure),
        _ => return Err(LinkError::BadServer),
    }
    let shown = match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_string(),
    };
    Ok((u.origin().ascii_serialization(), shown))
}

/// Whether two server addresses name the same origin (scheme, host and port).
pub fn same_origin(a: &str, b: &str) -> bool {
    match (url::Url::parse(a.trim()), url::Url::parse(b.trim())) {
        (Ok(a), Ok(b)) => a.origin().is_tuple() && a.origin() == b.origin(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str =
        "0192f0c1a2b34c5d8e9f0a1b2c3d4e5f.AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_AbCde";

    fn link(server: &str) -> String {
        format!("commons://invite?server={server}#{TOKEN}")
    }

    #[test]
    fn a_good_link_gives_the_origin_the_host_and_the_token() {
        let want = InviteLink {
            server_url: "https://commons.example.org".into(),
            host: "commons.example.org".into(),
            token: TOKEN.into(),
        };
        for server in [
            "https://commons.example.org",
            "https://commons.example.org/",
            "https%3A%2F%2Fcommons.example.org",
            "https://COMMONS.example.org:443",
        ] {
            assert_eq!(
                parse_with(&link(server), false),
                Ok(want.clone()),
                "{server}"
            );
        }
        assert_eq!(
            parse_with(
                &format!("commons://invite/?server=https://x.org#{TOKEN}"),
                false
            )
            .map(|l| l.server_url),
            Ok("https://x.org".into())
        );
        let port = parse_with(&link("https://x.org:8443"), false).unwrap();
        assert_eq!(
            (port.server_url.as_str(), port.host.as_str()),
            ("https://x.org:8443", "x.org:8443")
        );
    }

    #[test]
    fn plain_http_only_for_this_machine_in_debug_builds() {
        for local in ["http://localhost:8080", "http://127.0.0.1:8080"] {
            assert!(parse_with(&link(local), true).is_ok(), "{local}");
            assert_eq!(parse_with(&link(local), false), Err(LinkError::Insecure));
        }
        for remote in [
            "http://evil.example",
            "http://10.0.0.1",
            "http://localhost.evil.example",
        ] {
            assert_eq!(
                parse_with(&link(remote), true),
                Err(LinkError::Insecure),
                "{remote}"
            );
        }
    }

    #[test]
    fn anything_else_is_refused() {
        let bad_server = [
            "https://user@x.org",
            "https://user:pw@x.org",
            "https://x.org/path",
            "https://x.org/?q=1",
            "https://x.org/?",
            "https://x.org%23frag",
            "https://x.org/%3F",
            "ftp://x.org",
            "javascript:alert(1)",
            "commons://invite",
            "x.org",
            "",
        ];
        for s in bad_server {
            assert_eq!(parse_with(&link(s), true), Err(LinkError::BadServer), "{s}");
        }
        let ok = "https://x.org";
        let cases = [
            (
                format!("commons://reset?server={ok}#{TOKEN}"),
                LinkError::Unknown,
            ),
            (
                format!("commons://invite/x?server={ok}#{TOKEN}"),
                LinkError::Unknown,
            ),
            (
                format!("commons://u@invite?server={ok}#{TOKEN}"),
                LinkError::Unknown,
            ),
            (
                format!("commons://invite:1?server={ok}#{TOKEN}"),
                LinkError::Unknown,
            ),
            (
                format!("https://invite?server={ok}#{TOKEN}"),
                LinkError::Unknown,
            ),
            ("not a url".into(), LinkError::Unknown),
            (format!("commons://invite#{TOKEN}"), LinkError::BadServer),
            (
                format!("commons://invite?server={ok}&server=https://evil.example#{TOKEN}"),
                LinkError::BadServer,
            ),
            (
                format!("commons://invite?server={ok}&x=1#{TOKEN}"),
                LinkError::BadServer,
            ),
            (format!("commons://invite?server={ok}"), LinkError::BadToken),
            (
                format!("commons://invite?server={ok}#"),
                LinkError::BadToken,
            ),
            (
                format!("commons://invite?server={ok}#{TOKEN}x"),
                LinkError::BadToken,
            ),
            (
                format!("commons://invite?server={ok}#abc.def"),
                LinkError::BadToken,
            ),
            (
                format!(
                    "commons://invite?server={ok}#{TOKEN}{}",
                    "A".repeat(MAX_LEN)
                ),
                LinkError::TooLong,
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_with(&raw, true), Err(want.clone()), "{raw}");
        }
    }

    #[test]
    fn refusals_never_repeat_the_link() {
        let raw = format!("commons://invite?server=http://evil.example#{TOKEN}");
        let msg = parse_with(&raw, false).unwrap_err().to_string();
        assert!(!msg.contains(TOKEN) && !msg.contains("evil"), "{msg}");
    }

    #[test]
    fn same_origin_compares_scheme_host_and_port() {
        assert!(same_origin("https://x.org", "https://X.org/"));
        assert!(same_origin("https://x.org:443", "https://x.org"));
        assert!(same_origin(
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/"
        ));
        assert!(!same_origin("https://x.org", "http://x.org"));
        assert!(!same_origin("https://x.org", "https://x.org:8443"));
        assert!(!same_origin("https://x.org", "https://y.x.org"));
        assert!(!same_origin("https://x.org", "not a url"));
    }
}
