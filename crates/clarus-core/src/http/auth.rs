// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! Client identity resolution (E-009).
//!
//! Clarus sits on a loopback; nginx is the only real peer. We trust the
//! identity headers (`X-Real-IP`, `X-Forwarded-For`, `X-Authenticated-User`)
//! **only** when the raw socket peer falls inside `trusted_proxies`. A peer
//! outside it is never trusted - spoofed headers are ignored and the identity
//! comes from the socket address.
//!
//! The resolution is three-state, not two. The crux: *no proxy headers at
//! all* from a trusted peer means a legitimate local client (embedded viewer,
//! `curl`, local tests) - **not** a misconfigured nginx. The two cases are
//! deliberately distinguished so the strict `require_proxy_identity` mode does
//! not break the built-in viewer:
//!
//! | Case | Meaning | Strict (`require_proxy_identity`) |
//! |---|---|---|
//! | `LocalViewer` | trusted peer, zero proxy headers | allowed (200) |
//! | `MissingIdentity` | trusted peer, >=1 proxy header but no `X-Authenticated-User` | 502 |
//! | `UntrustedPeer` | peer outside `trusted_proxies` | headers ignored |
//!
//! A peer sending any proxy header with none of `X-Real-IP`/`X-Forwarded-For`/
//! `X-Authenticated-User` present is impossible to hit `MissingIdentity`
//! without also supplying an IP, so `MissingIdentity` keys on the *presence of
//! any header we would have trusted* paired with the *absence of the user*.

/// The client identity for the current request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Identity {
    /// Direct loopback client with **no** proxy headers at all - the embedded
    /// viewer, `curl`, or a local test. Exempt from `require_proxy_identity`.
    LocalViewer,
    /// Trusted peer (nginx) with identity headers fully delivered.
    Proxied { ip: Vec<u8>, user: Vec<u8> },
    /// Trusted peer that sent at least one proxy header but no user.
    /// `require_proxy_identity = true` -> 502; false -> WARN + `anonymous`.
    MissingIdentity,
    /// Peer outside `trusted_proxies`: headers ignored, ip = socket addr.
    UntrustedPeer { ip: Vec<u8> },
}

/// The address our own proxy appended to `X-Forwarded-For`, or the value
/// unchanged when it carries no list.
///
/// The header is `client, proxy1, proxy2`: each hop APPENDS its peer, so the
/// RIGHTMOST entry is the one the last proxy before us saw, and everything to
/// its left is what an upstream party claimed. Taking the LEFTMOST entry made
/// the identity whatever the client wrote there - a request carrying
/// `X-Forwarded-For: 127.0.0.1` was resolved as loopback and reached the admin
/// routes behind any proxy that forwards the header verbatim. nginx's
/// `$proxy_add_x_forwarded_for` appends `$remote_addr`, so the rightmost entry
/// is the only one this server can attribute to a peer it observed.
///
/// `X-Real-IP` stays the preferred source where it is present: it is a single
/// address the proxy sets from `$remote_addr`, with no list for a client to
/// prepend to.
fn first_forwarded_ip(raw: &[u8]) -> Option<&[u8]> {
    let text = std::str::from_utf8(raw).ok()?;
    // `rsplit` walks from the end: the last element of a comma list.
    let last = text.rsplit(',').next()?.trim();
    (!last.is_empty()).then_some(last.as_bytes())
}

/// Resolve the client identity.
///
/// `peer_ip` is the raw socket peer address; `trusted` says whether it fell
/// inside `trusted_proxies`. `real_ip`, `xff`, and `auth_user` are the parsed
/// header values (already borrowed-and-copied by the caller).
pub fn resolve_identity(
    peer_ip: &[u8],
    trusted: bool,
    real_ip: Option<&[u8]>,
    xff: Option<&[u8]>,
    auth_user: Option<&[u8]>,
) -> Identity {
    if !trusted {
        // Never trust headers from an untrusted peer (spoofing).
        return Identity::UntrustedPeer {
            ip: peer_ip.to_vec(),
        };
    }

    let has_any_proxy_header = real_ip.is_some() || xff.is_some() || auth_user.is_some();

    if !has_any_proxy_header {
        // Trusted peer with no headers at all - a direct local client, not
        // nginx. The viewer works even in strict mode.
        return Identity::LocalViewer;
    }

    match auth_user {
        Some(user) => {
            let ip = real_ip
                .and_then(first_forwarded_ip)
                .or_else(|| xff.and_then(first_forwarded_ip))
                .map(|s| s.to_vec())
                .unwrap_or_else(|| peer_ip.to_vec());
            Identity::Proxied {
                ip,
                user: user.to_vec(),
            }
        }
        None => {
            // Trusted peer delivered an IP but forgot the authenticated user -
            // a real nginx configuration error, distinct from LocalViewer.
            Identity::MissingIdentity
        }
    }
}

/// Decide whether the request may proceed.
///
/// Returns `Ok(())` when the identity is serviceable, or `Err(502)` when a
/// strict server cannot trust a `MissingIdentity`.
pub fn enforce(client: &Identity, require_proxy_identity: bool) -> Result<(), u16> {
    match client {
        // The built-in viewer / local tools are always allowed - they do not
        // route through nginx and are exempt from the strict requirement.
        Identity::LocalViewer => Ok(()),
        Identity::Proxied { .. } => Ok(()),
        Identity::UntrustedPeer { .. } => Ok(()),
        Identity::MissingIdentity => {
            if require_proxy_identity {
                Err(502)
            } else {
                Ok(())
            }
        }
    }
}

/// The effective user for the request (`anonymous` when unknown/untrusted).
pub fn effective_user<'a>(client: &'a Identity, fallback: &'a [u8]) -> &'a [u8] {
    match client {
        Identity::Proxied { user, .. } => user,
        _ => fallback,
    }
}

/// The effective client IP for the request.
pub fn effective_ip(client: &Identity) -> &[u8] {
    match client {
        Identity::Proxied { ip, .. } | Identity::UntrustedPeer { ip } => ip,
        Identity::LocalViewer | Identity::MissingIdentity => &[],
    }
}

// -- Tests -------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_viewer_exempt_from_strict() {
        // Trusted peer (loopback), no headers at all -> LocalViewer.
        let id = resolve_identity(b"127.0.0.1", true, None, None, None);
        assert_eq!(id, Identity::LocalViewer);
        // Strict mode must NOT 502 the viewer.
        assert_eq!(enforce(&id, true), Ok(()));
    }

    #[test]
    fn missing_identity_502_when_strict() {
        // Trusted peer sent X-Real-IP but dropped the user.
        let id = resolve_identity(b"127.0.0.1", true, Some(b"10.0.0.5"), None, None);
        assert_eq!(id, Identity::MissingIdentity);
        assert_eq!(enforce(&id, true), Err(502));
        // Non-strict serves with anonymous.
        assert_eq!(enforce(&id, false), Ok(()));
        assert_eq!(effective_user(&id, b"anonymous"), b"anonymous");
    }

    #[test]
    fn missing_identity_via_xff_only() {
        // Header present but no user -> MissingIdentity, not LocalViewer.
        let id = resolve_identity(b"127.0.0.1", true, None, Some(b"192.0.2.1"), None);
        assert_eq!(id, Identity::MissingIdentity);
    }

    #[test]
    fn untrusted_peer_ignores_headers() {
        // Peer outside trusted_proxies -> headers ignored, ip = socket.
        let id = resolve_identity(
            b"203.0.113.9",
            false,
            Some(b"1.2.3.4"),
            None,
            Some(b"admin"),
        );
        assert_eq!(
            id,
            Identity::UntrustedPeer {
                ip: b"203.0.113.9".to_vec()
            }
        );
        assert_eq!(effective_user(&id, b"anonymous"), b"anonymous");
        assert_eq!(effective_ip(&id), b"203.0.113.9");
    }

    #[test]
    fn proxied_uses_x_real_ip_and_user() {
        let id = resolve_identity(
            b"127.0.0.1",
            true,
            Some(b"10.0.0.9"),
            Some(b"192.0.2.7"),
            Some(b"dr-smith"),
        );
        match id {
            Identity::Proxied { ip, user } => {
                assert_eq!(ip, b"10.0.0.9");
                assert_eq!(user, b"dr-smith");
            }
            other => panic!("expected Proxied, got {other:?}"),
        }
        assert_eq!(
            enforce(
                &Identity::Proxied {
                    ip: b"10.0.0.9".to_vec(),
                    user: b"x".to_vec()
                },
                true
            ),
            Ok(())
        );
    }

    #[test]
    fn proxied_falls_back_to_peer_ip_when_no_real_ip() {
        let id = resolve_identity(b"127.0.0.1", true, None, None, Some(b"tester"));
        match id {
            Identity::Proxied { ip, user } => {
                assert_eq!(ip, b"127.0.0.1");
                assert_eq!(user, b"tester");
            }
            other => panic!("expected Proxied, got {other:?}"),
        }
    }

    /// `X-Forwarded-For` is `client, proxy1, proxy2` and each hop APPENDS, so
    /// the RIGHTMOST entry is the peer our own proxy observed. Taking the
    /// leftmost one made the identity whatever the client wrote there - which
    /// is a spoof, not an identity.
    #[test]
    fn only_the_last_forwarded_address_is_the_client() {
        let id = resolve_identity(
            b"127.0.0.1",
            true,
            None,
            Some(b"203.0.113.9, 10.0.0.1, 172.16.0.5"),
            Some(b"dr-smith"),
        );
        match id {
            Identity::Proxied { ip, .. } => assert_eq!(
                ip, b"172.16.0.5",
                "the last entry is the one our proxy appended"
            ),
            other => panic!("expected Proxied, got {other:?}"),
        }
        // A single address (the common case) and stray whitespace are unchanged.
        let id = resolve_identity(b"127.0.0.1", true, None, Some(b" 203.0.113.9 "), Some(b"u"));
        match id {
            Identity::Proxied { ip, .. } => assert_eq!(ip, b"203.0.113.9"),
            other => panic!("expected Proxied, got {other:?}"),
        }
        // `X-Real-IP` wins over the forwarded list, and is itself single-valued.
        let id = resolve_identity(
            b"127.0.0.1",
            true,
            Some(b"198.51.100.4"),
            Some(b"203.0.113.9, 10.0.0.1"),
            Some(b"u"),
        );
        match id {
            Identity::Proxied { ip, .. } => assert_eq!(ip, b"198.51.100.4"),
            other => panic!("expected Proxied, got {other:?}"),
        }
    }

    /// A client that WRITES `X-Forwarded-For: 127.0.0.1` must not be resolved
    /// as loopback: every hop appends, so only the RIGHTMOST entry is a peer
    /// this server's proxy observed. Reading the leftmost one handed the
    /// identity to whatever the client sent, and a loopback identity reaches
    /// the admin routes.
    #[test]
    fn a_spoofed_forwarded_address_does_not_make_the_client_local() {
        let id = resolve_identity(
            b"127.0.0.1",
            true,
            None,
            // The client prepended a loopback claim; nginx appended the
            // address it actually saw.
            Some(b"127.0.0.1, 203.0.113.9"),
            Some(b"someone"),
        );
        match id {
            Identity::Proxied { ip, .. } => assert_eq!(
                ip, b"203.0.113.9",
                "the rightmost entry is the address our proxy observed, not the \
                 loopback claim a client can write"
            ),
            other => panic!("expected Proxied, got {other:?}"),
        }
    }
}
