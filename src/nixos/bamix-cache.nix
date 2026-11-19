# Bamix binary cache machine (NixOS): harmonia + a locked-down push user.
#
# UNTESTED: written from the harmonia/NixOS docs, not evaluated. See docs/cache-setup.md.
#
# Two separate keys are involved:
#   - cache key  (secret stays on this machine): harmonia signs everything it serves with it;
#                clients/workers put its PUBLIC half in `trusted-public-keys`.
#   - worker key (secret stays on the build machine): workers sign results before pushing;
#                this machine trusts its PUBLIC half so the daemon accepts the pushed paths
#                without making the push user a `trusted-user`.
{ config, pkgs, lib, ... }:
let
  # Contents of worker.pub (nix-store --generate-binary-cache-key bamix-worker-1 ...)
  workerPublicKey = "bamix-worker-1:REPLACE_WITH_CONTENTS_OF_worker.pub";
  # Contents of ~/.ssh/bamix-push.pub from the build machine
  pushSshKey = "ssh-ed25519 REPLACE_WITH_bamix-push.pub bamix-worker";
in
{
  # --- serving -------------------------------------------------------------------------
  # Option names changed recently. If evaluation says `services.harmonia.cache` does not
  # exist, use `services.harmonia.enable = true;` instead (and `signKeyPaths` stays).
  services.harmonia.cache.enable = true;
  services.harmonia.signKeyPaths = [ "/var/lib/harmonia/cache-priv-key.pem" ];

  # Plain HTTP on the LAN for now. TLS (reverse proxy) is a Phase 11 item.
  networking.firewall.allowedTCPPorts = [ 5000 ];

  # --- receiving pushes ----------------------------------------------------------------
  nix.settings.trusted-public-keys = [ workerPublicKey ];

  services.openssh.enable = true;

  users.groups.bamix-push = { };
  users.users.bamix-push = {
    isSystemUser = true;
    group = "bamix-push";
    home = "/var/empty";
    shell = pkgs.bash; # sshd runs the forced command through the login shell
    openssh.authorizedKeys.keys = [
      # `restrict` disables forwarding/pty; the forced command means this key can only
      # speak the Nix daemon protocol, nothing else.
      ''restrict,command="${config.nix.package}/bin/nix-daemon --stdio" ${pushSshKey}''
    ];
  };
}
