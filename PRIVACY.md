# Privacy policy

Filex is a file explorer. It reads and indexes files on your own computer, and
the search index never leaves it. Filex makes only the two kinds of network
request below.

## Anonymous usage data and crash reports (optional)

When **Share anonymous usage data** is on, the Filex app sends the following
to [PostHog](https://posthog.com) ([privacy policy](https://posthog.com/privacy)):

- **How features are used:** for example that a folder was opened from the
  sidebar, a search found 40 results and the third one was opened, a file
  was copied, a setting was changed, or a keyboard shortcut was used. Only
  counts, timings, on/off values and fixed labels written into Filex are
  sent.
- **Performance:** for example how long a folder took to load, how fast
  search answered, start-up time, memory use and how many files are indexed.
- **Crash reports and errors**, including stack traces, with file names,
  paths, quoted text, email addresses and web addresses removed.
- **The app version and your operating system.**

Filex never sends file or folder names, paths, search text, tags, the
contents of files, your user name, your computer name or your email address.
Data is anonymous: it is tied to a random ID created for this install, not to
you or your computer. That ID is deleted when you turn sharing off, so
turning it back on starts a new, unconnected one. Filex tells PostHog not to
look up your location, and our PostHog project is set to discard IP
addresses. PostHog still sees the network connection, like any web request. The background index service
(`filex-indexd`) contains no reporting code at all.

**Feedback** you type into *Send feedback* goes to PostHog when you press
Send, even if sharing is off, with file names, paths and email addresses
removed. Because of that, we can't reply to it.

**Your choice:**

- **Windows:** the installer asks on its Privacy page. For an unattended install,
  pass `FILEX_CRASH_REPORTS=0` to `msiexec` to opt out.
- **macOS and Linux:** on by default; there is no installer step.
- **Everywhere:** turn it off at any time in Settings → Privacy → Share
  anonymous usage data. It takes effect immediately, and nothing is sent
  while it is off.

## Update checks

The app downloads a small update manifest from GitHub
(`github.com/NayanVR/Filex/releases`) to see whether a newer version exists,
and downloads the update itself from the same place. No personal data is sent.
GitHub sees the request like any web request
([GitHub privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement)).

## Data on your computer

Settings, tags, recent folders, the search index, logs, the random
analytics ID and usage data waiting to be sent (when Filex closes before
sending it) are stored locally in your user profile (see [Uninstall](README.md#uninstall)) and are never uploaded.
