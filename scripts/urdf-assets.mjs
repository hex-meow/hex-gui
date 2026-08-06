#!/usr/bin/env node

// Vendored web assets are generated from hex-descriptions. The source working
// tree (not its git HEAD) is intentional: mechanical changes are often reviewed
// in both repositories before the descriptions commit is published.

import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import {
  mkdir,
  readFile,
  readdir,
  rename,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";

const execFileAsync = promisify(execFile);

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const generatedRoot = path.join(repoRoot, "public", "urdf", "packages");
const manifestPath = path.join(repoRoot, "src", "generated", "urdf-assets.json");
const configPath = path.join(repoRoot, "scripts", "urdf-assets.config.json");

function usage() {
  console.error("usage: node scripts/urdf-assets.mjs <sync|check-upstream|verify> [--source PATH]");
  process.exitCode = 2;
}

function parseArgs(argv) {
  const command = argv[0];
  let source = path.resolve(repoRoot, "../hex-descriptions");
  for (let i = 1; i < argv.length; i += 1) {
    if (argv[i] === "--source" && argv[i + 1]) {
      source = path.resolve(repoRoot, argv[++i]);
    } else {
      throw new Error(`unknown argument: ${argv[i]}`);
    }
  }
  return { command, source };
}

function sha256(data) {
  return createHash("sha256").update(data).digest("hex");
}

function compareText(a, b) {
  return a < b ? -1 : a > b ? 1 : 0;
}

function topLevelScalar(yaml, key, file) {
  const match = yaml.match(new RegExp(`^${key}\\s*:\\s*([^#\\r\\n]+)`, "m"));
  if (!match) throw new Error(`${file}: missing top-level '${key}'`);
  let value = match[1].trim();
  if ((value.startsWith('"') && value.endsWith('"')) || (value.startsWith("'") && value.endsWith("'"))) {
    value = value.slice(1, -1);
  }
  if (!value) throw new Error(`${file}: empty top-level '${key}'`);
  return value;
}

function assertSafeRelative(relative, label) {
  const normalized = relative.replaceAll("\\", "/");
  if (path.posix.isAbsolute(normalized) || normalized.split("/").includes("..")) {
    throw new Error(`${label}: unsafe relative path '${relative}'`);
  }
  return normalized;
}

async function listFiles(root, relative = "") {
  const dir = path.join(root, relative);
  const entries = await readdir(dir, { withFileTypes: true });
  const files = [];
  for (const entry of entries.sort((a, b) => compareText(a.name, b.name))) {
    const child = path.posix.join(relative.replaceAll("\\", "/"), entry.name);
    if (entry.isDirectory()) files.push(...await listFiles(root, child));
    else if (entry.isFile()) files.push(child);
    else throw new Error(`${path.join(root, child)}: symlinks and special files are not allowed`);
  }
  return files;
}

function visualMeshUris(xml, label) {
  const uris = [];
  const visuals = [...xml.matchAll(/<visual\b[\s\S]*?<\/visual>/gi)];
  if (visuals.length === 0) throw new Error(`${label}: URDF has no <visual> elements`);
  for (const [visualIndex, visual] of visuals.entries()) {
    const geometries = [...visual[0].matchAll(/<geometry\b[^>]*>([\s\S]*?)<\/geometry>/gi)];
    if (geometries.length !== 1) {
      throw new Error(`${label}: visual ${visualIndex} must contain exactly one non-empty <geometry>`);
    }
    const geometryBody = geometries[0][1].replace(/<!--[\s\S]*?-->/g, "");
    const geometryChildren = [...geometryBody.matchAll(/<([A-Za-z_][\w:.-]*)\b[^>]*>/g)];
    if (geometryChildren.length !== 1 || !new Set(["mesh", "box", "sphere", "cylinder"]).has(geometryChildren[0][1].toLowerCase())) {
      throw new Error(`${label}: visual ${visualIndex} geometry must contain exactly one supported shape`);
    }
    for (const mesh of visual[0].matchAll(/<mesh\b[^>]*>/gi)) {
      const filename = mesh[0].match(/\bfilename\s*=\s*["']([^"']*)["']/i)?.[1]?.trim();
      if (!filename) throw new Error(`${label}: visual ${visualIndex} has a <mesh> without a non-empty filename`);
      uris.push(filename);
    }
  }
  return uris;
}

function productContentDigest(model, kind, version, packageName, files) {
  return sha256(JSON.stringify({ model, kind, version, package: packageName, files }));
}

function manifestDigest(manifestWithoutDigest) {
  return sha256(JSON.stringify(manifestWithoutDigest));
}

async function buildExpected(sourceRoot) {
  const productsRoot = path.join(sourceRoot, "products");
  const config = JSON.parse(await readFile(configPath, "utf8"));
  const productDirs = (await readdir(productsRoot, { withFileTypes: true }))
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort(compareText);

  const products = {};
  const packageRoots = {};
  const payloads = new Map();

  for (const productDir of productDirs) {
    const sourceProductRoot = path.join(productsRoot, productDir);
    const modelFile = path.join(sourceProductRoot, "model.yaml");
    let yaml;
    try {
      yaml = await readFile(modelFile, "utf8");
    } catch (error) {
      if (error?.code === "ENOENT") continue;
      throw error;
    }

    const model = topLevelScalar(yaml, "model", modelFile);
    const kind = topLevelScalar(yaml, "kind", modelFile);
    const version = topLevelScalar(yaml, "version", modelFile);
    const packageName = topLevelScalar(yaml, "package", modelFile);
    const urdfRelative = assertSafeRelative(topLevelScalar(yaml, "urdf", modelFile), modelFile);

    if (!/^[A-Za-z0-9_]+$/.test(packageName)) throw new Error(`${modelFile}: invalid package '${packageName}'`);
    if (!/^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(version)) throw new Error(`${modelFile}: invalid version '${version}'`);
    if (products[model]) throw new Error(`${modelFile}: duplicate model '${model}'`);
    if (packageRoots[packageName]) throw new Error(`${modelFile}: duplicate package '${packageName}'`);

    const packageXmlPath = path.join(sourceProductRoot, "package.xml");
    const packageXml = await readFile(packageXmlPath, "utf8");
    const packageXmlName = packageXml.match(/<name>\s*([^<]+?)\s*<\/name>/)?.[1];
    const packageXmlVersion = packageXml.match(/<version>\s*([^<]+?)\s*<\/version>/)?.[1];
    if (packageXmlName !== packageName) throw new Error(`${packageXmlPath}: package name '${packageXmlName}' != '${packageName}'`);
    if (packageXmlVersion !== version) throw new Error(`${packageXmlPath}: version '${packageXmlVersion}' != '${version}'`);

    const urdfPath = path.join(sourceProductRoot, urdfRelative);
    const urdf = await readFile(urdfPath, "utf8");
    const robotName = urdf.match(/<robot\b[^>]*\bname\s*=\s*["']([^"']+)["']/i)?.[1];
    if (robotName !== model) throw new Error(`${urdfPath}: robot name '${robotName}' != '${model}'`);

    const visualRoot = path.join(sourceProductRoot, "meshes", "visual");
    const visualFiles = (await listFiles(visualRoot)).map((file) => `meshes/visual/${file}`);
    const copiedFiles = [urdfRelative, ...visualFiles].sort(compareText);
    const copiedSet = new Set(copiedFiles);
    for (const uri of visualMeshUris(urdf, urdfPath)) {
      const match = uri.match(/^package:\/\/([^/]+)\/(.+)$/);
      if (!match) throw new Error(`${urdfPath}: visual mesh must use package://, got '${uri}'`);
      if (match[1] !== packageName) throw new Error(`${urdfPath}: visual mesh package '${match[1]}' != '${packageName}'`);
      if (!copiedSet.has(assertSafeRelative(match[2], urdfPath))) {
        throw new Error(`${urdfPath}: visual mesh '${uri}' is missing from the copied visual assets`);
      }
    }

    const files = [];
    const productPayloads = [];
    for (const relative of copiedFiles) {
      const sourcePath = path.join(sourceProductRoot, relative);
      const data = await readFile(sourcePath);
      files.push({ path: relative, bytes: data.byteLength, sha256: sha256(data) });
      productPayloads.push([relative, data]);
    }
    const contentSha256 = productContentDigest(model, kind, version, packageName, files);
    const contentId = contentSha256.slice(0, 16);
    const packageRoot = `/urdf/packages/${packageName}/${version}/${contentId}`;
    packageRoots[packageName] = packageRoot;
    for (const [relative, data] of productPayloads) {
      payloads.set(path.posix.join(packageName, version, contentId, relative), data);
    }
    products[model] = {
      kind,
      version,
      package: packageName,
      contentSha256,
      packageRoot,
      urdfUrl: `${packageRoot}/${urdfRelative}`,
      files,
    };
  }

  if (Object.keys(products).length === 0) throw new Error(`${productsRoot}: no products with model.yaml found`);

  const legacyMeshAliases = {};
  for (const packageName of Object.keys(config.legacyMeshAliases ?? {}).sort(compareText)) {
    const packageRoot = packageRoots[packageName];
    if (!packageRoot) throw new Error(`${configPath}: alias package '${packageName}' is not in the source products`);
    const product = Object.values(products).find((value) => value.package === packageName);
    const productFiles = new Set(product.files.map((file) => file.path));
    const aliases = config.legacyMeshAliases[packageName];
    for (const from of Object.keys(aliases).sort(compareText)) {
      const safeFrom = assertSafeRelative(from, configPath);
      const safeTo = assertSafeRelative(aliases[from], configPath);
      if (!productFiles.has(safeTo)) throw new Error(`${configPath}: alias target '${packageName}/${safeTo}' does not exist`);
      legacyMeshAliases[`${packageRoot}/${safeFrom}`] = `${packageRoot}/${safeTo}`;
    }
  }

  const body = { schemaVersion: 2, products, packageRoots, legacyMeshAliases };
  const manifest = { ...body, assetSetSha256: manifestDigest(body) };
  return { manifest, payloads };
}

async function writeExpected(expected) {
  const parent = path.dirname(generatedRoot);
  const tempRoot = path.join(parent, `.packages-tmp-${process.pid}`);
  const backupRoot = path.join(parent, `.packages-backup-${process.pid}`);
  const tempManifest = `${manifestPath}.tmp-${process.pid}`;
  const backupManifest = `${manifestPath}.backup-${process.pid}`;
  await rm(tempRoot, { recursive: true, force: true });
  await rm(backupRoot, { recursive: true, force: true });
  await rm(tempManifest, { force: true });
  await rm(backupManifest, { force: true });
  await mkdir(tempRoot, { recursive: true });
  try {
    for (const [relative, data] of expected.payloads) {
      const destination = path.join(tempRoot, relative);
      await mkdir(path.dirname(destination), { recursive: true });
      await writeFile(destination, data);
    }
    await mkdir(path.dirname(manifestPath), { recursive: true });
    await writeFile(tempManifest, `${JSON.stringify(expected.manifest, null, 2)}\n`);

    let hadOldRoot = false;
    let hadOldManifest = false;
    let installedRoot = false;
    let installedManifest = false;
    try {
      await rename(generatedRoot, backupRoot);
      hadOldRoot = true;
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
    try {
      try {
        await rename(manifestPath, backupManifest);
        hadOldManifest = true;
      } catch (error) {
        if (error?.code !== "ENOENT") throw error;
      }
      await rename(tempRoot, generatedRoot);
      installedRoot = true;
      await rename(tempManifest, manifestPath);
      installedManifest = true;
    } catch (error) {
      if (installedManifest) await rm(manifestPath, { force: true });
      if (hadOldManifest) await rename(backupManifest, manifestPath);
      if (installedRoot) await rm(generatedRoot, { recursive: true, force: true });
      if (hadOldRoot) await rename(backupRoot, generatedRoot);
      throw error;
    }
    if (hadOldRoot) await rm(backupRoot, { recursive: true, force: true });
    if (hadOldManifest) await rm(backupManifest, { force: true });
  } finally {
    await rm(tempRoot, { recursive: true, force: true });
    await rm(tempManifest, { force: true });
  }
}

async function readManifest() {
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  if (manifest.schemaVersion !== 2) throw new Error(`${manifestPath}: unsupported schemaVersion '${manifest.schemaVersion}'`);
  const { assetSetSha256, ...body } = manifest;
  const actualDigest = manifestDigest(body);
  if (assetSetSha256 !== actualDigest) {
    throw new Error(`${manifestPath}: assetSetSha256 ${assetSetSha256} != ${actualDigest}`);
  }
  return manifest;
}

async function verifyGenerated(manifest) {
  const expectedPaths = new Set();
  for (const [model, product] of Object.entries(manifest.products)) {
    const contentSha256 = productContentDigest(model, product.kind, product.version, product.package, product.files);
    if (contentSha256 !== product.contentSha256) {
      throw new Error(`${model}: contentSha256 ${product.contentSha256} != ${contentSha256}`);
    }
    const wantedRoot = `/urdf/packages/${product.package}/${product.version}/${contentSha256.slice(0, 16)}`;
    if (product.packageRoot !== wantedRoot || manifest.packageRoots[product.package] !== wantedRoot) {
      throw new Error(`${model}: packageRoot is not content-addressed (${wantedRoot})`);
    }
    const relativeRoot = wantedRoot.replace(/^\/urdf\/packages\//, "");
    for (const file of product.files) {
      const relative = path.posix.join(relativeRoot, file.path);
      expectedPaths.add(relative);
      const data = await readFile(path.join(generatedRoot, relative));
      if (data.byteLength !== file.bytes) throw new Error(`${relative}: ${data.byteLength} bytes != manifest ${file.bytes}`);
      const digest = sha256(data);
      if (digest !== file.sha256) throw new Error(`${relative}: sha256 ${digest} != manifest ${file.sha256}`);
    }
  }
  const actualPaths = new Set(await listFiles(generatedRoot));
  for (const relative of expectedPaths) {
    if (!actualPaths.has(relative)) throw new Error(`${relative}: listed in manifest but missing from generated assets`);
  }
  for (const relative of actualPaths) {
    if (!expectedPaths.has(relative)) throw new Error(`${relative}: generated asset is not listed in manifest`);
  }
  for (const target of Object.values(manifest.legacyMeshAliases)) {
    const relative = target.replace(/^\/urdf\/packages\//, "");
    if (!actualPaths.has(relative)) throw new Error(`legacy alias target '${target}' is missing`);
  }
}

async function validateUpstream(sourceRoot) {
  const validator = path.join(sourceRoot, "tools", "validate.py");
  const productEntries = (await readdir(path.join(sourceRoot, "products"), { withFileTypes: true }))
    .filter((entry) => entry.isDirectory())
    .map((entry) => path.posix.join("products", entry.name))
    .sort();
  try {
    await execFileAsync("python3", [validator, ...productEntries], {
      cwd: sourceRoot,
      maxBuffer: 4 * 1024 * 1024,
    });
  } catch (error) {
    const details = [error?.stdout, error?.stderr].filter(Boolean).join("\n").trim();
    throw new Error(`hex-descriptions validator failed${details ? `:\n${details}` : ""}`, { cause: error });
  }
}

async function compareUpstream(expected) {
  const current = await readManifest();
  const wanted = `${JSON.stringify(expected.manifest, null, 2)}\n`;
  const got = `${JSON.stringify(current, null, 2)}\n`;
  if (got !== wanted) throw new Error("generated manifest differs from the current hex-descriptions working tree; run npm run urdf:sync");
  await verifyGenerated(current);
}

async function main() {
  let args;
  try {
    args = parseArgs(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    usage();
    return;
  }
  if (!new Set(["sync", "check-upstream", "verify"]).has(args.command)) {
    usage();
    return;
  }

  if (args.command === "verify") {
    const manifest = await readManifest();
    await verifyGenerated(manifest);
    console.log(`URDF assets OK: ${Object.keys(manifest.products).length} products, sha256 ${manifest.assetSetSha256}`);
    return;
  }

  const sourceStat = await stat(path.join(args.source, "products"));
  if (!sourceStat.isDirectory()) throw new Error(`${args.source}: products is not a directory`);
  await validateUpstream(args.source);
  const expected = await buildExpected(args.source);
  if (args.command === "sync") {
    await writeExpected(expected);
    console.log(`synced ${Object.keys(expected.manifest.products).length} URDF products from ${args.source}`);
  } else {
    await compareUpstream(expected);
    console.log(`URDF assets match ${args.source}`);
  }
}

main().catch((error) => {
  console.error(error?.stack ?? error);
  process.exitCode = 1;
});
