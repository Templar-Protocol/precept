# Agent Guide: Precept

Templar's fork of Blend v2 for Soroban. See `README.md` for layout, build and test commands.

## Documentation

Precept keeps two documentation sets, and a change that affects users can need both.

| Set | Path | Audience | Covers |
| --- | --- | --- | --- |
| Repo docs | `docs/` | Auditors, integrators, maintainers | The fork in depth: changes from Blend v2 with code permalinks, admin levers, runbooks, release reproduction |
| Public docs | `docs-library/` (submodule, `Templar-Protocol/precept-docs`) | Lenders, borrowers, market creators, developers | The published site (HonKit, deployed by Vercel from its `main`) |

### When to write docs

- A change to contract behaviour, parameters, admin or governance levers, risks, or the release process updates `docs/`
  in the same PR.
- If the change is visible to users, market creators or integrators, also update the matching page in `docs-library/`.
  Commit that in the precept-docs repo first, then bump the submodule pointer here.
- Leave a page as it is only if it is still accurate. If you leave a public page stale on purpose, say so in the PR.

### How to write them

- Ground every claim in contract code at a cited commit, or in live pool state. Do not paraphrase other docs.
- Write for the reader of that set. Public pages explain what a user can do and what can go wrong. Repo pages carry the
  mechanism, the code links and the edge cases.
- Keep one source for each fact. Public pages link to repo docs for depth and do not copy them.
- Follow `docs-library/CONTRIBUTING.md` for public pages (market not pool, no em dashes, pinned code links only on
  `developers/code-references.md`, Blend named only on `developers/code-lineage.md`, no market lists). Run
  `npm run check` and `npm run build` in `docs-library/` before committing.
- Edit to plain, specific prose. No filler, no marketing tone, no unverified numbers.
