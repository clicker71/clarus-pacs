# Clarus edge authentication (nginx / ALB / mTLS)

Clarus does **not** authenticate or authorize requests. Authentication and
access control belong to the reverse proxy or load balancer in front of it
(nginx, an ALB, mTLS termination, etc.). This document is the reference for
wiring that edge, including LDAP.

## The contract

- The edge authenticates the caller and, on success, injects the request
  header:

      X-Authenticated-User: <verified-identity>

- Clarus reads that header to attribute the request to a user: the access
  log, the admin audit, and the journal (a compact 32-bit token in the CBOR
  journal). It never verifies passwords or directories itself.
- Clarus only honors identity headers from addresses listed in
  `trusted_proxies` (loopback by default). Add your proxy's address there,
  or the header is ignored. With `require_proxy_identity = true`, a trusted
  proxy that forwards proxy headers but no `X-Authenticated-User` is denied
  with `502`.
- `proxy_set_header` overwrites any value the client sent, so setting the
  header with the verified identity is also how you strip a spoofed one.
  Never pass the client's raw header through.

Therefore: never expose Clarus directly. Put an authenticating proxy in
front and let it decide who gets in. The server-side half of the contract
(`trusted_proxies`, `require_proxy_identity`, `admin_cidrs`) is documented
in [production-setup.md](production-setup.md).

## Recipe 1: HTTP Basic Auth (htpasswd)

Simplest. Static user list, no directory.

```nginx
server {
    listen 443 ssl;
    # ... TLS configuration ...

    location / {
        auth_basic "Clarus";
        auth_basic_user_file /etc/nginx/.htpasswd;

        # $remote_user is set by Basic Auth to the authenticated username.
        proxy_set_header X-Authenticated-User $remote_user;
        proxy_pass http://127.0.0.1:8024;
    }
}
```

## Recipe 2: mTLS client certificate

No directory at all. The verified client certificate identifies the caller.

```nginx
server {
    listen 443 ssl;
    ssl_client_certificate /etc/nginx/ca.crt;
    ssl_verify_client on;
    ssl_verify_depth 2;

    location / {
        # Identity is the Common Name of the verified client certificate.
        proxy_set_header X-Authenticated-User $ssl_client_s_dn_cn;
        proxy_pass http://127.0.0.1:8024;
    }
}
```

If a certificate authority (CA) is shared with the organisation, client
certificates can be issued by that CA; revocation is handled at the CA.

## Recipe 3: LDAP / SSO via auth_request

For an LDAP directory, the recommended free path is nginx `auth_request`
delegating to an LDAP-aware verifier (Keycloak with LDAP user federation,
oauth2-proxy, Authelia, or a small internal auth service). The verifier
checks the client against LDAP and answers 200/401; nginx maps the resolved
username into the identity header.

```nginx
# Internal subrequest: asks the verifier "is this request allowed?"
location = /auth {
    internal;
    # Example: oauth2-proxy listening on 127.0.0.1:4180.
    proxy_pass http://127.0.0.1:4180/auth;
    proxy_pass_request_body off;
    proxy_set_header Content-Length "";
    proxy_set_header X-Original-URI $request_uri;
}

location / {
    auth_request /auth;
    # The verifier returns the resolved username; nginx forwards it.
    auth_request_set $auth_user $upstream_http_x_auth_request_user;

    proxy_set_header X-Authenticated-User $auth_user;
    proxy_pass http://127.0.0.1:8024;
}
```

Keycloak variant: point `auth_request` at a token-introspection or a small
sidecar that validates the bearer token and returns the subject. The LDAP
bind stays inside Keycloak (its "user federation" against LDAP); nginx and
Clarus never speak LDAP.

## Notes

- Stock (community) nginx has no LDAP module. nginx-plus ships a commercial
  LDAP auth module; the third-party `ngx_http_auth_ldap` module exists but is
  not stock. Use Recipe 3 for a free, supported path.
- The same edge can also terminate TLS (standard TLS, or GOST via CryptoPro
  for Russian government networks). Clarus stays behind it either way.
- If the proxy and Clarus are on the same host, bind Clarus to 127.0.0.1 so
  it is reachable only through the authenticating edge.
