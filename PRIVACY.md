# Privacy policy

Filex is a file explorer. It reads and indexes files on your own computer, and
the search index never leaves it. Filex makes only the two kinds of network
request below.

## Crash reports and diagnostics (optional)

When **Share anonymous diagnostics** is on, the Filex app sends the following
to [Sentry](https://sentry.io) ([privacy policy](https://sentry.io/privacy/)):

- crash reports and error events, including stack traces;
- anonymous performance measurements (for example search latency, index
  start-up time and resource use);
- app-session health (whether a session ended in a crash, and the app version).

Before anything is sent, file names, folder paths, search queries and tags are
removed. Filex does not attach your user name, computer name or IP address.
Sentry receives the network connection like any web request. The background
index service (`filex-indexd`) contains no reporting code at all.

**Your choice:**

- **Windows:** the installer asks on its Privacy page. For an unattended install,
  pass `FILEX_CRASH_REPORTS=0` to `msiexec` to opt out.
- **macOS and Linux:** on by default; there is no installer step.
- **Everywhere:** turn it off at any time in Settings → Share anonymous
  diagnostics. Nothing is sent while it is off.

## Update checks

The app downloads a small update manifest from GitHub
(`github.com/NayanVR/Filex/releases`) to see whether a newer version exists,
and downloads the update itself from the same place. No personal data is sent.
GitHub sees the request like any web request
([GitHub privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement)).

## Data on your computer

Settings, tags, recent folders, the search index and logs are stored locally in
your user profile (see [Uninstall](README.md#uninstall)) and are never uploaded.
