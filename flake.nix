{
  inputs = {
    fenix = {
      url = "github:nix-community/fenix";
      flake = false;
    };

    naersk = {
      url = "github:nix-community/naersk";

      inputs = {
        nixpkgs = {
          follows = "nixpkgs";
        };
      };
    };

    nixpkgs = {
      url = "github:NixOS/nixpkgs/nixos-unstable";
    };

    utils = {
      url = "github:numtide/flake-utils";
    };
  };

  outputs = { self, fenix, naersk, nixpkgs, utils }:
    let
      packages = utils.lib.eachDefaultSystem (system:
        let
          pkgs = import nixpkgs {
            inherit system;
          };

          fenix' = import fenix {
            inherit system;
          };

          toolchain = fenix'.fromToolchainFile {
            file = ./rust-toolchain;
            sha256 = "sha256-q6N3P+VoojTj1kcZYMCKYV33mpNOzRSGLxFbi7CXrMI=";
          };

          naersk' = pkgs.callPackage naersk {
            cargo = toolchain;
            rustc = toolchain;
          };

        in
        {
          defaultPackage = naersk'.buildPackage {
            src = ./.;
          };
        }
      );

      module = {
        nixosModule = { config, lib, pkgs, ... }:
          with lib;
          let cfg = config.services.skicka;
          in
          {
            options = {
              services.skicka = {
                enable = mkEnableOption "Send files between machines";

                package = mkOption {
                  type = types.package;
                  default = self.defaultPackage.${pkgs.system};
                };

                listen = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                };

                remote = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                };

                motto = mkOption {
                  type = types.nullOr types.str;
                  default = null;
                };
              };
            };

            config = mkIf cfg.enable {
              systemd.services.skicka = {
                script =
                  let
                    mottoFile = pkgs.writeText "motto.txt" (
                      (builtins.replaceStrings [ "\n" ] [ "\r\n" ] cfg.motto)
                        + "\r\n"
                    );

                  in
                  ''
                    ${cfg.package}/bin/skicka \
                        ${optionalString (cfg.listen != null) "--listen ${cfg.listen}"} \
                        ${optionalString (cfg.remote != null) "--remote ${cfg.remote}"} \
                        ${optionalString (cfg.motto != null) "--motto \"$(cat ${mottoFile})\""}
                  '';

                wantedBy = [ "multi-user.target" ];
                after = [ "network.target" ];
              };
            };
          };
      };

    in
    packages // module;
}
