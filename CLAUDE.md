# Working in this repo

## Branches
- `main` tracks upstream (`origin`, asd123ch/rdp123). Don't commit to it.
- `daily` is the local integration branch; it tracks `fork/daily` (willie/rdp123).
- Each change goes on its own branch cut from `daily` (`feat/…`, `fix/…`).
- Finished branches are merged locally into `daily` with
  `git merge --no-ff <branch> -m "Merge branch '<branch>' into daily"`. No PRs.

## Vendored IronRDP crates
Crates under `vendor/` replace their crates.io versions through
`[patch.crates-io]` in `Cargo.toml`. Each patch entry has a comment listing
what differs from the published version; update it when changing the crate.
