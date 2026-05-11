# Computeza documentation

This directory hosts engineering and product documentation for Computeza.

## The spec

The canonical source of architectural and product truth is the
**Architecture & Product Specification v1.5** (104 pages, April 2026).
Source comments throughout the workspace cite it by section number
(`spec section 3.4`, `spec section 6.1`, etc.). When the spec and the code disagree,
the spec wins until the code is updated and the spec amended in the same
change.

The PDF is held outside this repository to keep clones small. To work
with it locally, place it next to this README:

```
docs/
+-- README.md                                       (this file)
+-- Architecture-and-Product-Specification-v1.5.pdf (NOT committed)
```

The spec was written before the product name was decided. It uses
"the product" / "the platform" throughout; we always say **Computeza**
in code, READMEs, UI copy, and customer-facing documentation. The crate
prefix is `computeza-*` (the spec's `platform-*` is a placeholder).

## Document set (planned)

These will land here as their corresponding implementation milestones
ship. Today they're empty placeholders.

| Document                       | Status   | Purpose                                          |
| ------------------------------ | -------- | ------------------------------------------------ |
| `architecture.md`              | TODO     | Living architecture overview (mirrors spec section 3)   |
| `naming.md`                    | TODO     | Naming conventions for crates, traits, types     |
| `i18n.md`                      | TODO     | How to add localisable strings; locale workflow  |
| `release-process.md`           | TODO     | Release cadence, versioning, packaging matrix    |
| `security.md`                  | TODO     | Threat model, crypto choices (mirrors spec section 8)   |
| `licensing.md`                 | TODO     | License file format, activation flow, telemetry  |
| `connectors.md`                | TODO     | Connector trait, YAML CDK reference, certification |
| `pipelines.md`                 | TODO     | Pipeline YAML schema, canvas dual-mode contract  |
| `ai-workspace.md`              | TODO     | RAG patterns, agent runtime, Model Gateway, MCP  |
| `runbooks/`                    | TODO     | Operator runbooks (DR drills, key rotation, ...)   |

## Contributing

This is a private commercial codebase; contribution rules will be added
to `docs/contributing.md` once the team beyond a single developer.
