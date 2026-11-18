Test-only TLS material for `rate_limit_redis_tls.rs` (CFG-09): a CA (`ca.pem`, its key
discarded), a server certificate for `DNS:redis.test` only, and a client certificate, all
valid for 100 years. Not secret. Regenerate with `openssl` (P-256, PKCS#8 keys) if needed.
