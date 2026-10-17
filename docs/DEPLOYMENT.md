# Deployment

Daruma exposes HTTP on `:8080` and pairing/TLS discovery on `:8443` by default.
Start the server with:

```bash
daruma-server
```

Pair a desktop client from an admin token:

```bash
curl -s -H "Authorization: Bearer $DARUMA_TOKEN" \
  http://SERVER:8080/v1/devices/pair/ticket | jq -r .pairing_url
daruma-desktop pair 'daruma://pair?host=SERVER:8443&token=...&fpr=sha256:...'
```

The ticket returns `host` (`DARUMA_HOSTNAME:DARUMA_TLS_PORT`) and
`tls_fingerprint` (`sha256:<hex>`, the same value as `fpr` in `pairing_url`).
`daruma-desktop pair` connects to `host` over TLS, pins the server's
self-signed certificate to that fingerprint (the handshake aborts on a
mismatch), and sends `POST /v1/devices/pair` with `token` and the observed
`tls_fingerprint`; the server rejects a missing or different fingerprint.
The pairing connection must therefore reach Daruma's own TLS listener: a proxy
that terminates TLS with another certificate breaks pairing.

## LAN Only

Use this when all devices are on the same trusted network.

```bash
DARUMA_HOSTNAME=my-host.local DARUMA_TLS_PORT=8443 daruma-server
```

Open firewall ports `8080/tcp` and `8443/tcp` on the LAN only. Leave mDNS on
unless your network blocks it; clients can run `daruma-desktop discover`.

## VPN

Bind the server normally, but expose ports only on the VPN interface or firewall
group.

```bash
DARUMA_HOSTNAME=daruma.vpn.example DARUMA_TLS_PORT=8443 daruma-server
```

Pair with the VPN hostname. Keep `8080/tcp` and `8443/tcp` closed on the public
interface; only VPN peers should reach them.

## Public

Put a TLS reverse proxy in front of the HTTP API, and publish the pairing TLS
port directly. The proxy's certificate differs from the self-signed one in the
ticket, so pairing cannot go through `443`.

```nginx
server {
  listen 443 ssl;
  server_name daruma.example.com;

  location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-Proto https;
  }
}
```

```bash
DARUMA_HOSTNAME=daruma.example.com DARUMA_TLS_PORT=8443 daruma-server
```

Open `443/tcp` and `8443/tcp` publicly; keep `8080/tcp` closed by the firewall
(both listeners bind `0.0.0.0`). Daruma terminates TLS on `8443` itself, and
the ticket sends clients to `daruma.example.com:8443`. The TLS listener serves
the same API as `8080` with the same authentication; `POST /v1/devices/pair`
is the only unauthenticated write, single-use and rate limited. If `8443` cannot be
public, open it only while pairing, or pair over LAN/VPN. Forwarding is fine
only as plain TCP passthrough (no TLS termination) on the same public port,
because the ticket advertises `DARUMA_TLS_PORT`.

Issue the ticket over the public API and check that the certificate served on
the pairing port is the one in the ticket before pairing:

```bash
curl -s -H "Authorization: Bearer $DARUMA_TOKEN" \
  https://daruma.example.com/v1/devices/pair/ticket | jq -r .tls_fingerprint
openssl s_client -connect daruma.example.com:8443 </dev/null 2>/dev/null \
  | openssl x509 -outform DER | sha256sum
```

The `sha256sum` digest must equal the ticket's `tls_fingerprint` without the
`sha256:` prefix.

## Manual telemetry retention

Domain rows in a tenant SQLite `events` table are the event-sourced source of
truth and must not be deleted. Operational metrics share that table but carry
`event_class = 'telemetry'`; rotate them manually by age in one transaction:

```sql
BEGIN IMMEDIATE;
DELETE FROM events
WHERE event_class = 'telemetry'
  AND occurred_at < '2026-08-01T00:00:00+00:00';
COMMIT;
```

Back up the tenant database and replace the example UTC cutoff first. Sequence
gaps are intentional (`seq > cursor`), deleted numbers are never reused, and
the cabinet continues reading retained telemetry.
