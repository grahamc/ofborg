let
  hostpkgs = builtins.fetchurl {
    url = "https://github.com/NixOS/nixpkgs/archive/1dbbb021cd17c24bf4628ea663af7642272e4a39.tar.gz";
    sha256 = "08frw2plmf9k9w1idj6kaysa1izijdfk7lr6gq3a8iqba7lvc65z";
  };
  
  srcDef = builtins.fromJSON (builtins.readFile ./nixpkgs.json);

  inherit (hostpkgs) fetchFromGitHub fetchpatch fetchurl;
in import (hostpkgs.stdenv.mkDerivation {
  name = "ofborg-nixpkgs-${builtins.substring 0 10 srcDef.rev}";
  phases = [ "unpackPhase" "patchPhase" "moveToOut" ];

  src = fetchFromGitHub {
    owner = "NixOS";
    repo = "nixpkgs-channels";
    inherit (srcDef) rev sha256;
  };

  patches = [
  ];

  moveToOut = ''
    root=$(pwd)
    cd ..
    mv "$root" $out
  '';
})
