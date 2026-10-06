# ADR 0043 — Free cloud onboarding

- **Status:** Accepted
- **Date:** 2026-10-06
- **Related:** [ADR 0004 — Default models](0004-default-models.md)

## Context

A first-time user who starts Fono from the app menu never saw the setup
wizard: it runs only in a terminal, so Fono wrote a local-only config and
never offered a cloud service. Several providers have an ongoing free tier
that runs dictation well, but getting a key meant finding the right console
page, creating a key, and pasting it into a terminal prompt or a TOML file.

We want a user with no technical background to reach a working free cloud
setup in about 20 seconds, without making existing keys or local models
harder to use.

## Decision

1. **Fono never creates provider accounts for the user.** Sign-up pages use
   captchas and email or phone checks, and their terms forbid automated
   sign-up. Fono opens the provider's own page and the user signs in there.
   Fono does not run a proxy that hands out shared keys: that would mean
   paying for traffic, policing abuse, and sending users' audio through a
   Fono server.

2. **One welcome screen in the browser settings page** (`#/welcome`), opened
   automatically on the first start of a graphical session with no config,
   and from the tray ("Connect a service…"). It shows, side by side:
   - one card for every **free** provider,
   - "This computer" (local models),
   - a paste-anywhere field for a key the user already has,
   - a collapsed "Other services" list.

   The terminal wizard asks the same question in the same order.

3. **"Free" means an ongoing free tier with no card that runs full
   dictation (speech-to-text and cleanup) on Fono's default models.**
   One-off trial credit does not count, and neither does a free plan that
   covers only part of dictation. The row is built from catalogue data
   (`Onboarding::free_tier` in `provider_catalog.rs`), so the UI names no
   provider. Free tiers change: the catalogue records the date the tiers
   were last checked against each provider's pricing page.

4. **A saved key that works is reused before anything else.** The welcome
   screen and the wizard test every saved or environment key first and
   offer the one that works, so a reinstall or a second run never makes
   the user create a new key.

5. **The provider is recognised from the key's prefix**, so pasting a key
   never needs a provider dropdown. Providers without a distinctive prefix
   are only matched when the user is connecting that provider.

6. **OAuth (PKCE, S256) where the provider offers it** (OpenRouter today).
   The callback is served on loopback; `state` is random, single use and
   expires after 10 minutes. Keys are labelled `fono-<hostname>` wherever
   the provider lets us name them, so old keys are easy to find and revoke.

7. **The clipboard watcher is limited in scope and time.** It runs only
   after the user clicks Connect on a provider without OAuth, stops after
   2 minutes or when the screen closes, accepts only a whole clipboard that
   matches that provider's key prefix, and never logs or stores anything
   else. Where background clipboard reads fail (Wayland without focus),
   pasting on the page does the same job.

8. **Connecting a provider changes only what it covers.** Choosing local
   never deletes keys; connecting a cloud provider never turns on the
   opt-in assistant or moves a capability the provider lacks.

## Consequences

- First-run users get a cloud option without a terminal, and a key they
  already have connects in one paste.
- No new dependency: OAuth, the clipboard and opening the browser reuse
  crates already in the binary.
- The free row depends on providers' pricing, which we do not control. When
  a tier changes, update the catalogue entry and its check date; the UI
  follows.
- A provider that adds a free audio model or OAuth later needs only
  catalogue changes to move into the free row or gain one-click sign-in.
