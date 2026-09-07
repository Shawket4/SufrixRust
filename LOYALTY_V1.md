# Loyalty / points program — v1 scope

Decided with the owner 2026-09-07. Madar-only. Customers get no app: their identity and
balance live in an Apple Wallet / Google Wallet pass.

## The two QR codes
1. **Join QR** — per branch, printed and stood at the counter. Static. Opens the public
   signup form on `loyalty.madar-pos.cloud`. Rendered by the existing `src/qr_card`
   module (Shlink short URL + branded A6 card), exactly like the booking QR.
2. **Member QR** — the barcode on the customer's Wallet pass. An opaque random token,
   not a customer id: it must not be guessable or enumerable. This is what the teller
   scans at checkout.

## Decisions

| Question | Decision |
|---|---|
| Pass issuer | ONE Madar-owned Apple Pass Type ID + ONE Google Wallet issuer, per-org branding (logo, colours, program name) from data. Credential lookup is per-org from day one so per-tenant certs can drop in later without a schema change. |
| Credentials | Not held yet. Pass generation is **degrade-safe behind config** — skipped and logged when the env vars are unset, exactly like `WHATSAPP_SERVICE_URL`. Everything else is testable now. |
| Balance scope | **One balance per org.** A customer joins once and earns/spends at every branch of that tenant; the branch is recorded on each transaction. |
| What is collected | **One mode per scope — points OR stamps, never both.** `points` earns on money spent by the rule below; `visits` earns one stamp per order, whatever the bill ("5 orders, free coffee"). A card that counted two things at once would need two progress lines and two answers to "how close am I". Members carry both balances anyway, because an org may switch mode and what was earned under the old rules is still theirs. |
| Earning | **A rule, not a keypad** — the dashboard sets EGP→points and the server does the arithmetic. But the award is **never automatic**: it happens only when a teller presses "add points". |
| Earn basis | Subtotal, with two toggles: *earn on the discounted amount* (default on) and *include tax* (default off). **Tips never earn** — that is the staff's money, not a sale. |
| Scan point | **A button, after the sale.** On the receipt right after checkout, and on that order in the POS history for **24 hours** — so "I left my card in the car" is answered by the customer coming back, not by the queue waiting. Enforced on BOTH sides: the client hides the button (`madar_core::loyalty::award_window_open`), and the server re-checks it against the order's own `created_at` (`loyalty::award::window_check`), which is what makes it true. |
| Offline | The press is **stamped and queued** in the outbox and drains through `/sync/replay` into the same server code. The window is anchored to when the button was PRESSED, not when the op arrived, so a till offline for two days still credits an award made in time — and the claim is bounded (never before the sale, never in the future) so it can only ever land inside the window it claims. The queued award is gated behind its own sale's outbox op, so it can never reach the server before the order does. |
| Awarded once | `loyalty_transactions_earn_order_key` allows one earn per order. A double tap, a retry and a replayed offline op all converge on one award. |
| Redemption | **Rewards cover lines of the cart, at tender.** Redeeming changes what is owed, so it happens before payment, not after: the teller scans, ticks which lines the balance covers, and the total drops before the customer pays. A mixed basket can carry several — a free coffee among four paid ones — and the balance is checked against the WHOLE basket, so a sale that outruns it fails rather than handing over half of what was promised. The covered line is priced at zero and marked `is_reward`, so the receipt says REWARD rather than showing a mystery free item, and reporting can separate goods given away from goods discounted. |
| Redeeming offline | **Refused, with a reason.** A balance is shared state any till can spend and a reward gives away goods: two disconnected tills could each honour the last one and neither could be undone, because the coffee is gone. Earning is unaffected and works offline. |
| Reward items | Admins select **which menu items can be claimed**, and **price each one**: "espresso, 5 orders" beside "cake, 10 orders" in the same catalogue — the thing a single program-wide threshold could not express. The org-level `default_reward_cost` is what a new reward is offered at, and the pass's fallback target when nothing is curated yet. Item selection governs redemption only; nothing item-level is consulted when earning. |
| Settings shape | Org default with per-branch override, following `attendance_settings`: `org_id NOT NULL`, nullable `branch_id` where NULL = the org-wide default. |
| Money in the UI | Piastres on the wire, **EGP everywhere in the dashboard**, via the existing `piastresToEgp` / `egpToPiastres` / `fmtMoney` helpers. |
| Teller scanning | Camera (`mobile_scanner`) where one exists, **USB keyboard-wedge scanner** via a focused hidden field for Windows/macOS counter tills, and a **manual phone-number lookup** when the customer's battery is dead. |
| Pass front | Primary: points balance. Secondary: "30 / 100 to your next reward". Auxiliary: member name. Back: how it works, reward list, branch addresses, terms. |
| OTP | Reuses the existing `delivery_otp` table + `otp_request`/`otp_verify` + the 90-day device-trust token. Per-branch `require_otp` toggle, same as `branch_booking_settings`. |
| Branch locations | Already exist — `branches.latitude/longitude/geo_radius_meters` (added for staff geofencing) with inputs already in the dashboard's branch dialog. Reused for the pass's proximity/lock-screen surfacing; no new columns. |

## Also in scope: the Settings rebuild
The dashboard's Settings page is rebuilt from scratch with better UX, and **feature
configuration moves into it**: delivery (settings, channels, zones), bookings/reservations,
loyalty, QR, payment methods, integrations, WhatsApp, kitchen stations.

**Not moving:** users, orgs, branches, permissions, staff — those are entities with their
own top-level pages, not configuration.

Shipped as **one combined piece of work** with loyalty, per the owner.

## Where the code goes
- **MadarRust** — `migrations/` + `src/loyalty/{mod,model,settings,public,handlers,routes,tests}.rs`
  and `src/loyalty/wallet/{apple,google}.rs`. The award hooks into `create_order_inner`,
  which is already the replay-safe half of the split, so offline tills award on drain for free.
- **MadarDashboard** — a fourth public SPA entry (`vite.loyalty.config.ts` → `dist-loyalty`,
  `loyalty.html`, `src/loyalty/`) for `loyalty.madar-pos.cloud`, following the ordering and
  reservations apps: no admin code, no shared storage. Plus the rebuilt Settings feature.
- **madar** (Flutter POS) — scan in the checkout flow, over `madar-core`; no business logic
  in Dart.

## Open / deferred
- Apple pass updates need APNs + the pass-registration web service endpoints; Google needs
  only a PATCH to the Wallet API. Both land behind the same config gate.
- Per-tenant white-label certs (schema already allows it).
