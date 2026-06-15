# localmem for teams (startups + small-to-mid-size companies)

The Community Edition is built for **one developer per machine**. That's
the right primitive. But a small startup or mid-size company can still
get a lot of value from it without paying for the Enterprise Edition —
as long as you understand where the edges are.

This document is a pragmatic guide: what works, what doesn't, and when
to graduate to the Enterprise Edition.

---

## What works on Community Edition (single user, multiple machines)

If every developer at your company runs localmem on their own laptop,
each person has:

- Their own memory across every MCP tool (Claude Desktop / Code /
  Cursor / Windsurf / Cline)
- Their own dashboard at http://127.0.0.1:8088
- Their own auditable trail of what their AI tools learned about them

This is the **one-developer-per-machine** pattern. It works at any
team size. Each engineer's memory is their own; nothing is shared
between teammates.

**When this is enough:**

- Solo founders and very small teams (1-5 engineers)
- Teams where each engineer's memory is genuinely personal (preferences,
  decisions, todos)
- Companies whose AI tooling is consumer-style (each engineer has their
  own Claude Desktop account, Cursor seat, etc.)

**When this isn't enough:**

- You want a shared knowledge base that everyone on the team can read
  from
- You need RBAC: "interns can only read, leads can write to the team
  channel, only admins can forget"
- You have compliance requirements (SOC 2, HIPAA, GDPR DPA)
- You need an audit log per user that survives "what did Alice's AI
  tool retrieve about customer X last quarter?"

For those, see [the Enterprise Edition](#when-to-graduate-to-the-enterprise-edition)
below.

---

## What you can do today without the Enterprise Edition

### Pattern 1: shared `events.jsonl` via git

For a small team (say <10 engineers) where everyone trusts everyone:

1. One person sets up localmem on a shared dev VM or NAS
2. Everyone points their `LOCALMEM_HOME` env var to that path
3. The MCP server runs on the VM; engineers connect from their laptops
   via SSH tunnel: `ssh -L 7788:127.0.0.1:7788 dev-vm`

**What you get:** every team member's writes are visible to everyone.
The journal is shared. The dashboard is one URL.

**What you don't get:** RBAC (everyone has full access). Per-user
audit logs (you can see *what* was written but not *who* wrote it
beyond the `source.user` field). Conflict resolution across multiple
writers (the lex index allows one writer at a time; concurrent writes
serialize).

This works for a trusted small team. It does **not** scale to 50+
engineers or any regulated environment.

### Pattern 2: per-team localmem instances

A more conservative pattern: each team (frontend, backend, ML) runs
its own localmem instance with its own `events.jsonl`. Use
container-tag scoping (`--tags team=frontend`) inside each.

Migration between teams is straightforward (`localmem export` →
`localmem import archive`), so people who change teams can carry
their memory.

### Pattern 3: project-scoped memory via tags

For any pattern, use tags to scope memories to projects:

```bash
# Developer A writes (from their AI tool)
memory_write content="We use Postgres 16 with pgvector for the search service" \
  tags={"project": "search-service", "kind": "decision"}

# Anyone on the team queries
localmem profile --tags project=search-service
```

Tags are first-class in the Community Edition. Use them aggressively
— it costs nothing and pays off the first time you need to filter.

### Pattern 4: cold-start onboarding

When a new engineer joins, you can hand them a bootstrap memory:

```bash
# On the existing senior engineer's machine, export their team-tagged subset
localmem export --tags team=backend ~/Downloads/team-backend-bootstrap.tar.gz

# New engineer imports
localmem import archive ~/Downloads/team-backend-bootstrap.tar.gz
```

The new engineer now has all the team's accumulated decisions, conventions,
constraints, and prefs. From there, their AI tools start from a useful
baseline instead of zero.

---

## What you should be careful about

### Don't share `LOCALMEM_HOME` over Dropbox / Google Drive / iCloud

The event log is append-only and crash-safe. Cloud sync tools that
do file-level deduplication or conflict-renaming will corrupt it.
Use one of the patterns above (NAS + SSH, or per-team
instances) instead.

### Don't put production credentials in memory

The Community Edition does **not** have BYOK encryption at rest
(that's an Enterprise feature). If you tell your AI tool a real
AWS access key, it goes into `events.jsonl` in plaintext. The
scrubber (`core/src/rewriter.rs`) catches common formats like
`SECRET=...`, JWTs, and API keys with known prefixes, but it's a
defense-in-depth, not a guarantee.

**Rule of thumb:** if the value would harm your company if a
laptop got stolen, don't tell your AI tool about it directly.
Use a vault reference instead (`{{vault:db_password}}`).

### Don't expose `localmem serve` to the internet

The daemon binds to `127.0.0.1:7788` by default. Don't change
this to `0.0.0.0:7788` unless you've put a proper authentication
proxy in front (and at that point, you want the Enterprise
Edition).

### Don't try to grow it into a multi-tenant SaaS

The Community Edition is single-user by design. Trying to bolt
multi-tenancy onto it will lead to subtle bugs (write conflicts,
missing audit trails, shared embedder state). The Enterprise
Edition is built for this from the ground up.

---

## When to graduate to the Enterprise Edition

You probably need to talk to us about Enterprise if any of these
become true:

| Trigger | What changes |
|---|---|
| **You have >10 engineers** sharing memory | Multi-tenancy + RBAC + per-user audit becomes load-bearing |
| **You need SSO** (Okta, Azure AD, Google Workspace) | Built into Enterprise; not in Community |
| **You have a compliance requirement** (SOC 2, HIPAA, GDPR DPA, ISO 27001) | Enterprise carries the certifications |
| **You want audit log streaming to your SIEM** (Splunk, Datadog) | Enterprise has webhook + structured log export |
| **You need data residency** (EU-only, US-only, on-prem-only) | Enterprise supports geo-pinned deployment |
| **You want BYOK encryption at rest** | Enterprise feature |
| **You want a managed cloud instance** (no self-hosting) | That's Localmem Cloud |

**What it costs:** annual contract, sized by seats + features. Talk
to us at [localmem.org](https://localmem.org). We're not going to
quote a number here that ages badly; the pricing page is the source
of truth.

**What it doesn't change:** the Community Edition keeps working
exactly the same. If you migrate to Enterprise, you can export your
data back to Community any time. We don't lock you in.

---

## When to skip localmem entirely and use SaaS

We're honest about this: localmem isn't the right tool for every team.

You're probably better off with mem0 / Letta / Cognee if:

- You don't have anyone on the team comfortable installing CLI tools
  on every developer's machine
- Your developers all use one AI tool (e.g. you're a pure ChatGPT
  shop) and don't need cross-tool memory
- You explicitly want a vendor to handle compliance, hosting, and
  on-call

That's a real trade-off. We're not going to pretend SaaS doesn't have
benefits. But you should be aware that:

- mem0 sends plaintext to their cloud
- Letta hosts model state for you (more vendor-lock-in than memory
  alone)
- Cognee is graph-first and has its own opinions

Pick the tool whose trade-offs match your team's reality.

---

## The internal-tools play

A pattern we see working well: localmem as the **memory layer for
your internal AI tools**.

Suppose you're building a customer-support AI for your own employees.
Each support agent uses Claude (via your internal tool). Their AI
needs to remember:

- The customer's history
- Past internal decisions about how to handle similar tickets
- Each agent's personal preferences (which tone, which level of detail)

You can:

1. Run localmem on the internal tool's backend server (single instance)
2. Use container tags to scope: `customer:acme-corp`, `agent:alice`,
   `kind:decision`
3. Wire the internal tool's MCP layer to localmem
4. Build agent-specific scoping at your application layer

**This works for**: any small-to-mid internal tool where you control
the deployment.

**What you'd need from Enterprise** at scale: RBAC ("only customer
X's account team can read customer X's memories"), SOC 2 (if you're
selling the tool externally), audit log streaming (for compliance
review).

But the bootstrap can be Community Edition. You don't have to pay
until you actually have the multi-user / compliance requirement.

---

## Roadmap signposts

The community edition will keep getting better at the
**single-user-on-multiple-machines** story. Things on the v0.2.1+
roadmap that help teams without crossing into Enterprise territory:

- Branch-scope memory grammar (per-git-branch context)
- Curated YAML "packs" you can install into anyone's localmem
  (engineering-simplicity, twelve-factor, etc.)
- Typed conflict view (see contradictions across your team's
  combined memory)
- Memorybench published numbers (LongMemEval / LoCoMo)

If your team's needs are pushing past what the Community Edition
can do, that's a signal to talk to us about Enterprise — not to
fork the OSS and bolt multi-tenancy onto it. We've designed the seam
so the migration is straightforward; forking is hard.

---

## Donations

If localmem saves your team time and you want to support the OSS
work without buying Enterprise: GitHub Sponsors is the cleanest way.

- [github.com/sponsors/VJ-yadav](https://github.com/sponsors/VJ-yadav)

Every dollar goes to keeping localmem under active development with
zero corporate alignment pressure. We are deliberately not chasing
ad revenue, telemetry, or referral kickbacks.

---

## What to do next

Solo developer or 1-5 person team:
→ [QUICKSTART.md](QUICKSTART.md) is enough. Try the patterns here
when the team grows.

5-20 person team:
→ Start with Pattern 1 (shared `events.jsonl` over SSH) or Pattern 2
(per-team instances). Both are free. Revisit Enterprise when one of
the trigger conditions above hits.

20+ person team or any regulated environment:
→ Start a conversation about Enterprise at
[localmem.org](https://localmem.org). The Community Edition will
keep working in the meantime; we're not gating evaluation.
