{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.gaffer;
in
{
  options.services.gaffer = {
    enable = lib.mkEnableOption "gaffer, the Elgato Key Light daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.gaffer;
      defaultText = lib.literalExpression "pkgs.gaffer";
      description = "The gaffer package to use.";
    };

    autoStart = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Start gaffer at login rather than on first use.

        gaffer is D-Bus activated, so this is not needed to make it work.
        Activation returns as soon as the daemon claims its bus name, which is
        before mDNS discovery has found anything — so the first command after a
        cold start can report no lights. Enable this when something is always
        watching, such as a status-bar applet, and discovery will have settled
        long before anything asks.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Allow inbound mDNS (UDP 5353), without which discovery finds nothing on
        most default NixOS firewall configurations.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # Picks up lib/systemd/user/gaffer.service from the package.
    systemd.packages = [ cfg.package ];

    # Picks up share/dbus-1/services/io.mineiro.gaffer.service, which is what
    # makes the daemon start on demand.
    services.dbus.packages = [ cfg.package ];

    systemd.user.services.gaffer = lib.mkIf cfg.autoStart {
      wantedBy = [ "default.target" ];
    };

    networking.firewall.allowedUDPPorts = lib.mkIf cfg.openFirewall [ 5353 ];
  };
}
