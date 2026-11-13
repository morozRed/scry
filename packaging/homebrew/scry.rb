# frozen_string_literal: true

# Template for the formula in morozRed/homebrew-tap (Formula/scry.rb). The Release
# workflow fills __VERSION__ and __SHA256__ with scripts/render-formula.sh, checks the result
# with brew and pushes it to the tap. Edit it here, never in the tap.
class Scry < Formula
  desc "See what your coding agents left running on your Mac"
  homepage "https://github.com/morozRed/scry"
  url "https://github.com/morozRed/scry/releases/download/v__VERSION__/scry-__VERSION__-darwin-arm64.tar.gz"
  sha256 "__SHA256__"
  license "MIT"

  depends_on arch: :arm64
  depends_on macos: :sonoma

  def install
    bin.install "scry"
  end

  def caveats
    <<~EOS
      Optional, for disk history and exact agent sessions:
        scry daemon install
        scry hook install claude

      The LaunchAgent and the hooks run #{HOMEBREW_PREFIX}/bin/scry, not the Cellar
      path, so they keep working across upgrades. After `brew upgrade scry` the daemon
      restarts itself on the new binary within a minute.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/scry --version")
    ENV["SCRY_HOME"] = testpath/"scry"
    assert_match "scan", shell_output("#{bin}/scry doctor")
    system bin/"scry", "overview", "--json"
  end
end
