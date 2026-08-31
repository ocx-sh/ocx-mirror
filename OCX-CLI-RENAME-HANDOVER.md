# Handover: OCX CLI rename (0.6)

The `ocx` CLI renames three commands in **0.6**. The old spellings keep working
in 0.6 as hidden commands that print a deprecation warning to stderr, and are
**deleted in 0.7**.

| Old | New |
|---|---|
| `ocx run` | `ocx exec` |
| `ocx package describe` | `ocx package description push` |
| `ocx package info` | `ocx package description pull` |

New visible aliases, additive and safe to ignore: `x` on `ocx exec` and
`ocx package exec`, `rm` on `ocx remove`, `ls` on `ocx index list`.
`ocx index list` itself is **not** renamed.

## Why the verbs moved

`ocx run` and `ocx package exec` were the same operation — compose an
environment, spawn a child — differing only in whether symbols resolve as
binding names or OCI identifiers. That is the `ocx pull` / `ocx package pull`
shape, which already shares one verb. Renaming also frees `run` for a future
`ocx-run` plugin, which cannot exist while a built-in claims the name.

`ocx package describe` **wrote** the `__ocx.desc` tag while `ocx package info`
**read** it — a set/get pair with two read-sounding names. `push`/`pull` are the
tier's own transport verbs and say which direction the data moves.

## What this repository uses
Reference counts measured 2026-08-30:
| Spelling | Occurrences |
|---|---|
| `ocx run` | 551 |
| `package describe` | 39 |
| `package info` | 65 |
| `index list` (unchanged, alias only) | 118 |

## Do not ride the deprecation window

The window exists for third-party scripts we do not control. This repository is
ours, so it moves to the new spellings **in the 0.6 wave**. If it is still on the
old spellings when 0.7 lands, it breaks with no warning.

## Verification

After updating, grep for the old spellings and confirm zero hits:

```sh
grep -rn -e 'ocx run' -e 'package describe' -e 'package info' . \
  --exclude-dir=.git --exclude-dir=node_modules --exclude-dir=target
```

Running against ocx 0.6 with the old spelling still works but prints
`warning: \`ocx run\` is renamed to \`ocx exec\` and is removed in 0.7` on
stderr — a useful way to find call sites a grep missed.
