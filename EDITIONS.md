# localmem editions

localmem ships as three distinct products. Each has a clear license,
audience, and revenue model. The seam between them is **capabilities,
not quality** — the Community Edition is feature-complete for solo
developers; the paid tiers add multi-user, compliance, and managed
hosting that solo developers don't need.

## The three editions

| Edition | Repo | License | Audience |
|---|---|---|---|
| **Community** (this repo) | [localmem-community](https://github.com/VJ-yadav/localmem-community) | **Apache-2.0** | Individual developers, indie hackers, internal-tool builders at any size company |
| **Enterprise** | `localmem-enterprise` (private) | Closed proprietary, annual contract | Teams + companies needing SSO, RBAC, audit export, compliance |
| **Cloud** | `localmem-cloud` (private) | Closed proprietary SaaS | Anyone who wants hosted, no-ops, cross-device sync |

## The shape of the model

This is **open-core**, the same pattern used by GitLab, Sentry,
PostHog, Supabase, and ClickHouse. The free Community Edition is the
product 90% of users need; the paid tiers exist because the remaining
10% (teams with compliance + multi-user identity requirements) get
real value from capabilities that require operational complexity to
ship.

What you can rely on:

- **Trust** — the core that touches your memory is Apache-2.0,
  auditable, forkable.
- **Adoption** — zero friction for individual developers.
- **No reverse-degradation** — anything that ships Apache-2.0 stays
  Apache-2.0. We never "take a feature private" after the fact.

## What's in each edition

### Community Edition (Apache-2.0, this repo)

Everything required for a single developer on a single machine to
have a complete local-first memory layer. Free forever.

- Core Rust binary + TypeScript MCP server
- Hybrid retriever (BM25 + ANN + facts + recency + per-kind decay
  + MMR + cross-encoder rerank)
- Active contradiction resolution + bitemporal facts + event-log
  replay
- Container tags + kind taxonomy + visibility/retention reserved
  tags
- Discovery API (`subjects`, `tags`, `summarize`, `recent`, `audit`)
- Local dashboard at `127.0.0.1:8088`
- Local LLM extraction via Ollama + rule extractors + user-authored
  YAML extractors
- MCP auto-installer for Claude Desktop / Claude Code / Cursor /
  Windsurf / Cline
- ChatGPT + Claude export importers + first-run import wizard
- Lifecycle commands (`setup`, `service`, `doctor`, `status`,
  `todo`, `journal`)

If you're a solo developer or small team, you never need to leave
the Community Edition. That's deliberate.

### Enterprise Edition (closed proprietary, sold via contract)

Capabilities solo developers don't need but teams and companies do.

- **Multi-tenancy** — one localmem serving multiple users, isolated
  memory homes
- **SSO** — SAML / OIDC (Okta, Azure AD, Google Workspace)
- **RBAC** — admin / writer / reader / auditor + per-tag scopes
- **Per-user audit logs** — who recalled what, who wrote what, who
  forgot what
- **SIEM audit log export** — Splunk, Datadog, Sumo Logic
- **BYOK encryption at rest** — customer-managed keys
- **Data residency** — EU-only, US-only, on-prem-only
- **Domain-specific extractors** — legal, medical, financial
- **Webhook integration** — on commit / contradiction / supersede
  → Slack / Teams / PagerDuty
- **Database export connectors** — Snowflake, BigQuery, S3, Postgres
- **Terraform provider + Helm chart + Kubernetes operator**
- **Premium connectors** — Notion API, Slack, Gmail, Google Drive
- **Multi-team analytics dashboard**
- **Compliance certifications** — SOC 2 Type II, ISO 27001, HIPAA
  BAA

Pricing: annual contract, sized by seats + features. Contact
[localmem.org](https://localmem.org) for a quote.

### Localmem Cloud (closed proprietary SaaS)

The Enterprise Edition + we run it for you. Same proprietary code,
plus hosting / monitoring / backup / SLA.

- Hosted relay — cross-device sync, E2E encrypted
- Hosted Intelligence — GPT-4-grade extraction vs. local Ollama
- Per-account JWT + Stripe metering
- Customer-facing dashboard — signup, usage, billing, team mgmt
- Multi-region replication + 99.9% SLA + automated backups
- Mobile companion app (iOS / Android)
- 24/7 support + dedicated CSM (Enterprise plan)

Pricing: per-seat or per-request, depending on workload shape.

## Hard rules (what you can trust us not to do)

1. **Community Edition is feature-complete for solo developers.** We
   do not cripple it to upsell. A single developer on a single machine
   never feels they're missing something paid users have.
2. **Enterprise Edition wraps Community Edition; doesn't fork it.**
   The `localmem-enterprise` repo imports the OSS core as a Rust
   crate dependency. No divergent forks of the same module.
3. **Cloud is Enterprise + hosting.** No features exist in Cloud
   that self-hosted Enterprise can't run.
4. **No reverse-degradation.** Anything that ships Apache-2.0 in
   Community stays Apache-2.0 forever. We never "take a feature
   private" after the fact.
5. **The seam is enforced by repository boundaries, not by
   code-level feature flags.** Enterprise modules live in the
   enterprise repo.

## When should you pay us?

You probably should stay on the Community Edition if:

- You're a solo developer
- You're a small team (<10) and trust each other with full access
- You don't have regulatory requirements

You probably should talk to us about Enterprise if:

- You have 10+ developers sharing memory
- You need SSO (Okta, Azure AD, Google Workspace)
- You have a compliance requirement (SOC 2, HIPAA, GDPR DPA, ISO 27001)
- You want audit log streaming to your SIEM
- You need data residency (EU-only, US-only, on-prem-only)
- You want BYOK encryption at rest

You probably should pick Localmem Cloud if:

- You want a managed cloud instance (no self-hosting)
- You want cross-device sync that just works
- You want a mobile app

## Contributing

Contributions to this repo (Community Edition) are welcome under
Apache-2.0. See [CONTRIBUTING.md](CONTRIBUTING.md).

We do not accept OSS contributions to the Enterprise or Cloud repos
— they're proprietary. This isn't us saying your work isn't valuable;
it's us saying the open-core boundary is enforced by repository
rather than by license-on-license inside the same repo. That's the
only way the open-core model stays honest long-term.

## Contact

- **Open-source questions / bug reports:** [Issues](https://github.com/VJ-yadav/localmem-community/issues)
  or [Discussions](https://github.com/VJ-yadav/localmem-community/discussions)
- **Enterprise / Cloud inquiries:** [localmem.org](https://localmem.org)
- **Donations (support the OSS work):** [GitHub Sponsors](https://github.com/sponsors/VJ-yadav)
