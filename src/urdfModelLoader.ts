import * as THREE from "three";
import { STLLoader } from "three/addons/loaders/STLLoader.js";
import URDFLoader from "urdf-loader";
import type { URDFRobot } from "urdf-loader";
import assetManifest from "./generated/urdf-assets.json" with { type: "json" };

export type UrdfFailureStage =
  | "urdf-fetch"
  | "urdf-parse"
  | "mesh-fetch"
  | "mesh-parse"
  | "material";

export interface UrdfFailure {
  stage: UrdfFailureStage;
  url: string;
  status?: number;
  cause: unknown;
}

export class UrdfLoadError extends Error {
  readonly failures: readonly UrdfFailure[];

  constructor(failures: readonly UrdfFailure[]) {
    const unique = dedupeUrdfFailures(failures);
    super(unique.map(formatFailure).join("\n") || "URDF 模型加载失败");
    this.name = "UrdfLoadError";
    this.failures = unique;
  }
}

export type UrdfSource =
  | { kind: "xml"; xml: string; label: string; workingPath?: string }
  | { kind: "url"; url: string };

export interface UrdfLoadResult {
  robot: URDFRobot;
  meshUrls: readonly string[];
}

export interface UrdfLoadJob {
  promise: Promise<UrdfLoadResult>;
  cancel(): void;
}

export interface BeginUrdfLoadOptions {
  source: UrdfSource;
  packages?: Readonly<Record<string, string>>;
  configureMaterial?: (material: THREE.Material, context: { url: string }) => void;
}

type GeneratedManifest = {
  products: Record<string, { kind: string; version: string; package: string; packageRoot: string; urdfUrl: string }>;
  packageRoots: Record<string, string>;
  legacyMeshAliases: Record<string, string>;
};

const manifest = assetManifest as GeneratedManifest;

/** Canonical package roots generated from the current hex-descriptions assets. */
export const URDF_PACKAGE_ROOTS: Readonly<Record<string, string>> = Object.freeze({ ...manifest.packageRoots });
export const DEFAULT_ARM_URDF_URL = manifest.products.firefly_y6.urdfUrl;
export const URDF_ASSET_PRODUCTS = manifest.products;

function causeMessage(cause: unknown): string {
  if (cause instanceof Error) return cause.message;
  if (typeof cause === "string") return cause;
  try { return JSON.stringify(cause); } catch { return String(cause); }
}

function formatFailure(failure: UrdfFailure): string {
  const status = failure.status == null ? "" : `HTTP ${failure.status} `;
  return `${failure.stage}: ${failure.url} — ${status}${causeMessage(failure.cause)}`;
}

export function dedupeUrdfFailures(failures: readonly UrdfFailure[]): UrdfFailure[] {
  const seen = new Set<string>();
  return failures.filter((failure) => {
    const key = `${failure.stage}\0${failure.url}\0${failure.status ?? ""}\0${causeMessage(failure.cause)}`;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

export function isUrdfAbort(error: unknown): boolean {
  return (typeof DOMException !== "undefined" && error instanceof DOMException)
    ? error.name === "AbortError"
    : error instanceof Error && error.name === "AbortError";
}

function abortError(): Error {
  try {
    return new DOMException("URDF load cancelled", "AbortError");
  } catch {
    const error = new Error("URDF load cancelled");
    error.name = "AbortError";
    return error;
  }
}

/**
 * Applies only reviewed compatibility aliases. In particular this supports the
 * pre-1.1 Firefly flat mesh paths and GP80 0.1's uppercase base_link.STL.
 */
export function resolveUrdfAssetUrl(url: string): string {
  const aliases = manifest.legacyMeshAliases;
  const absolute = /^[A-Za-z][A-Za-z\d+.-]*:/.test(url) || url.startsWith("//");
  const parsed = new URL(url, "http://urdf-assets.invalid");
  const mapped = aliases[parsed.pathname];
  if (!mapped) return url;
  parsed.pathname = mapped;
  if (absolute) return parsed.toString();
  return `${parsed.pathname}${parsed.search}${parsed.hash}`;
}

function validateUrdf(xml: string, label: string, packages: Readonly<Record<string, string>>) {
  const failures: UrdfFailure[] = [];
  const document = new DOMParser().parseFromString(xml, "text/xml");
  const parserError = document.querySelector("parsererror");
  if (parserError) {
    throw new UrdfLoadError([{
      stage: "urdf-parse",
      url: label,
      cause: new Error(parserError.textContent?.trim() || "XML 语法错误"),
    }]);
  }
  const robot = [...document.children].find((element) => element.nodeName.toLowerCase() === "robot");
  if (!robot) {
    throw new UrdfLoadError([{ stage: "urdf-parse", url: label, cause: new Error("XML 中没有 <robot> 根节点") }]);
  }

  const visuals = [...robot.querySelectorAll("visual")];
  if (visuals.length === 0) {
    failures.push({ stage: "urdf-parse", url: label, cause: new Error("URDF 没有任何 <visual>，不能生成可见预览") });
  }
  const visualMeshUris: string[] = [];
  visuals.forEach((visual, index) => {
    const geometries = [...visual.children].filter((element) => element.nodeName.toLowerCase() === "geometry");
    const visualLabel = `${label}#visual-${index}`;
    if (geometries.length !== 1 || geometries[0].children.length !== 1) {
      failures.push({
        stage: "urdf-parse",
        url: visualLabel,
        cause: new Error("每个 <visual> 必须包含且只包含一个非空 <geometry>"),
      });
      return;
    }
    const geometry = geometries[0].children[0];
    const geometryType = geometry.nodeName.toLowerCase();
    if (geometryType === "mesh") {
      const filename = geometry.getAttribute("filename")?.trim();
      if (!filename) {
        failures.push({ stage: "urdf-parse", url: visualLabel, cause: new Error("<mesh> 缺少非空 filename") });
      } else {
        visualMeshUris.push(filename);
      }
    } else if (!new Set(["box", "sphere", "cylinder"]).has(geometryType)) {
      failures.push({
        stage: "urdf-parse",
        url: visualLabel,
        cause: new Error(`不支持的 visual geometry <${geometryType}>`),
      });
    }
  });
  for (const uri of visualMeshUris) {
    const match = uri.match(/^package:\/\/([^/]+)\/(.+)$/);
    if (uri.startsWith("package://") && !match) {
      failures.push({ stage: "urdf-parse", url: uri, cause: new Error("畸形 package:// mesh URI") });
    } else if (match && !packages[match[1]]) {
      failures.push({
        stage: "urdf-parse",
        url: uri,
        cause: new Error(`未知 URDF package '${match[1]}'；本 GUI 没有该型号的 visual 资产`),
      });
    }
  }
  for (const texture of robot.querySelectorAll("texture[filename]")) {
    const uri = texture.getAttribute("filename")!;
    failures.push({
      stage: "material",
      url: uri,
      cause: new Error("当前可靠加载器不支持 URDF texture；请将材质颜色写入 URDF 或扩展资产清单"),
    });
  }
  if (failures.length) throw new UrdfLoadError(failures);
  return { document, visualMeshUris };
}

async function fetchChecked(url: string, signal: AbortSignal, stage: "urdf-fetch" | "mesh-fetch"): Promise<Response> {
  try {
    const response = await fetch(url, { signal });
    if (!response.ok) {
      throw new UrdfLoadError([{
        stage,
        url,
        status: response.status,
        cause: new Error(response.statusText || "request failed"),
      }]);
    }
    return response;
  } catch (error) {
    if (isUrdfAbort(error) || error instanceof UrdfLoadError) throw error;
    throw new UrdfLoadError([{ stage, url, cause: error }]);
  }
}

type MeshRecord = {
  requestedUrl: string;
  url: string;
  material: THREE.Material;
  onComplete: (object: THREE.Object3D | null, error?: Error) => void;
  geometry: THREE.BufferGeometry | null;
  promise: Promise<void>;
};

function attachedMaterials(root: THREE.Object3D | null): Set<THREE.Material> {
  const materials = new Set<THREE.Material>();
  root?.traverse((object) => {
    const mesh = object as THREE.Mesh;
    if (!mesh.isMesh) return;
    const value = mesh.material;
    if (Array.isArray(value)) value.forEach((material) => materials.add(material));
    else if (value) materials.add(value);
  });
  return materials;
}

function disposeQueued(records: readonly MeshRecord[], robot: THREE.Object3D | null) {
  const attached = attachedMaterials(robot);
  const queuedMaterials = new Set(records.map((record) => record.material));
  for (const record of records) {
    record.geometry?.dispose();
    record.geometry = null;
  }
  disposeObject3D(robot);
  for (const material of queuedMaterials) {
    if (!attached.has(material)) material.dispose();
  }
}

async function runUrdfLoad(options: BeginUrdfLoadOptions, controller: AbortController): Promise<UrdfLoadResult> {
  const { signal } = controller;
  const packages = options.packages ?? URDF_PACKAGE_ROOTS;
  let xml: string;
  let label: string;
  let workingPath = "";

  if (options.source.kind === "url") {
    label = options.source.url;
    const response = await fetchChecked(options.source.url, signal, "urdf-fetch");
    try {
      xml = await response.text();
    } catch (error) {
      if (isUrdfAbort(error)) throw error;
      throw new UrdfLoadError([{ stage: "urdf-fetch", url: label, cause: error }]);
    }
    workingPath = options.source.url.slice(0, options.source.url.lastIndexOf("/") + 1);
  } else {
    ({ xml, label } = options.source);
    workingPath = options.source.workingPath ?? "";
  }
  if (signal.aborted) throw abortError();
  const validated = validateUrdf(xml, label, packages);

  const loader = new URDFLoader();
  loader.packages = { ...packages };
  const records: MeshRecord[] = [];

  // urdf-loader 0.13's declaration omits the material argument, while its
  // runtime callback has four parameters. Keep the cast local to this adapter.
  (loader as unknown as { loadMeshCb: (
    url: string,
    manager: THREE.LoadingManager,
    material: THREE.Material,
    onComplete: (object: THREE.Object3D | null, error?: Error) => void,
  ) => void }).loadMeshCb = (requestedUrl, _manager, material, onComplete) => {
    const url = resolveUrdfAssetUrl(requestedUrl);
    const record: MeshRecord = {
      requestedUrl,
      url,
      material,
      onComplete,
      geometry: null,
      promise: Promise.resolve(),
    };
    record.promise = (async () => {
      if (!/\.stl(?:[?#]|$)/i.test(url)) {
        throw new UrdfLoadError([{ stage: "mesh-parse", url, cause: new Error("只支持 STL visual mesh") }]);
      }
      const response = await fetchChecked(url, signal, "mesh-fetch");
      const contentType = response.headers.get("content-type") ?? "";
      if (/\btext\/html\b/i.test(contentType)) {
        throw new UrdfLoadError([{
          stage: "mesh-parse",
          url,
          cause: new Error("服务器返回了 HTML 而不是 STL；通常表示静态资源路径不存在并被页面 fallback 接管"),
        }]);
      }
      let buffer: ArrayBuffer;
      try {
        buffer = await response.arrayBuffer();
      } catch (error) {
        if (isUrdfAbort(error)) throw error;
        throw new UrdfLoadError([{ stage: "mesh-fetch", url, cause: error }]);
      }
      if (signal.aborted) throw abortError();
      try {
        record.geometry = new STLLoader().parse(buffer);
      } catch (error) {
        throw new UrdfLoadError([{ stage: "mesh-parse", url, cause: error }]);
      }
    })();
    records.push(record);
  };

  let robot: URDFRobot | null = null;
  try {
    robot = (loader.parse as unknown as (content: Document, workingPath?: string) => URDFRobot)(validated.document, workingPath);
  } catch (error) {
    controller.abort();
    await Promise.allSettled(records.map((record) => record.promise));
    disposeQueued(records, robot);
    if (isUrdfAbort(error)) throw error;
    throw new UrdfLoadError([{ stage: "urdf-parse", url: label, cause: error }]);
  }

  const expectedMeshCount = validated.visualMeshUris.length;
  if (expectedMeshCount !== records.length) {
    controller.abort();
    await Promise.allSettled(records.map((record) => record.promise));
    disposeQueued(records, robot);
    throw new UrdfLoadError([{
      stage: "urdf-parse",
      url: label,
      cause: new Error(`visual mesh 数量不一致：XML ${expectedMeshCount}，loader ${records.length}`),
    }]);
  }

  const settled = await Promise.allSettled(records.map((record) => record.promise));
  if (signal.aborted) {
    disposeQueued(records, robot);
    throw abortError();
  }
  const failures = settled.flatMap((result) => {
    if (result.status === "fulfilled") return [];
    if (result.reason instanceof UrdfLoadError) return [...result.reason.failures];
    if (isUrdfAbort(result.reason)) return [];
    return [{ stage: "mesh-fetch" as const, url: label, cause: result.reason }];
  });
  if (failures.length) {
    disposeQueued(records, robot);
    throw new UrdfLoadError(failures);
  }

  try {
    for (const record of records) {
      const material = record.material.clone();
      try {
        options.configureMaterial?.(material, { url: record.url });
        const mesh = new THREE.Mesh(record.geometry!, material);
        record.geometry = null; // ownership moved into robot through onComplete
        record.onComplete(mesh);
      } catch (error) {
        material.dispose();
        throw error;
      }
    }
  } catch (error) {
    disposeQueued(records, robot);
    throw new UrdfLoadError([{ stage: "material", url: label, cause: error }]);
  }

  // Parser materials are not attached when a mesh callback supplies a cloned
  // material. Preserve any that are also used by primitive geometry.
  const inUse = attachedMaterials(robot);
  for (const material of new Set(records.map((record) => record.material))) {
    if (!inUse.has(material)) material.dispose();
  }
  return { robot, meshUrls: records.map((record) => record.url) };
}

export function beginUrdfLoad(options: BeginUrdfLoadOptions): UrdfLoadJob {
  const controller = new AbortController();
  return {
    promise: runUrdfLoad(options, controller),
    cancel: () => controller.abort(),
  };
}

/** Dispose one or more possibly-sharing object trees exactly once. */
export function disposeObject3D(...roots: Array<THREE.Object3D | null | undefined>) {
  const geometries = new Set<THREE.BufferGeometry>();
  const materials = new Set<THREE.Material>();
  const textures = new Set<THREE.Texture>();
  for (const root of roots) {
    if (!root) continue;
    root.removeFromParent();
    root.traverse((object) => {
      const mesh = object as THREE.Mesh;
      if (!mesh.isMesh) return;
      if (mesh.geometry) geometries.add(mesh.geometry);
      const value = mesh.material;
      if (Array.isArray(value)) value.forEach((material) => materials.add(material));
      else if (value) materials.add(value);
    });
  }
  for (const material of materials) {
    for (const value of Object.values(material)) {
      if (value instanceof THREE.Texture) textures.add(value);
    }
  }
  textures.forEach((texture) => texture.dispose());
  materials.forEach((material) => material.dispose());
  geometries.forEach((geometry) => geometry.dispose());
}

export const URDF_RETRY_DELAYS_MS = [1000, 3000, 10000] as const;

/** failureCount starts at 1; null means automatic retries are exhausted. */
export function urdfRetryDelayMs(failureCount: number): number | null {
  return URDF_RETRY_DELAYS_MS[failureCount - 1] ?? null;
}
