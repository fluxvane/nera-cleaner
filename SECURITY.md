# Security policy

Nera Cleaner deletes files, so a bug in its safety rules is a security problem. Reports about any of these
are especially welcome:

- a way to make the app remove or trash something the safety layer (`nera_core::safety`) should refuse
- a cleanup category that removes data an app cannot recreate
- anything that runs with more privileges than the screen says (the maintenance tasks ask for an administrator)

## Supported versions

Only the latest commit on `main` is supported. There are no maintained release branches yet.

## Reporting a vulnerability

Please do not open a public issue.

- Preferred: [report it privately on GitHub](https://github.com/fluxvane/nera-cleaner/security/advisories/new)
- Or e-mail hello@fluxvane.com

Include the platform and OS version, the steps to reproduce it, and what was removed or exposed. You will get
a reply within 7 days. Once a fix is on `main`, the report is credited in the advisory unless you prefer
otherwise.
