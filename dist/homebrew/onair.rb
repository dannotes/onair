# frozen_string_literal: true
#
# Homebrew formula for onair.
#
# This file goes in a separate tap repo named "homebrew-onair":
#   https://github.com/dannotes/homebrew-onair/blob/main/Formula/onair.rb
#
# After publishing, users install with:
#   brew tap dannotes/onair
#   brew install onair
#
# When you cut a new release, update `version` and the four `sha256` lines.
# Compute SHAs with:  shasum -a 256 onair-*.tar.gz

class Onair < Formula
  desc "Watch Microsoft Teams and turn a Philips WiZ smart bulb red while on a call"
  homepage "https://github.com/dannotes/onair"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/dannotes/onair/releases/download/v#{version}/onair-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_ME_WITH_REAL_SHA256"
    end
    on_intel do
      url "https://github.com/dannotes/onair/releases/download/v#{version}/onair-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_ME_WITH_REAL_SHA256"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/dannotes/onair/releases/download/v#{version}/onair-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_ME_WITH_REAL_SHA256"
    end
  end

  def install
    bin.install "onair"
    doc.install "README.md", "LICENSE"
  end

  def caveats
    <<~EOS
      Onair runs as a foreground daemon and serves a dashboard at:
          http://localhost:9876

      To run it on login, create a launchd agent. See the README for details.

      Config and call history are stored at:
          ~/Library/Application Support/Onair/onair.db   (macOS)
          ~/.config/onair/onair.db                       (Linux)
    EOS
  end

  test do
    assert_match "onair #{version}", shell_output("#{bin}/onair --version")
  end
end
