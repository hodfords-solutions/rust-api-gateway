Test-only TLS material for `gateway_forwarding.rs` (FWD-19): a self-signed certificate for
`DNS:localhost` and `IP:127.0.0.1` (`upstream.pem`, not a CA) and its P-256 key
(`upstream.key`), valid for 100 years. Not secret. Regenerate with:

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout upstream.key -out upstream.pem -days 36500 -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -addext "basicConstraints=critical,CA:FALSE" -addext "keyUsage=digitalSignature" -addext "extendedKeyUsage=serverAuth"
```
