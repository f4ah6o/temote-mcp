{
  description = "Sandboxed local-machine MCP server";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "temote-mcp";
            version = "0.1.0";
            src = self;

            # Refresh after every Cargo.lock change: run `nix build`, then copy
            # the "got:" hash from the mismatch error.
            cargoHash = pkgs.lib.fakeHash;

            nativeBuildInputs = with pkgs; [
              cmake
              makeWrapper
              perl
              pkg-config
            ];

            postInstall = ''
              wrapProgram $out/bin/temote-mcp \
                --prefix PATH : ${
                  pkgs.lib.makeBinPath (
                    [
                      pkgs.bash
                      pkgs.curl
                    ]
                    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.bubblewrap ]
                  )
                }
            '';

            meta = {
              description = "Sandboxed local-machine MCP server with out-of-band approvals";
              homepage = "https://github.com/openai/codex";
              license = pkgs.lib.licenses.asl20;
              mainProgram = "temote-mcp";
              platforms = supportedSystems;
            };
          };
        }
      );

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/temote-mcp";
          meta.description = "Run the temote-mcp stdio server";
        };
      });

      devShells = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          default = pkgs.mkShell {
            packages = with pkgs; [
              cargo
              cmake
              curl
              openssl
              perl
              pkg-config
              rustc
              rustfmt
            ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.bubblewrap ];
          };
        }
      );

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt-tree);
    };
}
