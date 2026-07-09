# Deployment

Daruma exposes HTTP on `:8080` and pairing/TLS discovery on `:8443` by default.
Start the server with:

```bash
daruma-server
```

Pair a desktop client from an admin token:

```bash
curl -H "Authorization: Bearer $DARUMA_TOKEN" \
  http://SERVER:8080/v1/devices/pair/ticket
daruma-desktop pair 'daruma://pair?host=SERVER:8443&token=...&fpr=sha256:...'
```

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

Put a TLS reverse proxy in front of the API and keep the server private.

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

Open `443/tcp` publicly. Keep `8080/tcp` and `8443/tcp` bound to localhost or a
private network. For pairing, issue the ticket over the public API and use the
advertised `host`/`tls_fingerprint` values from `/v1/devices/pair/ticket`.
