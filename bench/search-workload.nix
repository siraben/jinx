# Mimics `nix search`'s traversal: recurse the package set through
# `recurseForDerivations`, and for every derivation force name + meta.description
# (exactly the metadata a search matches against). Returns total chars forced, so
# the whole walk is evaluated strictly. Run with -I nixpkgs=<path>.
let
  pkgs = import <nixpkgs> { };
  lib = pkgs.lib;
  isDrv = x: (builtins.tryEval (lib.isDerivation x)).value or false;
  recurse = x: builtins.isAttrs x
    && (builtins.tryEval (x.recurseForDerivations or false)).value or false;
  walk = attrs:
    builtins.foldl' (acc: name:
      let e = builtins.tryEval attrs.${name}; in
      if !e.success then acc
      else let v = e.value; in
        if isDrv v then
          let m = builtins.tryEval
            ((v.name or "") + "\n" + (v.meta.description or "")); in
          acc + (if m.success then builtins.stringLength m.value else 0)
        else if recurse v then acc + walk v
        else acc
    ) 0 (builtins.attrNames attrs);
in walk pkgs
