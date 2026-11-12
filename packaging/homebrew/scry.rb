# frozen_string_literal: true

# Homebrew formula for Scry. Not published yet: it goes into a tap with the first tagged
# release. Before publishing, replace the two PLACEHOLDER values below with the tag's
# source tarball and its checksum:
#
#   curl -sL https://github.com/morozRed/scry/archive/refs/tags/v0.1.0.tar.gz | shasum -a 256
#
# Check it locally with:
#
#   brew install --build-from-source ./packaging/homebrew/scry.rb
#   brew test scry
#   brew audit --strict --new scry
class Scry < Formula
  desc "See what your coding agents left running on your Mac"
  homepage "https://github.com/morozRed/scry"
  # PLACEHOLDER: the tagged source tarball.
  url "https://github.com/morozRed/scry/archive/refs/tags/v0.1.0.tar.gz"
  # PLACEHOLDER: shasum -a 256 of that tarball.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  head "https://github.com/morozRed/scry.git", branch: "main"

  depends_on "rust" => :build
  depends_on arch: :arm64
  depends_on macos: :sonoma

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/scry")
  end

  def caveats
    <<~EOS
      Optional, for disk history and exact agent sessions:
        scry daemon install
        scry hook install claude

      The LaunchAgent and the hooks run #{HOMEBREW_PREFIX}/bin/scry, not the Cellar
      path, so they keep working across upgrades. Run `scry daemon install` again after
      `brew upgrade scry` to restart the daemon on the new binary.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/scry --version")
    ENV["SCRY_HOME"] = testpath/"scry"
    assert_match "scan", shell_output("#{bin}/scry doctor")
    system bin/"scry", "overview", "--json"
  end
end
