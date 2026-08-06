import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import manifest from "../src/generated/urdf-assets.json" with { type: "json" };
import {
  DEFAULT_ARM_URDF_URL,
  resolveUrdfAssetUrl,
  urdfRetryDelayMs,
  URDF_ASSET_PRODUCTS,
  URDF_PACKAGE_ROOTS,
} from "../src/urdfModelLoader.ts";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

test("generated package map contains every current hex-descriptions product", () => {
  assert.deepEqual(
    Object.fromEntries(Object.entries(URDF_ASSET_PRODUCTS).map(([model, product]) => [model, product.version])),
    {
      firefly_y6: "1.1.0",
      gp80: "0.2.0",
      gr80: "0.0.1",
      lifta70: "0.0.1",
    },
  );
  assert.equal(DEFAULT_ARM_URDF_URL, URDF_ASSET_PRODUCTS.firefly_y6.urdfUrl);
  assert.equal(Object.keys(URDF_PACKAGE_ROOTS).length, 4);
});

test("reviewed legacy Firefly and GP80 mesh paths resolve to canonical assets", () => {
  const fireflyRoot = URDF_PACKAGE_ROOTS.xpkg_urdf_firefly_y6;
  assert.equal(
    resolveUrdfAssetUrl(`${fireflyRoot}/meshes/base_link.STL`),
    `${fireflyRoot}/meshes/visual/base_link.STL`,
  );
  assert.equal(
    resolveUrdfAssetUrl(`https://robot.local${fireflyRoot}/meshes/link_6.STL?rev=old`),
    `https://robot.local${fireflyRoot}/meshes/visual/link_6.STL?rev=old`,
  );
  const gp80Root = URDF_PACKAGE_ROOTS.hex_gp80_description;
  assert.equal(
    resolveUrdfAssetUrl(`${gp80Root}/meshes/visual/base_link.STL`),
    `${gp80Root}/meshes/visual/base_link.stl`,
  );
  assert.equal(
    resolveUrdfAssetUrl(`${fireflyRoot}/meshes/visual/base_link.STL`),
    `${fireflyRoot}/meshes/visual/base_link.STL`,
  );
});

test("vendored files match manifest hashes and every visual URI resolves", async () => {
  for (const product of Object.values(manifest.products)) {
    const listed = new Set(product.files.map((file) => file.path));
    assert.match(product.contentSha256, /^[0-9a-f]{64}$/);
    assert.ok(product.packageRoot.endsWith(`/${product.contentSha256.slice(0, 16)}`));
    const packageRelativeRoot = product.packageRoot.replace(/^\/urdf\/packages\//, "");
    for (const file of product.files) {
      const diskPath = path.join(
        repoRoot,
        "public",
        "urdf",
        "packages",
        packageRelativeRoot,
        file.path,
      );
      const data = await readFile(diskPath);
      assert.equal(data.byteLength, file.bytes, diskPath);
      assert.equal(createHash("sha256").update(data).digest("hex"), file.sha256, diskPath);
    }

    const urdfFile = product.files.find((file) => file.path.startsWith("urdf/"));
    assert.ok(urdfFile, `${product.package}: URDF missing from manifest`);
    const urdf = await readFile(path.join(
      repoRoot,
      "public",
      "urdf",
      "packages",
      packageRelativeRoot,
      urdfFile.path,
    ), "utf8");
    const visuals = [...urdf.matchAll(/<visual\b[\s\S]*?<\/visual>/gi)];
    assert.ok(visuals.length > 0, `${product.package}: URDF has no visual elements`);
    for (const [index, visual] of visuals.entries()) {
      const geometries = [...visual[0].matchAll(/<geometry\b[^>]*>([\s\S]*?)<\/geometry>/gi)];
      assert.equal(geometries.length, 1, `${product.package}: visual ${index} geometry count`);
      const shapes = [...geometries[0][1].replace(/<!--[\s\S]*?-->/g, "").matchAll(/<(mesh|box|sphere|cylinder)\b[^>]*>/gi)];
      assert.equal(shapes.length, 1, `${product.package}: visual ${index} shape count`);
      for (const meshTag of visual[0].matchAll(/<mesh\b[^>]*>/gi)) {
        assert.match(meshTag[0], /\bfilename\s*=\s*["'][^"']+["']/i, `${product.package}: mesh filename missing`);
      }
      for (const mesh of visual[0].matchAll(/<mesh\b[^>]*\bfilename\s*=\s*["']package:\/\/([^/]+)\/([^"']+)["']/gi)) {
        assert.equal(mesh[1], product.package);
        assert.ok(listed.has(mesh[2]), `${product.package}: ${mesh[2]} not vendored`);
      }
    }
  }
});

test("automatic URDF retries are bounded", () => {
  assert.equal(urdfRetryDelayMs(1), 1000);
  assert.equal(urdfRetryDelayMs(2), 3000);
  assert.equal(urdfRetryDelayMs(3), 10000);
  assert.equal(urdfRetryDelayMs(4), null);
});
