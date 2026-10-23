# Homebrew formula for ward. The release workflow rewrites version, url
# and sha256 through scripts/update-formula.sh; do not edit them by hand.
class Ward < Formula
  desc "Sandboxed microVM runtime for AI agents"
  homepage "https://github.com/igorjs/ward"
  version "0.0.0"
  license "AGPL-3.0-only"

  on_macos do
    on_arm do
      url "https://github.com/igorjs/ward/releases/download/v0.0.0/ward-0.0.0-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/igorjs/ward/releases/download/v0.0.0/ward-0.0.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
    on_arm do
      url "https://github.com/igorjs/ward/releases/download/v0.0.0/ward-0.0.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000"
    end
  end

  def install
    bin.install "bin/ward", "bin/wardd", "bin/ward-mcp"
  end

  service do
    run [opt_bin/"wardd"]
    keep_alive true
    working_dir Dir.home
    log_path var/"log/ward.log"
    error_log_path var/"log/ward.log"
    environment_variables WARD_LOG_LEVEL: "info"
  end

  def caveats
    <<~EOS
      On Linux, wardd needs read and write access to /dev/kvm:
        sudo usermod -aG kvm $USER
      The service logs to #{var}/log/ward.log.
    EOS
  end

  test do
    socket = testpath/"ward.sock"
    ENV["WARD_SOCKET"] = socket.to_s
    ENV["WARD_DATA_DIR"] = (testpath/"data").to_s
    log = testpath/"wardd.log"

    pid = spawn bin/"wardd", [:out, :err] => log.to_s
    begin
      20.times do
        break if socket.exist?

        sleep 0.5
      end
      assert_path_exists socket, "wardd did not create its socket:\n#{log.read}"

      assert_match "status:", shell_output("#{bin}/ward health")
    ensure
      Process.kill("TERM", pid)
      Process.wait(pid)
    end
  end
end
