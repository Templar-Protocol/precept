# Hosted sign page

A static page for co-signers who hold their key in a wallet and should install nothing.

**URL:** `https://templar-protocol.github.io/precept/sign/` (deployed by
`.github/workflows/pages-sign.yml` from `sign/**` on `main` only).

A proposer sends one chat message:

```
network: <testnet|mainnet>
smart account: C…
action:
  contract  C…
  function  <name>
  arg[0]    …
valid until: <ledger> (<UTC time>)
sign this: https://templar-protocol.github.io/precept/sign/#b=…&s=<your G address>
expected signature_payload: <64 hex>            (also: 32 decimal bytes — see below)
```

The page decodes the bundle from the URL **fragment** (so it never appears in a server log), re-derives
every value from the XDR itself, shows the action in plain words, and hands the signing to Freighter
(extension or mobile QR). It answers with a short code, `pss1.…`, to send back. The proposer runs
`submit --bundle <signed files> --share pss1.…`.

## What the signer must check

Three things, in this order:

1. **Who sent the message.** It has to come from the known proposer, on a channel you have already
   authenticated (and for mainnet, confirmed once on a second channel). A link plus a matching payload
   only proves the link and the message agree — an attacker can send you both.
2. **What the action is.** The contract, function and every argument in the message should be what you
   expect from the hand-off plan, and the page must show the same lines.
3. **What Freighter shows.** Expand `__check_auth` (it is collapsed by default), then check the
   network, the smart account and the `signature_payload`. The released apps (extension 5.49,
   Mobile 1.26) render the payload as **32 decimal bytes**: `{"0":124,…,"31":238}`. Hex and base64 are
   printed too; any form is fine **as long as all 32 bytes match the message**.

## What this page deliberately does

* **Refuses anything it cannot verify.** Every field is re-derived: the network must be one of the two
  pinned passphrases, the account must be the entry's own address, the action in the host function must
  carry the same values as the entries' invocation, one auth context, uniform credentials, the preimage
  must commit to that exact account/payload, and each signer entry must be `account.__check_auth` with
  that preimage. Any disagreement stops the page before a wallet is involved.
* **Only signs for pinned accounts.** `decode.mjs:KNOWN_ACCOUNTS` lists the admin accounts this page
  will sign for (mainnet `CCDZKWLV…J2AI`; testnet the rehearsal accounts). A link naming anything else
  is refused with a message saying so: adding an account is a reviewed change plus a redeploy, which is
  the point.
* **Refuses expired or long-lived bundles.** Anything expired, or valid for more than two days, is
  refused (the CLI signs with the same limit).
* **Refuses sub-invocations.** If an action authorizes nested calls, the page says so and refuses;
  use the command-line tool for those (none of the hand-off actions have sub-invocations).
* **Never asks for a secret.** No key input, no cookies, and no network calls except the WalletConnect
  relay when you choose the mobile path. It does write to browser storage in one case only: the
  WalletConnect session (the mobile path) keeps its keys in this origin's localStorage while it is
  open. That session is disconnected and those keys are cleared in a `finally` block as soon as the
  request finishes, success or failure — and because GitHub Pages gives every project of the org the
  **same origin** (`templar-protocol.github.io`), the real boundary is the org's origin, not this
  path. Treat any script the org serves from any `/…` path as same-origin with this page.
* **No third-party code at runtime.** `sign/vendor/` holds the wallet libraries with hashes recorded in
  `sign/vendor/SHA256SUMS`; the page loads nothing from a CDN. The Freighter API script also carries an
  `integrity=` (SRI) pin; ES-module imports cannot use SRI, which is why the proposer's `link
  --verify-page` compares every deployed file's sha256 with the reviewed checkout before a link is sent.
* **CSP in a `<meta>` tag** (`default-src 'none'; script-src 'self'; style-src 'self'
  'unsafe-inline'; img-src 'self' data:; connect-src wss://relay.walletconnect.org; base-uri 'none';
  form-action 'none'`). GitHub Pages cannot send headers, so `frame-ancestors` is not available — the
  one protection this page cannot have.

## XDR decoding

`xdr-lite.mjs` is a ~300-line reader/writer for exactly these structures: a Soroban authorization
entry and the `HashIdPreimage` (legacy and address-bound). It refuses every arm it does not implement
(no skipping, no best effort) and refuses trailing bytes. The payload binding re-uses the entry's
**original invocation bytes**, so a decoding bug can never change what gets hashed.

Validated against the Python tool that the proposer runs:

* **44 golden vectors** — every call shape the hand-off signs (pool `propose_admin`/`accept_admin`/
  `set_status`, Lazer `transfer_ownership`/`accept_ownership`, governance `create_proposal` with
  `SetRole` and with `SetProxy`/struct/`Option`, `execute_proposal`, Comet `set_controller`/
  `set_freeze_status`, SAC `transfer(i128)`) × testnet/mainnet × ADDRESS_V2/legacy. Each asserts the
  account payload, the pre-image bytes, the **per-signer** pre-image and digest, and the plain-words
  rendering line for line.
* **120 randomised SCVal trees** (seeded, generated in Python) — decoded to the same bytes and the same
  words, re-encoded to the same bytes.
* tamper cases: changed action, changed network, wrong passphrase, mixed credentials, wrong signer,
  foreign signer entry, trailing bytes, unprintable symbols, forged shares.

## Files

| file | what |
|---|---|
| `index.html` | markup + CSP + the two `<script>` tags |
| `app.mjs` | DOM wiring, extension + WalletConnect paths, share output |
| `decode.mjs` | fragment decode, full re-derivation, rendering, share encode/decode |
| `xdr-lite.mjs` | the XDR reader/writer |
| `vendor/` | freighter-api 6.0.1, @walletconnect/universal-provider 2.26.0, qrcode 1.5.4, their four esm.sh node polyfills, @noble/ed25519 3.2.0, `SHA256SUMS` |
| `test/` | bun tests and the generated golden vectors |

Run the tests: `bun test` in this directory.

## Deploying

Push to `main` touching `sign/**`. The workflow copies `sign/` to `_site/sign/`, so the URL stays
`…/precept/sign/`. Actions are pinned by commit SHA (tags move, SHAs do not), permissions are minimal
(`contents: read` for the build, `pages: write` + `id-token: write` only for the deploy job), and the
deploy uses the `github-pages` environment.

The WalletConnect project id in `app.mjs` is a **public client id**, and the origin
`https://templar-protocol.github.io` is allow-listed for it in WalletConnect Cloud. Nothing else in
this directory is a secret.
