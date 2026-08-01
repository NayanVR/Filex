# Canonical shape of the Homebrew cask. The release pipeline
# (.github/workflows/release.yml) renders this into the tap repo
# (<owner>/homebrew-filex, Casks/filex.rb) on each release, updating
# `version` and `sha256`. Kept here for reference and local edits.
#
# Ad-hoc-signed, un-notarized: `brew install --cask` applies quarantine, so
# users see one Gatekeeper "unidentified developer" prompt on first launch
# (right-click → Open, or System Settings → Privacy & Security). This is the
# intended free-tier cost — see docs/design-distribution.md decision 2.
cask "filex" do
  version "0.0.0"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"

  url "https://github.com/NayanVR/filex/releases/download/v#{version}/filex-#{version}-macos.tar.gz"
  name "Filex"
  desc "Fast cross-platform file explorer"
  homepage "https://github.com/NayanVR/filex"

  app "Filex.app"
end
