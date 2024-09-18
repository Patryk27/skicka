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
            sha256 = "sha256-VZZnlyP69+Y3crrLHQyJirqlHrTtGTsyiSnZB8jEvVo=";
          };

          naersk' = pkgs.callPackage naersk {
            cargo = toolchain;
            rustc = toolchain;
          };

        in
        {
          packages = {
            default = naersk'.buildPackage {
              src = ./.;
            };
          };
        }
      );

      nixosModules = {
        default = { config, lib, pkgs, ... }:
          with lib;
          let cfg = config.services.skicka;
          in
          {
            options = {
              services.skicka = {
                enable = mkEnableOption "Send files between machines";

                package = mkOption {
                  type = types.package;
                  default = self.packages.${pkgs.system}.default;
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

      nixosConfigurations = {
        container =
          nixpkgs.lib.nixosSystem {
            system = "x86_64-linux";

            modules = [
              self.nixosModules.default

              ({ pkgs, ... }: {
                boot = {
                  isContainer = true;
                };

                environment = {
                  systemPackages = with pkgs; [
                    htop
                  ];
                };

                networking = {
                  firewall = {
                    enable = false;
                  };
                };

                services = {
                  caddy = {
                    enable = true;

                    extraConfig = ''
                      http://10.233.1.2:80 {
                        reverse_proxy http://127.0.0.1:8080
                      }
                    '';
                  };

                  skicka = {
                    enable = true;
                    listen = "0.0.0.0:8080";
                    motto = "Hello, World!";
                  };
                };

                system = {
                  stateVersion = "24.05";
                };
              })
            ];
          };
      };

    in
    packages
    // { inherit nixosModules; }
    // { inherit nixosConfigurations; };
}
