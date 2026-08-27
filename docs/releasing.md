# Releasing

A release is an ordinary build that also gets signed, notarised and published.
It runs on a tag and only after the same gates every other build passes.

```bash
# The tag and the version in app/tauri.conf.json have to match, and CI checks.
git tag v0.1.0 && git push origin v0.1.0
```

That produces a signed, notarised `Errand-AI_<version>_aarch64.dmg` on a GitHub
release, with notes generated from the commits.

## What has to be set up once

Five repository secrets. Without them the release still builds and publishes;
it is simply not signed, and macOS warns whoever opens it. That is a real
outcome rather than a broken one, so CI says which step it skipped instead of
failing.

| Secret | What it is |
|---|---|
| `MACOS_CERTIFICATE` | A Developer ID Application certificate, exported as `.p12` and base64 encoded |
| `MACOS_CERTIFICATE_PASSWORD` | The password that `.p12` was exported with |
| `APPLE_ID` | The Apple ID that owns the certificate |
| `APPLE_PASSWORD` | An app-specific password for that Apple ID, never the real one |
| `APPLE_TEAM_ID` | The ten-character team id |

To make the first two:

```bash
# In Keychain Access, export your "Developer ID Application" certificate as
# certificate.p12 with a password, then:
base64 -i certificate.p12 | pbcopy
```

A **Developer ID Application** certificate is the one that matters. An "Apple
Development" certificate signs an app for your own machine and will not let
anybody else open it, which is the whole point of the exercise.

## What the release job actually does

It imports the certificate into a keychain of its own, thrown away with the
runner, never the login keychain. Then it stages the daemon, builds through the
Tauri bundler, and lets the bundler sign and submit: it signs the app, the
browser helper and the daemon in the right order, which hand-rolled `codesign`
calls get wrong by signing the outside before the inside.

Then it says out loud what the build actually is, because "it built" and "it is
signed" are different facts and only one of them decides whether a stranger can
open it:

```
signature: valid
notarised: yes
```

And it checks the daemon is really inside the bundle, since a release without
it is a release that cannot do anything.

## Why the daemon is staged during the build

The bundler copies `app/binaries/errandd-<triple>` into the app. Nothing
rebuilds that file, so it is whatever was last put there. A bundle built from a
stale one ships a daemon older than the database it opens, and the symptom is
not obvious: the app installs its background service, the service starts, and
dies with *"migration 8 was previously applied but is missing in the resolved
migrations"*, which reads like a corrupt database and is nothing of the kind.

CI never hit this, because CI stages the file fresh every time. Only somebody
building locally, from a working copy where that file is hours old, got an app
that could not start. So `scripts/stage-daemon.sh` runs as part of the build now
rather than being a step anybody remembers, and it refuses to stage a binary
older than the code.
