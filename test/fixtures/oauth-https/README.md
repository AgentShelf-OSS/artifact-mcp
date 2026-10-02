# OAuth HTTPS test fixture

`localhost-cert.pem` and `localhost-key.pem` are a disposable, self-signed
TLS pair used only by local Node and Rust tests. The certificate covers
`localhost`, `127.0.0.1`, and `::1`, and expires in 2036. It is trusted by the
tests explicitly as a local CA; it must never be installed in a system trust
store or used for production OAuth configuration.

The pair can be regenerated with OpenSSL when needed:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -subj '/CN=localhost' \
  -addext 'subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1' \
  -keyout localhost-key.pem -out localhost-cert.pem
```

The private key is intentionally committed as test-only material so the
cross-language redirect and secure JWKS tests can share a deterministic local
fixture. It has no production trust or credential value.
