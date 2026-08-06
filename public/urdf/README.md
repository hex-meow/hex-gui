# URDF web assets

`packages/` is generated from the current `hex-descriptions/products/*` files.
Do not edit meshes or URDF files here by hand.

- `npm run urdf:sync` first runs `hex-descriptions/tools/validate.py`, then
  copies each product's URDF and `meshes/visual/` into a
  package/version/content-hash directory and regenerates the SHA-256 manifest.
  The content segment prevents an unchanged semantic version from mixing old
  browser-cached STL files with newly synchronized files.
- `npm run urdf:check-upstream` checks this checkout against the neighboring
  `hex-descriptions` working tree without changing files.
- `npm run urdf:verify` checks only committed files and the generated manifest;
  it is suitable for a standalone CI/release checkout and runs before builds.

The GUI deliberately does not vendor collision meshes because `urdf-loader`
renders visuals with collision parsing disabled. Compatibility for reviewed old
controller mesh paths is declared in `scripts/urdf-assets.config.json`; old
binary copies must not be added back here.

The current robot protocol does not carry a description version or asset hash.
The manifest therefore proves the GUI bundle's provenance, but it cannot yet
prove that a live controller's inline URDF was produced from the same version.
