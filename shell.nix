{ mkShell, rustPlatform, cargo, rustc, clippy, rustfmt, rust-analyzer }:

mkShell {
  packages = [ cargo rustc clippy rustfmt rust-analyzer ];
  RUST_SRC_PATH = rustPlatform.rustLibSrc;
}
