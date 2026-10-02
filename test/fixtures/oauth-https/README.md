# OAuth HTTPS test fixture

`ca-cert.pem`, `localhost-cert.pem`, and `localhost-key.pem` are a disposable
TLS fixture used only by local Node and Rust tests. The CA is trusted explicitly
by the tests; the localhost leaf covers `localhost`, `127.0.0.1`, and `::1`,
and both certificates expire in 2036. Neither certificate should be installed
in a system trust store or used for production OAuth configuration.

Regenerate the chain in a temporary directory containing a copy of `localhost-key.pem`:

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
  -subj '/CN=artifact-mcp OAuth test CA' \
  -addext 'basicConstraints=critical,CA:TRUE,pathlen:0' \
  -addext 'keyUsage=critical,keyCertSign,cRLSign' \
  -keyout ca-key.pem -out ca-cert.pem
openssl req -new -key localhost-key.pem -subj '/CN=localhost' -out localhost.csr
cat > leaf-ext.cnf <<'EOF'
basicConstraints=critical,CA:FALSE
keyUsage=critical,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectAltName=DNS:localhost,IP:127.0.0.1,IP:::1
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid,issuer
EOF
openssl x509 -req -in localhost.csr -CA ca-cert.pem -CAkey ca-key.pem \
  -CAcreateserial -days 3650 -sha256 -extfile leaf-ext.cnf \
  -out localhost-cert.pem
```

Copy only `ca-cert.pem` and `localhost-cert.pem` back into the fixture directory, then
delete the temporary directory, including the CA private key, request, and serial file.

The leaf private key is intentionally committed as test-only material so the
cross-language redirect and secure JWKS tests can share a deterministic local
fixture. The CA private key and generated serial file stay outside the
repository. These files have no production trust or credential value.
