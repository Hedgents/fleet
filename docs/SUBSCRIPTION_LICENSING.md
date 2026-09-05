# Fleet subscription — licensing, payment, enforcement

How clients pay for and are licensed to run the self-hosted fleet. First
client: CaveBroDAO. Written to NOT break the product's core promise
(runs on the client's hardware, their keys, no Hedgents server in the
trading path).

## The tension to respect

The selling point is "no Hedgents server, self-custody." A naive license
model (the fleet phones home to a license server to validate, or worse,
to authorize trades) contradicts that and adds a single point of failure
in someone else's trading path. So:

- The trading path must NEVER depend on a Hedgents server being up.
- Licensing must be either fully offline (local crypto verification) or
  on-chain (the fleet reads the chain, which it already does).

Also be honest internally: self-hosted license enforcement is inherently
soft. The client has the binaries; a determined client could patch out a
check. The real lever is **support + updates + the relationship**, not DRM.
Don't over-build unbreakable licensing. Make compliance easy, make the
ongoing value (support, new strategies, updates, attestations) worth
paying for.

## Three models, in order of build cost

### 1. Manual (use this for CaveBroDAO / client #1)
- **Payment**: invoice the DAO in USDC to a Hedgents wallet (DAOs pay from
  their multisig treasury — crypto is the natural rail). Stripe is the
  fiat alternative if they prefer.
- **License**: trust-based for the pilot, or a hand-issued signed license
  file (below) if you want it formal. For client #1, a signed agreement +
  an invoice is enough. Don't build a system for one customer.
- **Enforcement**: the relationship. They pay to get onboarding, updates,
  and support. Stop paying → you stop supporting and updating.
- This is the right scope now. Treat CaveBro as a hand-run engagement.

### 2. Offline signed license (build when you have a few clients, fiat-friendly)
- Hedgents holds an Ed25519 signing key. On payment you issue a **signed
  license file**: `{ tenant, tier, max_aum, features, issued, expires }`
  signed with the private key. Ship the matching **public key in the
  binary**.
- The fleet verifies the signature + expiry **locally** at boot and
  periodically. No server call. Expired/invalid → it refuses to start live
  trading (but never touches funds; it just won't trade).
- Renewal = issue a new file on the next payment. Revocation = let it
  expire, or ship a short-lived license (e.g. 35 days) so non-renewal
  auto-stops within the window.
- Payment stays separate (Stripe / crypto invoice → you issue the license).

### 3. On-chain subscription (the crypto-native target; best brand fit)
- Client pays USDC into a **subscription program** on Solana → gets a
  time-bound on-chain subscription record (or a subscription NFT) keyed to
  their wallet/tenant.
- The fleet (which already reads Solana) checks the subscription is
  **active on-chain** before enabling live trading. No Hedgents server, no
  signed-file distribution. Payment + license are the same on-chain action.
- Renewal = pay again on-chain; lapse = the record expires and the fleet
  rolls down to read-only/paused. Fully self-serve, fully self-custody,
  on-brand for a Solana product. Most elegant, most build.
- Natural fit for a DAO client (pays from multisig; subscription visible
  on-chain).

**Recommendation:** manual for CaveBro now → on-chain subscription (model
3) as the productized form, because it preserves the no-server promise and
is the most coherent with the product. Offline signed license (model 2) is
the fallback if you need Stripe/fiat before the on-chain version exists.

## Pricing / tiers (deck says "tiered by AUM")

- Tier by AUM bracket (e.g. up to $1M / $1-10M / $10M+), flat monthly per
  bracket, with an optional performance component on yield delivered.
- For CaveBroDAO: consider a discounted or free pilot — first reference
  customer is worth more as a logo + case study + demand signal than the
  fee. Charge once it's proven on their treasury.

## What the subscription actually buys (so it's a product, not a favor)
- Onboarding + the staged bring-up (CLIENT_ONBOARDING.md).
- Release updates and new strategies as they ship.
- Incident support + cap/risk reviews + an SLA you define.
- On-chain attestation / reporting of what the fleet did.
The client always keeps custody and the stop/withdraw controls.

## Concrete steps for CaveBroDAO (do these)
1. Agree commercial terms: pilot price (or free pilot), tier, support SLA.
2. Light agreement: scope, risk disclosure (from CLIENT_ONBOARDING.md),
   "not an offer of securities," support terms.
3. Payment rail: a Hedgents USDC receiving wallet (or Stripe). For a free
   pilot, skip.
4. Onboard per CLIENT_ONBOARDING.md (devnet smoke → mainnet-tiny → scale).
5. Optional: hand-issue a signed license file if you want the formal
   mechanism in place from day one (otherwise trust-based for the pilot).

## Build roadmap (don't over-invest before there's demand)
- Now: manual (invoice + agreement). No code.
- When 3-5 clients: build model 2 (signed license) OR jump to model 3.
- Productized: model 3 (on-chain subscription) + a self-serve "download,
  run in sim, subscribe on-chain to go live" flow. This is the same
  Phase-1 "fleet multi-tenancy" line in the fundraise.

Honest note: licensing is the secondary product (hgMETAL is the flagship).
Keep it manual until a real client pipeline justifies the build.
