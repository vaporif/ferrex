{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
    crane,
    ...
  }: let
    systems = ["x86_64-linux" "aarch64-linux" "aarch64-darwin"];
    forAllSystems = f:
      nixpkgs.lib.genAttrs systems (system:
        f {
          pkgs = nixpkgs.legacyPackages.${system};
          fenixPkgs = fenix.packages.${system};
          craneLib =
            (crane.mkLib nixpkgs.legacyPackages.${system}).overrideToolchain
            fenix.packages.${system}.stable.toolchain;
        });

    perSystem = forAllSystems ({
      pkgs,
      fenixPkgs,
      craneLib,
    }: let
      src = craneLib.cleanCargoSource ./.;

      pname = "ferrex";

      commonArgs = {
        inherit src pname;
        strictDeps = true;
        nativeBuildInputs =
          [
            pkgs.pkg-config
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.openssl
          ];
        buildInputs =
          pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.openssl
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
            pkgs.apple-sdk_26
          ];
      };

      cargoArtifacts = craneLib.buildDepsOnly (commonArgs
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [pkgs.openssl];
        });

      pkg = (craneLib.buildPackage (commonArgs
        // {
          inherit cargoArtifacts;
          nativeBuildInputs =
            (commonArgs.nativeBuildInputs or [])
            ++ [pkgs.makeWrapper];
          cargoTestExtraArgs = "--workspace --exclude ferrex-embed --exclude ferrex-server --lib";
          postInstall = ''
            wrapProgram $out/bin/ferrex \
              --set ORT_DYLIB_PATH "${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}" \
              --prefix PATH : "${pkgs.qdrant}/bin" \
              ${pkgs.lib.optionalString pkgs.stdenv.isLinux "--prefix LD_LIBRARY_PATH : \"${pkgs.lib.makeLibraryPath [pkgs.openssl]}\""}
          '';
        }
        // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [pkgs.openssl];
        })).overrideAttrs {meta.mainProgram = "ferrex";};

      toolchain = fenixPkgs.stable.withComponents [
        "cargo"
        "clippy"
        "rustc"
        "rustfmt"
        "rust-src"
        "rust-analyzer"
        "llvm-tools"
      ];

      devToolchain =
        if pkgs.stdenv.isLinux
        then
          fenixPkgs.combine [
            fenixPkgs.stable.cargo
            fenixPkgs.stable.clippy
            fenixPkgs.stable.rustc
            fenixPkgs.stable.rustfmt
            fenixPkgs.stable.rust-src
            fenixPkgs.stable.rust-analyzer
            fenixPkgs.stable.llvm-tools
            fenixPkgs.targets."x86_64-unknown-linux-musl".stable.rust-std
            fenixPkgs.targets."aarch64-unknown-linux-musl".stable.rust-std
          ]
        else if pkgs.stdenv.isDarwin && pkgs.stdenv.isAarch64
        then
          fenixPkgs.combine [
            fenixPkgs.stable.cargo
            fenixPkgs.stable.clippy
            fenixPkgs.stable.rustc
            fenixPkgs.stable.rustfmt
            fenixPkgs.stable.rust-src
            fenixPkgs.stable.rust-analyzer
            fenixPkgs.stable.llvm-tools
            fenixPkgs.targets."x86_64-apple-darwin".stable.rust-std
          ]
        else toolchain;
    in {
      packages = {
        inherit pkg cargoArtifacts;
        default = pkg;
      };

      checks = {
        fmt = craneLib.cargoFmt {
          inherit src pname;
        };

        taplo =
          pkgs.runCommand "taplo-check" {
            nativeBuildInputs = [pkgs.taplo];
          } ''
            cd ${self}
            taplo check
            touch $out
          '';

        typos =
          pkgs.runCommand "typos-check" {
            nativeBuildInputs = [pkgs.typos];
          } ''
            cd ${self}
            typos
            touch $out
          '';

        nix-fmt =
          pkgs.runCommand "nix-fmt-check" {
            nativeBuildInputs = [pkgs.alejandra];
          } ''
            alejandra --check ${self}/flake.nix
            touch $out
          '';
      };

      devShells.default = pkgs.mkShell {
        packages =
          [
            devToolchain
            pkgs.cargo-nextest
            pkgs.cargo-llvm-cov
            pkgs.cargo-deny
            pkgs.taplo
            pkgs.typos
            pkgs.qdrant
            pkgs.onnxruntime
            pkgs.maturin
            pkgs.python3
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
            pkgs.pkg-config
            pkgs.openssl
            pkgs.pkgsCross.musl64.stdenv.cc
            pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.apple-sdk_26
          ];

        env =
          {
            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${devToolchain}/lib/rustlib/src/rust/library";
            ORT_DYLIB_PATH = "${pkgs.onnxruntime}/lib/libonnxruntime${pkgs.stdenv.hostPlatform.extensions.sharedLibrary}";
          }
          // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [pkgs.openssl pkgs.stdenv.cc.cc.lib];
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${pkgs.pkgsCross.musl64.stdenv.cc}/bin/${pkgs.pkgsCross.musl64.stdenv.cc.targetPrefix}cc";
            CC_x86_64_unknown_linux_musl = "${pkgs.pkgsCross.musl64.stdenv.cc}/bin/${pkgs.pkgsCross.musl64.stdenv.cc.targetPrefix}cc";
            CFLAGS_x86_64_unknown_linux_musl = "-U_FORTIFY_SOURCE";
            CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = "${pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc}/bin/${pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc.targetPrefix}cc";
            CC_aarch64_unknown_linux_musl = "${pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc}/bin/${pkgs.pkgsCross.aarch64-multiplatform-musl.stdenv.cc.targetPrefix}cc";
            CFLAGS_aarch64_unknown_linux_musl = "-U_FORTIFY_SOURCE";
            X86_64_UNKNOWN_LINUX_MUSL_OPENSSL_STATIC = "1";
            X86_64_UNKNOWN_LINUX_MUSL_OPENSSL_LIB_DIR = "${pkgs.pkgsCross.musl64.openssl.out}/lib";
            X86_64_UNKNOWN_LINUX_MUSL_OPENSSL_INCLUDE_DIR = "${pkgs.pkgsCross.musl64.openssl.dev}/include";
            AARCH64_UNKNOWN_LINUX_MUSL_OPENSSL_STATIC = "1";
            AARCH64_UNKNOWN_LINUX_MUSL_OPENSSL_LIB_DIR = "${pkgs.pkgsCross.aarch64-multiplatform-musl.openssl.out}/lib";
            AARCH64_UNKNOWN_LINUX_MUSL_OPENSSL_INCLUDE_DIR = "${pkgs.pkgsCross.aarch64-multiplatform-musl.openssl.dev}/include";
          };
      };
    });
  in {
    formatter = nixpkgs.lib.genAttrs systems (system: nixpkgs.legacyPackages.${system}.alejandra);
    packages = nixpkgs.lib.mapAttrs (_: s: s.packages) perSystem;
    checks = nixpkgs.lib.mapAttrs (_: s: s.checks) perSystem;
    devShells = nixpkgs.lib.mapAttrs (_: s: s.devShells) perSystem;
  };
}
