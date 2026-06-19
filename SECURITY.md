# Security Policy

localmem is local-first: your memory lives in a file on your own machine, and
no content ever leaves it on the core read/write paths. That shapes what a
vulnerability looks like here. The issues we care most about are anything that
could exfiltrate memory content, weaken the local trust boundary, or let a
crafted capture or import corrupt the append-only event log.

## Supported versions

We support the latest released version. Please reproduce on the newest release
before reporting.

| Version | Supported |
|---|---|
| 0.3.x (latest) | yes |
| < 0.3 | no |

## Reporting a vulnerability

Please report privately. Do not open a public issue for a security problem.

- Preferred: GitHub private vulnerability reporting, the "Report a vulnerability"
  button under the repository's Security tab
  (https://github.com/VJ-yadav/localmem-community/security/advisories/new).
- Or email vjyadav193@gmail.com with "localmem security" in the subject line.

Please include a description, reproduction steps, the affected version, and the
impact you observed.

## What to expect

- Acknowledgement within 72 hours.
- An initial assessment and a fix timeline once the report is triaged.
- Credit in the release notes if you would like it.

Thanks for helping keep localmem trustworthy.
