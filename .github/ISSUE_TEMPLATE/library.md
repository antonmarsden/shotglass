---
name: Kit library / library system
about: Using, fetching, pinning or checking shader libraries ([libraries], library.toml, shotglass.lock)
labels: lane:kit
---

<!-- This repo handles how films load libraries. A new function, transition or
     library *in* the kit goes to https://github.com/antonmarsden/shotglass-kit/issues
     (it has its own template and CONVENTIONS.md). -->

**What you're trying to do** (the `[libraries]` entry, the `#include` or `type = "<library>/<name>"`, the command)


**What happens** (error message with its path, or the unexpected result)


**Library** (name, version or branch, source: git URL or local path; the relevant lines of `shotglass.lock`)


**Proposal** (if it's a feature: spec syntax, `library.toml` fields, lock or peer/contract checks)


**Done when**
- [ ] 
- [ ] Unit test in `library.rs`, README.md updated
