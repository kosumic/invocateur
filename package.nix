{
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage {
  pname = "invocateur";
  version = "0.1.0";
  src = lib.cleanSourceWith {
    src = ./.;
    filter = path: type: baseNameOf path != "target" && lib.cleanSourceFilter path type;
  };
  cargoLock.lockFile = ./Cargo.lock;
  meta = {
    description = "Summon coding agents in Herdr from your GitHub comments";
    license = lib.licenses.mit;
    mainProgram = "invocateur";
    platforms = lib.platforms.unix;
  };
}
