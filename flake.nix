{
  description = "A rotating file with customizable rotation behavior";

  inputs = {
    # The main nixpkgs repository of package definitions.
    nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";

    # A few helper functions for writing flakes.
    utils.url = "github:numtide/flake-utils";

    # Allows selecting a specific version of the Rust toolchain.
    fenix.url = "github:nix-community/fenix";
    fenix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    { self
    , nixpkgs
    , utils
    , fenix
    } @ inputs:
    let
      overlays = [
        # The overlay from the fenix flake (an input to this flake) allows us to
        # select a version of Rust that matches the one in the ./rust-toolchain
        # file.
        fenix.overlays.default

        # Use the overlay just above this (which puts `fenix` into `final`) to
        # set up a rust-toolchain package using the same version as in
        # ./rust-toolchain, plus nightly rustfmt.
        (final: prev:
          let
            inherit (final.lib.strings) fileContents removeSuffix;

            fileContents' = path: removeSuffix "\r" (fileContents path);

            # Produce a toolchain of the stable version specified in the
            # ./rust-toolchain file.
            stable = final.fenix.toolchainOf {
              channel = fileContents' ./rust-toolchain;
              sha256 = "sha256-e4mlaJehWBymYxJGgnbuCObVlqMlQSilZ8FljG9zPHY=";
            };

            # Get the `rustfmt` component from the nightly toolchain.
            inherit (final.fenix.default) rustfmt;
          in
          {
            rust-toolchain = (final.fenix.combine [
              rustfmt
              stable.toolchain
            ]);
          })
      ];

      pkgsFor = system: import nixpkgs {
        inherit system overlays;

        config = {
          # Allow the installation of packages without free software licenses.
          allowUnfree = true;
        };
      };

      # The current list of systems we support, for both developer and CI
      # machines.
      supportedSystems = with utils.lib.system; [
        aarch64-darwin
        x86_64-darwin
        x86_64-linux
      ];
    in
    (utils.lib.eachSystem supportedSystems (system:
    let
      pkgs = pkgsFor system;
    in
    {
      inherit overlays;

      devShells.default = pkgs.mkShell {
        name = "rotating-file";

        packages = with pkgs; [
          rust-toolchain
          libiconv
        ];
      };

      # Checks to run when `nix flake check` is run.
      checks = {
        # Check to make sure that all .nix files in the repository are formatted
        # correctly according to nixpkgs-fmt.
        nix-format = pkgs.runCommand "check-nix-format" { } ''
          ${pkgs.nixpkgs-fmt}/bin/nixpkgs-fmt --check ${./.}
          touch $out
        '';
      };

      # Set the formatter, so `nix fmt` can be run instead of manually invoking
      # `nixpkgs-fmt .`.
      formatter = pkgs.nixpkgs-fmt;
    }));
}
