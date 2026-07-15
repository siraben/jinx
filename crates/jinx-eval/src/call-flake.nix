# This is a helper to callFlake() to lazily fetch flake inputs.

# The contents of the lock file, in JSON format.
lockFileStr:

# A mapping of lock file node IDs to { sourceInfo, subdir } attrsets,
# with sourceInfo.outPath providing an SourceAccessor to a previously
# fetched tree. This is necessary for possibly unlocked inputs, in
# particular the root input, but also --override-inputs pointing to
# unlocked trees.
overrides:

# This is `prim_fetchFinalTree`.
fetchTreeFinal:

let
  inherit (builtins) mapAttrs;

  rawLockFile = builtins.fromJSON lockFileStr;

  # Return one representative node name for each equivalent key. Lock files
  # retain duplicate nodes for fidelity; this evaluation-only rewrite shares
  # their lazy result graph without changing the on-disk format.
  deduplicationMap = f: m:
    builtins.listToAttrs (builtins.attrValues (builtins.mapAttrs
      (name: value: { name = f value; value = name; })
      m
    ));

  # Locked inputs with identical fetch descriptions are equivalent. An
  # override deliberately makes its node unique so overriding one input never
  # changes another input that happened to be equivalent before the override.
  nodeDeduplicationKey = { nodeKey, node }:
    if overrides ? ${nodeKey}
    then builtins.toJSON { overriddenAs = nodeKey; }
    else builtins.toJSON node.locked;

  nodeKeyByDeduplicationKey =
    deduplicationMap nodeDeduplicationKey
      (builtins.mapAttrs (nodeKey: node: { inherit nodeKey node; }) rawLockFile.nodes);

  # Resolve follows paths before choosing the representative, preserving the
  # existing follows semantics while sharing their final locked node.
  deduplicateInputName = inputSpec:
    let
      resolvedInputName = resolveInput inputSpec;
      resolvedInputDeduplicationKey = nodeDeduplicationKey {
        nodeKey = resolvedInputName;
        node = lockFile.nodes.${resolvedInputName};
      };
    in
    nodeKeyByDeduplicationKey.${resolvedInputDeduplicationKey};

  lockFile = rawLockFile // {
    nodes = builtins.mapAttrs
      (key: node: node // {
        inputs = builtins.mapAttrs (_: deduplicateInputName) (node.inputs or { });
      })
      rawLockFile.nodes;
  };

  # Resolve a input spec into a node name. An input spec is
  # either a node name, or a 'follows' path from the root
  # node.
  resolveInput =
    inputSpec: if builtins.isList inputSpec then getInputByPath lockFile.root inputSpec else inputSpec;

  # Follow an input attrpath (e.g. ["dwarffs" "nixpkgs"]) from the
  # root node, returning the final node.
  getInputByPath =
    nodeName: path:
    if path == [ ] then
      nodeName
    else
      getInputByPath
        # Since this could be a 'follows' input, call resolveInput.
        (resolveInput lockFile.nodes.${nodeName}.inputs.${builtins.head path})
        (builtins.tail path);

  allNodes = mapAttrs (
    key: node:
    let
      hasOverride = overrides ? ${key};
      isRelative = node.locked.type or null == "path" && builtins.substring 0 1 node.locked.path != "/";

      parentNode = allNodes.${getInputByPath lockFile.root node.parent};

      sourceInfo =
        if hasOverride then
          overrides.${key}.sourceInfo
        else if isRelative then
          parentNode.sourceInfo
        else
          # FIXME: remove obsolete node.info.
          # Note: lock file entries are always final.
          fetchTreeFinal (node.info or { } // removeAttrs node.locked [ "dir" ]);

      subdir = overrides.${key}.dir or node.locked.dir or "";

      outPath =
        if !hasOverride && isRelative then
          parentNode.outPath + (if node.locked.path == "" then "" else "/" + node.locked.path)
        else
          sourceInfo.outPath + (if subdir == "" then "" else "/" + subdir);

      flake = import (outPath + "/flake.nix");

      inputs = mapAttrs (inputName: inputSpec: allNodes.${resolveInput inputSpec}.result) (
        node.inputs or { }
      );

      outputs = flake.outputs (inputs // { self = result; });

      result =
        outputs
        # We add the sourceInfo attribute for its metadata, as they are
        # relevant metadata for the flake. However, the outPath of the
        # sourceInfo does not necessarily match the outPath of the flake,
        # as the flake may be in a subdirectory of a source.
        # This is shadowed in the next //
        // sourceInfo
        // {
          # This shadows the sourceInfo.outPath
          inherit outPath;

          inherit inputs;
          inherit outputs;
          inherit sourceInfo;
          _type = "flake";
        };

    in
    {
      result =
        if node.flake or true then
          assert builtins.isFunction flake.outputs;
          result
        else
          sourceInfo // { inherit sourceInfo outPath; };

      inherit outPath sourceInfo;
    }
  ) lockFile.nodes;

in
allNodes.${lockFile.root}.result
