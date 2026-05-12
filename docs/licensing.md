# Licensing position

Where Computeza sits relative to the licenses of the components it
manages. Written for sales, legal review, and funding-due-diligence
audiences. Cross-references `docs/sbom.md`.

Last updated: 2026-05-12.

## TL;DR

- **Computeza itself** (the Rust binary in this repo) is **proprietary**,
  sold per-user under three retail tiers (Team, Business, Sovereign) plus
  a Provider channel program (spec section 11). Source is not licensed for
  redistribution.

- The **components** Computeza installs and manages are individually
  licensed (mostly Apache-2.0 / MIT-equivalent, two AGPL-3.0
  exceptions: Garage and Grafana). See `docs/sbom.md` for the
  per-component table.

- **Computeza's license posture is independent of any component's
  license**. The control plane and data plane communicate over
  documented external APIs (HTTP, SQL, S3) and run in separate OS
  processes. Under AGPLv3 section 5 ("Conveying Modified Source
  Versions") this is "mere aggregation"; the AGPL obligations of
  Garage / Grafana stay with Garage / Grafana.

## The aggregation / process-boundary argument in detail

AGPLv3 section 5 (paragraph 5):

> A compilation of a covered work with other separate and independent
> works, which are not by their nature extensions of the covered work,
> and which are not combined with it such as to form a larger program,
> in or on a volume of a storage or distribution medium, is called an
> "aggregate" if the compilation and its resulting copyright are not
> used to limit the access or legal rights of the compilation's users
> beyond what the individual works permit. Inclusion of a covered work
> in an aggregate does not cause this License to apply to the other
> parts of the aggregate.

What Computeza does that puts the relationship inside this clause:

1. **Process isolation.** Computeza's `computeza` binary runs as its
   own OS process. Garage's `garage` binary runs as its own OS
   process (via systemd unit). Grafana's `grafana` binary runs as
   its own OS process. None of them share an address space, none of
   them link each other's code at compile time, and none of them
   invoke each other's functions through anything other than an
   external API.

2. **No source-level modification.** We download the unmodified
   upstream binaries (Garage from the deuxfleurs CDN, Grafana from
   dl.grafana.com) and register them as services as-is. Patches to
   either component land upstream first, not in this repo.

3. **External API surface only.** The Computeza reconciler talks to
   Garage over Garage's documented HTTP admin API and to Grafana
   over Grafana's documented HTTP API. These are arms-length client
   relationships, not linker integration.

4. **No license-of-the-whole claim.** Computeza's licence (proprietary,
   per-user) makes no claim over the AGPL components. The bundling we
   do at install time is mere aggregation -- each component retains
   its own license terms.

This is the same legal posture AWS RDS / Aiven / Crunchy Data /
Supabase / Cloudflare use to operate AGPL components without their
own control planes becoming AGPL. The pattern is well-understood by
specialist software-licensing lawyers and is not novel to Computeza.

## AGPLv3 section 13 (the network-interaction clause)

> Notwithstanding any other provision of this License, if you modify
> the Program, your modified version must prominently offer all users
> interacting with it remotely through a computer network ... an
> opportunity to receive the Corresponding Source of your version by
> providing access to the Corresponding Source ...

This only triggers when:

- You **modify** the AGPL software, AND
- You **let users interact with the modified version over a network**.

Computeza meets neither condition for Garage or Grafana:

- We do not modify them. The install path is `download upstream
  binary -> drop into cache -> register as service`.
- We do not run them; the operator's host runs them. End-customer
  AGPL obligations to *Garage* or *Grafana* sit with the end customer,
  not with Computeza-the-vendor.

If a customer modifies Garage and exposes it to their users over a
network, that customer takes on the AGPLv3 section 13 obligation for
*their* modification. Computeza-the-vendor is not party to that
obligation.

## What changes if Computeza ever DOES modify Garage / Grafana

If a future Computeza release patches Garage or Grafana before
shipping it to customers -- for example, to backport a security fix
or apply a Computeza-specific configuration patch -- the calculus
shifts. The patched binary that ships through our installer would be
a derivative work of the AGPL component, and AGPLv3 sections 5 + 13
would require Computeza to:

- Publish the modified source under AGPLv3.
- Offer the same modified source to any customer who interacts with
  it over a network (which is the install target's whole point).

This obligation would apply only to the *modified component*, not to
Computeza's control-plane source. But it would still be a meaningful
change in posture.

**Engineering rule:** never patch an AGPL component in this repo.
Always upstream the patch first, then bump the bundle pin once the
upstream release lands. This rule is in [`AGENTS.md`](../AGENTS.md)
under the per-component install playbook.

## Risk tier per business goal

(Mirrored from the chat-time analysis so it's referencable in
funding decks and customer-facing one-pagers.)

| Goal | AGPL risk | Mitigation |
|------|-----------|------------|
| Scale | None | AGPL doesn't restrict customer count. |
| White-label | Mild | Control-plane white-labels cleanly. AGPL components stay visible in `/components` so operators know what's running -- some white-label deals want this hidden behind generic labels. The `/components` page renders the upstream license per row so the user can audit. |
| Resell / sub-reseller | None for control plane | The proprietary control plane resells cleanly through the multi-tier channel (Computeza -> reseller -> sub-reseller -> end-customer, spec section 11). Each end-customer's relationship to Garage / Grafana is between *them* and the upstream project, not between the reseller and the end-customer. |
| Raise funding | Mild | Investors scrutinise license posture. The combination of this document + the SBOM table + a one-line answer ("control plane proprietary, data plane aggregated open-source, no AGPL in our own binary") covers it. |
| Sell to enterprises | Moderate (sales friction, not a blocker) | ~10-20% of Fortune 500 + most financial-services procurement teams have AGPL exclusion clauses. The mitigation is to surface this SBOM + the process-isolation argument *before* legal review, not after. Some customers will still refuse anyway; the v0.1+ alternative-component roadmap (Garage -> SeaweedFS, Grafana -> SkyWalking UI) is the long-term backstop. |

## Recommended customer-facing one-pager outline

Pages to ship alongside any enterprise sales deck:

1. **What you're installing** -- the SBOM table verbatim.
2. **The licensing model** -- "Computeza is proprietary; data-plane
   components retain their upstream licenses; process isolation keeps
   the two independent."
3. **The AGPL-specific answer** -- a one-paragraph version of the
   process-boundary argument above, with a citation to AGPLv3
   section 5 aggregation language.
4. **Alternatives we offer for AGPL-averse customers** -- (deferred
   to v0.1+) Garage swappable for SeaweedFS, Grafana swappable for
   SkyWalking UI. Pricing and feature deltas described.
5. **The audit trail** -- this document plus the SBOM are versioned
   in the repo; any change to the license posture creates a git diff
   that legal can review.

This document does not constitute legal advice. Specialist counsel
should review before any AGPL-sensitive deal closes.
