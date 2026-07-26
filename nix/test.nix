{ pkgs, self }:

# Boots a real NixOS guest to prove the one thing a sandboxed build cannot:
# that D-Bus activation actually starts the daemon.
#
# gaffer is a *user* service, which is the awkward part. NixOS tests drive the
# guest as root, and `systemctl --user` there talks to root's session, not a
# real user's. So the test lingers a user manager for alice with
# `loginctl enable-linger` and runs everything as her with XDG_RUNTIME_DIR
# pointed at her runtime directory; without that, this would be checking
# nothing at all while appearing to pass.
pkgs.testers.runNixOSTest {
  name = "gaffer-activation";

  nodes.machine =
    { ... }:
    {
      imports = [ self.nixosModules.gaffer ];

      services.gaffer = {
        enable = true;
        package = self.packages.${pkgs.system}.gaffer;
        # Deliberately left off: the point is to prove on-demand activation,
        # not that a unit told to start at boot starts at boot.
        autoStart = false;
      };

      users.users.alice = {
        isNormalUser = true;
        uid = 1000;
      };
    };

  testScript = ''
    machine.wait_for_unit("multi-user.target")

    # Give alice a user manager and session bus without a login.
    machine.succeed("loginctl enable-linger alice")
    machine.wait_for_unit("user@1000.service")

    def as_alice(cmd):
        return f"su alice -c 'XDG_RUNTIME_DIR=/run/user/1000 {cmd}'"

    with subtest("both programs are on PATH"):
        machine.succeed(as_alice("gaffer --version"))
        machine.succeed(as_alice("gafferd --version"))

    # Assert what systemd and dbus actually resolved, not where files landed.
    # nixpkgs relocates user units from lib/systemd/user to share/systemd/user
    # during fixup and leaves a compat symlink, so a path assertion here would
    # be testing nixpkgs' packaging conventions rather than this module.
    with subtest("systemd knows the unit"):
        machine.succeed(as_alice("systemctl --user cat gaffer.service"))

    with subtest("dbus knows the activation name"):
        machine.succeed(
            as_alice("busctl --user list --activatable | grep -w io.mineiro.gaffer")
        )

    with subtest("the daemon is not running yet"):
        machine.fail(as_alice("systemctl --user is-active gaffer.service"))

    with subtest("a client activates it on demand"):
        # No lights exist in a VM, so this must still succeed and simply report
        # none — the activation is what is under test, not discovery.
        machine.succeed(as_alice("gaffer list"))
        machine.wait_until_succeeds(as_alice("systemctl --user is-active gaffer.service"))

    with subtest("systemd owns the activated process, not dbus"):
        # The activation file defers to systemd; if that regressed, the daemon
        # would run as a child of dbus-daemon with no unit to show for it.
        machine.succeed(as_alice("systemctl --user show gaffer.service -p MainPID | grep -v MainPID=0"))

    with subtest("the API the clients bind to is present"):
        machine.succeed(
            as_alice("busctl --user introspect io.mineiro.gaffer /io/mineiro/gaffer/lights/all")
        )
  '';
}
