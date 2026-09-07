# loyalty.madar-pos.cloud

The public face of the loyalty program: the signup form a customer reaches by
scanning the join QR on a counter, the card page their Wallet pass links back
to, and the pass web service Apple's devices talk to. No admin code ships here
and this origin never holds a Madar session.

## Deploy
1. Point DNS at the box: `A loyalty.madar-pos.cloud → <VPS IP>`.
2. Build in MadarDashboard: `npm run build:loyalty` → `dist-loyalty/`.
3. Sync it to `/var/www/madar-loyalty`.
4. Install `nginx-loyalty.conf` (commands in its header), then
   `certbot --nginx -d loyalty.madar-pos.cloud`.

**The TLS certificate is not optional and cannot be self-signed.** Apple's
devices call the pass web service directly and reject anything they cannot
verify to a public root — a pass issued over a bad chain installs and then
silently never updates, which is the hardest version of this to debug.

## Apple Wallet

### Turning the `.p12` into what the backend wants
```bash
openssl pkcs12 -in pass.p12 -clcerts -nokeys  -out pass-cert.pem
openssl pkcs12 -in pass.p12 -nocerts -nodes   -out pass-key.pem
# The WWDR intermediate. G4 is current for passes; a pass signed without it
# verifies on a Mac (which has WWDR installed) and fails on a customer's phone.
curl -O https://www.apple.com/certificateauthority/AppleWWDRCAG4.cer
openssl x509 -inform DER -in AppleWWDRCAG4.cer -out wwdr.pem
```

Mount those three as files and point at them by path. The `*_FILE` variables
exist precisely so a private key never has to go in an env var, where it would
need its newlines escaped and would show up in `docker inspect`.

### Pushing updates
Balance changes reach a pass already in someone's wallet over APNs, which needs
a **`.p8` auth key** (Apple Developer → Keys → new key with "Apple Push
Notifications service" enabled) — *not* the pass certificate. Without it,
everything else still works: passes issue, balances are right on the server, and
the customer sees the new number the next time they open the pass. With it, the
pass updates on its own.

The push carries no payload by design: it tells the device something changed and
the device calls back for the pass, so no balance crosses APNs.

## Backend environment
All of it degrades safely. With the wallet variables unset, signup still works
and the customer is shown their QR on the page instead of dead buttons.

| Variable | Purpose | Unset |
|---|---|---|
| `PUBLIC_LOYALTY_BASE_URL` | Base of this site. The join QR's target and the pass's `webServiceURL`. | The join-QR endpoint returns 503; signup still works. |
| `LOYALTY_APPLE_PASS_TYPE_ID` | Pass Type ID. **Must match the certificate exactly.** | No Apple button. |
| `LOYALTY_APPLE_TEAM_ID` | Your 10-character team id. Also the APNs issuer. | No Apple button. |
| `LOYALTY_APPLE_CERT_PEM_FILE` | Pass certificate (or `LOYALTY_APPLE_CERT_PEM` inline). | No Apple button. |
| `LOYALTY_APPLE_KEY_PEM_FILE` | Its private key. | No Apple button. |
| `LOYALTY_APPLE_WWDR_PEM_FILE` | Apple WWDR intermediate. | No Apple button. |
| `LOYALTY_APNS_KEY_FILE` | APNs `.p8` auth key. | Passes issue but never update themselves. |
| `LOYALTY_APNS_KEY_ID` | That key's id. | Same. |
| `LOYALTY_APNS_SANDBOX` | `1` for a development-profile pass. | Production APNs host. |
| `LOYALTY_GOOGLE_ISSUER_ID` | Google Wallet issuer. | No Google button. |
| `LOYALTY_GOOGLE_SA_EMAIL` | Service account email. | No Google button. |
| `LOYALTY_GOOGLE_SA_KEY` | Service account RSA private key (PEM). | No Google button. |

A mismatch between `LOYALTY_APPLE_PASS_TYPE_ID`/`LOYALTY_APPLE_TEAM_ID` and the
certificate is the usual cause of "the pass downloads but iOS won't add it" —
iOS gives no reason, so check these two first.

## Checking it works
```bash
# The pass should be a zip, ~4 KB, of exactly pass.json + manifest.json + signature.
curl -sD- -o /tmp/p.pkpass https://loyalty.madar-pos.cloud/api/public/loyalty/pass/<token>/apple.pkpass
unzip -l /tmp/p.pkpass
# On a Mac, this verifies the chain the way a device does:
openssl smime -verify -inform DER -in <(unzip -p /tmp/p.pkpass signature) \
  -content <(unzip -p /tmp/p.pkpass manifest.json) -noverify
```
When a pass installs but will not update, the device says why through
`POST /wallet/v1/log` — it lands in the backend log as
"Apple Wallet device log", and it is usually the only diagnostic available.

## The runtime image
Pass signing is `openssl`'s `PKCS7_sign`, so the runtime stage installs
`libssl3` (the builder already had `libssl-dev`). Nothing else in the backend
uses OpenSSL — HTTP is still rustls.
